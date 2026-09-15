// ---------------------------------------------------------------------------
// OpenAPI doc (also used by the offline exporter)
// ---------------------------------------------------------------------------

#[derive(OpenApi)]
#[openapi(
    paths(
        healthz,
        list_markets,
        get_market,
        market_chart,
        market_tape,
        leaderboard_traders,
        leaderboard_voters,
        preview_trade,
        place_trade,
        cast_vote,
        user_positions,
        user_by_channel,
        create_user,
        admin_advance,
        admin_resolve,
        admin_flagged_markets,
        admin_fee_summary,
        list_comments,
        post_comment,
        vote_comment,
        report_comment,
        market_holders,
        user_profile,
        list_notifications,
        mark_notifications_read,
        notification_unread_count,
        admin_reported_comments,
        admin_moderate_comment,
        content::list_drafts,
        content::create_drafts,
        content::edit_draft,
        content::approve_draft,
        content::reject_draft,
        content::publish_now,
        video::serve_asset,
        video::share_card,
        ops_config::get_config,
        ops_config::set_config,
        ops_config::create_proposal,
        ops_config::confirm_proposal,
        ops_config::reject_proposal,
        ops_admin::invariants,
        ops_admin::audit_read,
        ops_admin::withdrawal_eligibility,
        ops_admin::faucet_deposit,
        ops_admin::unwind_propose,
        ops_admin::unwind_confirm,
        ops_admin::unwind_reject,
        ops_admin::remedial_credit_propose,
        ops_admin::remedial_credit_confirm,
        ops_admin::write_off_propose,
        ops_admin::write_off_confirm,
        ops_admin::publish_status,
        withdraw::request_withdraw,
        withdraw::get_withdrawal,
        withdraw::admin_approve,
        withdraw::admin_deny,
        withdraw::admin_propose,
        withdraw::admin_confirm,
        deposit_admin::admit_propose,
        deposit_admin::admit_confirm,
        deposit_admin::refund_propose,
        deposit_admin::refund_confirm,
    ),
    components(schemas(
        ApiError,
        SideDto,
        TradeActionDto,
        MarketStateDto,
        MarketSummaryDto,
        PricePointDto,
        TapeRowDto,
        TraderRowDto,
        VoterRowDto,
        DailyFeeRowDto,
        PreviewTradeRequest,
        TradePreviewDto,
        PlaceTradeRequest,
        TradeReceiptDto,
        CastVoteRequest,
        VoteReceiptDto,
        PositionDto,
        UserIdDto,
        CreateUserRequest,
        AdvanceMarketRequest,
        MarketAdvancedDto,
        ResolveMarketDto,
        ResolveMarketRequest,
        CuratorDecisionDto,
        IntegrityReportDto,
        FlaggedMarketDto,
        CommentDto,
        CommentPageDto,
        PostCommentRequest,
        VoteCommentRequest,
        CommentVoteDto,
        ReportCommentRequest,
        CommentReportDto,
        HolderDto,
        HoldersDto,
        ProfileTradeDto,
        ProfileVoteDto,
        VoterSummaryDto,
        UserProfileDto,
        NotificationDto,
        NotificationPageDto,
        MarkNotificationsReadRequest,
        UpdatedDto,
        UnreadCountDto,
        ReportedCommentDto,
        ModerateCommentRequest,
        ModeratedCommentDto,
        CreateDraftRequest,
        DraftTierDto,
        DraftSourceDto,
        DraftSpecDto,
        EditDraftRequest,
        ReviewDraftRequest,
        DraftDto,
        PublishDraftDto,
        ConfigEntryDto,
        ConfigSnapshotDto,
        SetConfigRequest,
        ConfigAppliedDto,
        CreateConfigProposalRequest,
        ConfigProposalDto,
        SettleConfigProposalRequest,
        InvariantIdentityDto,
        InvariantReportDto,
        WithdrawalEligibilityDto,
        FaucetDepositRequest,
        FaucetDepositDto,
        DualControlRequest,
        MarketUnwindDto,
        RemedialCreditRequest,
        RemedialCreditDto,
        ReceivableWriteOffDto,
        AuditActionDto,
        AuditPageDto,
        PublishStatusDto,
        WithdrawRequestDto,
        WithdrawalReceiptDto,
        DecideRequestDto,
        WithdrawalDecisionDto,
        deposit_admin::DepositAdmissionDto,
        deposit_admin::DepositRefundDto,
    )),
    tags(
        (name = "health", description = "Liveness"),
        (name = "markets", description = "Market reads"),
        (name = "trades", description = "Preview + place trades"),
        (name = "votes", description = "Cast votes"),
        (name = "users", description = "Positions + identity"),
        (name = "economy", description = "Settled leaderboards"),
        (name = "social", description = "Comments and committed-capital holders"),
        (name = "notifications", description = "Scoped in-app notifications"),
        (name = "admin", description = "Curator stand-in"),
        (name = "content", description = "Curator draft pipeline"),
        (name = "video", description = "Rendered market assets"),
        (name = "ops", description = "Phase 6 control plane (RBAC-gated; W1/W2 land the behavior)"),
    )
)]
pub struct ApiDoc;

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

