//! D24 read path: one watch snapshot per process, maintained by a single
//! reconciler. Outbox events are WAKE-UPS ONLY — the reload always reads the
//! authoritative `config_generation` and replaces the snapshot wholesale
//! when it exceeds the applied one; a periodic heal tick recovers any missed
//! wake-up (Pg contract b).

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::RwLock;
use serde_json::Value;

use crate::error::StoreError;
use crate::model::ConfigEntry;
use crate::ports::ConfigReads;

/// One immutable snapshot generation.
#[derive(Debug, Clone, Default)]
pub struct ConfigSnapshot {
    pub generation: i64,
    pub entries: BTreeMap<String, Value>,
}

/// The process-wide watch handle: lock-free-ish reads for previews and the
/// HTTP surface; the reconciler is the only writer.
#[derive(Clone, Default)]
pub struct ConfigWatch {
    inner: Arc<RwLock<ConfigSnapshot>>,
}

impl ConfigWatch {
    /// Starts at generation 0 so the very first reconcile installs the
    /// migration seed (generation 1).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn generation(&self) -> i64 {
        self.inner.read().generation
    }

    #[must_use]
    pub fn value(&self, key: &str) -> Option<Value> {
        self.inner.read().entries.get(key).cloned()
    }

    /// Replaces the snapshot wholesale. The reconciler is the only
    /// production writer; public for composition and fixture use.
    pub fn install(&self, snapshot: ConfigSnapshot) {
        *self.inner.write() = snapshot;
    }
}

#[async_trait]
impl ConfigReads for ConfigWatch {
    async fn current_generation(&self) -> Result<i64, StoreError> {
        Ok(self.generation())
    }

    async fn snapshot_value(&self, key: &str) -> Result<Option<Value>, StoreError> {
        Ok(self.value(key))
    }
}

/// Adapter-side IO for the reconciler. `drain_wakeups` consumes the
/// `config_reconciler` outbox cursor (0008 seed) and reports whether any
/// config event arrived; `load_snapshot` reads the authoritative generation
/// and the complete entry set consistently.
#[async_trait]
pub trait ConfigWatchIo: Send + Sync {
    async fn drain_wakeups(&self) -> Result<bool, StoreError>;
    async fn load_snapshot(&self) -> Result<(i64, Vec<ConfigEntry>), StoreError>;
}

pub struct Reconciler<IO> {
    pub io: IO,
    pub watch: ConfigWatch,
}

impl<IO: ConfigWatchIo> Reconciler<IO> {
    /// One reconciler turn. Wake-ups only gate the CHEAP path: a heal tick
    /// reloads from the authoritative generation regardless, so a missed
    /// wake-up can delay a reload by at most one heal interval — never lose
    /// it. Returns whether a new snapshot was installed.
    ///
    /// # Errors
    /// Backend failures (the loop retries next tick; the cursor advance may
    /// then have consumed the wake, which is exactly what the heal covers).
    pub async fn tick(&self, heal: bool) -> Result<bool, StoreError> {
        let woken = self.io.drain_wakeups().await?;
        if !woken && !heal {
            return Ok(false);
        }
        self.reconcile().await
    }

    /// Unconditional authoritative check: reload wholesale when the
    /// committed generation exceeds the applied one.
    ///
    /// # Errors
    /// Backend failures.
    pub async fn reconcile(&self) -> Result<bool, StoreError> {
        let (generation, entries) = self.io.load_snapshot().await?;
        if generation <= self.watch.generation() {
            return Ok(false);
        }
        let snapshot = ConfigSnapshot {
            generation,
            entries: entries.into_iter().map(|e| (e.key, e.value)).collect(),
        };
        self.watch.install(snapshot);
        Ok(true)
    }

