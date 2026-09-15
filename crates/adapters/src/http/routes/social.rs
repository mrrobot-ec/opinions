#[derive(Debug, Deserialize)]
struct CommentsQuery {
    sort: Option<String>,
    limit: Option<u32>,
    cursor: Option<String>,
    viewer_id: Option<Uuid>,
}

fn bounded_limit(value: Option<u32>, default: u32, maximum: u32) -> Result<u32, ErrorResponse> {
    let limit = value.unwrap_or(default);
    if limit == 0 || limit > maximum {
        return Err(ErrorResponse::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "InvalidLimit",
            &format!("limit must be in 1..={maximum}"),
        ));
    }
    Ok(limit)
}

fn encode_comment_cursor(cursor: CommentCursor) -> Result<String, ErrorResponse> {
    use time::format_description::well_known::Rfc3339;
    let raw = match cursor {
        CommentCursor::Hot {
            as_of,
            hot_score,
            created_at,
            id,
        } => format!(
            "{}|{hot_score}|{}|{}",
            as_of.format(&Rfc3339).map_err(cursor_error)?,
            created_at.format(&Rfc3339).map_err(cursor_error)?,
            id.0
        ),
        CommentCursor::Recent { created_at, id } => format!(
            "{}|{}",
            created_at.format(&Rfc3339).map_err(cursor_error)?,
            id.0
        ),
    };
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw))
}

fn decode_comment_cursor(sort: CommentSort, cursor: &str) -> Result<CommentCursor, ErrorResponse> {
    use time::format_description::well_known::Rfc3339;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(cursor)
        .map_err(cursor_error)?;
    let raw = std::str::from_utf8(&bytes).map_err(cursor_error)?;
    let fields = raw.split('|').collect::<Vec<_>>();
    match (sort, fields.as_slice()) {
        (CommentSort::Hot, [as_of, hot_score, created_at, id]) => Ok(CommentCursor::Hot {
            as_of: time::OffsetDateTime::parse(as_of, &Rfc3339).map_err(cursor_error)?,
            hot_score: hot_score.parse().map_err(cursor_error)?,
            created_at: time::OffsetDateTime::parse(created_at, &Rfc3339).map_err(cursor_error)?,
            id: CommentId(Uuid::parse_str(id).map_err(cursor_error)?),
        }),
        (CommentSort::Recent, [created_at, id]) => Ok(CommentCursor::Recent {
            created_at: time::OffsetDateTime::parse(created_at, &Rfc3339).map_err(cursor_error)?,
            id: CommentId(Uuid::parse_str(id).map_err(cursor_error)?),
        }),
        _ => Err(cursor_error("cursor field count does not match sort")),
    }
}

fn cursor_error(error: impl std::fmt::Display) -> ErrorResponse {
    ErrorResponse::new(
        StatusCode::UNPROCESSABLE_ENTITY,
        "InvalidCursor",
        &format!("invalid comment cursor: {error}"),
    )
}

fn comment_id_for(market: Uuid, user: Uuid, key: &str) -> CommentId {
    let digest = Sha256::digest(format!("{market}:{user}:{key}").as_bytes());
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    CommentId(Uuid::from_bytes(bytes))
}

fn comment_error(error: application::error::AppError) -> ErrorResponse {
    use application::error::AppError;
    match error {
        AppError::CommentBlocked(reason) => ErrorResponse::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            &format!("blocked_{reason}"),
            &format!("comment was blocked: {reason}"),
        ),
        AppError::ThreadTooDeep => ErrorResponse::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "thread_too_deep",
            &error.to_string(),
        ),
        AppError::CommentAlreadyVoted => {
            ErrorResponse::new(StatusCode::CONFLICT, "duplicate_vote", &error.to_string())
        }
        AppError::CommentNotVisible => {
            ErrorResponse::new(StatusCode::CONFLICT, "not_visible", &error.to_string())
        }
        AppError::ReporterNotQualified => {
            ErrorResponse::new(StatusCode::FORBIDDEN, "reporter_floor", &error.to_string())
        }
        AppError::ReportVelocityExceeded => ErrorResponse::new(
            StatusCode::TOO_MANY_REQUESTS,
            "report_velocity",
            &error.to_string(),
        ),
        other => other.into(),
    }
}

#[utoipa::path(get, path = "/markets/{id_or_slug}/comments", tag = "social", responses((status = 200, body = CommentPageDto)))]
async fn list_comments<S: Store + MarketQueries + SocialQueries + 'static>(
    State(state): State<AppState<S>>,
    Path(reference): Path<String>,
    Query(query): Query<CommentsQuery>,
    headers: HeaderMap,
) -> ApiResult<Json<CommentPageDto>> {
    let sort = match query.sort.as_deref().unwrap_or("hot") {
        "hot" => CommentSort::Hot,
        "recent" => CommentSort::Recent,
        _ => return Err(cursor_error("sort must be hot or recent")),
    };
    let viewer = if let Some(viewer) = query.viewer_id {
        require_demo(&headers, &state.inner.demo_token)?;
        Some(UserId(viewer))
    } else {
        None
    };
    let cursor = query
        .cursor
        .as_deref()
        .map(|cursor| decode_comment_cursor(sort, cursor))
        .transpose()?;
    let market = state
        .inner
        .store
        .market_by_ref(&reference)
        .await
        .map_err(application::error::AppError::from)?;
    let page = state
        .inner
        .store
        .comments(
            market.id,
            sort,
            viewer,
            bounded_limit(query.limit, 50, 100)?,
            cursor,
            state.inner.clock.now(),
        )
        .await
        .map_err(application::error::AppError::from)?;
    Ok(Json(CommentPageDto {
        comments: page.comments.into_iter().map(CommentDto::from).collect(),
        next_cursor: page.next.map(encode_comment_cursor).transpose()?,
    }))
}

