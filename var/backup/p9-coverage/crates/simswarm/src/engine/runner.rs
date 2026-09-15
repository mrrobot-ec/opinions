//! Deterministic actor-task runner over injected time, sleep, and transport roles.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::domain::action::{Action, ActorRef, AgentAction, Decision};
use crate::domain::rng::CountedRng;
use crate::invariants_client::InvariantsClient;
use crate::manifest::RunManifest;
use crate::ports::{Sleeper, SwarmError, Ticker, Transport};
use crate::trace::{
    close_to_paid_ms, DecidedAction, DecidedTrace, LatencyLog, NormalizedOutcome,
    PlannedOpportunity, PlannedTrace, RetryClass,
};
use crate::transport::{execute_decision, fetch_market_view};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

pub struct Runner {
    pub transport: Arc<dyn Transport>,
    pub ticker: Arc<dyn Ticker>,
    pub sleeper: Arc<dyn Sleeper>,
    pub retry_delay_ticks: u64,
    pub max_retries: u8,
    /// Named risk-hold subtracted from Closed → Paid (never `tally_hidden_at`).
    pub risk_hold_ms: u64,
    /// Bounded polls used to observe Closed → Paid after actor tasks.
    /// Zero skips the live lifecycle series (unit tests).
    pub lifecycle_polls: u32,
}

pub struct RunReport {
    pub trace: DecidedTrace,
    pub latency: LatencyLog,
}

/// Measures committed trade-event time to receipt of its real WebSocket
/// frame. Multiple wire frames can share an `outbox_seq`, so each durable
/// event contributes at most one SLO sample.
#[derive(Default)]
pub struct WsDeliveryTracker {
    seen_outbox_sequences: BTreeSet<i64>,
}

fn parse_server_timestamp(value: &str) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(value, &Rfc3339).ok().or_else(|| {
        let format = time::format_description::parse_borrowed::<3>(
            "[year]-[month]-[day] [hour]:[minute]:[second].[subsecond] \
             [offset_hour sign:mandatory]:[offset_minute]:[offset_second]",
        )
        .ok()?;
        OffsetDateTime::parse(value, &format).ok()
    })
}

impl WsDeliveryTracker {
    pub fn observe(
        &mut self,
        frame: &serde_json::Value,
        delivered_at: OffsetDateTime,
        latency: &mut LatencyLog,
    ) {
        if frame.get("type").and_then(serde_json::Value::as_str) != Some("trade") {
            return;
        }
        let Some(sequence) = frame.get("outbox_seq").and_then(serde_json::Value::as_i64) else {
            return;
        };
        let Some(created_at) = frame.get("created_at").and_then(serde_json::Value::as_str) else {
            return;
        };
        let Some(created_at) = parse_server_timestamp(created_at) else {
            return;
        };
        let Ok(elapsed_ms) = u64::try_from((delivered_at - created_at).whole_milliseconds()) else {
            return;
        };
        if self.seen_outbox_sequences.insert(sequence) {
            latency.record("ws-delivery", 200, elapsed_ms);
        }
    }
}

impl Runner {
    pub async fn run(&self, manifest: &RunManifest) -> Result<RunReport, SwarmError> {
        let planned = PlannedTrace::from_manifest(manifest)
            .map_err(|error| SwarmError::Protocol(error.to_string()))?;
        self.run_planned(manifest, planned).await
    }

    pub async fn run_with_invariants(
        &self,
        manifest: &RunManifest,
        invariants: &InvariantsClient<'_>,
    ) -> Result<RunReport, SwarmError> {
        let _ = invariants.check_quiet_segment(true).await?;
        let report = self.run(manifest).await?;
        let _ = invariants.check_quiet_segment(true).await?;
        Ok(report)
    }

