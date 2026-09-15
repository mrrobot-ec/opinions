//! D35 Alerter incident lifecycle: durable outbox, detector+subject+episode
//! keys, dedup inside an open incident, ack/resolve, re-page after recovery.

use std::collections::BTreeMap;

use async_trait::async_trait;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::StoreError;
use crate::ports::Alerter;

/// Pages the alerter must emit (D35).
pub const DETECTOR_INVARIANT: &str = "invariant_breach";
pub const DETECTOR_RECON: &str = "reconciliation_residual";
pub const DETECTOR_AML: &str = "aml_flag";
pub const DETECTOR_STUCK_SEND: &str = "stuck_send_unknown";
pub const DETECTOR_STUCK_SCREEN: &str = "stuck_screening";
pub const DETECTOR_RESERVE: &str = "reserve_coverage";

/// Stable episode for an invariant identity: one live episode per identity,
/// so an ongoing breach dedups and a recurrence after recovery re-pages.
pub const INVARIANT_EPISODE: &str = "active";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IncidentStatus {
    Open,
    Acked,
    Resolved,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncidentKey {
    pub detector: String,
    pub subject: String,
    pub episode: String,
}

impl IncidentKey {
    #[must_use]
    pub fn new(
        detector: impl Into<String>,
        subject: impl Into<String>,
        episode: impl Into<String>,
    ) -> Self {
        Self {
            detector: detector.into(),
            subject: subject.into(),
            episode: episode.into(),
        }
    }

    #[must_use]
    pub fn encoded(&self) -> String {
        format!("{}:{}:{}", self.detector, self.subject, self.episode)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Incident {
    pub id: Uuid,
    pub key: IncidentKey,
    pub severity: String,
    pub body: String,
    pub status: IncidentStatus,
    pub delivery_attempts: u32,
    pub last_paged_at: Option<OffsetDateTime>,
    pub acked_at: Option<OffsetDateTime>,
    pub resolved_at: Option<OffsetDateTime>,
}

/// Durable incident store. Adapters persist; tests use [`MemoryAlertStore`].
#[async_trait]
pub trait AlertStore: Send + Sync {
    async fn find_open(&self, key: &IncidentKey) -> Result<Option<Incident>, StoreError>;
    async fn insert(&self, incident: Incident) -> Result<(), StoreError>;
    /// Atomic open-or-get. Inserts and returns `None`, or — when an incident
    /// for the same key is already `Open`/`Acked` — writes nothing and returns
    /// the incident that won. `raise` cannot dedup with a separate read
    /// followed by an insert: two concurrent callers both read nothing and
    /// both insert, opening two incidents and paging twice.
    async fn insert_if_absent(&self, incident: Incident) -> Result<Option<Incident>, StoreError>;
    async fn save(&self, incident: &Incident) -> Result<(), StoreError>;
    async fn pending_delivery(&self) -> Result<Vec<Incident>, StoreError>;
}

#[derive(Default)]
pub struct MemoryAlertStore {
    inner: std::sync::Mutex<BTreeMap<String, Incident>>,
}

impl MemoryAlertStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl AlertStore for MemoryAlertStore {
    async fn find_open(&self, key: &IncidentKey) -> Result<Option<Incident>, StoreError> {
        let guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Ok(guard.get(&key.encoded()).and_then(|incident| {
            matches!(
                incident.status,
                IncidentStatus::Open | IncidentStatus::Acked
            )
            .then(|| incident.clone())
        }))
    }

    async fn insert(&self, incident: Incident) -> Result<(), StoreError> {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.insert(incident.key.encoded(), incident);
        Ok(())
    }

    /// The lock is the whole point: the read and the write are one step, so a
    /// second caller with the same key can never also see "absent". The Pg
    /// adapter gets the same guarantee from a partial unique index over
    /// `incident_key` restricted to the open statuses.
    async fn insert_if_absent(&self, incident: Incident) -> Result<Option<Incident>, StoreError> {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(existing) = guard.get(&incident.key.encoded()).filter(|existing| {
            matches!(
                existing.status,
                IncidentStatus::Open | IncidentStatus::Acked
            )
        }) {
            return Ok(Some(existing.clone()));
        }
        guard.insert(incident.key.encoded(), incident);
        Ok(None)
    }

    async fn save(&self, incident: &Incident) -> Result<(), StoreError> {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.insert(incident.key.encoded(), incident.clone());
        Ok(())
    }

    async fn pending_delivery(&self) -> Result<Vec<Incident>, StoreError> {
        let guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Ok(guard
            .values()
            .filter(|incident| {
                // Acked is included on purpose: an operator acknowledging an
                // incident is not the pager delivering it. Only Resolved
                // retires an undelivered incident from the queue.
                matches!(
                    incident.status,
                    IncidentStatus::Open | IncidentStatus::Acked
                ) && incident.last_paged_at.is_none()
            })
            .cloned()
            .collect())
    }
}

/// Incident manager: dedup, at-least-once page, ack/resolve, re-page after
/// a resolved incident recurs.
pub struct IncidentManager<S, A> {
    pub store: S,
    pub alerter: A,
}

impl<S: AlertStore, A: Alerter> IncidentManager<S, A> {
    /// Opens or dedups. An already-open incident is not re-paged.
    ///
    /// # Errors
    /// Store or alerter failures.
    pub async fn raise(
        &self,
        key: IncidentKey,
        severity: &str,
        body: &str,
        now: OffsetDateTime,
    ) -> Result<Incident, StoreError> {
        // Persisted PENDING first: zero attempts, never paged. A crash or a
        // pager failure between here and the page below therefore leaves the
        // incident in `pending_delivery`, which is the entire reason the
        // durable outbox exists. Stamping `last_paged_at` before the page
        // succeeded retired the incident from the retry queue and lost it.
        let pending = Incident {
            id: Uuid::new_v4(),
            key: key.clone(),
            severity: severity.to_owned(),
            body: body.to_owned(),
            status: IncidentStatus::Open,
            delivery_attempts: 0,
            last_paged_at: None,
            acked_at: None,
            resolved_at: None,
        };
        // Open-or-get in one step. A separate `find_open` + `insert` lets two
        // concurrent raises both observe "absent" and open two incidents.
        if let Some(existing) = self.store.insert_if_absent(pending.clone()).await? {
            return Ok(existing);
        }
        self.alerter.page(severity, &key.encoded(), body).await?;
        let mut delivered = pending;
        delivered.delivery_attempts = 1;
        delivered.last_paged_at = Some(now);
        self.store.save(&delivered).await?;
        Ok(delivered)
    }

    /// Records an operator ack. Does not close the incident.
    ///
    /// # Errors
    /// Missing incident or store failure.
    pub async fn ack(
        &self,
        key: &IncidentKey,
        now: OffsetDateTime,
    ) -> Result<Incident, StoreError> {
        let Some(mut incident) = self.store.find_open(key).await? else {
            return Err(StoreError::NotFound("alert incident"));
        };
        incident.status = IncidentStatus::Acked;
        incident.acked_at = Some(now);
        self.store.save(&incident).await?;
        Ok(incident)
    }

    /// Closes the incident. A later [`Self::raise`] with the same key is a
    /// new episode page (caller supplies a new episode, or reuse after
    /// resolve — reuse of the same key after resolve re-pages).
    ///
    /// # Errors
    /// Missing incident or store failure.
    pub async fn resolve(
        &self,
        key: &IncidentKey,
        now: OffsetDateTime,
    ) -> Result<Incident, StoreError> {
        let Some(mut incident) = self.store.find_open(key).await? else {
            return Err(StoreError::NotFound("alert incident"));
        };
        incident.status = IncidentStatus::Resolved;
        incident.resolved_at = Some(now);
        self.store.save(&incident).await?;
        Ok(incident)
    }

    /// Drive one invariant sweep's verdict into the incident lifecycle, so
    /// `main` stays wiring-only and nothing has to reimplement the policy.
    ///
    /// Each identity is its own subject under the `invariant_breach` detector
    /// with the stable episode `active`: a failing identity raises (and dedups
    /// inside the incident it already opened), a passing identity resolves the
    /// incident it had open, and a passing identity with nothing open is a
    /// no-op. A recurrence after recovery therefore opens and pages a new
    /// episode, which is D35's re-page-on-recurrence rule.
    ///
    /// # Errors
    /// The first store or alerter failure, after every identity in the report
    /// has been attempted — one broken identity must not hide the rest.
    pub async fn sync_invariant_report(
        &self,
        report: &crate::integrity::invariant_sweep::InvariantReport,
        now: OffsetDateTime,
    ) -> Result<(), StoreError> {
        let mut first_error: Option<StoreError> = None;
        for identity in &report.identities {
            let key = IncidentKey::new(DETECTOR_INVARIANT, identity.identity, INVARIANT_EPISODE);
            let outcome = if identity.pass {
                self.resolve_if_open(&key, now).await.map(|_| ())
            } else {
                let body = identity
                    .detail
                    .clone()
                    .unwrap_or_else(|| format!("{} failed", identity.identity));
                self.raise(key, "crit", &body, now).await.map(|_| ())
            };
            if let Err(error) = outcome {
                first_error.get_or_insert(error);
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// Resolve only when something is open. A passing identity that never
    /// breached must not be a `NotFound` error every sweep.
    ///
    /// # Errors
    /// Store failures.
    pub async fn resolve_if_open(
        &self,
        key: &IncidentKey,
        now: OffsetDateTime,
    ) -> Result<Option<Incident>, StoreError> {
        if self.store.find_open(key).await?.is_none() {
            return Ok(None);
        }
        self.resolve(key, now).await.map(Some)
    }

    /// At-least-once: redeliver any open incident that never observed a page.
    ///
    /// # Errors
    /// Store or alerter failures.
    pub async fn deliver_pending(&self, now: OffsetDateTime) -> Result<u32, StoreError> {
        let pending = self.store.pending_delivery().await?;
        let mut sent = 0_u32;
        // One incident whose page fails must not abort the sweep: the queue is
        // ordered oldest-first, so returning early let a single poison
        // incident starve every incident behind it, on every tick, forever.
        // The failure is kept and returned once the batch has been attempted —
        // continuing is not the same as swallowing, and a broken pager must
        // never read as a quiet, healthy sweep.
        let mut first_error: Option<StoreError> = None;
        for mut incident in pending {
            if let Err(error) = self
                .alerter
                .page(&incident.severity, &incident.key.encoded(), &incident.body)
                .await
            {
                first_error.get_or_insert(error);
                continue;
            }
            incident.delivery_attempts = incident.delivery_attempts.saturating_add(1);
            // The real time this page landed. `pending_delivery` only returns
            // rows whose `last_paged_at` is NULL, so the previous
            // `.or(UNIX_EPOCH)` was not a fallback — it was the only branch,
            // and it durably recorded every redelivery as having paged in 1970.
            incident.last_paged_at = Some(now);
            if let Err(error) = self.store.save(&incident).await {
                // The page landed but the bookkeeping did not, so the incident
                // stays pending and a later tick re-pages it: at-least-once,
                // as specified. The store failure is still reported.
                first_error.get_or_insert(error);
                continue;
            }
            sent = sent.saturating_add(1);
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(sent),
        }
    }
}

/// Recording [`Alerter`] used by tests and the adapter recording impl.
#[derive(Default)]
pub struct RecordingAlerter {
    pub pages: std::sync::Mutex<Vec<(String, String, String)>>,
}

impl RecordingAlerter {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.pages
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[async_trait]
impl Alerter for RecordingAlerter {
    async fn page(&self, severity: &str, key: &str, body: &str) -> Result<(), StoreError> {
        self.pages
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((severity.to_owned(), key.to_owned(), body.to_owned()));
        Ok(())
    }
}

#[async_trait]
impl Alerter for &RecordingAlerter {
    async fn page(&self, severity: &str, key: &str, body: &str) -> Result<(), StoreError> {
        (*self).page(severity, key, body).await
    }
}

#[async_trait]
impl AlertStore for &MemoryAlertStore {
    async fn find_open(&self, key: &IncidentKey) -> Result<Option<Incident>, StoreError> {
        (*self).find_open(key).await
    }
    async fn insert(&self, incident: Incident) -> Result<(), StoreError> {
        (*self).insert(incident).await
    }
    async fn insert_if_absent(&self, incident: Incident) -> Result<Option<Incident>, StoreError> {
        (*self).insert_if_absent(incident).await
    }
    async fn save(&self, incident: &Incident) -> Result<(), StoreError> {
        (*self).save(incident).await
    }
    async fn pending_delivery(&self) -> Result<Vec<Incident>, StoreError> {
        (*self).pending_delivery().await
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
    }

    /// Pages every key except the ones named, which fail. No `Alerter` in the
    /// tree could fail before this, so the at-least-once path was untestable.
    #[derive(Default)]
    struct PartlyFailingAlerter {
        fail_keys: Vec<String>,
        pages: std::sync::Mutex<Vec<String>>,
    }

    impl PartlyFailingAlerter {
        fn failing_on(keys: &[&str]) -> Self {
            Self {
                fail_keys: keys.iter().map(|key| (*key).to_string()).collect(),
                pages: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn paged(&self) -> Vec<String> {
            self.pages
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }
    }

    #[async_trait]
    impl Alerter for &PartlyFailingAlerter {
        async fn page(&self, _severity: &str, key: &str, _body: &str) -> Result<(), StoreError> {
            if self.fail_keys.iter().any(|failing| failing == key) {
                return Err(StoreError::Unavailable("pager down"));
            }
            self.pages
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(key.to_owned());
            Ok(())
        }
    }

    /// Models the window between `find_open` and the insert: the reader never
    /// sees the concurrent writer's row. Only an atomic open-or-get in the
    /// store can dedup here.
    struct RacyStore(MemoryAlertStore);

    #[async_trait]
    impl AlertStore for RacyStore {
        async fn find_open(&self, _key: &IncidentKey) -> Result<Option<Incident>, StoreError> {
            Ok(None)
        }
        async fn insert(&self, incident: Incident) -> Result<(), StoreError> {
            self.0.insert(incident).await
        }
        async fn insert_if_absent(
            &self,
            incident: Incident,
        ) -> Result<Option<Incident>, StoreError> {
            self.0.insert_if_absent(incident).await
        }
        async fn save(&self, incident: &Incident) -> Result<(), StoreError> {
            if incident.body == "fail-save" {
                return Err(StoreError::Unavailable("alert save failed"));
            }
            self.0.save(incident).await
        }
        async fn pending_delivery(&self) -> Result<Vec<Incident>, StoreError> {
            self.0.pending_delivery().await
        }
    }

    #[tokio::test]
    async fn a_failed_first_page_leaves_the_incident_pending_for_redelivery() {
        let store = MemoryAlertStore::new();
        let key = IncidentKey::new(DETECTOR_RECON, "cut", "residual:1");
        let alerter = PartlyFailingAlerter::failing_on(&[&key.encoded()]);
        let manager = IncidentManager {
            store: &store,
            alerter: &alerter,
        };
        assert!(
            manager
                .raise(key.clone(), "crit", "1us", now())
                .await
                .is_err(),
            "a pager failure must surface, not be swallowed"
        );
        let pending = store.pending_delivery().await.unwrap();
        assert_eq!(
            pending.len(),
            1,
            "an incident whose first page failed must stay in the at-least-once queue"
        );
        assert_eq!(pending[0].delivery_attempts, 0);
        assert_eq!(pending[0].last_paged_at, None);
    }

    #[tokio::test]
    async fn a_post_page_save_failure_surfaces_and_keeps_the_incident_pending() {
        let key = IncidentKey::new(DETECTOR_RECON, "save", "residual:1");
        let incident = Incident {
            id: Uuid::from_u128(13),
            key: key.clone(),
            severity: "crit".into(),
            body: "fail-save".into(),
            status: IncidentStatus::Open,
            delivery_attempts: 0,
            last_paged_at: None,
            acked_at: None,
            resolved_at: None,
        };
        let store = RacyStore(MemoryAlertStore::new());
        store.insert(incident.clone()).await.unwrap();
        assert_eq!(store.find_open(&key).await.unwrap(), None);

        let mirror = MemoryAlertStore::new();
        <&MemoryAlertStore as AlertStore>::insert(&&mirror, incident)
            .await
            .unwrap();
        assert_eq!(mirror.pending_delivery().await.unwrap().len(), 1);

        let alerter = RecordingAlerter::new();
        let manager = IncidentManager {
            store,
            alerter: &alerter,
        };
        assert_eq!(
            manager.deliver_pending(now()).await,
            Err(StoreError::Unavailable("alert save failed"))
        );
        assert_eq!(alerter.len(), 1, "the page landed before the save failed");
        let pending = manager.store.pending_delivery().await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].delivery_attempts, 0);
        assert_eq!(pending[0].last_paged_at, None);
    }

    #[tokio::test]
    async fn a_successful_first_page_records_one_attempt_at_the_supplied_time() {
        let store = MemoryAlertStore::new();
        let alerter = RecordingAlerter::new();
        let manager = IncidentManager {
            store: &store,
            alerter: &alerter,
        };
        let key = IncidentKey::new(DETECTOR_INVARIANT, "suite", "ep-1");
        let raised = manager
            .raise(key.clone(), "crit", "drift", now())
            .await
            .unwrap();
        assert_eq!(raised.delivery_attempts, 1);
        assert_eq!(raised.last_paged_at, Some(now()));
        assert_eq!(alerter.len(), 1);
        assert!(
            store.pending_delivery().await.unwrap().is_empty(),
            "a paged incident is not pending"
        );
        let stored = store.find_open(&key).await.unwrap().unwrap();
        assert_eq!(stored.delivery_attempts, 1);
        assert_eq!(stored.last_paged_at, Some(now()));
    }

    #[tokio::test]
    async fn redelivery_stamps_the_supplied_time_and_never_the_unix_epoch() {
        let store = MemoryAlertStore::new();
        let key = IncidentKey::new(DETECTOR_RECON, "cut", "residual:1");
        store
            .insert(Incident {
                id: Uuid::from_u128(9),
                key: key.clone(),
                severity: "crit".into(),
                body: "1us".into(),
                status: IncidentStatus::Open,
                delivery_attempts: 0,
                last_paged_at: None,
                acked_at: None,
                resolved_at: None,
            })
            .await
            .unwrap();
        let alerter = RecordingAlerter::new();
        let manager = IncidentManager {
            store: &store,
            alerter: &alerter,
        };
        let later = now() + time::Duration::minutes(5);
        assert_eq!(manager.deliver_pending(later).await.unwrap(), 1);
        let stored = store.find_open(&key).await.unwrap().unwrap();
        assert_eq!(
            stored.last_paged_at,
            Some(later),
            "the pump must stamp the real time it paged, not 1970"
        );
        assert_ne!(stored.last_paged_at, Some(OffsetDateTime::UNIX_EPOCH));
        assert_eq!(stored.delivery_attempts, 1);
        assert_eq!(manager.deliver_pending(later).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn a_poison_incident_does_not_starve_the_incidents_behind_it() {
        let store = MemoryAlertStore::new();
        let poison = IncidentKey::new(DETECTOR_AML, "user-a", "ep-1");
        let healthy = IncidentKey::new(DETECTOR_AML, "user-b", "ep-1");
        for (index, key) in [&poison, &healthy].into_iter().enumerate() {
            store
                .insert(Incident {
                    id: Uuid::from_u128(index as u128 + 1),
                    key: key.clone(),
                    severity: "crit".into(),
                    body: "flag".into(),
                    status: IncidentStatus::Open,
                    delivery_attempts: 0,
                    last_paged_at: None,
                    acked_at: None,
                    resolved_at: None,
                })
                .await
                .unwrap();
        }
        let alerter = PartlyFailingAlerter::failing_on(&[&poison.encoded()]);
        let manager = IncidentManager {
            store: &store,
            alerter: &alerter,
        };
        // Non-starvation must not become silence: the whole batch is attempted
        // AND the pager failure is still returned, so a broken pager cannot
        // look like a quiet, healthy sweep.
        assert!(
            matches!(
                manager.deliver_pending(now()).await,
                Err(StoreError::Unavailable("pager down"))
            ),
            "a pager failure must surface after the batch, never be swallowed"
        );
        assert_eq!(
            alerter.paged(),
            vec![healthy.encoded()],
            "the healthy incident is delivered even though the first one failed"
        );
        let pending = store.pending_delivery().await.unwrap();
        assert_eq!(pending.len(), 1, "only the poison incident stays pending");
        assert_eq!(pending[0].key, poison);
        let stored = store.find_open(&healthy).await.unwrap().unwrap();
        assert_eq!(stored.delivery_attempts, 1, "the healthy page was recorded");
        assert_eq!(stored.last_paged_at, Some(now()));
    }

    #[tokio::test]
    async fn an_acked_but_never_paged_incident_stays_pending() {
        let store = MemoryAlertStore::new();
        let key = IncidentKey::new(DETECTOR_INVARIANT, "suite", "ep-1");
        store
            .insert(Incident {
                id: Uuid::from_u128(3),
                key: key.clone(),
                severity: "crit".into(),
                body: "drift".into(),
                status: IncidentStatus::Acked,
                delivery_attempts: 0,
                last_paged_at: None,
                acked_at: Some(now()),
                resolved_at: None,
            })
            .await
            .unwrap();
        let pending = store.pending_delivery().await.unwrap();
        assert_eq!(
            pending.len(),
            1,
            "an operator ack is not a page; the incident is still undelivered"
        );
        assert_eq!(pending[0].key, key);
    }

    #[tokio::test]
    async fn a_resolved_incident_is_never_redelivered() {
        let store = MemoryAlertStore::new();
        store
            .insert(Incident {
                id: Uuid::from_u128(4),
                key: IncidentKey::new(DETECTOR_RESERVE, "reserve", "ep-1"),
                severity: "crit".into(),
                body: "coverage".into(),
                status: IncidentStatus::Resolved,
                delivery_attempts: 0,
                last_paged_at: None,
                acked_at: None,
                resolved_at: Some(now()),
            })
            .await
            .unwrap();
        assert!(store.pending_delivery().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_concurrent_raise_opens_exactly_one_incident_and_pages_once() {
        let alerter = RecordingAlerter::new();
        let manager = IncidentManager {
            store: RacyStore(MemoryAlertStore::new()),
            alerter: &alerter,
        };
        let key = IncidentKey::new(DETECTOR_RECON, "cut", "residual:1");
        let first = manager
            .raise(key.clone(), "crit", "1us", now())
            .await
            .unwrap();
        let second = manager
            .raise(key.clone(), "crit", "1us", now())
            .await
            .unwrap();
        assert_eq!(
            first.id, second.id,
            "the loser of the open-or-get race must adopt the winner's incident"
        );
        assert_eq!(
            alerter.len(),
            1,
            "a duplicate raise must never page an operator twice"
        );
    }

    #[tokio::test]
    async fn insert_if_absent_reports_the_existing_open_incident_and_reopens_after_resolve() {
        let store = MemoryAlertStore::new();
        let key = IncidentKey::new(DETECTOR_STUCK_SEND, "wd-1", "ep-1");
        let first = Incident {
            id: Uuid::from_u128(11),
            key: key.clone(),
            severity: "crit".into(),
            body: "stuck".into(),
            status: IncidentStatus::Open,
            delivery_attempts: 0,
            last_paged_at: None,
            acked_at: None,
            resolved_at: None,
        };
        assert_eq!(store.insert_if_absent(first.clone()).await.unwrap(), None);
        let mut second = first.clone();
        second.id = Uuid::from_u128(12);
        assert_eq!(
            store
                .insert_if_absent(second.clone())
                .await
                .unwrap()
                .map(|incident| incident.id),
            Some(first.id),
            "an open incident with the same key is returned, not duplicated"
        );
        let mut resolved = first.clone();
        resolved.status = IncidentStatus::Resolved;
        resolved.resolved_at = Some(now());
        store.save(&resolved).await.unwrap();
        assert_eq!(
            store.insert_if_absent(second).await.unwrap(),
            None,
            "a recurrence after recovery opens a new episode"
        );
    }

    fn report(
        identities: &[(&'static str, bool)],
    ) -> crate::integrity::invariant_sweep::InvariantReport {
        crate::integrity::invariant_sweep::InvariantReport {
            as_of: now(),
            pass: identities.iter().all(|(_, pass)| *pass),
            identities: identities
                .iter()
                .map(
                    |(identity, pass)| crate::integrity::invariant_sweep::IdentityResult {
                        identity,
                        pass: *pass,
                        detail: (!*pass).then(|| format!("{identity} drifted")),
                    },
                )
                .collect(),
        }
    }

    #[tokio::test]
    async fn an_ongoing_invariant_breach_pages_once_and_then_dedups() {
        let store = MemoryAlertStore::new();
        let alerter = RecordingAlerter::new();
        let manager = IncidentManager {
            store: &store,
            alerter: &alerter,
        };
        let breach = report(&[("per_txn_sum_zero", false), ("job_idempotency", true)]);
        manager.sync_invariant_report(&breach, now()).await.unwrap();
        manager.sync_invariant_report(&breach, now()).await.unwrap();
        assert_eq!(
            alerter.len(),
            1,
            "an ongoing breach pages once, not once per sweep"
        );
        let key = IncidentKey::new(DETECTOR_INVARIANT, "per_txn_sum_zero", "active");
        let open = store.find_open(&key).await.unwrap().unwrap();
        assert_eq!(open.severity, "crit");
        assert!(
            open.body.contains("per_txn_sum_zero"),
            "the identity's detail is the incident body"
        );
        assert!(
            store
                .find_open(&IncidentKey::new(
                    DETECTOR_INVARIANT,
                    "job_idempotency",
                    "active"
                ))
                .await
                .unwrap()
                .is_none(),
            "a passing identity opens nothing"
        );
    }

    #[tokio::test]
    async fn a_recovered_identity_resolves_and_a_recurrence_pages_a_new_incident() {
        let store = MemoryAlertStore::new();
        let alerter = RecordingAlerter::new();
        let manager = IncidentManager {
            store: &store,
            alerter: &alerter,
        };
        let key = IncidentKey::new(DETECTOR_INVARIANT, "per_txn_sum_zero", "active");
        manager
            .sync_invariant_report(&report(&[("per_txn_sum_zero", false)]), now())
            .await
            .unwrap();
        let first = store.find_open(&key).await.unwrap().unwrap();
        assert_eq!(alerter.len(), 1);

        manager
            .sync_invariant_report(&report(&[("per_txn_sum_zero", true)]), now())
            .await
            .unwrap();
        assert!(
            store.find_open(&key).await.unwrap().is_none(),
            "recovery resolves the open incident"
        );
        assert_eq!(alerter.len(), 1, "recovery does not page");

        manager
            .sync_invariant_report(&report(&[("per_txn_sum_zero", false)]), now())
            .await
            .unwrap();
        let second = store.find_open(&key).await.unwrap().unwrap();
        assert_ne!(
            second.id, first.id,
            "a recurrence after recovery is a new incident, not the corpse"
        );
        assert_eq!(alerter.len(), 2, "a recurrence pages again");
    }

    #[tokio::test]
    async fn a_continued_pass_with_nothing_open_is_a_no_op() {
        let store = MemoryAlertStore::new();
        let alerter = RecordingAlerter::new();
        let manager = IncidentManager {
            store: &store,
            alerter: &alerter,
        };
        let clean = report(&[("per_txn_sum_zero", true), ("job_idempotency", true)]);
        manager.sync_invariant_report(&clean, now()).await.unwrap();
        manager.sync_invariant_report(&clean, now()).await.unwrap();
        assert_eq!(alerter.len(), 0);
        assert!(store.pending_delivery().await.unwrap().is_empty());
        assert!(store
            .find_open(&IncidentKey::new(
                DETECTOR_INVARIANT,
                "per_txn_sum_zero",
                "active"
            ))
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn a_failing_identity_whose_page_fails_stays_pending_and_the_error_surfaces() {
        let store = MemoryAlertStore::new();
        let key = IncidentKey::new(DETECTOR_INVARIANT, "per_txn_sum_zero", "active");
        let alerter = PartlyFailingAlerter::failing_on(&[&key.encoded()]);
        let manager = IncidentManager {
            store: &store,
            alerter: &alerter,
        };
        let breach = report(&[("per_txn_sum_zero", false), ("job_idempotency", false)]);
        assert!(
            manager.sync_invariant_report(&breach, now()).await.is_err(),
            "the pager failure must surface once the whole report is attempted"
        );
        assert_eq!(
            alerter.paged(),
            vec![IncidentKey::new(DETECTOR_INVARIANT, "job_idempotency", "active").encoded()],
            "the second identity is still raised despite the first one failing"
        );
        let pending = store.pending_delivery().await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].key, key);
    }

    #[tokio::test]
    async fn open_incident_is_deduped_until_resolved_then_repages() {
        let store = MemoryAlertStore::new();
        let alerter = RecordingAlerter::new();
        let manager = IncidentManager {
            store: &store,
            alerter: &alerter,
        };
        let key = IncidentKey::new(DETECTOR_INVARIANT, "suite", "ep-1");
        let first = manager
            .raise(key.clone(), "crit", "drift", now())
            .await
            .unwrap();
        let again = manager
            .raise(key.clone(), "crit", "drift", now())
            .await
            .unwrap();
        assert_eq!(first.id, again.id);
        assert_eq!(alerter.len(), 1);
        manager.ack(&key, now()).await.unwrap();
        assert!(manager.ack(&key, now()).await.is_ok());
        manager.resolve(&key, now()).await.unwrap();
        assert!(manager.ack(&key, now()).await.is_err());
        let recur = manager
            .raise(key.clone(), "crit", "drift-again", now())
            .await
            .unwrap();
        assert_ne!(recur.id, first.id);
        assert_eq!(alerter.len(), 2);
        assert_eq!(key.encoded(), "invariant_breach:suite:ep-1");
    }

    #[tokio::test]
    async fn pending_delivery_is_at_least_once_and_missing_ack_is_typed() {
        let store = MemoryAlertStore::new();
        let inserted = Incident {
            id: Uuid::from_u128(1),
            key: IncidentKey::new(DETECTOR_RECON, "cut", "ep"),
            severity: "crit".into(),
            body: "1us".into(),
            status: IncidentStatus::Open,
            delivery_attempts: 0,
            last_paged_at: None,
            acked_at: None,
            resolved_at: None,
        };
        store.insert(inserted).await.unwrap();
        let alerter = RecordingAlerter::new();
        let manager = IncidentManager {
            store: &store,
            alerter: &alerter,
        };
        assert_eq!(manager.deliver_pending(now()).await.unwrap(), 1);
        assert_eq!(alerter.len(), 1);
        assert_eq!(manager.deliver_pending(now()).await.unwrap(), 0);
        let missing = IncidentKey::new(DETECTOR_AML, "user", "none");
        assert!(matches!(
            manager.resolve(&missing, now()).await,
            Err(StoreError::NotFound("alert incident"))
        ));
        assert!(alerter.is_empty() || alerter.len() == 1);
        for detector in [DETECTOR_STUCK_SEND, DETECTOR_STUCK_SCREEN, DETECTOR_RESERVE] {
            assert!(!detector.is_empty());
        }
    }
}