#[utoipa::path(post, path = "/markets/{id}/comments", tag = "social", request_body = PostCommentRequest, responses((status = 201, body = CommentDto)))]
async fn post_comment<S: Store + MarketQueries + SocialQueries + 'static>(
    State(state): State<AppState<S>>,
    Path(market): Path<String>,
    headers: HeaderMap,
    Json(request): Json<PostCommentRequest>,
) -> ApiResult<Response> {
    require_demo(&headers, &state.inner.demo_token)?;
    let market = Uuid::parse_str(&market).map_err(|error| {
        ErrorResponse::new(
            StatusCode::NOT_FOUND,
            "NotFound",
            &format!("invalid market id: {error}"),
        )
    })?;
    let comment = comment_id_for(market, request.user_id, &request.idempotency_key);
    let receipt = PostComment {
        store: &state.inner.store,
        clock: &state.inner.clock,
        config: state.inner.social_config,
    }
    .execute(PostCommentCmd {
        comment,
        market: MarketId(market),
        author: UserId(request.user_id),
        parent: request.parent_id.map(CommentId),
        body: request.body,
    })
    .await
    .map_err(comment_error)?;
    let dto = state
        .inner
        .store
        .comment_view(comment, Some(UserId(request.user_id)))
        .await
        .map(CommentDto::from)
        .map_err(application::error::AppError::from)?;
    Ok((
        if receipt.replayed {
            StatusCode::OK
        } else {
            StatusCode::CREATED
        },
        Json(dto),
    )
        .into_response())
}

#[utoipa::path(post, path = "/comments/{id}/vote", tag = "social", request_body = VoteCommentRequest, responses((status = 200, body = CommentVoteDto)))]
async fn vote_comment<S: Store + MarketQueries + 'static>(
    State(state): State<AppState<S>>,
    Path(comment): Path<Uuid>,
    headers: HeaderMap,
    Json(request): Json<VoteCommentRequest>,
) -> ApiResult<Json<CommentVoteDto>> {
    require_demo(&headers, &state.inner.demo_token)?;
    if request.idempotency_key.is_empty() {
        return Err(ErrorResponse::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "InvalidIdempotencyKey",
            "idempotency_key must be non-empty",
        ));
    }
    let receipt = VoteComment {
        store: &state.inner.store,
    }
    .execute(VoteCommentCmd {
        comment: CommentId(comment),
        user: UserId(request.user_id),
        value: request.value,
    })
    .await
    .map_err(comment_error)?;
    Ok(Json(CommentVoteDto {
        score: receipt.score,
    }))
}

#[utoipa::path(post, path = "/comments/{id}/report", tag = "social", request_body = ReportCommentRequest, responses((status = 200, body = CommentReportDto)))]
async fn report_comment<S: Store + MarketQueries + 'static>(
    State(state): State<AppState<S>>,
    Path(comment): Path<Uuid>,
    headers: HeaderMap,
    Json(request): Json<ReportCommentRequest>,
) -> ApiResult<Json<CommentReportDto>> {
    require_demo(&headers, &state.inner.demo_token)?;
    let receipt = ReportComment {
        store: &state.inner.store,
        clock: &state.inner.clock,
        config: state.inner.social_config,
    }
    .execute(ReportCommentCmd {
        comment: CommentId(comment),
        reporter: UserId(request.user_id),
    })
    .await
    .map_err(comment_error)?;
    Ok(Json(CommentReportDto {
        reported: true,
        report_count: receipt.report_count,
    }))
}

#[derive(Debug, Deserialize)]
struct LimitQuery {
    limit: Option<u32>,
}

#[utoipa::path(get, path = "/markets/{id}/holders", tag = "social", responses((status = 200, body = HoldersDto)))]
async fn market_holders<S: Store + MarketQueries + SocialQueries + 'static>(
    State(state): State<AppState<S>>,
    Path(market): Path<Uuid>,
    Query(query): Query<LimitQuery>,
) -> ApiResult<Json<HoldersDto>> {
    let rows = state
        .inner
        .store
        .holders(MarketId(market), bounded_limit(query.limit, 10, 50)?)
        .await
        .map_err(application::error::AppError::from)?;
    Ok(Json(rows.into()))
}