    pub async fn run_planned(
        &self,
        manifest: &RunManifest,
        planned: PlannedTrace,
    ) -> Result<RunReport, SwarmError> {
        let header = planned.header;
        if let Err(error) = header.validate(manifest) {
            return Err(SwarmError::Protocol(error.to_string()));
        }
        let mut by_actor = BTreeMap::<ActorRef, Vec<PlannedOpportunity>>::new();
        for opportunity in planned.opportunities {
            by_actor
                .entry(opportunity.actor.clone())
                .or_default()
                .push(opportunity);
        }
        let manifest = Arc::new(manifest.clone());
        let mut tasks = Vec::new();
        for opportunities in by_actor.into_values() {
            let worker = ActorWorker {
                transport: Arc::clone(&self.transport),
                ticker: Arc::clone(&self.ticker),
                sleeper: Arc::clone(&self.sleeper),
                manifest: Arc::clone(&manifest),
                retry_delay_ticks: self.retry_delay_ticks,
                max_retries: self.max_retries,
            };
            tasks.push(tokio::spawn(worker.run(opportunities)));
        }
        let mut actions = Vec::new();
        let mut latency = LatencyLog::default();
        for task in tasks {
            let actor_report = match task.await {
                Ok(report) => report?,
                Err(error) => return Err(SwarmError::Process(error.to_string())),
            };
            actions.extend(actor_report.actions);
            latency.samples.extend(actor_report.latency.samples);
        }
        actions.sort_by(|left, right| {
            left.opportunity
                .order_key()
                .cmp(&right.opportunity.order_key())
        });
        self.collect_close_to_paid(manifest.as_ref(), &mut latency)
            .await?;
        Ok(RunReport {
            trace: DecidedTrace { header, actions },
            latency,
        })
    }

    /// Observe Closed → Paid on planned books and record the gated timer.
    async fn collect_close_to_paid(
        &self,
        manifest: &RunManifest,
        latency: &mut LatencyLog,
    ) -> Result<(), SwarmError> {
        if self.lifecycle_polls == 0 {
            return Ok(());
        }
        let mut markets = vec![
            manifest.scenario.lifecycle_market.clone(),
            manifest.scenario.near_close_market.clone(),
        ];
        markets.sort();
        markets.dedup();
        for market in markets {
            let mut closed_at: Option<Instant> = None;
            let mut paid_at: Option<Instant> = None;
            for _ in 0..self.lifecycle_polls {
                let view = fetch_market_view(
                    self.transport.as_ref(),
                    &market,
                    self.ticker.now_tick(),
                    manifest.scenario.schedule.close_tick,
                )
                .await?;
                if view.state == "closed" && closed_at.is_none() {
                    closed_at = Some(Instant::now());
                }
                if view.state == "paid" {
                    if closed_at.is_some() {
                        paid_at = Some(Instant::now());
                    }
                    break;
                }
                self.sleeper.sleep(Duration::from_millis(1)).await;
            }
            if let (Some(closed), Some(paid)) = (closed_at, paid_at) {
                let raw = u64::try_from(paid.duration_since(closed).as_millis()).unwrap_or(0);
                latency.record(
                    "close-to-paid",
                    200,
                    close_to_paid_ms(0, raw, self.risk_hold_ms),
                );
            }
        }
        Ok(())
    }
}

pub fn replay_trace(
    manifest: &RunManifest,
    trace: &DecidedTrace,
) -> Result<DecidedTrace, SwarmError> {
    if let Err(error) = trace.validate_for_replay(manifest) {
        return Err(SwarmError::Protocol(error.to_string()));
    }
    let worker = ReplayWorker { manifest };
    for action in &trace.actions {
        let replayed = worker.decide(&action.opportunity, &action.market_view)?;
        if replayed != action.decision {
            return Err(SwarmError::Protocol(
                "recorded decision does not reproduce".into(),
            ));
        }
    }
    Ok(trace.clone())
}

struct ReplayWorker<'a> {
    manifest: &'a RunManifest,
}

impl ReplayWorker<'_> {
    fn decide(
        &self,
        opportunity: &PlannedOpportunity,
        view: &crate::domain::action::MarketView,
    ) -> Result<Decision, SwarmError> {
        decision_for(self.manifest, opportunity, view)
    }
}

