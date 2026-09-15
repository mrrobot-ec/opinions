//! Binary wiring ONLY — any logic in `main` is a review-blocker (the crate
//! stays in the coverage exclusion allowlist with that justification).
//! Reads `DATABASE_URL`, `BIND_ADDR` (default `127.0.0.1:8080`); `DEMO_TOKEN`
//! is consumed by `AppState`; `ADMIN_TOKENS_JSON` (D26 — the legacy
//! `ADMIN_TOKEN` fallback is gone) is validated at startup. Any `CHAOS_*`
//! variable outside the two-factor staging arm is a startup error (D29).
//! Runs pending migrations on startup, then serves the HTTP adapter — or,
//! with `--seed-demo`, runs the idempotent demo seed through the use cases
//! and exits. `--continuous` runs the real invariant sweep loop (60s cadence; the
//! sweep).

use std::env;
use std::sync::Arc;

use adapters::http::middleware::AdminTokens;
use adapters::http::{router, AppState, Phase5Services, VoteMetadataConfig};
use adapters::notifier::OutboxNotifier;
use adapters::pg::PgStore;
use adapters::relay::crash_point::{StagingCrashPoint, READY_FILE_ENV};
use adapters::relay::{process_faults, OutboxRelay};
use application::model::{
    ContentConfig, ContentTierConfig, IntegritySweepConfig, LpKillConfig, RepConfig, ResolveConfig,
    SocialConfig, VoteIntegrityConfig,
};
use application::ports::{Clock, NoopCrashPoint, ResolutionCrashPoint, Store};
use domain::money::MicroUsd;
use time::OffsetDateTime;

mod seed;

/// Wall clock for production wiring.
struct SystemClock;

