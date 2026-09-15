//! Chaos controller contracts for the Phase 6 staging swarm (plan D29).

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::ports::{Exec, Sleeper, SwarmError, Transport};

/// The named fault set required by the Phase 6 exit gate. Keeping this list
/// typed prevents a smoke run from silently skipping a scenario because of a
/// spelling mismatch in shell orchestration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NamedScenario {
    ClockJump,
    TerminatePostgres,
    DiskFullRender,
    WebhookStorm,
    ReconcilerKillHeal,
    SimultaneousClose,
    DuplicateWebhook,
    RelayDelay,
    WsDrop,
    ResolutionCrash,
}

impl NamedScenario {
    pub const ALL: [Self; 10] = [
        Self::ClockJump,
        Self::TerminatePostgres,
        Self::DiskFullRender,
        Self::WebhookStorm,
        Self::ReconcilerKillHeal,
        Self::SimultaneousClose,
        Self::DuplicateWebhook,
        Self::RelayDelay,
        Self::WsDrop,
        Self::ResolutionCrash,
    ];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ClockJump => "clock_jump",
            Self::TerminatePostgres => "pg_terminate_backend",
            Self::DiskFullRender => "disk_full_render",
            Self::WebhookStorm => "webhook_storm",
            Self::ReconcilerKillHeal => "reconciler_kill_heal",
            Self::SimultaneousClose => "simultaneous_close",
            Self::DuplicateWebhook => "duplicate_webhook",
            Self::RelayDelay => "relay_delay",
            Self::WsDrop => "ws_drop",
            Self::ResolutionCrash => "resolution_crash",
        }
    }
}

/// A process invocation whose arguments are kept as separate OS arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessSpec {
    pub program: String,
    pub args: Vec<String>,
}

impl ProcessSpec {
    #[must_use]
    pub fn new<I, S>(program: impl Into<String>, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
        }
    }
}

/// Real Tokio-backed process boundary for local/staging chaos runs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokioExec;

fn require_child_pid(cmd: &str, pid: Option<u32>) -> Result<u32, SwarmError> {
    pid.ok_or_else(|| SwarmError::Process(format!("spawn {cmd}: child has no pid")))
}

fn require_kill_status(
    pid: u32,
    status: std::io::Result<std::process::ExitStatus>,
) -> Result<(), SwarmError> {
    let status = status.map_err(|error| SwarmError::Process(format!("SIGKILL {pid}: {error}")))?;
    if status.success() {
        Ok(())
    } else {
        Err(SwarmError::Process(format!(
            "SIGKILL {pid}: command exited {status}"
        )))
    }
}

#[async_trait::async_trait]
impl Exec for TokioExec {
    async fn spawn(&self, cmd: &str, args: &[String]) -> Result<u32, SwarmError> {
        let child = tokio::process::Command::new(cmd)
            .args(args)
            .spawn()
            .map_err(|error| SwarmError::Process(format!("spawn {cmd}: {error}")))?;
        require_child_pid(cmd, child.id())
    }

    async fn kill(&self, pid: u32) -> Result<(), SwarmError> {
        let pid_text = pid.to_string();
        let status = tokio::process::Command::new("kill")
            .args(["-KILL", pid_text.as_str()])
            .status()
            .await;
        require_kill_status(pid, status)
    }
}

/// Process lifecycle seam used for server, reconciler, Postgres, and relay
/// kill/restart scenarios. The real OS boundary lives behind [`Exec`].
pub struct ProcessController<'a, E: Exec> {
    exec: &'a E,
}

impl<'a, E: Exec> ProcessController<'a, E> {
    #[must_use]
    pub const fn new(exec: &'a E) -> Self {
        Self { exec }
    }

    /// # Errors
    /// Returns the typed process error produced by the injected executor.
    pub async fn start(&self, spec: &ProcessSpec) -> Result<u32, SwarmError> {
        self.exec.spawn(&spec.program, &spec.args).await
    }

    /// # Errors
    /// Returns the typed process error produced by the injected executor.
    pub async fn stop(&self, pid: u32) -> Result<(), SwarmError> {
        self.exec.kill(pid).await
    }

