//! Axum handlers — zero business logic; call use cases / `MarketQueries` only.

use std::env;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use application::advance_market::{AdvanceMarket, AdvanceMarketCmd};
use application::cast_vote::{CastVote, CastVoteCmd};
use application::comments::{
    ModerateComment, ModerateCommentAction, PostComment, PostCommentCmd, ReportComment,
    ReportCommentCmd, VoteComment, VoteCommentCmd,
};
use application::model::AdminContext;
use application::model::{
    CommentCursor, CommentId, CommentSort, ContentConfig, IntegritySweepConfig, LpKillConfig,
    MarketId, RepConfig, ResolveConfig, SocialConfig, UserId, VoteIntegrityConfig,
};
use application::place_trade::{PlaceTrade, PlaceTradeCmd};
use application::ports::{
    Clock, ConfigReads, DraftEngine, MarketQueries, ModerationPreflight, NoopCrashPoint,
    NotificationQueries, Renderer, ResolutionCrashPoint, SocialQueries, StaticConfigReads, Store,
};
use application::preview_trade::{PreviewTrade, PreviewTradeCmd};
use application::resolve_market::{ResolveMarket, ResolveMarketCmd, ResolveOutcome};
use axum::extract::{ConnectInfo, Extension, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use domain::market::MarketEvent;
use domain::money::MicroUsd;
use hmac::{Hmac, Mac};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use utoipa::OpenApi;
use uuid::Uuid;

use crate::relay::BusEvent;

use super::dto::*;
use super::error::{ApiError, ApiResult, ErrorResponse};

pub mod compliance_admin;
pub mod content;
pub mod deposit_admin;
pub mod kyc_webhook;
pub mod ops_admin;
pub mod ops_config;
pub mod phone;
pub mod video;
pub mod withdraw;

/// Sized wrapper so `PlaceTrade` can take `&impl Clock` without object-safety
/// holes on `dyn Clock`.
#[derive(Clone)]
pub struct SharedClock(pub Arc<dyn Clock>);

impl Clock for SharedClock {
    fn now(&self) -> time::OffsetDateTime {
        self.0.now()
    }
}

#[derive(Clone, Debug, Default)]
pub struct VoteMetadataConfig {
    pub trusted_proxy_cidrs: Vec<ipnet::IpNet>,
    pub device_hash_secret: Option<Vec<u8>>,
}

impl VoteMetadataConfig {
    /// # Errors
    /// Empty configured secrets are rejected instead of silently disabling HMAC.
    pub fn validate(self) -> Result<Self, &'static str> {
        if self.device_hash_secret.as_ref().is_some_and(Vec::is_empty) {
            return Err("DEVICE_HASH_SECRET must be non-empty when configured");
        }
        Ok(self)
    }
}

/// Shared app state: store is both [`Store`] and [`MarketQueries`].
/// Manual `Clone` so `S` need not be `Clone` (only the `Arc` is cloned).
pub struct AppState<S> {
    pub inner: Arc<AppStateInner<S>>,
}

impl<S> Clone for AppState<S> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

pub struct AppStateInner<S> {
    pub store: S,
    pub clock: SharedClock,
    pub demo_token: String,
    /// Startup-validated admin token registry (D26). Fail-closed: empty
    /// means every /admin request is 401.
    pub admin_tokens: crate::http::middleware::AdminTokens,
    /// Lock-free preview generation source (D25a); the W1 reconciler
    /// replaces the static seed default.
    pub config_reads: Arc<dyn ConfigReads>,
    /// D29 crash-point slot: Noop unless the two-factor chaos arm selects
    /// the W4 staging adapter in `main`.
    pub crash_point: Arc<dyn ResolutionCrashPoint>,
    /// Two-factor staging faucet arm: `OPINIONS_ENV=staging` AND
    /// `STAGING_FAUCET=1`. Off means the route is NOT mounted (404).
    pub staging_faucet: bool,
    /// D21 open-interest floor for `ResolveMarket` (default 0 = auto-void thin markets).
    pub resolve_config: ResolveConfig,
    pub rep_config: RepConfig,
    pub events: tokio::sync::broadcast::Sender<BusEvent>,
    pub vote_integrity_config: VoteIntegrityConfig,
    pub integrity_sweep_config: IntegritySweepConfig,
    pub vote_metadata_config: VoteMetadataConfig,
    pub social_config: SocialConfig,
    pub admin_handles: Vec<String>,
    pub phase5: Phase5Services,
}

