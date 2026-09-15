//! Thin command-line shell for the simswarm library.

use std::env;
use std::error::Error;
use std::fs;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use simswarm::engine::runner::{replay_trace, Runner};
use simswarm::invariants_client::InvariantsClient;
use simswarm::manifest::{Profile, RunManifest};
use simswarm::ports::{Sleeper, Ticker};
use simswarm::trace::{evaluate_slo, DecidedTrace, PlannedTrace, SeriesContract};
use simswarm::transport::HttpTransport;

struct WallTicker(Instant);

impl Ticker for WallTicker {
    fn now_tick(&self) -> u64 {
        self.0.elapsed().as_secs()
    }
}

struct TokioSleeper;

#[async_trait]
impl Sleeper for TokioSleeper {
    async fn sleep(&self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let args = env::args().skip(1).collect::<Vec<_>>();
    let profile = match value_after(&args, "--profile").as_deref() {
        Some("full") | Some("2k") => Profile::Full,
        Some("smoke") | None => Profile::Smoke,
        Some(other) => return Err(format!("unknown profile: {other}").into()),
    };
    let release_2k = value_after(&args, "--profile").as_deref() == Some("2k");
    let seed = value_after(&args, "--seed")
        .map(|raw| raw.parse::<u64>())
        .transpose()?
        .unwrap_or(1);
    let mut manifest = if let Some(path) = value_after(&args, "--manifest") {
        RunManifest::from_slice(&fs::read(path)?)?
    } else {
        RunManifest::profile(profile, seed)
    };
    if release_2k {
        manifest.scenario.slo_enforce = true;
    }
    if write_manifest_if_requested(&args, &manifest)? {
        return Ok(());
    }

    if args.iter().any(|arg| arg == "--dry-run") {
        let bytes = PlannedTrace::from_manifest(&manifest)?.canonical_json_lines()?;
        write_artifact(value_after(&args, "--output"), &bytes)?;
        return Ok(());
    }

    if let Some(path) = value_after(&args, "--replay-log") {
        let recorded = DecidedTrace::from_slice(&fs::read(path)?)?;
        let replayed = replay_trace(&manifest, &recorded)?;
        write_artifact(value_after(&args, "--output"), &replayed.canonical_bytes()?)?;
        return Ok(());
    }

    let base_url = value_after(&args, "--base-url").ok_or("live mode requires --base-url")?;
    let demo_token = value_after(&args, "--demo-token")
        .ok_or("live mode requires --demo-token for the public write API")?;
    let transport = Arc::new(HttpTransport::new(&base_url)?.with_demo_token(&demo_token));
    let runner = Runner {
        transport: transport.clone(),
        ticker: Arc::new(WallTicker(Instant::now())),
        sleeper: Arc::new(TokioSleeper),
        retry_delay_ticks: 2,
        max_retries: 2,
        risk_hold_ms: 0,
        lifecycle_polls: 32,
        http_send_marks: Arc::new(std::sync::Mutex::new(Vec::new())),
    };
    let report = if let Some(admin_token) = value_after(&args, "--admin-token") {
        runner
            .run_with_invariants(
                &manifest,
                &InvariantsClient {
                    api: transport.as_ref(),
                    bearer: &admin_token,
                },
            )
            .await?
    } else {
        runner.run(&manifest).await?
    };
    write_artifact(
        value_after(&args, "--output"),
        &report.trace.canonical_bytes()?,
    )?;
    if let Some(path) = value_after(&args, "--latency-log") {
        fs::write(path, report.latency.canonical_bytes()?)?;
    }
    let enforce = args.iter().any(|arg| arg == "--slo-gate") || manifest.scenario.slo_enforce;
    let contract = SeriesContract::from_manifest(&manifest);
    let slo = report.latency.slo_report_for(enforce, &contract);
    if let Some(path) = value_after(&args, "--slo-report") {
        fs::write(path, serde_json::to_vec(&slo)?)?;
    }
    if enforce {
        evaluate_slo(&slo)?;
    }
    Ok(())
}

fn value_after(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|arg| arg == flag)
        .and_then(|index| args.get(index + 1))
        .cloned()
}

fn write_manifest_if_requested(
    args: &[String],
    manifest: &RunManifest,
) -> Result<bool, Box<dyn Error>> {
    if let Some(path) = value_after(args, "--manifest-output") {
        fs::write(path, manifest.canonical_bytes()?)?;
    }
    Ok(args.iter().any(|arg| arg == "--manifest-only"))
}

fn write_artifact(path: Option<String>, bytes: &[u8]) -> Result<(), std::io::Error> {
    if let Some(path) = path {
        fs::write(path, bytes)
    } else {
        use std::io::Write;
        std::io::stdout().write_all(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_output_is_canonical_and_can_short_circuit_live_mode() {
        let path = std::env::temp_dir().join(format!(
            "opinions-simswarm-manifest-{}.json",
            std::process::id()
        ));
        let args = vec![
            "--manifest-output".to_string(),
            path.display().to_string(),
            "--manifest-only".to_string(),
        ];
        let manifest = RunManifest::profile(Profile::Smoke, 7);
        assert!(write_manifest_if_requested(&args, &manifest).unwrap());
        let decoded = RunManifest::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(decoded, manifest);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn slo_gate_flag_is_recognized_independently_of_profile() {
        let args = vec![
            "--slo-gate".to_string(),
            "--profile".to_string(),
            "2k".to_string(),
        ];
        assert!(args.iter().any(|arg| arg == "--slo-gate"));
        assert_eq!(value_after(&args, "--profile").as_deref(), Some("2k"));
    }
}