struct SchedulerConfig {
    tick: std::time::Duration,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Phase5Flags {
    publisher: bool,
    video_worker: bool,
    moderation_runner: bool,
}

impl Phase5Flags {
    fn from_env() -> anyhow::Result<Self> {
        Ok(Self {
            publisher: env_value("DRAFT_PUBLISHER_ENABLED", false)?,
            video_worker: env_value("VIDEO_WORKER_ENABLED", false)?,
            moderation_runner: env_value("MODERATION_RUNNER_ENABLED", false)?,
        })
    }
}

impl SchedulerConfig {
    fn from_env() -> anyhow::Result<Self> {
        let tick_ms = env_value("SCHEDULER_TICK_MS", 1_000_u64)?;
        if tick_ms == 0 {
            anyhow::bail!("SCHEDULER_TICK_MS must be positive");
        }
        Ok(Self {
            tick: std::time::Duration::from_millis(tick_ms),
        })
    }
}

fn env_value<T>(name: &str, default: T) -> anyhow::Result<T>
where
    T: std::str::FromStr,
    T::Err: std::error::Error + Send + Sync + 'static,
{
    match env::var(name) {
        Ok(value) => value
            .parse::<T>()
            .map_err(|error| anyhow::anyhow!("{name} is invalid: {error}")),
        Err(env::VarError::NotPresent) => Ok(default),
        Err(error) => Err(error.into()),
    }
}

fn env_csv<T>(name: &str, default: &[T]) -> anyhow::Result<Vec<T>>
where
    T: std::str::FromStr + ToString,
    T::Err: std::error::Error + Send + Sync + 'static,
{
    let values: Vec<String> = match env::var(name) {
        Ok(value) => value
            .split(',')
            .map(|part| part.trim().to_string())
            .collect(),
        Err(env::VarError::NotPresent) => default.iter().map(ToString::to_string).collect(),
        Err(error) => return Err(error.into()),
    };
    values
        .into_iter()
        .map(|part| {
            part.parse::<T>()
                .map_err(|error| anyhow::anyhow!("{name} is invalid: {error}"))
        })
        .collect()
}

fn resolve_config() -> anyhow::Result<ResolveConfig> {
    let oi_floor = env_value("OI_FLOOR_MICRO", 0_i64)?;
    if oi_floor < 0 {
        anyhow::bail!("OI_FLOOR_MICRO must be nonnegative");
    }
    Ok(ResolveConfig {
        oi_floor: MicroUsd(oi_floor),
    })
}

fn vote_integrity_config() -> anyhow::Result<VoteIntegrityConfig> {
    let defaults = VoteIntegrityConfig::default();
    let config = VoteIntegrityConfig {
        max_votes_per_window: env_value("MAX_VOTES_PER_WINDOW", defaults.max_votes_per_window)?,
        window_secs: env_value("VOTE_WINDOW_SECS", defaults.window_secs)?,
        near_close_secs: env_value("VOTE_NEAR_CLOSE_SECS", defaults.near_close_secs)?,
        min_account_age_secs: env_value(
            "VOTE_MIN_ACCOUNT_AGE_SECS",
            defaults.min_account_age_secs,
        )?,
    };
    if config.max_votes_per_window == 0 || config.window_secs == 0 {
        anyhow::bail!("vote window and maximum must be positive");
    }
    Ok(config)
}

fn rep_config() -> anyhow::Result<RepConfig> {
    // Phase 3 keeps a conservative unlimited tier-0 default; every supplied
    // array is parsed atomically and malformed values fail startup.
    let defaults = RepConfig::default();
    let thresholds: [i64; 4] =
        env_csv("REP_TIER_THRESHOLDS_MICRO", &defaults.tier_thresholds_micro)?
            .try_into()
            .map_err(|_| anyhow::anyhow!("REP_TIER_THRESHOLDS_MICRO must contain four values"))?;
    let caps: [i64; 5] = env_csv(
        "POSITION_CAP_MICRO_BY_TIER",
        &defaults.position_cap_micro_by_tier,
    )?
    .try_into()
    .map_err(|_| anyhow::anyhow!("POSITION_CAP_MICRO_BY_TIER must contain five values"))?;
    let discounts: [u16; 5] =
        env_csv("FEE_DISCOUNT_BP_BY_TIER", &defaults.fee_discount_bp_by_tier)?
            .try_into()
            .map_err(|_| anyhow::anyhow!("FEE_DISCOUNT_BP_BY_TIER must contain five values"))?;
    let half_life = match env::var("REP_HALF_LIFE") {
        Ok(value) if value.eq_ignore_ascii_case("h10") || value == "10" => {
            domain::reputation::HalfLife::H10
        }
        Ok(value) if value.eq_ignore_ascii_case("h20") || value == "20" => {
            domain::reputation::HalfLife::H20
        }
        Ok(value) if value.eq_ignore_ascii_case("h40") || value == "40" => {
            domain::reputation::HalfLife::H40
        }
        Ok(_) => anyhow::bail!("REP_HALF_LIFE must be H10, H20, or H40"),
        Err(env::VarError::NotPresent) => defaults.half_life,
        Err(error) => return Err(error.into()),
    };
    RepConfig {
        half_life,
        tier_thresholds_micro: thresholds,
        position_cap_micro_by_tier: caps,
        fee_discount_bp_by_tier: discounts,
        min_fee_bps: env_value("MIN_FEE_BPS", defaults.min_fee_bps)?,
        rep_score_min_pot_micro: env_value(
            "REP_SCORE_MIN_POT_MICRO",
            defaults.rep_score_min_pot_micro,
        )?,
        discount_flip_window_secs: env_value(
            "DISCOUNT_FLIP_WINDOW_SECS",
            defaults.discount_flip_window_secs,
        )?,
        leaderboard_min_scored: env_value(
            "LEADERBOARD_MIN_SCORED",
            defaults.leaderboard_min_scored,
        )?,
    }
    .validate()
    .map_err(anyhow::Error::from)
}

fn integrity_sweep_config() -> anyhow::Result<IntegritySweepConfig> {
    let defaults = IntegritySweepConfig::default();
    IntegritySweepConfig {
        payout_hold_threshold_micro: env_value(
            "PAYOUT_HOLD_THRESHOLD_MICRO",
            defaults.payout_hold_threshold_micro,
        )?,
        sweep_delay_secs: env_value("SWEEP_DELAY_SECS", defaults.sweep_delay_secs)?,
        burst_window_secs: env_value("BURST_WINDOW_SECS", defaults.burst_window_secs)?,
        prior_horizon_windows: env_value("PRIOR_HORIZON_WINDOWS", defaults.prior_horizon_windows)?,
        burst_multiplier_ppm: env_value("BURST_MULTIPLIER_PPM", defaults.burst_multiplier_ppm)?,
        young_account_age_secs: env_value(
            "YOUNG_ACCOUNT_AGE_SECS",
            defaults.young_account_age_secs,
        )?,
        young_account_share_max_ppm: env_value(
            "YOUNG_ACCOUNT_SHARE_MAX_PPM",
            defaults.young_account_share_max_ppm,
        )?,
        subnet_share_max_ppm: env_value("SUBNET_SHARE_MAX_PPM", defaults.subnet_share_max_ppm)?,
        device_share_max_ppm: env_value("DEVICE_SHARE_MAX_PPM", defaults.device_share_max_ppm)?,
        min_votes_for_ratios: env_value("MIN_VOTES_FOR_RATIOS", defaults.min_votes_for_ratios)?,
        min_metadata_coverage_ppm: env_value(
            "MIN_METADATA_COVERAGE_PPM",
            defaults.min_metadata_coverage_ppm,
        )?,
    }
    .validate()
    .map_err(anyhow::Error::from)
}

fn lp_kill_config() -> anyhow::Result<LpKillConfig> {
    let defaults = LpKillConfig::default();
    LpKillConfig {
        max_loss_micro: env_value("LP_MAX_LOSS_MICRO", defaults.max_loss_micro)?,
        window_days: env_value("LP_WINDOW_DAYS", defaults.window_days)?,
    }
    .validate()
    .map_err(anyhow::Error::from)
}

fn vote_metadata_config() -> anyhow::Result<VoteMetadataConfig> {
    let trusted_proxy_cidrs = match env::var("TRUSTED_PROXY_CIDRS") {
        Ok(value) if value.trim().is_empty() => Vec::new(),
        Ok(value) => value
            .split(',')
            .map(|part| {
                part.trim()
                    .parse()
                    .map_err(|error| anyhow::anyhow!("invalid TRUSTED_PROXY_CIDRS entry: {error}"))
            })
            .collect::<anyhow::Result<Vec<_>>>()?,
        Err(env::VarError::NotPresent) => Vec::new(),
        Err(error) => return Err(error.into()),
    };
    let device_hash_secret = match env::var("DEVICE_HASH_SECRET") {
        Ok(value) => Some(value.into_bytes()),
        Err(env::VarError::NotPresent) => None,
        Err(error) => return Err(error.into()),
    };
    VoteMetadataConfig {
        trusted_proxy_cidrs,
        device_hash_secret,
    }
    .validate()
    .map_err(anyhow::Error::msg)
}

/// Phase 7 rail-identity slot: when `RAIL_GENESIS_HASH` is set the full
/// identity is required and validated; otherwise the process stays off-rail.
fn rail_identity_from_env() -> anyhow::Result<Option<application::ports::RailIdentity>> {
    match env::var("RAIL_GENESIS_HASH") {
        Err(env::VarError::NotPresent) => Ok(None),
        Err(error) => Err(error.into()),
        Ok(genesis_hash) => {
            let identity = application::ports::RailIdentity {
                genesis_hash,
                rpc_endpoints: env::var("RAIL_RPC_ENDPOINTS")
                    .unwrap_or_default()
                    .split(',')
                    .map(|part| part.trim().to_string())
                    .filter(|part| !part.is_empty())
                    .collect(),
                usdc_mint: env::var("RAIL_USDC_MINT").unwrap_or_default(),
                decimals: env_value("RAIL_USDC_DECIMALS", 6_u8)?,
                treasury_owner: env::var("RAIL_TREASURY_OWNER").unwrap_or_default(),
                treasury_token_account: env::var("RAIL_TREASURY_TOKEN_ACCOUNT").unwrap_or_default(),
                commitment: env::var("RAIL_COMMITMENT").unwrap_or_else(|_| "finalized".into()),
            };
            identity
                .validate()
                .map_err(|reason| anyhow::anyhow!("rail identity: {reason}"))?;
            Ok(Some(identity))
        }
    }
}

/// D33 geo policy version stamped on every screening verdict. Fail LOUD like
/// every other startup input in this file: a present-but-malformed security
/// policy version silently falling back to the seed is the exact fail-open
/// D26/D29 forbids — a stale Clear from a superseded allowset would keep
/// passing under version 1. Bounds are the D24 catalog's (`>= 1, monotone +1`).
fn region_allowset_version() -> anyhow::Result<i64> {
    let version = env_value("REGION_ALLOWSET_VERSION", 1_i64)?;
    if version < 1 {
        anyhow::bail!("REGION_ALLOWSET_VERSION must be at least 1");
    }
    Ok(version)
}

fn social_config() -> anyhow::Result<SocialConfig> {
    let defaults = SocialConfig::default();
    SocialConfig {
        max_comment_len_chars: env_value("MAX_COMMENT_LEN_CHARS", defaults.max_comment_len_chars)?,
        max_links: env_value("MAX_COMMENT_LINKS", defaults.max_links)?,
        max_mentions: env_value("MAX_COMMENT_MENTIONS", defaults.max_mentions)?,
        spam_window_secs: env_value("COMMENT_SPAM_WINDOW_SECS", defaults.spam_window_secs)?,
        max_comments_per_window: env_value(
            "MAX_COMMENTS_PER_WINDOW",
            defaults.max_comments_per_window,
        )?,
        report_shadow_threshold: env_value(
            "REPORT_SHADOW_THRESHOLD",
            defaults.report_shadow_threshold,
        )?,
        reporter_min_age_secs: env_value("REPORTER_MIN_AGE_SECS", defaults.reporter_min_age_secs)?,
        reporter_min_tier: env_value("REPORTER_MIN_TIER", defaults.reporter_min_tier)?,
        max_reports_per_window: env_value(
            "MAX_REPORTS_PER_WINDOW",
            defaults.max_reports_per_window,
        )?,
        report_window_secs: env_value("REPORT_WINDOW_SECS", defaults.report_window_secs)?,
        mention_notifs_per_hour: env_value(
            "MENTION_NOTIFS_PER_HOUR",
            defaults.mention_notifs_per_hour,
        )?,
        max_thread_depth: env_value("MAX_THREAD_DEPTH", defaults.max_thread_depth)?,
    }
    .validate()
    .map_err(anyhow::Error::from)
}

fn content_tier_config(
    prefix: &str,
    default: ContentTierConfig,
) -> anyhow::Result<ContentTierConfig> {
    Ok(ContentTierConfig {
        open_secs: env_value(&format!("{prefix}_OPEN_SECS"), default.open_secs)?,
        hidden_window_secs: env_value(
            &format!("{prefix}_HIDDEN_WINDOW_SECS"),
            default.hidden_window_secs,
        )?,
        seed_micro: env_value(&format!("{prefix}_SEED_MICRO"), default.seed_micro)?,
        fee_bps: env_value(&format!("{prefix}_FEE_BPS"), default.fee_bps)?,
        min_votes_to_resolve: env_value(
            &format!("{prefix}_MIN_VOTES_TO_RESOLVE"),
            default.min_votes_to_resolve,
        )?,
        seed_floor_micro: env_value(
            &format!("{prefix}_SEED_FLOOR_MICRO"),
            default.seed_floor_micro,
        )?,
        min_votes_floor: env_value(
            &format!("{prefix}_MIN_VOTES_FLOOR"),
            default.min_votes_floor,
        )?,
    })
}

fn content_config() -> anyhow::Result<ContentConfig> {
    let defaults = ContentConfig::default();
    ContentConfig {
        flash_cadence_secs: env_value("FLASH_CADENCE_SECS", defaults.flash_cadence_secs)?,
        daily_slots: env_value("DAILY_SLOTS", defaults.daily_slots)?,
        draft_ttl_secs: env_value("DRAFT_TTL_SECS", defaults.draft_ttl_secs)?,
        max_pending_drafts: env_value("MAX_PENDING_DRAFTS", defaults.max_pending_drafts)?,
        max_slot_horizon_secs: env_value("MAX_SLOT_HORIZON_SECS", defaults.max_slot_horizon_secs)?,
        daily_seed_budget_micro: env_value(
            "DAILY_SEED_BUDGET_MICRO",
            defaults.daily_seed_budget_micro,
        )?,
        video_max_attempts: env_value("VIDEO_MAX_ATTEMPTS", defaults.video_max_attempts)?,
        lease_secs: env_value("CONTENT_LEASE_SECS", defaults.lease_secs)?,
        backoff_base_secs: env_value("CONTENT_BACKOFF_BASE_SECS", defaults.backoff_base_secs)?,
        render_dir: env::var_os("CONTENT_RENDER_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or(defaults.render_dir),
        tier_defaults: [
            content_tier_config("DAILY", defaults.tier_defaults[0])?,
            content_tier_config("FLASH", defaults.tier_defaults[1])?,
        ],
    }
    .validate()
    .map_err(anyhow::Error::from)
}

fn prepare_content(config: ContentConfig) -> anyhow::Result<ContentConfig> {
    let config = config.validate()?;
    std::fs::create_dir_all(&config.render_dir)?;
    if !std::fs::canonicalize(&config.render_dir)?.is_dir() {
        anyhow::bail!("CONTENT_RENDER_DIR must resolve to a directory");
    }
    Ok(config)
}

async fn phase5_preflight<S: Store, C: Clock + ?Sized>(
    store: &S,
    clock: &C,
    services: &Phase5Services,
    rep_config: RepConfig,
    lp_kill_config: LpKillConfig,
    flags: Phase5Flags,
) -> anyhow::Result<()> {
    if flags.publisher {
        application::content::publisher_tick(
            store,
            clock,
            &services.config,
            rep_config,
            lp_kill_config,
        )
        .await?;
    }
    if flags.video_worker {
        application::video::worker_tick(store, clock, services.renderer.as_ref(), &services.config)
            .await?;
    }
    if flags.moderation_runner {
        application::moderation_escalate::runner_tick(
            store,
            clock,
            services.moderation_preflight.as_ref(),
            &services.config,
        )
        .await?;
    }
    Ok(())
}

fn spawn_phase5_loops(
    store: PgStore,
    clock: Arc<dyn Clock>,
    services: &Phase5Services,
    rep_config: RepConfig,
    lp_kill_config: LpKillConfig,
    flags: Phase5Flags,
    tick: std::time::Duration,
) {
    if flags.publisher {
        let store = store.clone();
        let clock = Arc::clone(&clock);
        let config = services.config.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tick);
            loop {
                interval.tick().await;
                if let Err(error) = application::content::publisher_tick(
                    &store,
                    clock.as_ref(),
                    &config,
                    rep_config,
                    lp_kill_config,
                )
                .await
                {
                    eprintln!("publisher error: {error}");
                }
            }
        });
    }
    if flags.video_worker {
        let store = store.clone();
        let clock = Arc::clone(&clock);
        let renderer = Arc::clone(&services.renderer);
        let config = services.config.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tick);
            loop {
                interval.tick().await;
                if let Err(error) = application::video::worker_tick(
                    &store,
                    clock.as_ref(),
                    renderer.as_ref(),
                    &config,
                )
                .await
                {
                    eprintln!("video worker error: {error}");
                }
            }
        });
    }
    if flags.moderation_runner {
        let preflight = Arc::clone(&services.moderation_preflight);
        let config = services.config.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tick);
            loop {
                interval.tick().await;
                if let Err(error) = application::moderation_escalate::runner_tick(
                    &store,
                    clock.as_ref(),
                    preflight.as_ref(),
                    &config,
                )
                .await
                {
                    eprintln!("moderation runner error: {error}");
                }
            }
        });
    }
}