    /// Stops `pid` before starting the replacement. A failed stop never
    /// launches a second process.
    ///
    /// # Errors
    /// Returns the first typed process error.
    pub async fn restart(&self, pid: u32, spec: &ProcessSpec) -> Result<u32, SwarmError> {
        self.stop(pid).await?;
        self.start(spec).await
    }
}

/// Deterministic storm dimensions. The exit profile uses 100 rounds of ten
/// identical deliveries at one-second intervals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WebhookStorm {
    pub rounds: usize,
    pub copies_per_round: usize,
    pub interval: Duration,
}

impl WebhookStorm {
    /// # Errors
    /// A zero dimension would make a named storm a silent no-op.
    pub fn new(
        rounds: usize,
        copies_per_round: usize,
        interval: Duration,
    ) -> Result<Self, SwarmError> {
        if rounds == 0 || copies_per_round == 0 {
            return Err(SwarmError::Protocol(
                "webhook storm dimensions must be non-zero".into(),
            ));
        }
        Ok(Self {
            rounds,
            copies_per_round,
            interval,
        })
    }
}

/// Duplicate and storm injection over the same public webhook boundary used
/// by staging. Payloads and bearer material are intentionally reused exactly
/// so the server's idempotency behavior, not a mutated request, is tested.
pub struct WebhookController<'a, T: Transport, S: Sleeper> {
    transport: &'a T,
    sleeper: &'a S,
}

impl<'a, T: Transport, S: Sleeper> WebhookController<'a, T, S> {
    #[must_use]
    pub const fn new(transport: &'a T, sleeper: &'a S) -> Self {
        Self { transport, sleeper }
    }

    async fn post(
        &self,
        path: &str,
        body: serde_json::Value,
        bearer: Option<&str>,
    ) -> Result<(u16, serde_json::Value), SwarmError> {
        self.transport
            .post_json(path, body, bearer, None, None)
            .await
    }

    /// # Errors
    /// Returns the first transport failure; both accepted responses otherwise
    /// remain available for exact comparison by the caller.
    pub async fn duplicate(
        &self,
        path: &str,
        body: serde_json::Value,
        bearer: Option<&str>,
    ) -> Result<Vec<(u16, serde_json::Value)>, SwarmError> {
        let first = self.post(path, body.clone(), bearer).await?;
        let second = self.post(path, body, bearer).await?;
        Ok(vec![first, second])
    }

    /// # Errors
    /// Returns the first transport failure and stops issuing new deliveries.
    pub async fn storm(
        &self,
        path: &str,
        body: serde_json::Value,
        bearer: Option<&str>,
        storm: WebhookStorm,
    ) -> Result<Vec<(u16, serde_json::Value)>, SwarmError> {
        let mut outcomes = Vec::with_capacity(storm.rounds * storm.copies_per_round);
        for round in 0..storm.rounds {
            for _ in 0..storm.copies_per_round {
                outcomes.push(self.post(path, body.clone(), bearer).await?);
            }
            if round + 1 < storm.rounds {
                self.sleeper.sleep(storm.interval).await;
            }
        }
        Ok(outcomes)
    }
}

/// Explicit WS severing operation. Keeping it distinct from generic process
/// control makes scenario traces state what was intentionally disrupted.
pub struct WsSeverer<'a, E: Exec> {
    exec: &'a E,
}

impl<'a, E: Exec> WsSeverer<'a, E> {
    #[must_use]
    pub const fn new(exec: &'a E) -> Self {
        Self { exec }
    }

    /// # Errors
    /// Returns the typed process error from the injected boundary.
    pub async fn sever(&self, pid: u32) -> Result<(), SwarmError> {
        self.exec.kill(pid).await
    }
}

/// Observable barrier settings for the deterministic mid-resolution crash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrashBarrierPlan {
    pub readiness_file: PathBuf,
    pub max_polls: usize,
    pub poll_interval: Duration,
}

impl CrashBarrierPlan {
    /// # Errors
    /// At least one observation is required; zero could report success
    /// without ever seeing the application-side barrier.
    pub fn new(
        readiness_file: impl Into<PathBuf>,
        max_polls: usize,
        poll_interval: Duration,
    ) -> Result<Self, SwarmError> {
        if max_polls == 0 {
            return Err(SwarmError::Protocol(
                "crash readiness max_polls must be non-zero".into(),
            ));
        }
        Ok(Self {
            readiness_file: readiness_file.into(),
            max_polls,
            poll_interval,
        })
    }
}

