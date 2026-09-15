//! Phase 7 adapter impls of the 7.0a `Alerter`, `ChainBalance`, and
//! `Telemetry` ports. Production composition stays in frozen `main.rs`.

use std::sync::Mutex;

use application::error::StoreError;
use application::ops::alerts::RecordingAlerter;
use application::ports::{Alerter, ChainBalance, Telemetry};
use async_trait::async_trait;

pub use application::ops::alerts::RecordingAlerter as AdapterAlerter;

/// Pages through the shared recording alerter (tests + staging).
pub struct SharedAlerter {
    pub inner: RecordingAlerter,
}

impl SharedAlerter {
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: RecordingAlerter::new(),
        }
    }
}

impl Default for SharedAlerter {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Alerter for SharedAlerter {
    async fn page(&self, severity: &str, key: &str, body: &str) -> Result<(), StoreError> {
        self.inner.page(severity, key, body).await
    }
}

/// Chain balance read pinned to one finalized cut.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinnedChainBalance {
    pub cut_slot: i64,
    pub balance_micro: i64,
}

#[async_trait]
impl ChainBalance for PinnedChainBalance {
    async fn wallet_balance_micro(&self) -> Result<i64, StoreError> {
        Ok(self.balance_micro)
    }
}

/// No-op production telemetry default.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopTelemetry;

#[async_trait]
impl Telemetry for NoopTelemetry {
    fn counter(&self, _name: &str, _value: u64) {}
}

/// Recording fake used by tests and the swarm.
#[derive(Debug, Default)]
pub struct RecordingTelemetry {
    pub counters: Mutex<Vec<(String, u64)>>,
}

impl RecordingTelemetry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn snapshot(&self) -> Vec<(String, u64)> {
        self.counters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

#[async_trait]
impl Telemetry for RecordingTelemetry {
    fn counter(&self, name: &str, value: u64) {
        self.counters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((name.to_owned(), value));
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use application::ports::Alerter;

    #[tokio::test]
    async fn adapter_ports_record_and_pin_the_cut() {
        let alerter = SharedAlerter::new();
        alerter.page("crit", "k", "b").await.unwrap();
        assert_eq!(alerter.inner.len(), 1);
        assert!(SharedAlerter::default().inner.is_empty());

        let chain = PinnedChainBalance {
            cut_slot: 42,
            balance_micro: 99,
        };
        assert_eq!(chain.wallet_balance_micro().await.unwrap(), 99);
        assert_eq!(chain.cut_slot, 42);

        let telemetry = RecordingTelemetry::new();
        telemetry.counter("recon.residual", 1);
        assert_eq!(telemetry.snapshot(), vec![("recon.residual".into(), 1)]);
        NoopTelemetry.counter("ignored", 2);
        let _ = NoopTelemetry;
    }
}
