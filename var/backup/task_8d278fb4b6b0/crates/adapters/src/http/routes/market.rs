#[derive(Debug, Deserialize)]
struct ListMarketsQuery {
    status: Option<String>,
}

#[utoipa::path(
    get,
    path = "/markets",
    tag = "markets",
    params(("status" = Option<String>, Query, description = "Filter by lifecycle status, e.g. live")),
    responses((status = 200, body = Vec<MarketSummaryDto>))
)]
async fn list_markets<S: Store + MarketQueries + 'static>(
    State(state): State<AppState<S>>,
    Query(q): Query<ListMarketsQuery>,
) -> ApiResult<Json<Vec<MarketSummaryDto>>> {
    let rows = state
        .inner
        .store
        .list_markets(q.status.as_deref())
        .await
        .map_err(application::error::AppError::from)?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let pool = state
            .inner
            .store
            .pool(row.id)
            .await
            .map_err(application::error::AppError::from)?;
        out.push(MarketSummaryDto::from_row(&row, &pool.pool));
    }
    Ok(Json(out))
}

#[utoipa::path(
    get,
    path = "/markets/{id_or_slug}",
    tag = "markets",
    params(("id_or_slug" = String, Path, description = "Market UUID or slug")),
    responses((status = 200, body = MarketSummaryDto), (status = 404, body = ApiError))
)]
async fn get_market<S: Store + MarketQueries + 'static>(
    State(state): State<AppState<S>>,
    Path(id_or_slug): Path<String>,
) -> ApiResult<Json<MarketSummaryDto>> {
    let row = state
        .inner
        .store
        .market_by_ref(&id_or_slug)
        .await
        .map_err(application::error::AppError::from)?;
    let now = state.inner.clock.now();
    let snapshot = state
        .inner
        .store
        .market_snapshot(row.id, now)
        .await
        .map_err(application::error::AppError::from)?;
    Ok(Json(MarketSummaryDto::from_snapshot(&row, &snapshot, now)))
}

#[derive(Debug, Deserialize)]
struct ChartQuery {
    bucket: u32,
    since: String,
}

#[utoipa::path(
    get,
    path = "/markets/{id}/chart",
    tag = "markets",
    params(
        ("id" = Uuid, Path, description = "Market id"),
        ("bucket" = u32, Query, description = "Bucket width in seconds"),
        ("since" = String, Query, description = "RFC3339 inclusive start"),
    ),
    responses((status = 200, body = Vec<PricePointDto>), (status = 422, body = ApiError))
)]
async fn market_chart<S: Store + MarketQueries + 'static>(
    State(state): State<AppState<S>>,
    Path(id): Path<Uuid>,
    Query(query): Query<ChartQuery>,
) -> ApiResult<Json<Vec<PricePointDto>>> {
    if !(1..=86_400).contains(&query.bucket) {
        return Err(ErrorResponse::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "InvalidBucket",
            "bucket must be between 1 and 86400 seconds",
        ));
    }
    let since =
        time::OffsetDateTime::parse(&query.since, &time::format_description::well_known::Rfc3339)
            .map_err(|_| {
            ErrorResponse::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "InvalidSince",
                "since must be RFC3339",
            )
        })?;
    let rows = state
        .inner
        .store
        .price_history(MarketId(id), query.bucket, since)
        .await
        .map_err(application::error::AppError::from)?;
    Ok(Json(rows.into_iter().map(PricePointDto::from).collect()))
}

#[derive(Debug, Deserialize)]
struct TapeQuery {
    #[serde(default = "default_tape_limit")]
    limit: u32,
}

const fn default_tape_limit() -> u32 {
    50
}

#[utoipa::path(
    get,
    path = "/markets/{id}/tape",
    tag = "markets",
    params(
        ("id" = Uuid, Path, description = "Market id"),
        ("limit" = Option<u32>, Query, description = "Newest rows, clamped to 200"),
    ),
    responses((status = 200, body = Vec<TapeRowDto>))
)]
async fn market_tape<S: Store + MarketQueries + 'static>(
    State(state): State<AppState<S>>,
    Path(id): Path<Uuid>,
    Query(query): Query<TapeQuery>,
) -> ApiResult<Json<Vec<TapeRowDto>>> {
    let rows = state
        .inner
        .store
        .tape(MarketId(id), query.limit.min(200))
        .await
        .map_err(application::error::AppError::from)?;
    Ok(Json(rows.into_iter().map(TapeRowDto::from).collect()))
}