#[derive(Clone)]
pub struct Phase5Services {
    pub config: ContentConfig,
    pub lp_kill: LpKillConfig,
    pub draft_engine: Arc<dyn DraftEngine>,
    pub renderer: Arc<dyn Renderer>,
    pub moderation_preflight: Arc<dyn ModerationPreflight>,
}

impl Default for Phase5Services {
    fn default() -> Self {
        // Deterministic template engine: `Default` is a test/bootstrap surface and
        // must not read process env (frozen main wires the env selector itself).
        let template = application::content::draft_engine();
        Self {
            config: ContentConfig::default(),
            lp_kill: LpKillConfig::default(),
            draft_engine: template,
            renderer: crate::render::renderer(),
            moderation_preflight: crate::llm::moderation_preflight_from_env(),
        }
    }
}

impl<S> AppState<S> {
    #[must_use]
    pub fn new(store: S, clock: Arc<dyn Clock>) -> Self {
        Self::with_configs(
            store,
            clock,
            ResolveConfig {
                oi_floor: MicroUsd(0),
            },
            RepConfig::default(),
            VoteIntegrityConfig::default(),
        )
    }

    #[must_use]
    pub fn with_configs(
        store: S,
        clock: Arc<dyn Clock>,
        resolve_config: ResolveConfig,
        rep_config: RepConfig,
        vote_integrity_config: VoteIntegrityConfig,
    ) -> Self {
        let (events, _) = tokio::sync::broadcast::channel(256);
        Self {
            inner: Arc::new(AppStateInner {
                store,
                clock: SharedClock(clock),
                demo_token: env::var("DEMO_TOKEN").unwrap_or_else(|_| "demo-token".into()),
                admin_tokens: crate::http::middleware::AdminTokens::from_env_or_deny(),
                config_reads: Arc::new(StaticConfigReads::default()),
                crash_point: Arc::new(NoopCrashPoint),
                staging_faucet: staging_faucet_from_env(),
                resolve_config,
                rep_config,
                events,
                vote_integrity_config,
                integrity_sweep_config: IntegritySweepConfig::default(),
                vote_metadata_config: VoteMetadataConfig::default(),
                social_config: SocialConfig::default(),
                admin_handles: Vec::new(),
                phase5: Phase5Services::default(),
            }),
        }
    }

    #[must_use]
    pub fn with_tokens(store: S, clock: Arc<dyn Clock>, demo: &str, admin: &str) -> Self {
        let (events, _) = tokio::sync::broadcast::channel(256);
        Self {
            inner: Arc::new(AppStateInner {
                store,
                clock: SharedClock(clock),
                demo_token: demo.to_string(),
                admin_tokens: crate::http::middleware::AdminTokens::single_test_token(admin),
                config_reads: Arc::new(StaticConfigReads::default()),
                crash_point: Arc::new(NoopCrashPoint),
                staging_faucet: staging_faucet_from_env(),
                resolve_config: ResolveConfig {
                    oi_floor: MicroUsd(0),
                },
                rep_config: RepConfig::default(),
                events,
                vote_integrity_config: VoteIntegrityConfig::default(),
                integrity_sweep_config: IntegritySweepConfig::default(),
                vote_metadata_config: VoteMetadataConfig::default(),
                social_config: SocialConfig::default(),
                admin_handles: Vec::new(),
                phase5: Phase5Services::default(),
            }),
        }
    }

