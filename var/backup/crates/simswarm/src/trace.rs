//! Canonical planned and live trace schemas plus strict replay validation.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::domain::action::{ActorRef, Decision, MarketView};
use crate::domain::rng::CountedRng;
use crate::manifest::{ManifestError, RunManifest, DECISION_VERSION};

pub const TRACE_SCHEMA_VERSION: u16 = 1;

/// Spec §9 / D35 trade-confirm p95 target (HTTP send → 2xx).
pub const SLO_TRADE_CONFIRM_P95_MS: u64 = 300;
/// Spec §9 / D35 close-to-paid p99 target (Closed → Paid minus named risk-hold).
pub const SLO_CLOSE_TO_PAID_P99_MS: u64 = 1_000;
/// Spec §9 / D35 WS delivery p95 target.
pub const SLO_WS_DELIVERY_P95_MS: u64 = 100;

/// Operation names excluded from every gated series (chaos-delay legs).
pub const SLO_EXCLUDED_OPERATIONS: &[&str] = &["chaos-delay"];

/// `close-to-paid` = authoritative Closed transition → committed Paid
/// minus only the named risk-hold interval. `tally_hidden_at` is never
/// an input (D35 / NEW-M5).
#[must_use]
pub const fn close_to_paid_ms(
    closed_transition_ms: u64,
    paid_committed_ms: u64,
    risk_hold_ms: u64,
) -> u64 {
    paid_committed_ms
        .saturating_sub(closed_transition_ms)
        .saturating_sub(risk_hold_ms)
}

/// Deterministic unit-trace of the close-to-paid timer. The hidden-tally
/// watermark is carried so tests can prove it is unused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloseToPaidTrace {
    pub closed_transition_ms: u64,
    pub paid_committed_ms: u64,
    pub risk_hold_ms: u64,
    pub tally_hidden_at_ms: u64,
}

impl CloseToPaidTrace {
    #[must_use]
    pub const fn gated_ms(&self) -> u64 {
        close_to_paid_ms(
            self.closed_transition_ms,
            self.paid_committed_ms,
            self.risk_hold_ms,
        )
    }
}

/// Per-profile minimum-series contract derived from a run manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SeriesContract {
    pub expected_trade_2xx: usize,
    pub expected_close_to_paid: usize,
    pub expected_ws_delivery: usize,
    /// D35 release money path: attempts, not 2xx. The profile must actually
    /// walk the withdrawal rail; the rail's own outcome is W1's contract.
    pub expected_money_path: usize,
}

impl SeriesContract {
    /// Convoy 2xx live inside the gated trade series: expected trade
    /// count is ≥ 50% of lifecycle trade-capable agents. Close-to-paid
    /// expects one Paid book (the ≥1k-voter lifecycle book on Full/2k).
    /// WS expects one sample per 60 spike ticks, at least one when a
    /// spike is configured.
    #[must_use]
    pub fn from_manifest(manifest: &crate::manifest::RunManifest) -> Self {
        let lifecycle = manifest.scenario.lifecycle_market.as_str();
        let trade_capable = manifest
            .agents
            .iter()
            .filter(|agent| agent.trade_capable && agent.target_market == lifecycle)
            .count();
        let expected_trade_2xx = trade_capable.div_ceil(2);
        // Full/2k includes the ≥1k-voter lifecycle book in the gated
        // close-to-paid series. Smoke pays after the swarm process exits
        // (crash-barrier in the e2e), so its contract does not require a
        // live Paid sample from the runner.
        let expected_close_to_paid = match manifest.profile {
            crate::manifest::Profile::Full => 1,
            crate::manifest::Profile::Smoke => 0,
        };
        let spike = usize::try_from(manifest.scenario.schedule.close_spike_ticks).unwrap_or(0);
        let expected_ws_delivery = if spike == 0 { 0 } else { 1 };
        // A money-capable agent decides to withdraw on two of every three
        // opportunities, so one attempt each is the floor.
        let expected_money_path = manifest
            .agents
            .iter()
            .filter(|agent| agent.money_capable)
            .count()
            .div_ceil(2);
        Self {
            expected_trade_2xx,
            expected_close_to_paid,
            expected_ws_delivery,
            expected_money_path,
        }
    }
}