#[utoipa::path(get, path = "/healthz", tag = "health", responses((status = 200, description = "ok")))]
async fn healthz() -> Result<Json<serde_json::Value>, StatusCode> {
    let faults = crate::relay::process_faults()
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
        .disclosure();
    Ok(Json(serde_json::json!({"status": "ok", "faults": faults})))
}

async fn phase7_unavailable() -> StatusCode {
    StatusCode::SERVICE_UNAVAILABLE
}

fn phase7_admin_stubs<S: Store + Send + Sync + 'static>() -> Router<AppState<S>> {
    Router::new()
        .route("/admin/aml/{id}/clear/propose", post(phase7_unavailable))
        .route("/admin/aml/{id}/clear/confirm", post(phase7_unavailable))
        .route("/admin/users/{id}/ban/propose", post(phase7_unavailable))
        .route("/admin/users/{id}/ban/confirm", post(phase7_unavailable))
        .route("/admin/users/{id}/unban/propose", post(phase7_unavailable))
        .route("/admin/users/{id}/unban/confirm", post(phase7_unavailable))
        .route("/admin/users/{id}/shadow", post(phase7_unavailable))
        .route("/admin/users/{id}/unshadow", post(phase7_unavailable))
        .route(
            "/admin/self_exclusions/{id}/lift/propose",
            post(phase7_unavailable),
        )
        .route(
            "/admin/self_exclusions/{id}/lift/confirm",
            post(phase7_unavailable),
        )
        .route("/admin/credits/grant/propose", post(phase7_unavailable))
        .route("/admin/credits/grant/confirm", post(phase7_unavailable))
        .route(
            "/admin/bonus_reserve/topup/propose",
            post(phase7_unavailable),
        )
        .route(
            "/admin/bonus_reserve/topup/confirm",
            post(phase7_unavailable),
        )
        .route(
            "/admin/frozen_funds/{id}/license/propose",
            post(phase7_unavailable),
        )
        .route(
            "/admin/frozen_funds/{id}/license/confirm",
            post(phase7_unavailable),
        )
        .route(
            "/admin/markets/{id}/fee_override/propose",
            post(phase7_unavailable),
        )
        .route(
            "/admin/markets/{id}/fee_override/confirm",
            post(phase7_unavailable),
        )
}