    #[must_use]
    pub fn with_phase3_configs(
        store: S,
        clock: Arc<dyn Clock>,
        resolve_config: ResolveConfig,
        rep_config: RepConfig,
        vote_integrity_config: VoteIntegrityConfig,
        integrity_sweep_config: IntegritySweepConfig,
        vote_metadata_config: VoteMetadataConfig,
    ) -> Self {
        let (events, _) = tokio::sync::broadcast::channel(256);
        Self {
            inner: Arc::new(AppStateInner {
                store,
                clock: SharedClock(clock),
                demo_token: env::var("DEMO_TOKEN").unwrap_or_else(|_| "demo-token".into()),
                admin_tokens: crate::http::middleware::AdminTokens::from_env_or_deny(),
                config_reads: Arc::new(StaticConfigReads::default()),
                crash_point: Arc::new(NoopCrashPoint),
                staging_faucet: staging_faucet_from_env(),
                resolve_config,
                rep_config,
                events,
                vote_integrity_config,
                integrity_sweep_config,
                vote_metadata_config,
                social_config: SocialConfig::default(),
                admin_handles: Vec::new(),
                phase5: Phase5Services::default(),
            }),
        }
    }

    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn with_phase3_tokens(
        store: S,
        clock: Arc<dyn Clock>,
        demo: &str,
        admin: &str,
        resolve_config: ResolveConfig,
        rep_config: RepConfig,
        vote_integrity_config: VoteIntegrityConfig,
        integrity_sweep_config: IntegritySweepConfig,
        vote_metadata_config: VoteMetadataConfig,
    ) -> Self {
        let (events, _) = tokio::sync::broadcast::channel(256);
        Self {
            inner: Arc::new(AppStateInner {
                store,
                clock: SharedClock(clock),
                demo_token: demo.to_string(),
                admin_tokens: crate::http::middleware::AdminTokens::single_test_token(admin),
                config_reads: Arc::new(StaticConfigReads::default()),
                crash_point: Arc::new(NoopCrashPoint),
                staging_faucet: staging_faucet_from_env(),
                resolve_config,
                rep_config,
                events,
                vote_integrity_config,
                integrity_sweep_config,
                vote_metadata_config,
                social_config: SocialConfig::default(),
                admin_handles: Vec::new(),
                phase5: Phase5Services::default(),
            }),
        }
    }

    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn with_phase4_configs(
        store: S,
        clock: Arc<dyn Clock>,
        resolve_config: ResolveConfig,
        rep_config: RepConfig,
        vote_integrity_config: VoteIntegrityConfig,
        integrity_sweep_config: IntegritySweepConfig,
        vote_metadata_config: VoteMetadataConfig,
        social_config: SocialConfig,
        admin_handles: Vec<String>,
    ) -> Self {
        let (events, _) = tokio::sync::broadcast::channel(256);
        Self {
            inner: Arc::new(AppStateInner {
                store,
                clock: SharedClock(clock),
                demo_token: env::var("DEMO_TOKEN").unwrap_or_else(|_| "demo-token".into()),
                admin_tokens: crate::http::middleware::AdminTokens::from_env_or_deny(),
                config_reads: Arc::new(StaticConfigReads::default()),
                crash_point: Arc::new(NoopCrashPoint),
                staging_faucet: staging_faucet_from_env(),
                resolve_config,
                rep_config,
                events,
                vote_integrity_config,
                integrity_sweep_config,
                vote_metadata_config,
                social_config,
                admin_handles,
                phase5: Phase5Services::default(),
            }),
        }
    }

    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn with_phase5_configs(
        store: S,
        clock: Arc<dyn Clock>,
        resolve_config: ResolveConfig,
        rep_config: RepConfig,
        vote_integrity_config: VoteIntegrityConfig,
        integrity_sweep_config: IntegritySweepConfig,
        vote_metadata_config: VoteMetadataConfig,
        social_config: SocialConfig,
        admin_handles: Vec<String>,
        phase5: Phase5Services,
    ) -> Self {
        let (events, _) = tokio::sync::broadcast::channel(256);
        Self {
            inner: Arc::new(AppStateInner {
                store,
                clock: SharedClock(clock),
                demo_token: env::var("DEMO_TOKEN").unwrap_or_else(|_| "demo-token".into()),
                admin_tokens: crate::http::middleware::AdminTokens::from_env_or_deny(),
                config_reads: Arc::new(StaticConfigReads::default()),
                crash_point: Arc::new(NoopCrashPoint),
                staging_faucet: staging_faucet_from_env(),
                resolve_config,
                rep_config,
                events,
                vote_integrity_config,
                integrity_sweep_config,
                vote_metadata_config,
                social_config,
                admin_handles,
                phase5,
            }),
        }
    }