/// Arms a fresh readiness path, starts the process, waits until the injected
/// application crash point publishes readiness, then kills that exact PID.
/// A timeout deliberately leaves the process alive for diagnosis.
pub struct CrashBarrierController<'a, E: Exec, S: Sleeper> {
    exec: &'a E,
    sleeper: &'a S,
}

impl<'a, E: Exec, S: Sleeper> CrashBarrierController<'a, E, S> {
    #[must_use]
    pub const fn new(exec: &'a E, sleeper: &'a S) -> Self {
        Self { exec, sleeper }
    }

    fn remove_stale_readiness(path: &Path) -> Result<(), SwarmError> {
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(SwarmError::Process(format!(
                "remove stale crash readiness {}: {error}",
                path.display()
            ))),
        }
    }

    /// # Errors
    /// Fails on stale-path cleanup, spawn, readiness timeout, or kill.
    pub async fn arm_wait_and_kill(
        &self,
        process: &ProcessSpec,
        plan: &CrashBarrierPlan,
    ) -> Result<u32, SwarmError> {
        Self::remove_stale_readiness(&plan.readiness_file)?;
        let pid = self.exec.spawn(&process.program, &process.args).await?;
        for poll in 0..plan.max_polls {
            if plan.readiness_file.is_file() {
                self.exec.kill(pid).await?;
                return Ok(pid);
            }
            if poll + 1 < plan.max_polls {
                self.sleeper.sleep(plan.poll_interval).await;
            }
        }
        Err(SwarmError::Protocol(format!(
            "resolution crash readiness was not published at {}",
            plan.readiness_file.display()
        )))
    }
}

#[cfg(test)]
mod wave_tests {
    #![allow(clippy::unwrap_used)]

    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::time::Duration;

    use async_trait::async_trait;

    use crate::ports::{Exec, Sleeper, SwarmError, Transport};

    use super::{
        require_child_pid, require_kill_status, CrashBarrierController, CrashBarrierPlan,
        NamedScenario, ProcessController, ProcessSpec, TokioExec, WebhookController, WebhookStorm,
        WsSeverer,
    };

    #[derive(Default)]
    struct FakeExec {
        calls: Mutex<Vec<String>>,
        fail_spawn: bool,
        fail_kill: bool,
    }

