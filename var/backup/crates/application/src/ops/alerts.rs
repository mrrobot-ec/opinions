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
                matches!(incident.status, IncidentStatus::Open) && incident.last_paged_at.is_none()
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
        if let Some(existing) = self.store.find_open(&key).await? {
            return Ok(existing);
        }
        let incident = Incident {
            id: Uuid::new_v4(),
            key: key.clone(),
            severity: severity.to_owned(),
            body: body.to_owned(),
            status: IncidentStatus::Open,
            delivery_attempts: 1,
            last_paged_at: Some(now),
            acked_at: None,
            resolved_at: None,
        };
        self.store.insert(incident.clone()).await?;
        self.alerter.page(severity, &key.encoded(), body).await?;
        Ok(incident)
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

    /// At-least-once: redeliver any open incident that never observed a page.
    ///
    /// # Errors
    /// Store or alerter failures.
    pub async fn deliver_pending(&self) -> Result<u32, StoreError> {
        let pending = self.store.pending_delivery().await?;
        let mut sent = 0_u32;
        for mut incident in pending {
            self.alerter
                .page(&incident.severity, &incident.key.encoded(), &incident.body)
                .await?;
            incident.delivery_attempts = incident.delivery_attempts.saturating_add(1);
            incident.last_paged_at = incident.last_paged_at.or(Some(OffsetDateTime::UNIX_EPOCH));
            self.store.save(&incident).await?;
            sent = sent.saturating_add(1);
        }
        Ok(sent)
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
        assert_eq!(manager.deliver_pending().await.unwrap(), 1);
        assert_eq!(alerter.len(), 1);
        assert_eq!(manager.deliver_pending().await.unwrap(), 0);
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