struct ActorWorker {
    transport: Arc<dyn Transport>,
    ticker: Arc<dyn Ticker>,
    sleeper: Arc<dyn Sleeper>,
    manifest: Arc<RunManifest>,
    retry_delay_ticks: u64,
    max_retries: u8,
}

struct ActorReport {
    actions: Vec<DecidedAction>,
    latency: LatencyLog,
}

impl ActorWorker {
    async fn run(self, opportunities: Vec<PlannedOpportunity>) -> Result<ActorReport, SwarmError> {
        let mut queue = opportunities
            .into_iter()
            .map(|opportunity| (opportunity, 0_u8))
            .collect::<VecDeque<_>>();
        let mut cursor_tick = self.ticker.now_tick();
        let mut actions = Vec::new();
        let mut latency = LatencyLog::default();
        while let Some((opportunity, attempt)) = queue.pop_front() {
            if opportunity.sim_tick > cursor_tick {
                self.sleeper
                    .sleep(Duration::from_secs(opportunity.sim_tick - cursor_tick))
                    .await;
            }
            cursor_tick = opportunity.sim_tick;
            let view = fetch_market_view(
                self.transport.as_ref(),
                &opportunity.target_market,
                opportunity.sim_tick,
                self.manifest.scenario.schedule.close_tick,
            )
            .await?;
            let decision = self.decide(&opportunity, &view)?;
            let mut outcomes = Vec::with_capacity(decision.actions.len());
            for (index, agent_action) in decision.actions.iter().enumerate() {
                let operation = match agent_action.action {
                    Action::Trade { .. } => "trade-confirm",
                    Action::Vote { .. } => "vote",
                    // D35: the money path is its own series, never inside the
                    // gated trade series.
                    Action::Withdraw { .. } => "withdraw-request",
                    Action::Observe | Action::Abstain { .. } => "no-op",
                };
                let single = Decision::new(vec![agent_action.clone()]);
                let started = Instant::now();
                let one = execute_decision(
                    self.transport.as_ref(),
                    &self.manifest.agents,
                    &opportunity.target_market,
                    opportunity
                        .local_seq
                        .saturating_add(u64::try_from(index).unwrap_or(0)),
                    &single,
                )
                .await?;
                let elapsed = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                let status = one.first().map_or(0, |outcome| outcome.status);
                latency.record(operation, status, elapsed);
                outcomes.extend(one);
            }
            let outcome = NormalizedOutcome::Batch {
                outcomes: outcomes.into_iter().map(NormalizedOutcome::from).collect(),
            };
            let decided = DecidedAction::record_without_ws(
                opportunity.clone(),
                &view,
                decision,
                outcome,
                self.retry_delay_ticks,
            );
            if let RetryClass::Transient { retry_tick } = decided.retry {
                if attempt < self.max_retries {
                    let mut retry = opportunity;
                    retry.sim_tick = retry_tick;
                    insert_retry(&mut queue, (retry, attempt + 1));
                }
            }
            actions.push(decided);
        }
        Ok(ActorReport { actions, latency })
    }

    fn decide(
        &self,
        opportunity: &PlannedOpportunity,
        view: &crate::domain::action::MarketView,
    ) -> Result<Decision, SwarmError> {
        decision_for(&self.manifest, opportunity, view)
    }
}

fn decision_for(
    manifest: &RunManifest,
    opportunity: &PlannedOpportunity,
    view: &crate::domain::action::MarketView,
) -> Result<Decision, SwarmError> {
    let mut rng = CountedRng::at(
        manifest.stream_seed(opportunity.actor.id(), "decision"),
        opportunity.rng_before,
    );
    match &opportunity.actor {
        ActorRef::Agent { id } => {
            let mut found = None;
            for agent in &manifest.agents {
                if &agent.id == id {
                    found = Some(agent);
                    break;
                }
            }
            let Some(agent) = found else {
                return Err(SwarmError::Protocol(format!("unknown actor {id}")));
            };
            Ok(Decision::new(vec![AgentAction::new(
                id,
                agent.persona.decide(agent, view, &mut rng),
            )]))
        }
        ActorRef::Ring { id } => {
            let mut found = None;
            for ring in &manifest.rings {
                if &ring.id == id {
                    found = Some(ring);
                    break;
                }
            }
            let Some(ring) = found else {
                return Err(SwarmError::Protocol(format!("unknown actor {id}")));
            };
            match ring.decide(opportunity.local_seq, view, &mut rng) {
                Ok(decision) => Ok(decision),
                Err(error) => Err(SwarmError::Protocol(error.to_string())),
            }
        }
    }
}