#[utoipa::path(get, path = "/users/{id}/profile", tag = "users", responses((status = 200, body = UserProfileDto)))]
async fn user_profile<S: Store + MarketQueries + SocialQueries + 'static>(
    State(state): State<AppState<S>>,
    Path(user): Path<Uuid>,
) -> ApiResult<Json<UserProfileDto>> {
    let profile = state
        .inner
        .store
        .user_profile(UserId(user))
        .await
        .map_err(application::error::AppError::from)?;
    Ok(Json(profile.into()))
}

#[derive(Debug, Deserialize)]
struct NotificationsQuery {
    limit: Option<u32>,
    before_id: Option<i64>,
}

#[utoipa::path(get, path = "/users/{id}/notifications", tag = "notifications", responses((status = 200, body = NotificationPageDto)))]
async fn list_notifications<S: Store + MarketQueries + NotificationQueries + 'static>(
    State(state): State<AppState<S>>,
    Path(user): Path<Uuid>,
    Query(query): Query<NotificationsQuery>,
    headers: HeaderMap,
) -> ApiResult<Json<NotificationPageDto>> {
    require_demo(&headers, &state.inner.demo_token)?;
    let user = UserId(user);
    let notifications = state
        .inner
        .store
        .notifications(user, bounded_limit(query.limit, 50, 100)?, query.before_id)
        .await
        .map_err(application::error::AppError::from)?;
    let unread_count = state
        .inner
        .store
        .unread_count(user)
        .await
        .map_err(application::error::AppError::from)?;
    Ok(Json(NotificationPageDto {
        notifications: notifications
            .into_iter()
            .map(NotificationDto::from)
            .collect(),
        unread_count,
    }))
}

#[utoipa::path(post, path = "/users/{id}/notifications/read", tag = "notifications", request_body = MarkNotificationsReadRequest, responses((status = 200, body = UpdatedDto)))]
async fn mark_notifications_read<S: Store + MarketQueries + NotificationQueries + 'static>(
    State(state): State<AppState<S>>,
    Path(user): Path<Uuid>,
    headers: HeaderMap,
    Json(request): Json<MarkNotificationsReadRequest>,
) -> ApiResult<Json<UpdatedDto>> {
    require_demo(&headers, &state.inner.demo_token)?;
    let updated = state
        .inner
        .store
        .mark_notifications_read(UserId(user), &request.ids, state.inner.clock.now())
        .await
        .map_err(application::error::AppError::from)?;
    Ok(Json(UpdatedDto { updated }))
}

#[utoipa::path(get, path = "/users/{id}/notifications/unread_count", tag = "notifications", responses((status = 200, body = UnreadCountDto)))]
async fn notification_unread_count<S: Store + MarketQueries + NotificationQueries + 'static>(
    State(state): State<AppState<S>>,
    Path(user): Path<Uuid>,
    headers: HeaderMap,
) -> ApiResult<Json<UnreadCountDto>> {
    require_demo(&headers, &state.inner.demo_token)?;
    let unread_count = state
        .inner
        .store
        .unread_count(UserId(user))
        .await
        .map_err(application::error::AppError::from)?;
    Ok(Json(UnreadCountDto { unread_count }))
}

#[utoipa::path(get, path = "/admin/comments/reported", tag = "admin", responses((status = 200, body = Vec<ReportedCommentDto>)))]
async fn admin_reported_comments<S: Store + MarketQueries + SocialQueries + 'static>(
    State(state): State<AppState<S>>,
    Query(query): Query<LimitQuery>,
) -> ApiResult<Json<Vec<ReportedCommentDto>>> {
    let rows = state
        .inner
        .store
        .reported_comments(
            state.inner.social_config.report_shadow_threshold,
            bounded_limit(query.limit, 50, 100)?,
        )
        .await
        .map_err(application::error::AppError::from)?;
    Ok(Json(
        rows.into_iter().map(ReportedCommentDto::from).collect(),
    ))
}

#[utoipa::path(post, path = "/admin/comments/{id}/moderate", tag = "admin", request_body = ModerateCommentRequest, responses((status = 200, body = ModeratedCommentDto)))]
async fn admin_moderate_comment<S: Store + MarketQueries + 'static>(
    State(state): State<AppState<S>>,
    axum::extract::Extension(actor): axum::extract::Extension<application::model::AdminContext>,
    Path(comment): Path<Uuid>,
    Json(request): Json<ModerateCommentRequest>,
) -> ApiResult<Json<ModeratedCommentDto>> {
    let action = match request.status.as_str() {
        "visible" => ModerateCommentAction::Restore,
        "shadow" => ModerateCommentAction::Shadow,
        "blocked" => ModerateCommentAction::Block,
        _ => {
            return Err(ErrorResponse::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "InvalidModerationStatus",
                "status must be visible, shadow, or blocked",
            ));
        }
    };
    ModerateComment {
        store: &state.inner.store,
    }
    .execute_as(CommentId(comment), action, &actor)
    .await
    .map_err(comment_error)?;
    Ok(Json(ModeratedCommentDto {
        status: request.status,
    }))
}