/// Build the HTTP router for a store that implements both write factories and
/// lock-free queries.
///
/// Every `/admin/**` route — the Phase 0–5 surfaces, the phase-6 ops stubs,
/// and the two-factor faucet — is collected into ONE sub-router guarded by
/// the fail-closed RBAC layer (D26) BEFORE any handler runs.
pub fn router<S>(state: AppState<S>) -> Router
where
    S: Store
        + MarketQueries
        + SocialQueries
        + NotificationQueries
        + application::ports::OpsQueries
        + application::ports::WithdrawStore
        + Send
        + Sync
        + 'static,
{
    let admin = Router::new()
        .merge(content::router::<S>())
        .merge(ops_config::router::<S>())
        .merge(ops_admin::router::<S>(state.inner.staging_faucet))
        .route("/admin/markets/{id}/advance", post(admin_advance::<S>))
        .route("/admin/markets/{id}/resolve", post(admin_resolve::<S>))
        .route("/admin/markets/flagged", get(admin_flagged_markets::<S>))
        .route("/admin/fees/summary", get(admin_fee_summary::<S>))
        .route(
            "/admin/comments/reported",
            get(admin_reported_comments::<S>),
        )
        .route(
            "/admin/comments/{id}/moderate",
            post(admin_moderate_comment::<S>),
        )
        .merge(phase7_admin_stubs::<S>())
        .merge(withdraw::admin_router::<S>())
        .merge(deposit_admin::router::<S>())
        .route_layer(axum::middleware::from_fn_with_state(
            std::sync::Arc::new(state.inner.admin_tokens.clone()),
            crate::http::middleware::admin_rbac,
        ));
    Router::new()
        .merge(video::router::<S>())
        .merge(withdraw::router::<S>())
        .merge(admin)
        .route("/healthz", get(healthz))
        .route(
            "/ws",
            get(
                |ws: axum::extract::ws::WebSocketUpgrade,
                 State(state): State<AppState<S>>| async move {
                    super::ws::ws_upgrade(ws, super::ws::ConnectionState::from_app(state))
                },
            ),
        )
        .route("/markets", get(list_markets::<S>))
        .route("/markets/{id_or_slug}", get(get_market::<S>))
        .route("/markets/{id}/chart", get(market_chart::<S>))
        .route("/markets/{id}/tape", get(market_tape::<S>))
        .route(
            "/markets/{id_or_slug}/comments",
            get(list_comments::<S>).post(post_comment::<S>),
        )
        .route("/comments/{id}/vote", post(vote_comment::<S>))
        .route("/comments/{id}/report", post(report_comment::<S>))
        .route("/markets/{id}/holders", get(market_holders::<S>))
        .route("/leaderboards/traders", get(leaderboard_traders::<S>))
        .route("/leaderboards/voters", get(leaderboard_voters::<S>))
        .route("/trades/preview", post(preview_trade::<S>))
        .route("/trades", post(place_trade::<S>))
        .route("/votes", post(cast_vote::<S>))
        .route("/users/{id}/positions", get(user_positions::<S>))
        .route("/users/{id}/profile", get(user_profile::<S>))
        .route("/users/{id}/notifications", get(list_notifications::<S>))
        .route(
            "/users/{id}/notifications/read",
            post(mark_notifications_read::<S>),
        )
        .route(
            "/users/{id}/notifications/unread_count",
            get(notification_unread_count::<S>),
        )
        .route("/users/by-channel", get(user_by_channel::<S>))
        .route("/users", post(create_user::<S>))
        .with_state(state)
}