/// D29 two-factor chaos arm: ANY `CHAOS_*` variable demands
/// `OPINIONS_ENV=staging` AND `CHAOS_ENABLED=1`; a prod mis-set is a startup
/// error, never a silently armed fault.
fn validate_chaos_arm(
    vars: &[(String, String)],
    opinions_env: Option<&str>,
) -> anyhow::Result<bool> {
    let chaos: Vec<&String> = vars
        .iter()
        .map(|(name, _)| name)
        .filter(|name| name.starts_with("CHAOS_"))
        .collect();
    if chaos.is_empty() {
        return Ok(false);
    }
    let enabled = vars
        .iter()
        .any(|(name, value)| name == "CHAOS_ENABLED" && value == "1");
    if opinions_env == Some("staging") && enabled {
        return Ok(true);
    }
    anyhow::bail!(
        "CHAOS_* is set ({names}) but the two-factor arm is incomplete: \
         OPINIONS_ENV=staging AND CHAOS_ENABLED=1 are both required",
        names = chaos
            .iter()
            .map(|name| name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
}

/// D29 crash-point adapter slot: production and non-crash chaos profiles are
/// transparent; the readiness path explicitly selects the staging barrier.
fn crash_point_slot(
    armed: bool,
    readiness_file: Option<&str>,
) -> anyhow::Result<Arc<dyn ResolutionCrashPoint>> {
    if !armed || readiness_file.is_none() {
        return Ok(Arc::new(NoopCrashPoint));
    }
    let point = StagingCrashPoint::from_env_value(readiness_file)?;
    eprintln!(
        "chaos arm validated: resolution crash point awaits kill at {}",
        point.readiness_file().display()
    );
    Ok(Arc::new(point))
}

fn admin_handles() -> anyhow::Result<Vec<String>> {
    match env::var("ADMIN_HANDLES") {
        Ok(value) => {
            let handles = value
                .split(',')
                .map(str::trim)
                .filter(|handle| !handle.is_empty())
                .map(ToString::to_string)
                .collect::<Vec<_>>();
            if handles.is_empty() {
                anyhow::bail!("ADMIN_HANDLES must contain at least one non-empty handle");
            }
            Ok(handles)
        }
        Err(env::VarError::NotPresent) => Ok(Vec::new()),
        Err(error) => Err(error.into()),
    }
}

impl Clock for SystemClock {
    fn now(&self) -> OffsetDateTime {
        OffsetDateTime::now_utc()
    }
}

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../../migrations");

#[tokio::main]
#[allow(clippy::too_many_lines)]
async fn main() -> anyhow::Result<()> {
    let database_url = env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://opinions:opinions@localhost:15434/opinions".to_string());
    let store = PgStore::connect(&database_url)
        .await
        .map_err(|e| anyhow::anyhow!("database connect failed: {e}"))?;
    MIGRATOR.run(store.pool_handle()).await?;

    let rep_config = rep_config()?;
    let lp_kill_config = lp_kill_config()?;
    let content_config = prepare_content(content_config()?)?;
    let phase5_flags = Phase5Flags::from_env()?;

    if env::args().any(|a| a == "--seed-demo") {
        seed::seed_demo(&store, rep_config, lp_kill_config).await?;
        return Ok(());
    }

    let bind = env::var("BIND_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".to_string());
    if !(bind.starts_with("127.0.0.1") || bind.starts_with("localhost")) {
        // Phase 1 has no real authentication (grok-p1r1 M2): loopback is the
        // default posture; leaving it is an explicit, logged decision.
        eprintln!("WARNING: binding {bind} — Phase 1 trusts user_id behind DEMO_TOKEN; keep this loopback-only unless you know why not");
    }
    let social_config = social_config()?;
    let admin_handles = admin_handles()?;
    // Phase 6 startup validation (D26/D29): fail loud, never fail open.
    let env_vars: Vec<(String, String)> = env::vars().collect();
    let chaos_armed = validate_chaos_arm(&env_vars, env::var("OPINIONS_ENV").ok().as_deref())?;
    let fault_injector = process_faults()
        .map_err(|error| anyhow::anyhow!(error.to_string()))?
        .clone();
    let admin_tokens = AdminTokens::from_env_required().map_err(anyhow::Error::msg)?;
    let rail_identity = rail_identity_from_env()?;
    // Phase 7 money observability (W4 finalized ask, coordinator-applied):
    // Phase 7 money observability: the durable Pg alert store carries D35's
    // at-least-once delivery bookkeeping and incident lifecycle over the 0011
    // `alert_outbox` columns, and the pump redelivers anything that was raised
    // but never observed a page. `deliver_pending` takes the clock so the
    // delivery stamp is a real time — a synthesized one would record a page
    // that did not happen at that moment.
    //
    // The alerter is `SharedAlerter`, which records pages in-process. That is
    // NOT an external pager and this composition does not pretend otherwise:
    // the durable `alert_outbox` row is the operator-visible artefact until a
    // real pager adapter is chosen. See var/p9-db-runtime-report.md.
    let alert_store = adapters::pg::PgAlertStore::from_store(&store);
    tokio::spawn(async move {
        let manager = application::ops::alerts::IncidentManager {
            store: alert_store,
            alerter: adapters::money_ports::SharedAlerter::new(),
        };
        let mut interval = tokio::time::interval(std::time::Duration::from_mins(1));
        loop {
            interval.tick().await;
            if let Err(error) = manager.deliver_pending(SystemClock.now()).await {
                eprintln!("alert delivery error: {error}");
            }
        }
    });
    let crash_point = crash_point_slot(chaos_armed, env::var(READY_FILE_ENV).ok().as_deref())?;
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let template_engine = application::content::draft_engine();
    let phase5_services = Phase5Services {
        config: content_config,
        lp_kill: lp_kill_config,
        draft_engine: adapters::llm::draft_engine_from_env(template_engine),
        renderer: adapters::render::renderer(),
        moderation_preflight: adapters::llm::moderation_preflight_from_env(),
    };
    phase5_preflight(
        &store,
        clock.as_ref(),
        &phase5_services,
        rep_config,
        lp_kill_config,
        phase5_flags,
    )
    .await?;
    let config_watch = application::ops::reconciler::ConfigWatch::new();
    let state = AppState::with_phase6_ops(
        store.clone(),
        Arc::clone(&clock),
        resolve_config()?,
        rep_config,
        vote_integrity_config()?,
        integrity_sweep_config()?,
        vote_metadata_config()?,
        social_config,
        admin_handles.clone(),
        phase5_services.clone(),
        admin_tokens,
        Arc::clone(&crash_point),
        Arc::new(config_watch.clone()),
    );
    let relay = OutboxRelay::with_faults(
        state.inner.store.pool_handle().clone(),
        state.event_sender(),
        fault_injector,
    );
    tokio::spawn(relay.run(std::time::Duration::from_millis(100)));
    let notifier = OutboxNotifier::new(
        store.clone(),
        Arc::clone(&clock),
        state.event_sender(),
        social_config,
        admin_handles,
    );
    tokio::spawn(notifier.run(std::time::Duration::from_millis(100)));
    // D24 read path: watch snapshot per process; outbox events are wake-ups
    // only; the periodic heal tick recovers any missed wake-up.
    let reconciler = application::ops::reconciler::Reconciler {
        io: store.clone(),
        watch: config_watch.clone(),
    };
    if let Err(error) = reconciler.reconcile().await {
        eprintln!("config reconciler initial load failed: {error}");
    }
    tokio::spawn(reconciler.run(std::time::Duration::from_millis(200), 150));
    // `--continuous`: the real D27 invariant sweep — once immediately, then
    // every 60s; a single violation is loud, never fatal to the process.
    //
    // The verdict is driven into the D35 incident lifecycle, not merely
    // printed. spec.md's "an alert fires on a single micro-USDC of drift" is
    // not satisfied by a line on stderr: a failing identity must open a durable
    // `alert_outbox` incident (deduped inside the episode it already opened),
    // and a recovering identity must resolve it so a recurrence re-pages. The
    // policy lives in `sync_invariant_report` so this stays wiring.
    if env::args().any(|a| a == "--continuous") {
        let sweep_store = store.clone();
        let sweep_alerts = adapters::pg::PgAlertStore::from_store(&store);
        tokio::spawn(async move {
            let manager = application::ops::alerts::IncidentManager {
                store: sweep_alerts,
                alerter: adapters::money_ports::SharedAlerter::new(),
            };
            let mut interval = tokio::time::interval(std::time::Duration::from_mins(1));
            loop {
                interval.tick().await;
                match application::integrity::invariant_sweep::run(&sweep_store).await {
                    Ok(report) => {
                        if report.pass {
                            eprintln!("invariant sweep: PASS (as of {})", report.as_of);
                        } else {
                            eprintln!("invariant sweep: VIOLATION {report:?}");
                        }
                        if let Err(error) = manager
                            .sync_invariant_report(&report, SystemClock.now())
                            .await
                        {
                            eprintln!("invariant incident sync error: {error}");
                        }
                    }
                    Err(error) => eprintln!("invariant sweep error: {error}"),
                }
            }
        });
    }
    // Always-on leased ops-job runner: one consumer drains BOTH replay_job and
    // refanout durable commands (D26 manual ops).
    {
        let jobs_store = store.clone();
        tokio::spawn(async move {
            let clock = SystemClock;
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(200));
            loop {
                interval.tick().await;
                match application::ops::replay_job::run_due(&jobs_store, &clock, 60, 100).await {
                    Ok(0) => {}
                    Ok(delivered) => eprintln!("ops jobs delivered: {delivered}"),
                    Err(error) => eprintln!("ops job runner error: {error}"),
                }
            }
        });
    }
    let scheduler_state = state.clone();
    let scheduler = SchedulerConfig::from_env()?;
    spawn_phase5_loops(
        store.clone(),
        Arc::clone(&clock),
        &phase5_services,
        rep_config,
        lp_kill_config,
        phase5_flags,
        scheduler.tick,
    );
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(scheduler.tick);
        loop {
            interval.tick().await;
            let use_case = application::advance_due::AdvanceDue {
                store: &scheduler_state.inner.store,
                clock: &scheduler_state.inner.clock,
                resolve_config: scheduler_state.inner.resolve_config,
                rep_config: scheduler_state.inner.rep_config,
                integrity_config: scheduler_state.inner.integrity_sweep_config,
                crash_point: scheduler_state.inner.crash_point.as_ref(),
            };
            match use_case.sweep().await {
                Ok(report)
                    if report.advanced != 0
                        || report.resolved != 0
                        || report.curator_needed != 0
                        || report.held != 0
                        || report.swept_pass != 0
                        || report.swept_flag != 0
                        || !report.errors.is_empty() =>
                {
                    eprintln!("scheduler report: {report:?}");
                }
                Ok(_) => {}
                Err(error) => eprintln!("scheduler query error: {error}"),
            }
        }
    });
    // Phase 7 rails + screening composition (W1/W3 finalized asks). The rails
    // are constructed and REMOTE-validated only under a validated identity;
    // screening rides the staging-armed sandbox providers (unarmed = deny).
    if let Some(identity) = rail_identity.clone() {
        let rails = adapters::rails::solana::SolanaRails::new(identity)
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        rails
            .validate_remote_identity()
            .await
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        println!("solana rails: remote identity validated");
    }
    let region_allowset_version = region_allowset_version()?;
    let sandbox_compliance =
        std::sync::Arc::new(adapters::compliance::sandbox::SandboxCompliance::from_env(
            clock.clone(),
            None,
            region_allowset_version,
            None,
        ));
    let withdraw_services = adapters::http::routes::withdraw::WithdrawServices {
        geo: sandbox_compliance.clone(),
        sanctions: sandbox_compliance,
    };
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    println!("core listening on {bind}");
    axum::serve(
        listener,
        router(state)
            .layer(axum::Extension(withdraw_services))
            .into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use application::fakes::InMemoryStore;

    use super::*;

    #[tokio::test]
    async fn phase5_loops_disabled_by_default_and_enabled_loops_preflight_clean() {
        let store = InMemoryStore::new();
        let clock = application::fakes::FakeClock::at(OffsetDateTime::UNIX_EPOCH);
        let services = Phase5Services::default();
        assert!(phase5_preflight(
            &store,
            &clock,
            &services,
            RepConfig::default(),
            LpKillConfig::default(),
            Phase5Flags::default(),
        )
        .await
        .is_ok());
        // Coordinator re-manifest (wave): publisher and video are LLM-independent,
        // so enabling them preflights CLEAN keyless. The moderation runner has no
        // honest keyless fallback — enabling it without a configured engine is a
        // TYPED startup failure (moderation_escalate's documented probe contract),
        // never a silent no-op.
        for flags in [
            Phase5Flags {
                publisher: true,
                ..Phase5Flags::default()
            },
            Phase5Flags {
                video_worker: true,
                ..Phase5Flags::default()
            },
        ] {
            assert!(phase5_preflight(
                &store,
                &clock,
                &services,
                RepConfig::default(),
                LpKillConfig::default(),
                flags,
            )
            .await
            .is_ok());
        }
        let keyless_moderation = phase5_preflight(
            &store,
            &clock,
            &services,
            RepConfig::default(),
            LpKillConfig::default(),
            Phase5Flags {
                moderation_runner: true,
                ..Phase5Flags::default()
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(
            keyless_moderation.downcast_ref::<application::error::StoreError>(),
            Some(application::error::StoreError::Unavailable(
                "phase5:moderation-preflight"
            ))
        ));
    }

    #[test]
    fn chaos_arm_requires_both_factors_and_is_quiet_when_unset() {
        let none: Vec<(String, String)> = vec![("PATH".into(), "/bin".into())];
        assert!(!validate_chaos_arm(&none, None).unwrap());
        let armed = vec![
            ("CHAOS_ENABLED".to_string(), "1".to_string()),
            ("CHAOS_WS_DROP_EVERY_N".to_string(), "1000".to_string()),
        ];
        assert!(validate_chaos_arm(&armed, Some("staging")).unwrap());
        // Missing either factor with ANY CHAOS_* set is a startup error.
        assert!(validate_chaos_arm(&armed, Some("prod")).is_err());
        assert!(validate_chaos_arm(&armed, None).is_err());
        let unarmed = vec![("CHAOS_WS_DROP_EVERY_N".to_string(), "1000".to_string())];
        assert!(validate_chaos_arm(&unarmed, Some("staging")).is_err());
        let disabled = vec![("CHAOS_ENABLED".to_string(), "0".to_string())];
        assert!(validate_chaos_arm(&disabled, Some("staging")).is_err());
        let _ = crash_point_slot(false, Some("/tmp/ignored.ready")).unwrap();
        let _ = crash_point_slot(true, None).unwrap();
        assert!(crash_point_slot(true, Some(" ")).is_err());
        let _ = crash_point_slot(true, Some("/tmp/opinions-test.ready")).unwrap();
    }

    /// D26/D29 posture for this file: every startup input fails LOUD, never
    /// open. `REGION_ALLOWSET_VERSION` stamps the D33 geo policy version onto
    /// screening verdicts, and a stale-but-plausible version is what lets an
    /// old Clear survive an allowset change — so a present-but-malformed value
    /// must be a startup error, never a silent fallback to the seed version.
    /// The D24 catalog bounds the key at `>= 1, monotone +1`.
    #[test]
    fn a_malformed_region_allowset_version_is_a_startup_error() {
        std::env::remove_var("REGION_ALLOWSET_VERSION");
        assert_eq!(
            region_allowset_version().unwrap(),
            1,
            "absent keeps the documented seed"
        );
        std::env::set_var("REGION_ALLOWSET_VERSION", "7");
        assert_eq!(region_allowset_version().unwrap(), 7);
        for bad in ["v7", "", " ", "7.0", "9999999999999999999999"] {
            std::env::set_var("REGION_ALLOWSET_VERSION", bad);
            assert!(
                region_allowset_version().is_err(),
                "malformed {bad:?} must fail startup, not silently become the seed"
            );
        }
        for out_of_range in ["0", "-3"] {
            std::env::set_var("REGION_ALLOWSET_VERSION", out_of_range);
            assert!(
                region_allowset_version().is_err(),
                "{out_of_range} violates the D24 bound (>= 1)"
            );
        }
        std::env::remove_var("REGION_ALLOWSET_VERSION");
    }

    #[test]
    fn render_directory_and_invalid_content_config_are_startup_failures() {
        let path = std::env::temp_dir().join(format!("opinions-phase5-{}", uuid::Uuid::new_v4()));
        let valid = ContentConfig {
            render_dir: path.clone(),
            ..ContentConfig::default()
        };
        assert_eq!(prepare_content(valid).unwrap().render_dir, path);
        std::fs::remove_dir(&path).unwrap();

        let invalid = ContentConfig {
            daily_slots: 0,
            ..ContentConfig::default()
        };
        assert!(prepare_content(invalid).is_err());
    }
}