#[derive(Debug, Deserialize)]
struct LeaderboardQuery {
    #[serde(default = "default_leaderboard_days")]
    days: u32,
    #[serde(default = "default_leaderboard_limit")]
    limit: u32,
}

const fn default_leaderboard_days() -> u32 {
    7
}

const fn default_leaderboard_limit() -> u32 {
    10
}

fn leaderboard_window<C: Clock>(
    clock: &C,
    query: &LeaderboardQuery,
) -> ApiResult<(time::OffsetDateTime, time::OffsetDateTime)> {
    if !(1..=3_650).contains(&query.days) || !(1..=100).contains(&query.limit) {
        return Err(ErrorResponse::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "InvalidWindow",
            "days must be 1..=3650 and limit must be 1..=100",
        ));
    }
    let until = clock.now();
    Ok((until - time::Duration::days(i64::from(query.days)), until))
}

#[utoipa::path(
    get,
    path = "/leaderboards/traders",
    tag = "economy",
    params(
        ("days" = Option<u32>, Query, description = "UTC lookback days, default 7"),
        ("limit" = Option<u32>, Query, description = "Row limit, default 10"),
    ),
    description = "Settled/realized PnL in window — not mark-to-market.",
    responses((status = 200, body = Vec<TraderRowDto>), (status = 422, body = ApiError))
)]
async fn leaderboard_traders<S: Store + MarketQueries + 'static>(
    State(state): State<AppState<S>>,
    Query(query): Query<LeaderboardQuery>,
) -> ApiResult<Json<Vec<TraderRowDto>>> {
    let (since, until) = leaderboard_window(&state.inner.clock, &query)?;
    let rows = state
        .inner
        .store
        .top_traders(since, until, query.limit)
        .await
        .map_err(application::error::AppError::from)?;
    Ok(Json(rows.into_iter().map(TraderRowDto::from).collect()))
}

#[utoipa::path(
    get,
    path = "/leaderboards/voters",
    tag = "economy",
    params(
        ("days" = Option<u32>, Query, description = "UTC lookback days, default 7"),
        ("limit" = Option<u32>, Query, description = "Row limit, default 10"),
    ),
    description = "Average vote score over markets scored; tier 0 and voters below the configured minimum are excluded.",
    responses((status = 200, body = Vec<VoterRowDto>), (status = 422, body = ApiError))
)]
async fn leaderboard_voters<S: Store + MarketQueries + 'static>(
    State(state): State<AppState<S>>,
    Query(query): Query<LeaderboardQuery>,
) -> ApiResult<Json<Vec<VoterRowDto>>> {
    let (since, until) = leaderboard_window(&state.inner.clock, &query)?;
    let rows = state
        .inner
        .store
        .top_voters(
            since,
            until,
            query.limit,
            state.inner.rep_config.leaderboard_min_scored,
        )
        .await
        .map_err(application::error::AppError::from)?;
    Ok(Json(rows.into_iter().map(VoterRowDto::from).collect()))
}

#[utoipa::path(
    post,
    path = "/trades/preview",
    tag = "trades",
    request_body = PreviewTradeRequest,
    responses((status = 200, body = TradePreviewDto), (status = 401, body = ApiError), (status = 404, body = ApiError))
)]
async fn preview_trade<S: Store + MarketQueries + 'static>(
    State(state): State<AppState<S>>,
    headers: HeaderMap,
    Json(body): Json<PreviewTradeRequest>,
) -> ApiResult<Json<TradePreviewDto>> {
    require_demo(&headers, &state.inner.demo_token)?;
    let uc = PreviewTrade {
        queries: &state.inner.store,
        clock: &state.inner.clock,
        rep_config: state.inner.rep_config,
        config: state.inner.config_reads.as_ref(),
    };
    let preview = uc
        .execute(PreviewTradeCmd {
            market_ref: body.market_ref,
            user_id: UserId(body.user_id),
            side: body.side.into(),
            action: body.action.into(),
            amount_micro: body.amount_micro,
        })
        .await?;
    Ok(Json(preview.into()))
}