    /// The spawn target: polls wake-ups at `wake_poll` and heals every
    /// `heal_every` polls. Errors are logged and retried — the reconciler
    /// must outlive transient backend failures.
    pub async fn run(self, wake_poll: std::time::Duration, heal_every: u32) {
        let mut polls: u32 = 0;
        loop {
            tokio::time::sleep(wake_poll).await;
            polls = polls.wrapping_add(1);
            let heal = heal_every != 0 && polls.is_multiple_of(heal_every);
            if let Err(error) = self.tick(heal).await {
                eprintln!("config reconciler tick failed (retrying): {error}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, Ordering};

    use serde_json::json;

    use super::*;

    #[derive(Default)]
    struct FakeIo {
        wake: AtomicBool,
        fail_load: AtomicBool,
        generation: AtomicI64,
        loads: AtomicU32,
    }

    #[async_trait]
    impl ConfigWatchIo for &FakeIo {
        async fn drain_wakeups(&self) -> Result<bool, StoreError> {
            Ok(self.wake.swap(false, Ordering::SeqCst))
        }

        async fn load_snapshot(&self) -> Result<(i64, Vec<ConfigEntry>), StoreError> {
            self.loads.fetch_add(1, Ordering::SeqCst);
            if self.fail_load.swap(false, Ordering::SeqCst) {
                return Err(StoreError::Backend("transient config load".into()));
            }
            let generation = self.generation.load(Ordering::SeqCst);
            Ok((
                generation,
                vec![ConfigEntry {
                    key: "trade_fee_bps".to_string(),
                    value: json!(100 + generation),
                }],
            ))
        }
    }

    #[tokio::test]
    async fn a_wake_up_installs_the_authoritative_snapshot() {
        let io = FakeIo::default();
        io.generation.store(1, Ordering::SeqCst);
        io.wake.store(true, Ordering::SeqCst);
        let reconciler = Reconciler {
            io: &io,
            watch: ConfigWatch::new(),
        };
        assert!(reconciler.tick(false).await.unwrap());
        assert_eq!(reconciler.watch.generation(), 1);
        assert_eq!(reconciler.watch.value("trade_fee_bps"), Some(json!(101)));
        assert_eq!(reconciler.watch.current_generation().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn no_wake_and_no_heal_reads_nothing() {
        let io = FakeIo::default();
        io.generation.store(5, Ordering::SeqCst);
        let reconciler = Reconciler {
            io: &io,
            watch: ConfigWatch::new(),
        };
        assert!(!reconciler.tick(false).await.unwrap());
        assert_eq!(
            io.loads.load(Ordering::SeqCst),
            0,
            "events are the wake gate"
        );
        assert_eq!(reconciler.watch.generation(), 0);
    }

    #[tokio::test]
    async fn a_missed_wake_up_heals_from_the_authoritative_generation() {
        // The generation moved but the wake-up was lost (Pg contract b): the
        // heal tick still converges on the committed state.
        let io = FakeIo::default();
        io.generation.store(3, Ordering::SeqCst);
        let reconciler = Reconciler {
            io: &io,
            watch: ConfigWatch::new(),
        };
        assert!(
            !reconciler.tick(false).await.unwrap(),
            "missed wake: nothing"
        );
        assert!(reconciler.tick(true).await.unwrap(), "heal tick reloads");
        assert_eq!(reconciler.watch.generation(), 3);
    }

    #[tokio::test]
    async fn an_unchanged_generation_never_reinstalls() {
        let io = FakeIo::default();
        io.generation.store(2, Ordering::SeqCst);
        io.wake.store(true, Ordering::SeqCst);
        let reconciler = Reconciler {
            io: &io,
            watch: ConfigWatch::new(),
        };
        assert!(reconciler.tick(false).await.unwrap());
        io.wake.store(true, Ordering::SeqCst);
        assert!(
            !reconciler.tick(false).await.unwrap(),
            "same generation: wake consumed, snapshot kept"
        );
        assert_eq!(io.loads.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn the_watch_serves_the_preview_pause_overlay() {
        let watch = ConfigWatch::new();
        assert_eq!(watch.snapshot_value("voting_paused:x").await.unwrap(), None);
        watch.install(ConfigSnapshot {
            generation: 4,
            entries: [("voting_paused:x".to_string(), json!(true))]
                .into_iter()
                .collect(),
        });
        assert_eq!(
            watch.snapshot_value("voting_paused:x").await.unwrap(),
            Some(json!(true))
        );
    }

    #[tokio::test]
    async fn run_loop_retries_a_transient_tick_failure_and_keeps_healing() {
        let io = Box::leak(Box::new(FakeIo::default()));
        io.fail_load.store(true, Ordering::SeqCst);
        io.generation.store(2, Ordering::SeqCst);
        let watch = ConfigWatch::new();
        let observed = watch.clone();
        let task = tokio::spawn(
            Reconciler { io: &*io, watch }.run(std::time::Duration::from_millis(1), 1),
        );
        tokio::time::timeout(std::time::Duration::from_millis(250), async {
            while observed.generation() != 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        task.abort();
        assert!(io.loads.load(Ordering::SeqCst) >= 2);
    }
}