fn insert_retry(queue: &mut VecDeque<(PlannedOpportunity, u8)>, retry: (PlannedOpportunity, u8)) {
    let index = queue
        .iter()
        .position(|(queued, _)| retry.0.order_key() < queued.order_key())
        .unwrap_or(queue.len());
    queue.insert(index, retry);
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use async_trait::async_trait;
    use serde_json::{json, Value};

    use super::*;
    use crate::invariants_client::{InvariantApi, InvariantReport};
    use crate::manifest::Profile;
    use crate::trace::{PlannedOpportunity, TraceHeader, TRACE_SCHEMA_VERSION};

    struct FixedTicker(u64);

    impl Ticker for FixedTicker {
        fn now_tick(&self) -> u64 {
            self.0
        }
    }

    #[derive(Default)]
    struct FakeSleeper(Mutex<Vec<Duration>>);

    #[async_trait]
    impl Sleeper for FakeSleeper {
        async fn sleep(&self, duration: Duration) {
            self.0.lock().unwrap().push(duration);
        }
    }

    struct FakeTransport {
        statuses: Mutex<VecDeque<u16>>,
    }

    struct PanickingTransport;

    struct PassingInvariants(Mutex<u8>);

    #[async_trait]
    impl InvariantApi for PassingInvariants {
        async fn invariant_report(&self, _bearer: &str) -> Result<InvariantReport, SwarmError> {
            *self.0.lock().unwrap() += 1;
            Ok(InvariantReport {
                as_of: "2026-01-01T00:00:00Z".into(),
                pass: true,
                identities: Vec::new(),
            })
        }
    }

    #[async_trait]
    impl Transport for PanickingTransport {
        async fn get_json(&self, _path: &str) -> Result<Value, SwarmError> {
            panic!("actor task panic")
        }

        async fn post_json(
            &self,
            _path: &str,
            _body: Value,
            _bearer: Option<&str>,
            _device_id: Option<&str>,
            _forwarded_for: Option<&str>,
        ) -> Result<(u16, Value), SwarmError> {
            Ok((200, json!({})))
        }
    }

    #[async_trait]
    impl Transport for FakeTransport {
        async fn get_json(&self, path: &str) -> Result<Value, SwarmError> {
            Ok(json!({
                "slug": path.trim_start_matches("/markets/"),
                "state":"live", "price_yes_micro":500000, "price_no_micro":500000
            }))
        }

        async fn post_json(
            &self,
            path: &str,
            _body: Value,
            _bearer: Option<&str>,
            _device_id: Option<&str>,
            _forwarded_for: Option<&str>,
        ) -> Result<(u16, Value), SwarmError> {
            if path == "/trades/preview" {
                return Ok((200, json!({"config_version":1})));
            }
            let status = self.statuses.lock().unwrap().pop_front().unwrap_or(200);
            Ok((status, json!({})))
        }
    }

    fn planned(manifest: &RunManifest, actor: ActorRef, ticks: &[u64]) -> PlannedTrace {
        PlannedTrace {
            header: TraceHeader::from_manifest(manifest).unwrap(),
            opportunities: ticks
                .iter()
                .enumerate()
                .map(|(seq, tick)| PlannedOpportunity {
                    schema_version: TRACE_SCHEMA_VERSION,
                    sim_tick: *tick,
                    actor: actor.clone(),
                    local_seq: u64::try_from(seq).unwrap(),
                    target_market: "lifecycle".into(),
                    rng_before: u64::try_from(seq).unwrap(),
                    rng_after: u64::try_from(seq + 1).unwrap(),
                })
                .collect(),
        }
    }

    #[tokio::test]
    async fn one_actor_task_sleeps_in_sim_time_and_records_separate_latency() {
        let manifest = RunManifest::profile(Profile::Smoke, 1);
        let transport = Arc::new(FakeTransport {
            statuses: Mutex::new(VecDeque::from([200, 200])),
        });
        let sleeper = Arc::new(FakeSleeper::default());
        let runner = Runner {
            transport,
            ticker: Arc::new(FixedTicker(0)),
            sleeper: sleeper.clone(),
            retry_delay_ticks: 2,
            max_retries: 0,
            risk_hold_ms: 0,
            lifecycle_polls: 0,
        };
        let report = runner
            .run_planned(
                &manifest,
                planned(&manifest, ActorRef::agent("agent-0000"), &[0, 2, 5]),
            )
            .await
            .unwrap();
        assert_eq!(report.trace.actions.len(), 3);
        assert!(!report.latency.samples.is_empty());
        assert_eq!(
            *sleeper.0.lock().unwrap(),
            [Duration::from_secs(2), Duration::from_secs(3)]
        );
    }

    #[tokio::test]
    async fn http_success_is_not_a_websocket_delivery_sample() {
        let manifest = Arc::new(RunManifest::profile(Profile::Smoke, 1));
        let mut opportunities =
            planned(manifest.as_ref(), ActorRef::ring("ring-wash-pair"), &[1]).opportunities;
        let worker = ActorWorker {
            transport: Arc::new(FakeTransport {
                statuses: Mutex::new(VecDeque::from([200, 200])),
            }),
            ticker: Arc::new(FixedTicker(0)),
            sleeper: Arc::new(FakeSleeper::default()),
            manifest,
            retry_delay_ticks: 1,
            max_retries: 0,
        };

        let report = worker.run(vec![opportunities.remove(0)]).await.unwrap();
        assert_eq!(report.latency.successful("trade-confirm").len(), 2);
        assert!(
            report.latency.successful("ws-delivery").is_empty(),
            "an HTTP response is not evidence that a WebSocket frame arrived"
        );
    }

    #[tokio::test]
    async fn transient_results_reschedule_on_a_deterministic_tick() {
        let mut manifest = RunManifest::profile(Profile::Smoke, 2);
        manifest.agents[0].persona = crate::domain::persona::Persona::SmallDabbler;
        let runner = Runner {
            transport: Arc::new(FakeTransport {
                statuses: Mutex::new(VecDeque::from([503, 200])),
            }),
            ticker: Arc::new(FixedTicker(0)),
            sleeper: Arc::new(FakeSleeper::default()),
            retry_delay_ticks: 4,
            max_retries: 1,
            risk_hold_ms: 0,
            lifecycle_polls: 0,
        };
        let report = runner
            .run_planned(
                &manifest,
                planned(&manifest, ActorRef::agent("agent-0000"), &[1]),
            )
            .await
            .unwrap();
        assert_eq!(report.trace.actions.len(), 2);
        assert_eq!(report.trace.actions[1].opportunity.sim_tick, 5);
    }

    #[tokio::test]
    async fn replay_recomputes_decisions_and_rejects_tampering() {
        let manifest = RunManifest::profile(Profile::Smoke, 3);
        let runner = Runner {
            transport: Arc::new(FakeTransport {
                statuses: Mutex::new(VecDeque::from([200])),
            }),
            ticker: Arc::new(FixedTicker(0)),
            sleeper: Arc::new(FakeSleeper::default()),
            retry_delay_ticks: 1,
            max_retries: 0,
            risk_hold_ms: 0,
            lifecycle_polls: 0,
        };
        let report = runner
            .run_planned(
                &manifest,
                planned(&manifest, ActorRef::agent("agent-0000"), &[1]),
            )
            .await
            .unwrap();
        assert_eq!(
            replay_trace(&manifest, &report.trace).unwrap(),
            report.trace
        );
        let mut tampered = report.trace;
        tampered.actions[0].decision.actions[0].action = crate::domain::action::Action::Observe;
        assert!(replay_trace(&manifest, &tampered).is_err());
        tampered.actions[0].decision = decision_for(
            &manifest,
            &tampered.actions[0].opportunity,
            &tampered.actions[0].market_view,
        )
        .unwrap();
        tampered.header.schema_version += 1;
        assert!(replay_trace(&manifest, &tampered).is_err());
    }

    #[tokio::test]
    async fn top_level_run_builds_plan_and_actor_panics_are_typed() {
        let mut manifest = RunManifest::profile(Profile::Smoke, 4);
        manifest.scenario.schedule.duration_ticks = 1;
        manifest.scenario.schedule.close_tick = 1;
        manifest.scenario.schedule.close_spike_ticks = 0;
        manifest.scenario.schedule.base_rate_ppm = 1_000_000;
        let runner = Runner {
            transport: Arc::new(FakeTransport {
                statuses: Mutex::new(VecDeque::new()),
            }),
            ticker: Arc::new(FixedTicker(0)),
            sleeper: Arc::new(FakeSleeper::default()),
            retry_delay_ticks: 1,
            max_retries: 0,
            risk_hold_ms: 0,
            lifecycle_polls: 0,
        };
        assert!(!runner
            .run(&manifest)
            .await
            .unwrap()
            .trace
            .actions
            .is_empty());
        let invariant_api = PassingInvariants(Mutex::new(0));
        let invariants = InvariantsClient {
            api: &invariant_api,
            bearer: "ops",
        };
        assert!(!runner
            .run_with_invariants(&manifest, &invariants)
            .await
            .unwrap()
            .trace
            .actions
            .is_empty());
        assert_eq!(*invariant_api.0.lock().unwrap(), 2);

        let panic_runner = Runner {
            transport: Arc::new(PanickingTransport),
            ticker: Arc::new(FixedTicker(0)),
            sleeper: Arc::new(FakeSleeper::default()),
            retry_delay_ticks: 1,
            max_retries: 0,
            risk_hold_ms: 0,
            lifecycle_polls: 0,
        };
        assert_eq!(
            PanickingTransport
                .post_json("/x", json!({}), None, None, None)
                .await
                .unwrap()
                .0,
            200
        );
        assert!(matches!(
            panic_runner
                .run_planned(
                    &manifest,
                    planned(&manifest, ActorRef::agent("agent-0000"), &[0])
                )
                .await,
            Err(SwarmError::Process(_))
        ));

        manifest.scenario.schedule.close_tick = 2;
        assert!(matches!(
            runner.run(&manifest).await,
            Err(SwarmError::Protocol(_))
        ));

        let valid_manifest = RunManifest::profile(Profile::Smoke, 4);
        let mut bad_plan = planned(&valid_manifest, ActorRef::agent("agent-0000"), &[0]);
        bad_plan.header.schema_version += 1;
        assert!(matches!(
            runner.run_planned(&valid_manifest, bad_plan).await,
            Err(SwarmError::Protocol(_))
        ));
    }

    #[test]
    fn decision_lookup_covers_ring_and_unknown_actors() {
        let manifest = RunManifest::profile(Profile::Smoke, 5);
        let view = crate::domain::action::MarketView {
            market_ref: "fat-pot".into(),
            state: "live".into(),
            price_yes_micro: 500_000,
            price_no_micro: 500_000,
            sim_tick: 1,
            closes_tick: 2,
        };
        let ring = PlannedOpportunity {
            schema_version: TRACE_SCHEMA_VERSION,
            sim_tick: 1,
            actor: ActorRef::ring("ring-aged-sybil"),
            local_seq: 0,
            target_market: "fat-pot".into(),
            rng_before: 0,
            rng_after: 1,
        };
        assert_eq!(
            decision_for(&manifest, &ring, &view).unwrap().actions.len(),
            31
        );
        let mut unknown = ring.clone();
        unknown.actor = ActorRef::ring("missing");
        assert!(decision_for(&manifest, &unknown, &view).is_err());
        unknown.actor = ActorRef::agent("missing");
        assert!(decision_for(&manifest, &unknown, &view).is_err());

        let mut broken = manifest;
        broken.rings[0].members.clear();
        assert!(decision_for(&broken, &ring, &view).is_err());
    }

    #[test]
    fn retry_insertion_preserves_total_order() {
        let one = PlannedOpportunity {
            sim_tick: 1,
            ..planned(
                &RunManifest::profile(Profile::Smoke, 1),
                ActorRef::agent("a"),
                &[1],
            )
            .opportunities
            .remove(0)
        };
        let three = PlannedOpportunity {
            sim_tick: 3,
            ..one.clone()
        };
        let two = PlannedOpportunity {
            sim_tick: 2,
            ..one.clone()
        };
        let mut queue = VecDeque::from([(one, 0), (three, 0)]);
        insert_retry(&mut queue, (two, 1));
        assert_eq!(
            queue.iter().map(|row| row.0.sim_tick).collect::<Vec<_>>(),
            [1, 2, 3]
        );
    }

    struct LifecycleTransport {
        states: Mutex<VecDeque<&'static str>>,
    }

    #[async_trait]
    impl Transport for LifecycleTransport {
        async fn get_json(&self, path: &str) -> Result<Value, SwarmError> {
            let state = self.states.lock().unwrap().pop_front().unwrap_or("paid");
            Ok(json!({
                "slug": path.trim_start_matches("/markets/"),
                "state": state,
                "price_yes_micro": 500000,
                "price_no_micro": 500000
            }))
        }

        async fn post_json(
            &self,
            _path: &str,
            _body: Value,
            _bearer: Option<&str>,
            _device_id: Option<&str>,
            _forwarded_for: Option<&str>,
        ) -> Result<(u16, Value), SwarmError> {
            Ok((200, json!({})))
        }
    }

    #[tokio::test]
    async fn live_close_to_paid_subtracts_the_named_risk_hold() {
        let manifest = RunManifest::profile(Profile::Smoke, 1);
        let runner = Runner {
            transport: Arc::new(LifecycleTransport {
                states: Mutex::new(VecDeque::from(["closed", "paid", "paid", "paid"])),
            }),
            ticker: Arc::new(FixedTicker(0)),
            sleeper: Arc::new(FakeSleeper::default()),
            retry_delay_ticks: 1,
            max_retries: 0,
            risk_hold_ms: 25,
            lifecycle_polls: 4,
        };
        let report = runner
            .run_planned(
                &manifest,
                planned(&manifest, ActorRef::agent("agent-0000"), &[]),
            )
            .await
            .unwrap();
        let closes: Vec<_> = report
            .latency
            .samples
            .iter()
            .filter(|sample| sample.operation == "close-to-paid")
            .collect();
        assert!(!closes.is_empty());
        assert!(closes.iter().all(|sample| sample.status == 200));
        assert!(closes
            .iter()
            .all(|sample| sample.latency_ms < 25 || sample.latency_ms == 0));
    }

    #[test]
    fn websocket_frames_are_the_only_ws_latency_evidence() {
        let base = OffsetDateTime::UNIX_EPOCH;
        let mut tracker = WsDeliveryTracker::default();
        let mut latency = LatencyLog::default();

        tracker.observe(
            &json!({"type":"snapshot"}),
            base + time::Duration::milliseconds(15),
            &mut latency,
        );
        tracker.observe(
            &json!({
                "type":"trade",
                "outbox_seq":7,
                "created_at":"1970-01-01T00:00:00Z"
            }),
            base + time::Duration::milliseconds(20),
            &mut latency,
        );
        tracker.observe(
            &json!({"type":"price","outbox_seq":7}),
            base + time::Duration::milliseconds(21),
            &mut latency,
        );
        tracker.observe(
            &json!({
                "type":"trade",
                "outbox_seq":8,
                "created_at":"1970-01-01T00:00:00.010Z"
            }),
            base + time::Duration::milliseconds(30),
            &mut latency,
        );

        assert_eq!(latency.successful("ws-delivery"), vec![20, 20]);
    }

    /// `ServerFrame` uses `time`'s `serde-human-readable` representation on
    /// the real adapter wire. An RFC3339-only positive control is fabricated
    /// evidence: the tracker can pass its unit test while dropping every live
    /// frame at the timestamp parser.
    #[test]
    fn websocket_tracker_accepts_the_real_server_timestamp_shape() {
        let delivered_at = OffsetDateTime::parse(
            "2026-08-13T09:43:53.628904Z",
            &time::format_description::well_known::Rfc3339,
        )
        .unwrap();
        let mut tracker = WsDeliveryTracker::default();
        let mut latency = LatencyLog::default();
        tracker.observe(
            &json!({
                "type": "trade",
                "outbox_seq": 9,
                "created_at": "2026-08-13 09:43:53.608904 +00:00:00"
            }),
            delivered_at,
            &mut latency,
        );
        assert_eq!(
            latency.successful("ws-delivery"),
            vec![20],
            "the tracker must consume the exact timestamp shape serialized by ServerFrame"
        );
    }

    // D35: the release money path is timed under its own operation and is
    // never folded into the gated trade or ws series.
    #[tokio::test]
    async fn money_path_actions_are_timed_outside_the_gated_series() {
        let mut manifest = RunManifest::profile(Profile::Smoke, 3);
        let money_agent = manifest.agents[0].id.clone();
        manifest.agents[0].persona = crate::domain::persona::Persona::MoneyPath;
        manifest.agents[0].money_capable = true;
        manifest.agents[0].trade_capable = false;
        let runner = Runner {
            transport: Arc::new(LifecycleTransport {
                states: Mutex::new(VecDeque::from(["live"])),
            }),
            ticker: Arc::new(FixedTicker(0)),
            sleeper: Arc::new(FakeSleeper::default()),
            retry_delay_ticks: 1,
            max_retries: 0,
            risk_hold_ms: 0,
            lifecycle_polls: 0,
        };
        let report = runner
            .run_planned(
                &manifest,
                planned(&manifest, ActorRef::agent(&money_agent), &[1, 2, 3, 4]),
            )
            .await
            .unwrap();
        assert!(report.latency.attempts("withdraw-request") > 0);
        assert_eq!(report.latency.attempts("trade-confirm"), 0);
        assert!(report.latency.successful("ws-delivery").is_empty());
        assert!(report.trace.actions.iter().any(|action| action
            .decision
            .actions
            .iter()
            .any(|agent_action| { matches!(agent_action.action, Action::Withdraw { .. }) })));
    }

    #[tokio::test]
    async fn paid_without_an_observed_closed_state_does_not_invent_a_timer() {
        let manifest = RunManifest::profile(Profile::Smoke, 2);
        let runner = Runner {
            transport: Arc::new(LifecycleTransport {
                states: Mutex::new(VecDeque::from(["paid", "paid", "paid", "paid"])),
            }),
            ticker: Arc::new(FixedTicker(0)),
            sleeper: Arc::new(FakeSleeper::default()),
            retry_delay_ticks: 1,
            max_retries: 0,
            risk_hold_ms: 0,
            lifecycle_polls: 2,
        };
        let report = runner
            .run_planned(
                &manifest,
                planned(&manifest, ActorRef::agent("agent-0000"), &[]),
            )
            .await
            .unwrap();
        assert!(!report
            .latency
            .samples
            .iter()
            .any(|sample| sample.operation == "close-to-paid"));
        assert_eq!(
            LifecycleTransport {
                states: Mutex::new(VecDeque::new()),
            }
            .post_json("/x", json!({}), None, None, None)
            .await
            .unwrap()
            .0,
            200
        );
    }
}
