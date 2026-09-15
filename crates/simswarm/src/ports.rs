//! Injected role traits (ISP): everything effectful is a port so the lib
//! reaches 100% line coverage with fake/local-stub contracts and the bin
//! stays thin. Implementations: `transport::HttpTransport` (W3), fakes in
//! each module's tests, chaos process control (W4).

use std::time::Duration;

use async_trait::async_trait;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SwarmError {
    #[error("transport: {0}")]
    Transport(String),
    #[error("process: {0}")]
    Process(String),
    #[error("protocol: {0}")]
    Protocol(String),
    #[error("not implemented until the wave lands: {0}")]
    Unavailable(&'static str),
}

/// HTTP + WS access to the staging deployment. The sole real implementation
/// lives in `transport.rs`.
#[async_trait]
pub trait Transport: Send + Sync {
    async fn get_json(&self, path: &str) -> Result<serde_json::Value, SwarmError>;
    async fn post_json(
        &self,
        path: &str,
        body: serde_json::Value,
        bearer: Option<&str>,
        device_id: Option<&str>,
        forwarded_for: Option<&str>,
    ) -> Result<(u16, serde_json::Value), SwarmError>;
}

/// Deterministic sim-time source: ticks, not wall clock, feed decisions.
pub trait Ticker: Send + Sync {
    fn now_tick(&self) -> u64;
}

/// Sleeping/backoff seam so retries are deterministic in tests.
#[async_trait]
pub trait Sleeper: Send + Sync {
    async fn sleep(&self, duration: Duration);
}

/// OS process control (chaos controller only — W4).
#[async_trait]
pub trait Exec: Send + Sync {
    async fn spawn(&self, cmd: &str, args: &[String]) -> Result<u32, SwarmError>;
    async fn kill(&self, pid: u32) -> Result<(), SwarmError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn swarm_error_is_typed_and_displayable() {
        let e = SwarmError::Unavailable("engine");
        assert_eq!(e, SwarmError::Unavailable("engine"));
        assert!(e.to_string().contains("engine"));
    }
}
