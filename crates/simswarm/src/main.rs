//! Thin command-line shell for the simswarm library.

use std::env;
use std::error::Error;
use std::fs;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use simswarm::engine::runner::{replay_trace, Runner, WsDeliveryTracker};
use simswarm::invariants_client::InvariantsClient;
use simswarm::manifest::{Profile, RunManifest};
use simswarm::ports::{Sleeper, SwarmError, Ticker, Transport};
use simswarm::trace::{evaluate_slo, DecidedTrace, LatencyLog, PlannedTrace, SeriesContract};
use simswarm::transport::{HttpTransport, WsTransport};
use time::OffsetDateTime;

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
    };
    let socket = subscribed_socket(&base_url, transport.as_ref(), &manifest).await?;
    let (stop_ws, stop_rx) = tokio::sync::oneshot::channel();
    let ws_task = tokio::spawn(collect_ws_latency(socket, stop_rx));
    let report_result = if let Some(admin_token) = value_after(&args, "--admin-token") {
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
    tokio::time::sleep(Duration::from_millis(100)).await;
    let _ = stop_ws.send(());
    let ws_latency = ws_task
        .await
        .map_err(|error| SwarmError::Process(error.to_string()))??;
    let mut report = report_result;
    report.latency.samples.extend(ws_latency.samples);
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

fn websocket_url(base_url: &str) -> Result<String, SwarmError> {
    let mut url =
        reqwest::Url::parse(base_url).map_err(|error| SwarmError::Transport(error.to_string()))?;
    let scheme = match url.scheme() {
        "http" => "ws",
        "https" => "wss",
        _ => {
            return Err(SwarmError::Transport(
                "live base URL must use http or https".into(),
            ))
        }
    };
    url.set_scheme(scheme)
        .map_err(|()| SwarmError::Transport("invalid WebSocket scheme".into()))?;
    url.set_path("/ws");
    url.set_query(None);
    url.set_fragment(None);
    Ok(url.to_string())
}

async fn subscribed_socket(
    base_url: &str,
    transport: &HttpTransport,
    manifest: &RunManifest,
) -> Result<WsTransport, SwarmError> {
    let market_refs = [
        &manifest.scenario.lifecycle_market,
        &manifest.scenario.fat_pot_market,
        &manifest.scenario.near_close_market,
        &manifest.scenario.suppress_market,
        &manifest.scenario.pad_market,
    ];
    let mut pending = std::collections::BTreeSet::new();
    let mut socket = WsTransport::connect(&websocket_url(base_url)?).await?;
    for market_ref in market_refs {
        let value = transport
            .get_json(&format!("/markets/{market_ref}"))
            .await?;
        let market_id = value
            .get("id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                SwarmError::Protocol(format!("market {market_ref} response omitted id"))
            })?;
        if pending.insert(market_id.to_owned()) {
            socket
                .send_json(&serde_json::json!({
                    "op": "subscribe",
                    "market_id": market_id,
                }))
                .await?;
        }
    }
    tokio::time::timeout(Duration::from_secs(10), async {
        while !pending.is_empty() {
            let frame = socket.next_json().await?.ok_or_else(|| {
                SwarmError::Protocol("WebSocket closed during subscription".into())
            })?;
            if frame.get("type").and_then(serde_json::Value::as_str) == Some("snapshot") {
                if let Some(market_id) = frame.get("market_id").and_then(serde_json::Value::as_str)
                {
                    pending.remove(market_id);
                }
            }
        }
        Ok::<(), SwarmError>(())
    })
    .await
    .map_err(|_| SwarmError::Transport("WebSocket subscription timed out".into()))??;
    Ok(socket)
}

async fn collect_ws_latency(
    mut socket: WsTransport,
    mut stop: tokio::sync::oneshot::Receiver<()>,
) -> Result<LatencyLog, SwarmError> {
    let mut tracker = WsDeliveryTracker::default();
    let mut latency = LatencyLog::default();
    loop {
        tokio::select! {
            _ = &mut stop => return Ok(latency),
            frame = socket.next_json() => {
                let frame = frame?.ok_or_else(|| {
                    SwarmError::Protocol("WebSocket closed during the live run".into())
                })?;
                tracker.observe(&frame, OffsetDateTime::now_utc(), &mut latency);
            }
        }
    }
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

    #[test]
    fn websocket_endpoint_is_derived_from_the_public_base_url() {
        assert_eq!(
            websocket_url("http://127.0.0.1:8080/api?ignored=1").unwrap(),
            "ws://127.0.0.1:8080/ws"
        );
        assert_eq!(
            websocket_url("https://example.test").unwrap(),
            "wss://example.test/ws"
        );
        assert!(websocket_url("ftp://example.test").is_err());
    }
}