    /// Phase 6 composition surface for `main`: everything
    /// `with_phase5_configs` wires PLUS the validated admin token registry
    /// and the chaos crash-point slot (D26/D29). `staging_faucet` still
    /// comes from the two-factor env arm — it is a mount decision, not a
    /// service.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn with_phase6_ops(
        store: S,
        clock: Arc<dyn Clock>,
        resolve_config: ResolveConfig,
        rep_config: RepConfig,
        vote_integrity_config: VoteIntegrityConfig,
        integrity_sweep_config: IntegritySweepConfig,
        vote_metadata_config: VoteMetadataConfig,
        social_config: SocialConfig,
        admin_handles: Vec<String>,
        phase5: Phase5Services,
        admin_tokens: crate::http::middleware::AdminTokens,
        crash_point: Arc<dyn ResolutionCrashPoint>,
        config_reads: Arc<dyn ConfigReads>,
    ) -> Self {
        let (events, _) = tokio::sync::broadcast::channel(256);
        Self {
            inner: Arc::new(AppStateInner {
                store,
                clock: SharedClock(clock),
                demo_token: env::var("DEMO_TOKEN").unwrap_or_else(|_| "demo-token".into()),
                admin_tokens,
                config_reads,
                crash_point,
                staging_faucet: staging_faucet_from_env(),
                resolve_config,
                rep_config,
                events,
                vote_integrity_config,
                integrity_sweep_config,
                vote_metadata_config,
                social_config,
                admin_handles,
                phase5,
            }),
        }
    }

    #[must_use]
    pub fn event_sender(&self) -> tokio::sync::broadcast::Sender<BusEvent> {
        self.inner.events.clone()
    }
}

/// Parse admin advance event names into domain events (non-financial only).
fn parse_advance_event(name: &str) -> Result<MarketEvent, ErrorResponse> {
    let key = name.trim().to_ascii_lowercase().replace('-', "_");
    match key.as_str() {
        "approve" => Ok(MarketEvent::Approve),
        "go_live" | "golive" => Ok(MarketEvent::GoLive),
        "enter_close_window" | "enter_close" | "closing" => Ok(MarketEvent::EnterCloseWindow),
        "close" => Ok(MarketEvent::Close),
        "start_integrity_sweep" | "integrity_sweep" | "resolving" => {
            Ok(MarketEvent::StartIntegritySweep)
        }
        // Financial — must use /resolve
        "resolve" | "pay" | "void" | "void_low_participation" | "void_by_admin" => {
            Err(ErrorResponse::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "UseResolveMarket",
                "this lifecycle event must go through ResolveMarket",
            ))
        }
        _ => Err(ErrorResponse::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "InvalidEvent",
            &format!("unknown advance event: {name}"),
        )),
    }
}

fn vote_metadata(
    peer: Option<SocketAddr>,
    headers: &HeaderMap,
    config: &VoteMetadataConfig,
) -> (Option<IpAddr>, Option<String>) {
    let cast_ip = peer.map(|socket| socket.ip()).map(|direct| {
        if !config
            .trusted_proxy_cidrs
            .iter()
            .any(|network| network.contains(&direct))
        {
            return direct;
        }
        headers
            .get("x-forwarded-for")
            .and_then(|value| value.to_str().ok())
            .into_iter()
            .flat_map(|value| value.split(',').rev())
            .filter_map(|part| part.trim().parse::<IpAddr>().ok())
            .find(|candidate| {
                !config
                    .trusted_proxy_cidrs
                    .iter()
                    .any(|network| network.contains(candidate))
            })
            .unwrap_or(direct)
    });
    let device_hash = config.device_hash_secret.as_ref().and_then(|secret| {
        let device = headers.get("x-device-id")?.to_str().ok()?;
        let mut mac = Hmac::<Sha256>::new_from_slice(secret).ok()?;
        mac.update(device.as_bytes());
        let bytes = mac.finalize().into_bytes();
        const HEX: &[u8; 16] = b"0123456789abcdef";
        Some(
            bytes[..16]
                .iter()
                .flat_map(|byte| {
                    [
                        char::from(HEX[usize::from(*byte >> 4)]),
                        char::from(HEX[usize::from(*byte & 0x0f)]),
                    ]
                })
                .collect(),
        )
    });
    (cast_ip, device_hash)
}

fn require_demo(headers: &HeaderMap, expected: &str) -> ApiResult<()> {
    let got = headers
        .get("x-demo-token")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    // Constant-time. A plain string equality short-circuits at the first
    // differing byte and checks the length first, turning this token into a
    // byte-at-a-time oracle. See `http::middleware::secret_eq`.
    if crate::http::middleware::secret_eq(got, expected) {
        Ok(())
    } else {
        Err(ErrorResponse::new(
            StatusCode::UNAUTHORIZED,
            "Unauthorized",
            "missing or invalid x-demo-token",
        ))
    }
}

/// Two-factor staging faucet arm (D26/plan §0 stand-ins): BOTH factors or
/// the route is never mounted.
fn staging_faucet_from_env() -> bool {
    env::var("OPINIONS_ENV").as_deref() == Ok("staging")
        && env::var("STAGING_FAUCET").as_deref() == Ok("1")
}

include!("market.rs");
include!("social.rs");
include!("core.rs");