#[utoipa::path(
    post,
    path = "/trades",
    tag = "trades",
    request_body = PlaceTradeRequest,
    responses(
        (status = 200, body = TradeReceiptDto),
        (status = 400, body = ApiError),
        (status = 401, body = ApiError),
        (status = 423, body = ApiError),
    )
)]
async fn place_trade<S: Store + MarketQueries + 'static>(
    State(state): State<AppState<S>>,
    headers: HeaderMap,
    Json(body): Json<PlaceTradeRequest>,
) -> ApiResult<Json<TradeReceiptDto>> {
    require_demo(&headers, &state.inner.demo_token)?;
    // Resolve market_ref → MarketId via lock-free query (PlaceTrade re-locks).
    let market = state
        .inner
        .store
        .market_by_ref(&body.market_ref)
        .await
        .map_err(application::error::AppError::from)?;
    let uc = PlaceTrade {
        store: &state.inner.store,
        clock: &state.inner.clock,
        rep_config: state.inner.rep_config,
    };
    let receipt = uc
        .execute(PlaceTradeCmd {
            market: market.id,
            user: UserId(body.user_id),
            side: body.side.into(),
            action: body.action.into(),
            amount_micro: body.amount_micro,
            idempotency_key: body.idempotency_key,
            run_id: body.run_id,
            pending_action_id: body.pending_action_id,
            expected_config_version: Some(body.expected_config_version),
        })
        .await?;
    Ok(Json(receipt.into()))
}

#[utoipa::path(
    post,
    path = "/votes",
    tag = "votes",
    request_body = CastVoteRequest,
    responses(
        (status = 200, body = VoteReceiptDto),
        (status = 401, body = ApiError),
        (status = 404, body = ApiError),
        (status = 409, body = ApiError),
        (status = 423, body = ApiError),
    )
)]
async fn cast_vote<S: Store + MarketQueries + 'static>(
    State(state): State<AppState<S>>,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    headers: HeaderMap,
    Json(body): Json<CastVoteRequest>,
) -> ApiResult<Json<VoteReceiptDto>> {
    require_demo(&headers, &state.inner.demo_token)?;
    let (cast_ip, device_hash) = vote_metadata(
        peer.map(|Extension(ConnectInfo(address))| address),
        &headers,
        &state.inner.vote_metadata_config,
    );
    let market = state
        .inner
        .store
        .market_by_ref(&body.market_ref)
        .await
        .map_err(application::error::AppError::from)?;
    let uc = CastVote {
        store: &state.inner.store,
        clock: &state.inner.clock,
        config: state.inner.vote_integrity_config,
    };
    let receipt = uc
        .execute(CastVoteCmd {
            market: market.id,
            user: UserId(body.user_id),
            side: body.side.into(),
            crowd_guess_pct: body.crowd_guess_pct,
            idempotency_key: body.idempotency_key,
            cast_ip,
            device_hash,
        })
        .await?;
    let _ = body.run_id; // reserved for agent causal chain (vote row linkage Phase 1.5+)
    Ok(Json(receipt.into()))
}

#[utoipa::path(
    get,
    path = "/users/{id}/positions",
    tag = "users",
    params(("id" = Uuid, Path, description = "User id")),
    responses((status = 200, body = Vec<PositionDto>))
)]
async fn user_positions<S: Store + MarketQueries + 'static>(
    State(state): State<AppState<S>>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Vec<PositionDto>>> {
    let views = state
        .inner
        .store
        .positions(UserId(id))
        .await
        .map_err(application::error::AppError::from)?;
    Ok(Json(views.into_iter().map(PositionDto::from).collect()))
}

#[derive(Debug, Deserialize)]
struct ByChannelQuery {
    channel: String,
    address: String,
}