/// Offline OpenAPI JSON document (no running server required).
#[must_use]
pub fn openapi_json() -> utoipa::openapi::OpenApi {
    ApiDoc::openapi()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::sync::Arc;

    use application::fakes::{FakeClock, InMemoryStore};
    use application::model::{IntegritySweepConfig, RepConfig, ResolveConfig, VoteIntegrityConfig};

    use super::*;

    #[tokio::test]
    async fn phase7_admin_stubs_are_unavailable() {
        assert_eq!(
            super::phase7_unavailable().await,
            StatusCode::SERVICE_UNAVAILABLE
        );
        let _router = super::phase7_admin_stubs::<InMemoryStore>();
    }

    #[test]
    fn advance_event_parser_covers_aliases_and_rejections() {
        let accepted = [
            ("approve", MarketEvent::Approve),
            ("go-live", MarketEvent::GoLive),
            ("golive", MarketEvent::GoLive),
            ("enter_close", MarketEvent::EnterCloseWindow),
            ("closing", MarketEvent::EnterCloseWindow),
            ("close", MarketEvent::Close),
            ("integrity_sweep", MarketEvent::StartIntegritySweep),
            ("resolving", MarketEvent::StartIntegritySweep),
        ];
        for (name, event) in accepted {
            assert_eq!(parse_advance_event(name).unwrap(), event);
        }
        for name in [
            "resolve",
            "pay",
            "void",
            "void_low_participation",
            "void_by_admin",
        ] {
            let error = parse_advance_event(name).unwrap_err();
            assert_eq!(error.status, StatusCode::UNPROCESSABLE_ENTITY);
            assert_eq!(error.body.code, "UseResolveMarket");
        }
        let unknown = parse_advance_event("teleport").unwrap_err();
        assert_eq!(unknown.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(unknown.body.code, "InvalidEvent");
        assert_eq!(default_tape_limit(), 50);
    }

    #[test]
    fn vote_metadata_ignores_spoofed_forwarding_and_hashes_device_id() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "198.51.100.7".parse().unwrap());
        headers.insert("x-device-id", "client-123".parse().unwrap());
        let config = VoteMetadataConfig {
            trusted_proxy_cidrs: vec!["10.0.0.0/8".parse().unwrap()],
            device_hash_secret: Some(b"secret".to_vec()),
        }
        .validate()
        .unwrap();
        let (ip, device) =
            vote_metadata(Some("203.0.113.9:1234".parse().unwrap()), &headers, &config);
        assert_eq!(ip, Some("203.0.113.9".parse().unwrap()));
        assert_eq!(device.as_deref(), Some("6b975ee9ae4fa08c109702cf04e462ca"));
        assert_ne!(device.as_deref(), Some("client-123"));
    }

    #[test]
    fn trusted_proxy_uses_last_untrusted_hop_and_invalid_headers_are_safe() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-forwarded-for",
            "198.51.100.8, 192.0.2.4, 10.1.2.3".parse().unwrap(),
        );
        let config = VoteMetadataConfig {
            trusted_proxy_cidrs: vec!["10.0.0.0/8".parse().unwrap()],
            device_hash_secret: None,
        };
        let (ip, device) = vote_metadata(Some("10.9.8.7:4321".parse().unwrap()), &headers, &config);
        assert_eq!(ip, Some("192.0.2.4".parse().unwrap()));
        assert_eq!(device, None);

        headers.insert("x-forwarded-for", "garbage, 10.1.2.3".parse().unwrap());
        let (fallback, _) =
            vote_metadata(Some("10.9.8.7:4321".parse().unwrap()), &headers, &config);
        assert_eq!(fallback, Some("10.9.8.7".parse().unwrap()));
        assert!(VoteMetadataConfig {
            trusted_proxy_cidrs: Vec::new(),
            device_hash_secret: Some(Vec::new()),
        }
        .validate()
        .is_err());
    }

    #[test]
    fn every_resolve_outcome_has_an_explicit_wire_state() {
        assert_eq!(
            resolve_state(ResolveOutcome::Settled),
            (MarketStateDto::Paid, false)
        );
        assert_eq!(
            resolve_state(ResolveOutcome::HeldForReview {
                due_at: time::OffsetDateTime::UNIX_EPOCH,
            }),
            (MarketStateDto::Resolving, false)
        );
        assert_eq!(
            resolve_state(ResolveOutcome::CuratorRequired),
            (MarketStateDto::Resolving, false)
        );
        assert_eq!(
            resolve_state(ResolveOutcome::Voided),
            (MarketStateDto::Voided, true)
        );
    }

    #[test]
    fn phase4_state_and_comment_cursor_contracts_are_explicit() {
        let social = SocialConfig::default();
        let state = AppState::with_phase4_configs(
            InMemoryStore::new(),
            Arc::new(FakeClock::at(time::OffsetDateTime::UNIX_EPOCH)),
            ResolveConfig {
                oi_floor: MicroUsd(0),
            },
            RepConfig::default(),
            VoteIntegrityConfig::default(),
            IntegritySweepConfig::default(),
            VoteMetadataConfig::default(),
            social,
            vec!["Admin".to_string()],
        );
        assert_eq!(state.inner.social_config, social);
        assert_eq!(state.inner.admin_handles, ["Admin"]);
        assert_eq!(bounded_limit(None, 50, 100).unwrap(), 50);
        for invalid in [Some(0), Some(101)] {
            let error = bounded_limit(invalid, 50, 100).unwrap_err();
            assert_eq!(
                (error.status, error.body.code.as_str()),
                (StatusCode::UNPROCESSABLE_ENTITY, "InvalidLimit")
            );
        }

        let id = CommentId(Uuid::new_v4());
        let at = time::OffsetDateTime::UNIX_EPOCH;
        for cursor in [
            CommentCursor::Hot {
                as_of: at,
                hot_score: -7,
                created_at: at,
                id,
            },
            CommentCursor::Recent { created_at: at, id },
        ] {
            let sort = if matches!(cursor, CommentCursor::Hot { .. }) {
                CommentSort::Hot
            } else {
                CommentSort::Recent
            };
            let encoded = encode_comment_cursor(cursor).unwrap();
            assert_eq!(decode_comment_cursor(sort, &encoded).unwrap(), cursor);
        }
        for invalid in [
            "%%%".to_string(),
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([0xff]),
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode("one|two|three"),
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(format!("bad|-7|1970-01-01T00:00:00Z|{}", id.0)),
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(format!(
                "1970-01-01T00:00:00Z|bad|1970-01-01T00:00:00Z|{}",
                id.0
            )),
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode("1970-01-01T00:00:00Z|not-a-uuid"),
        ] {
            assert_eq!(
                decode_comment_cursor(CommentSort::Hot, &invalid)
                    .unwrap_err()
                    .body
                    .code,
                "InvalidCursor"
            );
        }
        let recent_bad_date =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(format!("bad|{}", id.0));
        assert_eq!(
            decode_comment_cursor(CommentSort::Recent, &recent_bad_date)
                .unwrap_err()
                .body
                .code,
            "InvalidCursor"
        );
    }

    #[test]
    fn social_errors_have_stable_http_vocabulary() {
        use application::error::AppError;

        for (error, status, code) in [
            (
                AppError::CommentBlocked("links"),
                StatusCode::UNPROCESSABLE_ENTITY,
                "blocked_links",
            ),
            (
                AppError::ThreadTooDeep,
                StatusCode::UNPROCESSABLE_ENTITY,
                "thread_too_deep",
            ),
            (
                AppError::CommentAlreadyVoted,
                StatusCode::CONFLICT,
                "duplicate_vote",
            ),
            (
                AppError::CommentNotVisible,
                StatusCode::CONFLICT,
                "not_visible",
            ),
            (
                AppError::ReporterNotQualified,
                StatusCode::FORBIDDEN,
                "reporter_floor",
            ),
            (
                AppError::ReportVelocityExceeded,
                StatusCode::TOO_MANY_REQUESTS,
                "report_velocity",
            ),
            (
                AppError::InvalidCommentVote,
                StatusCode::UNPROCESSABLE_ENTITY,
                "InvalidCommentVote",
            ),
        ] {
            let response = comment_error(error);
            assert_eq!(
                (response.status, response.body.code.as_str()),
                (status, code)
            );
        }
    }

    #[test]
    fn phase5_state_constructor_preserves_every_injected_service() {
        let store = InMemoryStore::new();
        let clock: Arc<dyn Clock> = Arc::new(FakeClock::at(time::OffsetDateTime::UNIX_EPOCH));
        // Engine SELECTION is covered by `llm::selection` under its env lock;
        // this test must stay env-independent, so it checks preservation by
        // Arc identity instead of calling the engine.
        let services = Phase5Services::default();
        let draft_engine = Arc::clone(&services.draft_engine);
        let renderer = Arc::clone(&services.renderer);
        let moderation_preflight = Arc::clone(&services.moderation_preflight);
        let state = AppState::with_phase5_configs(
            store,
            clock,
            ResolveConfig {
                oi_floor: MicroUsd(7),
            },
            RepConfig::default(),
            VoteIntegrityConfig::default(),
            IntegritySweepConfig::default(),
            VoteMetadataConfig::default(),
            SocialConfig::default(),
            vec!["curator".into()],
            services,
        );
        assert_eq!(state.inner.resolve_config.oi_floor, MicroUsd(7));
        assert_eq!(state.inner.admin_handles, ["curator"]);
        assert_eq!(state.inner.phase5.config, ContentConfig::default());
        assert!(std::ptr::addr_eq(
            Arc::as_ptr(&state.inner.phase5.draft_engine),
            Arc::as_ptr(&draft_engine),
        ));
        assert!(std::ptr::addr_eq(
            Arc::as_ptr(&state.inner.phase5.renderer),
            Arc::as_ptr(&renderer),
        ));
        assert!(std::ptr::addr_eq(
            Arc::as_ptr(&state.inner.phase5.moderation_preflight),
            Arc::as_ptr(&moderation_preflight),
        ));
        assert_eq!(state.event_sender().receiver_count(), 0);
    }
}