/// Why a gated SLO report failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SloGateError {
    #[error("thin {series} series: observed {observed} < expected {expected}")]
    ThinSeries {
        series: &'static str,
        observed: usize,
        expected: usize,
    },
    #[error("{series} {percentile} {observed:?}ms exceeds {target}ms")]
    MissedTarget {
        series: &'static str,
        percentile: &'static str,
        observed: Option<u64>,
        target: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceHeader {
    pub schema_version: u16,
    pub code_version: String,
    pub decision_version: String,
    pub manifest_hash: String,
}

impl TraceHeader {
    pub fn from_manifest(manifest: &RunManifest) -> Result<Self, TraceError> {
        Ok(Self {
            schema_version: TRACE_SCHEMA_VERSION,
            code_version: manifest.code_version.clone(),
            decision_version: DECISION_VERSION.into(),
            manifest_hash: manifest.hash_hex()?,
        })
    }

    pub fn validate(&self, manifest: &RunManifest) -> Result<(), TraceError> {
        if self.schema_version != TRACE_SCHEMA_VERSION {
            return Err(TraceError::SchemaMismatch);
        }
        if self.code_version != manifest.code_version {
            return Err(TraceError::CodeMismatch);
        }
        if self.decision_version != manifest.decision_version {
            return Err(TraceError::DecisionMismatch);
        }
        if self.manifest_hash != manifest.hash_hex()? {
            return Err(TraceError::ManifestMismatch);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PlannedOpportunity {
    pub schema_version: u16,
    pub sim_tick: u64,
    pub actor: ActorRef,
    pub local_seq: u64,
    pub target_market: String,
    pub rng_before: u64,
    pub rng_after: u64,
}

impl PlannedOpportunity {
    #[must_use]
    pub fn order_key(&self) -> (u64, &ActorRef, u64) {
        (self.sim_tick, &self.actor, self.local_seq)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedTrace {
    pub header: TraceHeader,
    pub opportunities: Vec<PlannedOpportunity>,
}

impl PlannedTrace {
    pub fn from_manifest(manifest: &RunManifest) -> Result<Self, TraceError> {
        manifest.validate()?;
        let mut opportunities = Vec::new();
        for agent in &manifest.agents {
            let mut rng = CountedRng::new(manifest.stream_seed(&agent.id, "schedule"));
            let ticks = manifest.scenario.schedule.opportunity_ticks(&mut rng)?;
            let mut decision_words = 0_u64;
            for (local_seq, tick) in ticks.into_iter().enumerate() {
                let rng_before = decision_words;
                decision_words =
                    decision_words.saturating_add(agent.persona.rng_words_per_decision());
                opportunities.push(PlannedOpportunity {
                    schema_version: TRACE_SCHEMA_VERSION,
                    sim_tick: tick,
                    actor: ActorRef::agent(&agent.id),
                    local_seq: u64::try_from(local_seq).unwrap_or(u64::MAX),
                    target_market: agent.target_market.clone(),
                    rng_before,
                    rng_after: decision_words,
                });
            }
        }
        for ring in &manifest.rings {
            let ticks = ring.opportunity_ticks(manifest.scenario.schedule.close_tick);
            let mut decision_words = 0_u64;
            for (local_seq, tick) in ticks.into_iter().enumerate() {
                let rng_before = decision_words;
                decision_words = decision_words.saturating_add(ring.rng_words_per_decision());
                opportunities.push(PlannedOpportunity {
                    schema_version: TRACE_SCHEMA_VERSION,
                    sim_tick: tick,
                    actor: ActorRef::ring(&ring.id),
                    local_seq: u64::try_from(local_seq).unwrap_or(u64::MAX),
                    target_market: ring.target_market.clone(),
                    rng_before,
                    rng_after: decision_words,
                });
            }
        }
        let mut trace = Self {
            header: TraceHeader::from_manifest(manifest)?,
            opportunities,
        };
        trace.canonicalize();
        Ok(trace)
    }

    pub fn canonicalize(&mut self) {
        self.opportunities
            .sort_by(|left, right| left.order_key().cmp(&right.order_key()));
    }

    pub fn canonical_json_lines(&self) -> Result<Vec<u8>, TraceError> {
        let mut bytes = serde_json::to_vec(&self.header)?;
        bytes.push(b'\n');
        for opportunity in &self.opportunities {
            bytes.extend(serde_json::to_vec(opportunity)?);
            bytes.push(b'\n');
        }
        Ok(bytes)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NormalizedOutcome {
    Http { status: u16, code: Option<String> },
    Ws { source_seq: i64, frame_type: String },
    Transport { code: String },
    Protocol { code: String },
    Batch { outcomes: Vec<NormalizedOutcome> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RetryClass {
    Terminal,
    WindowBlocked,
    Conflict,
    Transient { retry_tick: u64 },
}

#[must_use]
pub fn classify_retry(outcome: &NormalizedOutcome, sim_tick: u64, delay_ticks: u64) -> RetryClass {
    match outcome {
        NormalizedOutcome::Http { status: 423, .. } => RetryClass::WindowBlocked,
        NormalizedOutcome::Http { status: 409, .. } => RetryClass::Conflict,
        NormalizedOutcome::Http { status, .. } if *status >= 500 => RetryClass::Transient {
            retry_tick: sim_tick.saturating_add(delay_ticks),
        },
        NormalizedOutcome::Transport { .. } => RetryClass::Transient {
            retry_tick: sim_tick.saturating_add(delay_ticks),
        },
        NormalizedOutcome::Batch { outcomes } => outcomes
            .iter()
            .map(|outcome| classify_retry(outcome, sim_tick, delay_ticks))
            .find(|class| matches!(class, RetryClass::Transient { .. }))
            .or_else(|| {
                outcomes
                    .iter()
                    .map(|outcome| classify_retry(outcome, sim_tick, delay_ticks))
                    .find(|class| !matches!(class, RetryClass::Terminal))
            })
            .unwrap_or(RetryClass::Terminal),
        NormalizedOutcome::Http { .. }
        | NormalizedOutcome::Ws { .. }
        | NormalizedOutcome::Protocol { .. } => RetryClass::Terminal,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WsRecord {
    Event { source_seq: i64, frame_type: String },
    Gap { expected: i64, observed: i64 },
    Snapshot { source_seq: i64, view_hash: String },
}

pub fn validate_ws_recovery(records: &[WsRecord]) -> Result<(), TraceError> {
    for (index, record) in records.iter().enumerate() {
        if matches!(record, WsRecord::Gap { .. })
            && !matches!(records.get(index + 1), Some(WsRecord::Snapshot { .. }))
        {
            return Err(TraceError::GapWithoutSnapshot);
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecidedAction {
    pub schema_version: u16,
    pub opportunity: PlannedOpportunity,
    pub market_view: MarketView,
    pub market_view_bytes: Vec<u8>,
    pub market_view_hash: String,
    pub decision: Decision,
    pub outcome: NormalizedOutcome,
    pub retry: RetryClass,
    pub ws_records: Vec<WsRecord>,
}

impl DecidedAction {
    pub fn record(
        opportunity: PlannedOpportunity,
        view: &MarketView,
        decision: Decision,
        outcome: NormalizedOutcome,
        ws_records: Vec<WsRecord>,
        retry_delay_ticks: u64,
    ) -> Result<Self, TraceError> {
        validate_ws_recovery(&ws_records)?;
        Ok(Self::record_validated(
            opportunity,
            view,
            decision,
            outcome,
            ws_records,
            retry_delay_ticks,
        ))
    }

    #[must_use]
    pub fn record_without_ws(
        opportunity: PlannedOpportunity,
        view: &MarketView,
        decision: Decision,
        outcome: NormalizedOutcome,
        retry_delay_ticks: u64,
    ) -> Self {
        Self::record_validated(
            opportunity,
            view,
            decision,
            outcome,
            Vec::new(),
            retry_delay_ticks,
        )
    }

    fn record_validated(
        opportunity: PlannedOpportunity,
        view: &MarketView,
        decision: Decision,
        outcome: NormalizedOutcome,
        ws_records: Vec<WsRecord>,
        retry_delay_ticks: u64,
    ) -> Self {
        let market_view_bytes = canonical_view_bytes(view);
        let market_view_hash = hex(&Sha256::digest(&market_view_bytes));
        let retry = classify_retry(&outcome, opportunity.sim_tick, retry_delay_ticks);
        Self {
            schema_version: TRACE_SCHEMA_VERSION,
            opportunity,
            market_view: view.clone(),
            market_view_bytes,
            market_view_hash,
            decision,
            outcome,
            retry,
            ws_records,
        }
    }
}

fn canonical_view_bytes(view: &MarketView) -> Vec<u8> {
    let mut bytes = Vec::new();
    push_field(&mut bytes, view.market_ref.as_bytes());
    push_field(&mut bytes, view.state.as_bytes());
    bytes.extend_from_slice(&view.price_yes_micro.to_le_bytes());
    bytes.extend_from_slice(&view.price_no_micro.to_le_bytes());
    bytes.extend_from_slice(&view.sim_tick.to_le_bytes());
    bytes.extend_from_slice(&view.closes_tick.to_le_bytes());
    bytes
}

fn push_field(target: &mut Vec<u8>, value: &[u8]) {
    target.extend_from_slice(&u64::try_from(value.len()).unwrap_or(u64::MAX).to_le_bytes());
    target.extend_from_slice(value);
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecidedTrace {
    pub header: TraceHeader,
    pub actions: Vec<DecidedAction>,
}

impl DecidedTrace {
    pub fn validate_for_replay(&self, manifest: &RunManifest) -> Result<(), TraceError> {
        self.header.validate(manifest)?;
        for action in &self.actions {
            if action.schema_version != TRACE_SCHEMA_VERSION
                || action.opportunity.schema_version != TRACE_SCHEMA_VERSION
            {
                return Err(TraceError::SchemaMismatch);
            }
            validate_ws_recovery(&action.ws_records)?;
        }
        Ok(())
    }

    pub fn from_slice(bytes: &[u8]) -> Result<Self, TraceError> {
        serde_json::from_slice(bytes).map_err(TraceError::from)
    }

    pub fn canonical_bytes(&self) -> Result<Vec<u8>, TraceError> {
        serde_json::to_vec(self).map_err(TraceError::from)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LatencySample {
    pub operation: String,
    pub status: u16,
    pub latency_ms: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LatencyLog {
    pub samples: Vec<LatencySample>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LatencySummary {
    pub count: usize,
    pub p50_ms: Option<u64>,
    pub p95_ms: Option<u64>,
    pub p99_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SloReport {
    pub slo_enforce: bool,
    pub trade_confirm_p95_target_ms: u64,
    pub close_to_paid_p99_target_ms: u64,
    pub ws_delivery_p95_target_ms: u64,
    pub expected_trade_2xx: usize,
    pub expected_close_to_paid: usize,
    pub expected_ws_delivery: usize,
    pub expected_money_path: usize,
    pub trade_confirm_2xx: LatencySummary,
    pub trade_window_blocked_423: LatencySummary,
    pub close_to_paid: LatencySummary,
    pub ws_delivery: LatencySummary,
    /// Money-path attempts at any status: the release profile must walk the
    /// withdrawal rail even while the rail itself refuses.
    pub money_path_attempts: usize,
    pub passed: Option<bool>,
}

impl LatencyLog {
    pub fn record(&mut self, operation: impl Into<String>, status: u16, latency_ms: u64) {
        self.samples.push(LatencySample {
            operation: operation.into(),
            status,
            latency_ms,
        });
    }

    #[must_use]
    pub fn successful(&self, operation: &str) -> Vec<u64> {
        self.filtered(operation, |status| (200..300).contains(&status))
    }

    #[must_use]
    pub fn blocked(&self, operation: &str) -> Vec<u64> {
        self.filtered(operation, |status| status == 423)
    }

    /// Attempts at any status — the money path is gated on being walked,
    /// not on the rail's verdict.
    #[must_use]
    pub fn attempts(&self, operation: &str) -> usize {
        self.samples
            .iter()
            .filter(|sample| sample.operation == operation)
            .count()
    }

    fn filtered(&self, operation: &str, accept: impl Fn(u16) -> bool) -> Vec<u64> {
        self.samples
            .iter()
            .filter(|sample| sample.operation == operation && accept(sample.status))
            .map(|sample| sample.latency_ms)
            .collect()
    }

    pub fn canonical_bytes(&self) -> Result<Vec<u8>, TraceError> {
        serde_json::to_vec(self).map_err(TraceError::from)
    }

    #[must_use]
    pub fn grouped_counts(&self) -> BTreeMap<(String, u16), usize> {
        let mut grouped = BTreeMap::new();
        for sample in &self.samples {
            *grouped
                .entry((sample.operation.clone(), sample.status))
                .or_default() += 1;
        }
        grouped
    }

    #[must_use]
    pub fn slo_report(&self, enforce: bool) -> SloReport {
        self.slo_report_for(enforce, &SeriesContract::default())
    }

    #[must_use]
    pub fn slo_report_for(&self, enforce: bool, contract: &SeriesContract) -> SloReport {
        let default_contract = contract == &SeriesContract::default();
        let expected_trade = if enforce && default_contract {
            1
        } else {
            contract.expected_trade_2xx
        };
        let expected_close = if enforce && default_contract {
            1
        } else {
            contract.expected_close_to_paid
        };
        let expected_ws = if enforce && default_contract {
            1
        } else {
            contract.expected_ws_delivery
        };
        let trade_confirm_2xx = summarize(self.successful_gated("trade-confirm"));
        let trade_window_blocked_423 = summarize(self.blocked("trade-confirm"));
        let close_to_paid = summarize(self.successful_gated("close-to-paid"));
        let ws_delivery = summarize(self.successful_gated("ws-delivery"));
        let mut report = SloReport {
            slo_enforce: enforce,
            trade_confirm_p95_target_ms: SLO_TRADE_CONFIRM_P95_MS,
            close_to_paid_p99_target_ms: SLO_CLOSE_TO_PAID_P99_MS,
            ws_delivery_p95_target_ms: SLO_WS_DELIVERY_P95_MS,
            expected_trade_2xx: expected_trade,
            expected_close_to_paid: expected_close,
            expected_ws_delivery: expected_ws,
            expected_money_path: contract.expected_money_path,
            trade_confirm_2xx,
            trade_window_blocked_423,
            close_to_paid,
            ws_delivery,
            money_path_attempts: self.attempts("withdraw-request"),
            passed: None,
        };
        if enforce {
            report.passed = Some(evaluate_slo(&report).is_ok());
        }
        report
    }

    fn successful_gated(&self, operation: &str) -> Vec<u64> {
        self.samples
            .iter()
            .filter(|sample| {
                sample.operation == operation
                    && (200..300).contains(&sample.status)
                    && !SLO_EXCLUDED_OPERATIONS.contains(&sample.operation.as_str())
            })
            .map(|sample| sample.latency_ms)
            .collect()
    }
}

/// Evaluates a report against lib-constant targets and the per-profile
/// `observed ≥ expected` series contract. Thin series fail when enforce
/// populated the expected floors.
///
/// # Errors
/// [`SloGateError`] naming the first violated series or percentile.
pub fn evaluate_slo(report: &SloReport) -> Result<(), SloGateError> {
    if !report.slo_enforce {
        return Ok(());
    }
    check_count(
        "trade-confirm",
        report.trade_confirm_2xx.count,
        report.expected_trade_2xx,
    )?;
    check_count(
        "close-to-paid",
        report.close_to_paid.count,
        report.expected_close_to_paid,
    )?;
    check_count(
        "ws-delivery",
        report.ws_delivery.count,
        report.expected_ws_delivery,
    )?;
    check_count(
        "withdraw-request",
        report.money_path_attempts,
        report.expected_money_path,
    )?;
    check_target(
        "trade-confirm",
        "p95",
        &report.trade_confirm_2xx,
        report.expected_trade_2xx,
        report.trade_confirm_2xx.p95_ms,
        SLO_TRADE_CONFIRM_P95_MS,
    )?;
    check_target(
        "close-to-paid",
        "p99",
        &report.close_to_paid,
        report.expected_close_to_paid,
        report.close_to_paid.p99_ms,
        SLO_CLOSE_TO_PAID_P99_MS,
    )?;
    check_target(
        "ws-delivery",
        "p95",
        &report.ws_delivery,
        report.expected_ws_delivery,
        report.ws_delivery.p95_ms,
        SLO_WS_DELIVERY_P95_MS,
    )?;
    Ok(())
}

/// A percentile target binds a series the profile REQUIRES, or any series
/// that produced samples. A series whose per-profile `n_min` is 0 and which
/// produced nothing has no percentile to miss — the smoke profile pays after
/// the swarm process exits, so its live `close-to-paid` series is empty by
/// contract. Thinness is `check_count`'s job, never this one's.
fn check_target(
    series: &'static str,
    percentile: &'static str,
    summary: &LatencySummary,
    expected: usize,
    observed: Option<u64>,
    target: u64,
) -> Result<(), SloGateError> {
    if expected == 0 && summary.count == 0 {
        return Ok(());
    }
    check_percentile(series, percentile, observed, target)
}

fn check_count(series: &'static str, observed: usize, expected: usize) -> Result<(), SloGateError> {
    if observed < expected {
        return Err(SloGateError::ThinSeries {
            series,
            observed,
            expected,
        });
    }
    Ok(())
}

fn check_percentile(
    series: &'static str,
    percentile: &'static str,
    observed: Option<u64>,
    target: u64,
) -> Result<(), SloGateError> {
    match observed {
        Some(ms) if ms <= target => Ok(()),
        other => Err(SloGateError::MissedTarget {
            series,
            percentile,
            observed: other,
            target,
        }),
    }
}

fn summarize(mut samples: Vec<u64>) -> LatencySummary {
    samples.sort_unstable();
    LatencySummary {
        count: samples.len(),
        p50_ms: percentile(&samples, 50),
        p95_ms: percentile(&samples, 95),
        p99_ms: percentile(&samples, 99),
    }
}

fn percentile(sorted: &[u64], percentile: usize) -> Option<u64> {
    if sorted.is_empty() {
        return None;
    }
    let rank = (percentile * sorted.len()).div_ceil(100).saturating_sub(1);
    sorted.get(rank).copied()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[derive(Debug, thiserror::Error)]
pub enum TraceError {
    #[error("trace schema mismatch")]
    SchemaMismatch,
    #[error("trace code-version mismatch")]
    CodeMismatch,
    #[error("trace decision-version mismatch")]
    DecisionMismatch,
    #[error("trace manifest mismatch")]
    ManifestMismatch,
    #[error("WebSocket gap was not followed by a recovery snapshot")]
    GapWithoutSnapshot,
    #[error(transparent)]
    Manifest(#[from] ManifestError),
    #[error(transparent)]
    Schedule(#[from] crate::domain::schedule::ScheduleError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

impl PartialEq for TraceError {
    fn eq(&self, other: &Self) -> bool {
        std::mem::discriminant(self) == std::mem::discriminant(other)
    }
}

impl Eq for TraceError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::action::{Action, ActorRef, Decision, MarketView};
    use crate::manifest::{Profile, RunManifest};

    fn opportunity() -> PlannedOpportunity {
        PlannedOpportunity {
            schema_version: TRACE_SCHEMA_VERSION,
            sim_tick: 4,
            actor: ActorRef::agent("agent-0001"),
            local_seq: 2,
            target_market: "lifecycle".into(),
            rng_before: 3,
            rng_after: 4,
        }
    }

    #[test]
    fn canonical_plans_are_byte_identical_and_totally_ordered() {
        let manifest = RunManifest::profile(Profile::Smoke, 5);
        let mut trace = PlannedTrace::from_manifest(&manifest).unwrap();
        let bytes = trace.canonical_json_lines().unwrap();
        assert_eq!(
            bytes,
            PlannedTrace::from_manifest(&manifest)
                .unwrap()
                .canonical_json_lines()
                .unwrap()
        );
        trace.opportunities.reverse();
        trace.canonicalize();
        assert!(trace
            .opportunities
            .windows(2)
            .all(|pair| pair[0].order_key() <= pair[1].order_key()));
        let by_actor = trace.opportunities.iter().fold(
            BTreeMap::<ActorRef, Vec<&PlannedOpportunity>>::new(),
            |mut grouped, opportunity| {
                grouped
                    .entry(opportunity.actor.clone())
                    .or_default()
                    .push(opportunity);
                grouped
            },
        );
        assert!(by_actor.values().all(|opportunities| {
            opportunities
                .windows(2)
                .all(|pair| pair[0].rng_after == pair[1].rng_before)
        }));
    }

    #[test]
    fn decided_action_pins_view_outcome_ws_and_retry_without_latency() {
        let view = MarketView {
            market_ref: "lifecycle".into(),
            state: "live".into(),
            price_yes_micro: 500_000,
            price_no_micro: 500_000,
            sim_tick: 4,
            closes_tick: 180,
        };
        let action = DecidedAction::record(
            opportunity(),
            &view,
            Decision::new(vec![crate::domain::action::AgentAction::new(
                "agent-0001",
                Action::Observe,
            )]),
            NormalizedOutcome::Http {
                status: 503,
                code: Some("busy".into()),
            },
            vec![
                WsRecord::Gap {
                    expected: 8,
                    observed: 10,
                },
                WsRecord::Snapshot {
                    source_seq: 10,
                    view_hash: "abc".into(),
                },
            ],
            3,
        )
        .unwrap();
        assert_eq!(action.retry, RetryClass::Transient { retry_tick: 7 });
        assert!(!action.market_view_hash.is_empty());
        assert!(serde_json::to_value(&action)
            .unwrap()
            .get("latency_ms")
            .is_none());
        assert!(validate_ws_recovery(&action.ws_records).is_ok());
    }

    #[test]
    fn retry_classification_and_ws_gap_rules_are_pinned() {
        assert_eq!(
            classify_retry(
                &NormalizedOutcome::Http {
                    status: 423,
                    code: None
                },
                5,
                9
            ),
            RetryClass::WindowBlocked
        );
        assert_eq!(
            classify_retry(
                &NormalizedOutcome::Http {
                    status: 409,
                    code: None
                },
                5,
                9
            ),
            RetryClass::Conflict
        );
        assert_eq!(
            classify_retry(
                &NormalizedOutcome::Http {
                    status: 200,
                    code: None
                },
                5,
                9
            ),
            RetryClass::Terminal
        );
        assert_eq!(
            classify_retry(&NormalizedOutcome::Protocol { code: "bad".into() }, 5, 9),
            RetryClass::Terminal
        );
        assert_eq!(
            classify_retry(
                &NormalizedOutcome::Transport {
                    code: "offline".into()
                },
                5,
                9
            ),
            RetryClass::Transient { retry_tick: 14 }
        );
        assert_eq!(
            classify_retry(
                &NormalizedOutcome::Batch {
                    outcomes: vec![NormalizedOutcome::Http {
                        status: 423,
                        code: None
                    }]
                },
                5,
                9
            ),
            RetryClass::WindowBlocked
        );
        assert_eq!(
            classify_retry(
                &NormalizedOutcome::Batch {
                    outcomes: vec![NormalizedOutcome::Protocol { code: "x".into() }]
                },
                5,
                9
            ),
            RetryClass::Terminal
        );
        assert_eq!(
            validate_ws_recovery(&[WsRecord::Gap {
                expected: 1,
                observed: 3
            }]),
            Err(TraceError::GapWithoutSnapshot)
        );
    }

    #[test]
    fn replay_rejects_every_version_or_manifest_mismatch() {
        let manifest = RunManifest::profile(Profile::Smoke, 5);
        let header = TraceHeader::from_manifest(&manifest).unwrap();
        assert!(header.validate(&manifest).is_ok());

        let mut bad = header.clone();
        bad.schema_version += 1;
        assert_eq!(bad.validate(&manifest), Err(TraceError::SchemaMismatch));
        let mut bad = header.clone();
        bad.code_version.push('x');
        assert_eq!(bad.validate(&manifest), Err(TraceError::CodeMismatch));
        let mut bad = header.clone();
        bad.decision_version.push('x');
        assert_eq!(bad.validate(&manifest), Err(TraceError::DecisionMismatch));
        let mut bad = header;
        bad.manifest_hash.push('x');
        assert_eq!(bad.validate(&manifest), Err(TraceError::ManifestMismatch));
    }

    #[test]
    fn latency_log_is_intentionally_separate_and_canonical() {
        let mut log = LatencyLog::default();
        log.record("trade-confirm", 200, 42);
        log.record("trade-confirm", 423, 5);
        assert_eq!(log.successful("trade-confirm"), vec![42]);
        assert_eq!(log.blocked("trade-confirm"), vec![5]);
        assert_eq!(
            log.canonical_bytes().unwrap(),
            log.canonical_bytes().unwrap()
        );
        assert_eq!(
            log.grouped_counts().get(&("trade-confirm".into(), 200)),
            Some(&1)
        );
        log.record("trade-confirm", 200, 10);
        log.record("close-to-paid", 200, 100);
        log.record("ws-delivery", 200, 7);
        let report = log.slo_report(false);
        assert!(!report.slo_enforce);
        assert_eq!(report.trade_confirm_p95_target_ms, SLO_TRADE_CONFIRM_P95_MS);
        assert_eq!(report.close_to_paid_p99_target_ms, SLO_CLOSE_TO_PAID_P99_MS);
        assert_eq!(report.ws_delivery_p95_target_ms, SLO_WS_DELIVERY_P95_MS);
        assert_eq!(report.trade_confirm_2xx.p50_ms, Some(10));
        assert_eq!(report.trade_confirm_2xx.p95_ms, Some(42));
        assert_eq!(report.trade_window_blocked_423.p99_ms, Some(5));
        assert_eq!(report.close_to_paid.count, 1);
        assert_eq!(report.ws_delivery.p95_ms, Some(7));
        assert_eq!(summarize(Vec::new()).p50_ms, None);
        assert!(evaluate_slo(&report).is_ok());
    }

    #[test]
    fn close_to_paid_subtracts_only_the_named_risk_hold() {
        let trace = CloseToPaidTrace {
            closed_transition_ms: 10_000,
            paid_committed_ms: 12_400,
            risk_hold_ms: 400,
            tally_hidden_at_ms: 9_000,
        };
        assert_eq!(trace.gated_ms(), 2_000);
        assert_eq!(close_to_paid_ms(10_000, 12_400, 400), 2_000);
        assert_ne!(
            trace.gated_ms(),
            trace
                .paid_committed_ms
                .saturating_sub(trace.tally_hidden_at_ms)
        );
        assert_eq!(close_to_paid_ms(5_000, 4_000, 0), 0);
    }

    #[test]
    fn enforced_slo_fails_thin_series_and_missed_targets() {
        let mut empty = LatencyLog::default();
        empty.record("chaos-delay", 200, 5_000);
        let thin = empty.slo_report(true);
        assert!(thin.slo_enforce);
        assert_eq!(thin.passed, Some(false));
        assert_eq!(thin.trade_confirm_2xx.count, 0);
        assert!(matches!(
            evaluate_slo(&thin),
            Err(SloGateError::ThinSeries {
                series: "trade-confirm",
                ..
            })
        ));

        let mut log = LatencyLog::default();
        log.record("trade-confirm", 200, 50);
        log.record("close-to-paid", 200, 80);
        log.record("ws-delivery", 200, 10);
        let contract = SeriesContract {
            expected_trade_2xx: 1,
            expected_close_to_paid: 1,
            expected_ws_delivery: 1,
            expected_money_path: 0,
        };
        let ok = log.slo_report_for(true, &contract);
        assert_eq!(ok.passed, Some(true));
        assert!(evaluate_slo(&ok).is_ok());

        log.record("trade-confirm", 200, 900);
        let slow = log.slo_report_for(true, &contract);
        assert_eq!(slow.passed, Some(false));
        assert!(matches!(
            evaluate_slo(&slow),
            Err(SloGateError::MissedTarget {
                series: "trade-confirm",
                percentile: "p95",
                ..
            })
        ));

        let mut close_only = LatencyLog::default();
        close_only.record("trade-confirm", 200, 10);
        let missing_close = close_only.slo_report_for(true, &contract);
        assert!(matches!(
            evaluate_slo(&missing_close),
            Err(SloGateError::ThinSeries {
                series: "close-to-paid",
                ..
            })
        ));
        close_only.record("close-to-paid", 200, 10);
        let missing_ws = close_only.slo_report_for(true, &contract);
        assert!(matches!(
            evaluate_slo(&missing_ws),
            Err(SloGateError::ThinSeries {
                series: "ws-delivery",
                ..
            })
        ));

        let mut late = LatencyLog::default();
        late.record("trade-confirm", 200, 10);
        late.record("close-to-paid", 200, 5_000);
        late.record("ws-delivery", 200, 10);
        assert!(matches!(
            evaluate_slo(&late.slo_report_for(true, &contract)),
            Err(SloGateError::MissedTarget {
                series: "close-to-paid",
                percentile: "p99",
                ..
            })
        ));
        let mut late_ws = LatencyLog::default();
        late_ws.record("trade-confirm", 200, 10);
        late_ws.record("close-to-paid", 200, 10);
        late_ws.record("ws-delivery", 200, 500);
        assert!(matches!(
            evaluate_slo(&late_ws.slo_report_for(true, &contract)),
            Err(SloGateError::MissedTarget {
                series: "ws-delivery",
                percentile: "p95",
                ..
            })
        ));
        assert!(matches!(
            check_percentile("ws-delivery", "p95", None, 100),
            Err(SloGateError::MissedTarget { observed: None, .. })
        ));
    }

    #[test]
    fn a_series_with_no_n_min_and_no_samples_has_no_percentile_to_miss() {
        // The smoke contract: close-to-paid n_min is 0 because the book pays
        // after the swarm process exits. An empty series must not be read as
        // a missed p99, but a REQUIRED empty series is still thin.
        let smoke = SeriesContract {
            expected_trade_2xx: 1,
            expected_close_to_paid: 0,
            expected_ws_delivery: 1,
            expected_money_path: 0,
        };
        let mut log = LatencyLog::default();
        log.record("trade-confirm", 200, 40);
        log.record("ws-delivery", 200, 9);
        let report = log.slo_report_for(true, &smoke);
        assert_eq!(report.close_to_paid.count, 0);
        assert_eq!(report.close_to_paid.p99_ms, None);
        assert_eq!(report.passed, Some(true));
        assert!(evaluate_slo(&report).is_ok());
        // Samples appearing anyway still bind the target.
        log.record("close-to-paid", 200, 5_000);
        assert!(matches!(
            evaluate_slo(&log.slo_report_for(true, &smoke)),
            Err(SloGateError::MissedTarget {
                series: "close-to-paid",
                ..
            })
        ));
    }

    #[test]
    fn a_release_profile_that_never_walks_the_money_path_fails_the_gate() {
        let contract = SeriesContract {
            expected_trade_2xx: 1,
            expected_close_to_paid: 1,
            expected_ws_delivery: 1,
            expected_money_path: 2,
        };
        let mut log = LatencyLog::default();
        log.record("trade-confirm", 200, 10);
        log.record("close-to-paid", 200, 10);
        log.record("ws-delivery", 200, 10);
        let thin = log.slo_report_for(true, &contract);
        assert_eq!(thin.money_path_attempts, 0);
        assert_eq!(thin.expected_money_path, 2);
        assert!(matches!(
            evaluate_slo(&thin),
            Err(SloGateError::ThinSeries {
                series: "withdraw-request",
                observed: 0,
                expected: 2,
            })
        ));
        // Attempts count at ANY status: a refused rail still proves the
        // profile walked the money path.
        log.record("withdraw-request", 503, 12);
        log.record("withdraw-request", 200, 8);
        let walked = log.slo_report_for(true, &contract);
        assert_eq!(walked.money_path_attempts, 2);
        assert_eq!(log.attempts("withdraw-request"), 2);
        assert_eq!(walked.passed, Some(true));
        assert!(evaluate_slo(&walked).is_ok());
    }

    #[test]
    fn series_contract_is_derived_from_the_manifest_roster() {
        let smoke = crate::manifest::RunManifest::profile(crate::manifest::Profile::Smoke, 3);
        let contract = SeriesContract::from_manifest(&smoke);
        let trade_capable = smoke
            .agents
            .iter()
            .filter(|agent| agent.trade_capable && agent.target_market == "lifecycle")
            .count();
        assert_eq!(contract.expected_trade_2xx, trade_capable.div_ceil(2));
        assert_eq!(contract.expected_close_to_paid, 0);
        assert_eq!(contract.expected_ws_delivery, 1);
        // Smoke carries no money path; the release profile carries 10%.
        assert_eq!(contract.expected_money_path, 0);
        let full = crate::manifest::RunManifest::profile(crate::manifest::Profile::Full, 3);
        assert!(full.roster.lifecycle_voters.len() >= 1_000);
        let full_contract = SeriesContract::from_manifest(&full);
        assert!(full_contract.expected_trade_2xx >= contract.expected_trade_2xx);
        assert_eq!(full_contract.expected_close_to_paid, 1);
        let money_agents = full
            .agents
            .iter()
            .filter(|agent| agent.money_capable)
            .count();
        assert_eq!(money_agents * 10, full.agents.len());
        assert_eq!(full_contract.expected_money_path, money_agents.div_ceil(2));
    }

    #[test]
    fn decided_trace_round_trips_and_validates_each_action_schema() {
        let manifest = RunManifest::profile(Profile::Smoke, 12);
        let header = TraceHeader::from_manifest(&manifest).unwrap();
        let view = MarketView {
            market_ref: "lifecycle".into(),
            state: "live".into(),
            price_yes_micro: 500_000,
            price_no_micro: 500_000,
            sim_tick: 4,
            closes_tick: 180,
        };
        let action = DecidedAction::record(
            opportunity(),
            &view,
            Decision::new(vec![crate::domain::action::AgentAction::new(
                "agent-0001",
                Action::Observe,
            )]),
            NormalizedOutcome::Http {
                status: 200,
                code: None,
            },
            Vec::new(),
            1,
        )
        .unwrap();
        let trace = DecidedTrace {
            header,
            actions: vec![action],
        };
        let bytes = trace.canonical_bytes().unwrap();
        assert_eq!(DecidedTrace::from_slice(&bytes).unwrap(), trace);
        assert!(DecidedTrace::from_slice(b"bad").is_err());
        let mut bad = trace;
        bad.actions[0].schema_version += 1;
        assert_eq!(
            bad.validate_for_replay(&manifest),
            Err(TraceError::SchemaMismatch)
        );
    }
}