#[utoipa::path(
    get,
    path = "/users/by-channel",
    tag = "users",
    params(
        ("channel" = String, Query, description = "e.g. imessage"),
        ("address" = String, Query, description = "phone E.164"),
    ),
    responses((status = 200, body = UserIdDto), (status = 404, body = ApiError))
)]
async fn user_by_channel<S: Store + MarketQueries + 'static>(
    State(state): State<AppState<S>>,
    Query(q): Query<ByChannelQuery>,
) -> ApiResult<Json<UserIdDto>> {
    let found = state
        .inner
        .store
        .user_by_channel(&q.channel, &q.address)
        .await
        .map_err(application::error::AppError::from)?;
    match found {
        Some(UserId(id)) => Ok(Json(UserIdDto { user_id: id })),
        None => Err(ErrorResponse::new(
            StatusCode::NOT_FOUND,
            "NotFound",
            "no user for channel address",
        )),
    }
}

#[utoipa::path(
    post,
    path = "/users",
    tag = "users",
    request_body = CreateUserRequest,
    responses(
        (status = 200, body = UserIdDto),
        (status = 422, body = ApiError),
    )
)]
async fn create_user<S: Store + MarketQueries + 'static>(
    State(state): State<AppState<S>>,
    Json(body): Json<CreateUserRequest>,
) -> ApiResult<Json<UserIdDto>> {
    let handle = body.handle.trim();
    let channel = body.channel.trim();
    let address = body.address.trim();
    if handle.is_empty() || channel.is_empty() || address.is_empty() {
        return Err(ErrorResponse::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "InvalidSignup",
            "handle, channel, and address must be non-empty",
        ));
    }
    let has_override = body.created_at_override.is_some() || body.rep_seed_micro.is_some();
    if has_override && !state.inner.staging_faucet {
        return Err(ErrorResponse::new(
            StatusCode::FORBIDDEN,
            "StagingOnly",
            "signup identity overrides require the two-factor staging arm",
        ));
    }
    if body.rep_seed_micro.is_some_and(|value| value < 0) {
        return Err(ErrorResponse::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "InvalidSignup",
            "rep_seed_micro must be non-negative",
        ));
    }
    let cmd = application::create_user::CreateUserCmd {
        handle: handle.to_string(),
        channel: Some((channel.to_string(), address.to_string())),
        idempotency_key: format!("signup:{channel}:{address}"),
        created_at_override: body.created_at_override,
        rep_seed_micro: body.rep_seed_micro,
    };
    let actor = if has_override {
        AdminContext::Admin {
            token_digest: "staging-signup-override".to_string(),
            role: application::model::AdminRole::Superadmin,
        }
    } else {
        AdminContext::Machine
    };
    let user = application::create_user::CreateUser {
        store: &state.inner.store,
    }
    .execute_as(cmd, &actor, state.inner.rep_config)
    .await?;
    Ok(Json(UserIdDto { user_id: user.0 }))
}

#[utoipa::path(
    post,
    path = "/admin/markets/{id}/advance",
    tag = "admin",
    params(("id" = Uuid, Path, description = "Market id")),
    request_body = AdvanceMarketRequest,
    responses(
        (status = 200, body = MarketAdvancedDto),
        (status = 401, body = ApiError),
        (status = 409, body = ApiError),
        (status = 422, body = ApiError),
    )
)]
async fn admin_advance<S: Store + MarketQueries + 'static>(
    State(state): State<AppState<S>>,
    // W1: the authenticated principal is extracted here; it flows into the
    // use case at the 6.5 barrier once W2's AdvanceMarket AdminContext seat
    // and real AuditWrite impls land (coordinator-sequenced).
    Extension(_actor): Extension<AdminContext>,
    Path(id): Path<Uuid>,
    Json(body): Json<AdvanceMarketRequest>,
) -> ApiResult<Json<MarketAdvancedDto>> {
    let event = parse_advance_event(&body.event)?;
    let uc = AdvanceMarket {
        store: &state.inner.store,
    };
    let receipt = uc
        .execute(AdvanceMarketCmd {
            market: MarketId(id),
            event,
            // Stable per (market, event) so admin retries replay cleanly.
            idempotency_key: format!(
                "advance:{id}:{key}",
                key = body.event.trim().to_ascii_lowercase()
            ),
        })
        .await?;
    Ok(Json(MarketAdvancedDto {
        market_id: receipt.market.0,
        from_state: receipt.from.into(),
        state: receipt.to.into(),
    }))
}