    #[async_trait]
    impl Exec for FakeExec {
        async fn spawn(&self, cmd: &str, args: &[String]) -> Result<u32, SwarmError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("spawn:{cmd}:{}", args.join(",")));
            if self.fail_spawn {
                Err(SwarmError::Process("spawn failed".into()))
            } else {
                Ok(4242)
            }
        }

        async fn kill(&self, pid: u32) -> Result<(), SwarmError> {
            self.calls.lock().unwrap().push(format!("kill:{pid}"));
            if self.fail_kill {
                Err(SwarmError::Process("kill failed".into()))
            } else {
                Ok(())
            }
        }
    }

    #[derive(Default)]
    struct FakeSleeper {
        sleeps: Mutex<Vec<Duration>>,
        touch_on_first_sleep: Option<PathBuf>,
    }

    #[async_trait]
    impl Sleeper for FakeSleeper {
        async fn sleep(&self, duration: Duration) {
            let mut sleeps = self.sleeps.lock().unwrap();
            sleeps.push(duration);
            if sleeps.len() == 1 {
                if let Some(path) = &self.touch_on_first_sleep {
                    std::fs::write(path, b"ready\n").unwrap();
                }
            }
        }
    }

    #[derive(Default)]
    struct FakeTransport {
        posts: Mutex<Vec<(String, serde_json::Value, Option<String>)>>,
        fail_at: Option<usize>,
    }

    #[async_trait]
    impl Transport for FakeTransport {
        async fn get_json(&self, _path: &str) -> Result<serde_json::Value, SwarmError> {
            unreachable!("webhook tests only post")
        }

        async fn post_json(
            &self,
            path: &str,
            body: serde_json::Value,
            bearer: Option<&str>,
            _device_id: Option<&str>,
            _forwarded_for: Option<&str>,
        ) -> Result<(u16, serde_json::Value), SwarmError> {
            let mut posts = self.posts.lock().unwrap();
            posts.push((path.into(), body, bearer.map(str::to_owned)));
            if self.fail_at == Some(posts.len()) {
                Err(SwarmError::Transport("injected webhook failure".into()))
            } else {
                Ok((202, serde_json::json!({"accepted": true})))
            }
        }
    }

    fn temp_file(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "opinions-{label}-{}",
            uuid::Uuid::new_v4().simple()
        ))
    }

    #[test]
    fn named_scenario_catalog_is_complete_and_stable() {
        assert_eq!(NamedScenario::ALL.len(), 10);
        assert_eq!(NamedScenario::ClockJump.as_str(), "clock_jump");
        assert_eq!(
            NamedScenario::TerminatePostgres.as_str(),
            "pg_terminate_backend"
        );
        assert_eq!(NamedScenario::DiskFullRender.as_str(), "disk_full_render");
        assert_eq!(NamedScenario::WebhookStorm.as_str(), "webhook_storm");
        assert_eq!(
            NamedScenario::ReconcilerKillHeal.as_str(),
            "reconciler_kill_heal"
        );
        assert_eq!(
            NamedScenario::SimultaneousClose.as_str(),
            "simultaneous_close"
        );
        assert_eq!(
            NamedScenario::DuplicateWebhook.as_str(),
            "duplicate_webhook"
        );
        assert_eq!(NamedScenario::RelayDelay.as_str(), "relay_delay");
        assert_eq!(NamedScenario::WsDrop.as_str(), "ws_drop");
        assert_eq!(NamedScenario::ResolutionCrash.as_str(), "resolution_crash");
    }

    #[tokio::test]
    async fn process_controller_starts_stops_and_restarts_exactly() {
        let exec = FakeExec::default();
        let controller = ProcessController::new(&exec);
        let spec = ProcessSpec::new("opinions", ["--continuous", "--serve"]);
        assert_eq!(controller.start(&spec).await.unwrap(), 4242);
        assert_eq!(controller.restart(12, &spec).await.unwrap(), 4242);
        controller.stop(13).await.unwrap();
        assert_eq!(
            *exec.calls.lock().unwrap(),
            [
                "spawn:opinions:--continuous,--serve",
                "kill:12",
                "spawn:opinions:--continuous,--serve",
                "kill:13",
            ]
        );
    }

    #[tokio::test]
    async fn process_controller_propagates_spawn_and_kill_failures() {
        let spawn_error = FakeExec {
            fail_spawn: true,
            ..FakeExec::default()
        };
        let spec = ProcessSpec::new("opinions", std::iter::empty::<&str>());
        assert!(ProcessController::new(&spawn_error)
            .start(&spec)
            .await
            .is_err());

        let kill_error = FakeExec {
            fail_kill: true,
            ..FakeExec::default()
        };
        assert!(ProcessController::new(&kill_error)
            .restart(9, &spec)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn tokio_exec_spawns_and_sigkills_real_processes() {
        let exec = TokioExec;
        let pid = exec.spawn("sleep", &["30".to_string()]).await.unwrap();
        exec.kill(pid).await.unwrap();
        assert!(exec
            .spawn("opinions-command-that-does-not-exist", &[])
            .await
            .is_err());
        assert!(exec.kill(u32::MAX).await.is_err());
    }

    #[test]
    fn tokio_process_boundary_maps_missing_pid_and_spawned_kill_errors() {
        assert!(matches!(
            require_child_pid("opinions", None),
            Err(SwarmError::Process(message)) if message.contains("child has no pid")
        ));
        assert!(matches!(
            require_kill_status(9, Err(std::io::Error::other("kill unavailable"))),
            Err(SwarmError::Process(message)) if message.contains("kill unavailable")
        ));
    }

    #[tokio::test]
    async fn duplicate_and_storm_preserve_payload_and_idempotency_headers() {
        let transport = FakeTransport::default();
        let sleeper = FakeSleeper::default();
        let controller = WebhookController::new(&transport, &sleeper);
        let body = serde_json::json!({"id":"evt_1"});
        let duplicate = controller
            .duplicate("/webhooks/rail", body.clone(), Some("secret"))
            .await
            .unwrap();
        assert_eq!(duplicate.len(), 2);

        let outcomes = controller
            .storm(
                "/webhooks/rail",
                body.clone(),
                Some("secret"),
                WebhookStorm::new(2, 3, Duration::from_secs(1)).unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(outcomes.len(), 6);
        assert_eq!(
            sleeper.sleeps.lock().unwrap().as_slice(),
            [Duration::from_secs(1)]
        );
        assert_eq!(transport.posts.lock().unwrap().len(), 8);
        assert!(transport
            .posts
            .lock()
            .unwrap()
            .iter()
            .all(|(path, posted, bearer)| {
                path == "/webhooks/rail" && posted == &body && bearer.as_deref() == Some("secret")
            }));
    }

    #[tokio::test]
    async fn webhook_failures_and_invalid_storms_are_typed() {
        assert!(WebhookStorm::new(0, 1, Duration::ZERO).is_err());
        assert!(WebhookStorm::new(1, 0, Duration::ZERO).is_err());
        let transport = FakeTransport {
            fail_at: Some(2),
            ..FakeTransport::default()
        };
        let sleeper = FakeSleeper::default();
        let error = WebhookController::new(&transport, &sleeper)
            .duplicate("/webhooks/rail", serde_json::json!({}), None)
            .await
            .unwrap_err();
        assert_eq!(
            error,
            SwarmError::Transport("injected webhook failure".into())
        );
    }

    #[tokio::test]
    #[should_panic(expected = "webhook tests only post")]
    async fn fake_webhook_transport_rejects_gets() {
        let _ = FakeTransport::default().get_json("/not-used").await;
    }

    #[tokio::test]
    async fn ws_severer_delegates_to_the_process_boundary() {
        let exec = FakeExec::default();
        WsSeverer::new(&exec).sever(77).await.unwrap();
        assert_eq!(*exec.calls.lock().unwrap(), ["kill:77"]);
    }

    #[tokio::test]
    async fn crash_barrier_removes_stale_ready_waits_then_kills() {
        let ready = temp_file("crash-ready");
        std::fs::write(&ready, b"stale").unwrap();
        let exec = FakeExec::default();
        let sleeper = FakeSleeper {
            touch_on_first_sleep: Some(ready.clone()),
            ..FakeSleeper::default()
        };
        let controller = CrashBarrierController::new(&exec, &sleeper);
        let plan = CrashBarrierPlan::new(&ready, 3, Duration::from_millis(1)).unwrap();
        let pid = controller
            .arm_wait_and_kill(
                &ProcessSpec::new("opinions", std::iter::empty::<&str>()),
                &plan,
            )
            .await
            .unwrap();
        assert_eq!(pid, 4242);
        assert_eq!(
            *exec.calls.lock().unwrap(),
            ["spawn:opinions:", "kill:4242"]
        );
        assert_eq!(std::fs::read(&ready).unwrap(), b"ready\n");
        std::fs::remove_file(ready).unwrap();
    }

    #[tokio::test]
    async fn crash_barrier_rejects_bad_plans_and_times_out_without_killing() {
        let ready = temp_file("crash-timeout");
        assert!(CrashBarrierPlan::new(&ready, 0, Duration::ZERO).is_err());
        let exec = FakeExec::default();
        let sleeper = FakeSleeper::default();
        let error = CrashBarrierController::new(&exec, &sleeper)
            .arm_wait_and_kill(
                &ProcessSpec::new("opinions", std::iter::empty::<&str>()),
                &CrashBarrierPlan::new(&ready, 2, Duration::ZERO).unwrap(),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, SwarmError::Protocol(message) if message.contains("readiness")));
        assert_eq!(*exec.calls.lock().unwrap(), ["spawn:opinions:"]);
    }

    #[test]
    fn crash_barrier_reports_stale_readiness_cleanup_errors() {
        let ready = temp_file("crash-ready-directory");
        std::fs::create_dir(&ready).unwrap();
        let error = CrashBarrierController::<FakeExec, FakeSleeper>::remove_stale_readiness(&ready)
            .unwrap_err();
        assert!(matches!(
            error,
            SwarmError::Process(message) if message.contains("remove stale crash readiness")
        ));
        std::fs::remove_dir(ready).unwrap();
    }
}