#[utoipa::path(
    post,
    path = "/admin/markets/{id}/resolve",
    tag = "admin",
    params(("id" = Uuid, Path, description = "Market id")),
    request_body = Option<ResolveMarketRequest>,
    responses(
        (status = 200, body = ResolveMarketDto),
        (status = 401, body = ApiError),
        (status = 409, body = ApiError),
        (status = 422, body = ApiError),
    )
)]
async fn admin_resolve<S: Store + MarketQueries + 'static>(
    State(state): State<AppState<S>>,
    Extension(actor): Extension<AdminContext>,
    Path(id): Path<Uuid>,
    body: Option<Json<ResolveMarketRequest>>,
) -> ApiResult<Json<ResolveMarketDto>> {
    let uc = ResolveMarket {
        store: &state.inner.store,
        clock: &state.inner.clock,
        config: state.inner.resolve_config,
        rep_config: state.inner.rep_config,
        integrity_config: state.inner.integrity_sweep_config,
        crash_point: state.inner.crash_point.as_ref(),
        actor,
    };
    let receipt = uc
        .execute(ResolveMarketCmd {
            market: MarketId(id),
            curator_override: body.map(|Json(body)| match body.decision {
                CuratorDecisionDto::ResolveAtTally => {
                    application::resolve_market::CuratorDecision::ResolveAtTally
                }
                CuratorDecisionDto::Void => application::resolve_market::CuratorDecision::Void,
            }),
        })
        .await?;
    let (state_dto, voided) = resolve_state(receipt.outcome);
    Ok(Json(ResolveMarketDto {
        market_id: receipt.market.0,
        state: state_dto,
        actual_yes_bps: receipt.final_yes_bps,
        voided,
        replayed: receipt.replayed,
        ledger_txn: receipt.ledger_txn,
    }))
}

fn resolve_state(outcome: ResolveOutcome) -> (MarketStateDto, bool) {
    match outcome {
        ResolveOutcome::Settled => (MarketStateDto::Paid, false),
        ResolveOutcome::HeldForReview { .. } | ResolveOutcome::CuratorRequired => {
            (MarketStateDto::Resolving, false)
        }
        ResolveOutcome::Voided => (MarketStateDto::Voided, true),
    }
}

#[utoipa::path(
    get,
    path = "/admin/markets/flagged",
    tag = "admin",
    responses(
        (status = 200, body = Vec<FlaggedMarketDto>),
        (status = 401, body = ApiError),
    )
)]
async fn admin_flagged_markets<S: Store + MarketQueries + 'static>(
    State(state): State<AppState<S>>,
) -> ApiResult<Json<Vec<FlaggedMarketDto>>> {
    let rows = state
        .inner
        .store
        .flagged_markets()
        .await
        .map_err(application::error::AppError::from)?;
    Ok(Json(rows.into_iter().map(FlaggedMarketDto::from).collect()))
}

#[derive(Debug, Deserialize)]
struct FeeSummaryQuery {
    #[serde(default = "default_fee_days")]
    days: u32,
}

const fn default_fee_days() -> u32 {
    30
}

#[utoipa::path(
    get,
    path = "/admin/fees/summary",
    tag = "admin",
    params(("days" = Option<u32>, Query, description = "UTC lookback days, default 30")),
    description = "Fees-account revenue split into trade fees and payout-transaction dust.",
    responses(
        (status = 200, body = Vec<DailyFeeRowDto>),
        (status = 401, body = ApiError),
        (status = 422, body = ApiError),
    )
)]
async fn admin_fee_summary<S: Store + MarketQueries + 'static>(
    State(state): State<AppState<S>>,
    Query(query): Query<FeeSummaryQuery>,
) -> ApiResult<Json<Vec<DailyFeeRowDto>>> {
    if !(1..=3_650).contains(&query.days) {
        return Err(ErrorResponse::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "InvalidWindow",
            "days must be 1..=3650",
        ));
    }
    let until = state.inner.clock.now();
    let since = until - time::Duration::days(i64::from(query.days));
    let rows = state
        .inner
        .store
        .fee_summary(since, until)
        .await
        .map_err(application::error::AppError::from)?;
    Ok(Json(rows.into_iter().map(DailyFeeRowDto::from).collect()))
}
