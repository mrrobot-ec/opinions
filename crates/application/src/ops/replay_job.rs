//! D26 — replay-job authorization: a multi-transaction admin effect is
//! command-based. The admin's single transaction inserts (idempotent durable
//! command + audit); leased machine runners deliver by re-emitting the
//! subject's outbox event, exactly once per command.

use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::AppError;
use crate::model::{AdminContext, Event};
use crate::ports::{Clock, OpsJobCommand, OpsJobStatus, Store};

use super::audit::{principal_digest, required_audit_for, OpsError};

pub const KIND_REPLAY_JOB: &str = "replay_job";
pub const KIND_REFANOUT: &str = "refanout";

#[derive(Debug, Clone)]
pub struct OpsJobCmd {
    /// Subject reference (`market:<uuid>` style) whose delivery re-runs.
    pub subject: String,
    pub reason: String,
    pub idempotency_key: String,
}

/// Authorizes one durable ops job command (audit + command in one tx).
///
/// # Errors
/// Conflicts for duplicate ids; store failures.
pub async fn authorize<S: Store, C: Clock>(
    store: &S,
    _clock: &C,
    actor: &AdminContext,
    kind: &'static str,
    cmd: OpsJobCmd,
) -> Result<OpsJobCommand, OpsError> {
    let requested_by = principal_digest(actor)?.to_string();
    let mut tx = store.unwind_tx().await?;
    tx.serialize_key(&format!("ops-job:{}", cmd.idempotency_key))
        .await?;
    if let Some(existing) = tx.job_command_by_key(&cmd.idempotency_key).await? {
        return Ok(existing);
    }
    let command = OpsJobCommand {
        id: Uuid::new_v4(),
        kind: kind.to_string(),
        subject: cmd.subject.clone(),
        idempotency_key: cmd.idempotency_key.clone(),
        requested_by,
        status: OpsJobStatus::Pending,
        attempts: 0,
        lease_expires_at: None,
        error: None,
    };
    tx.insert_job_command(command.clone()).await?;
    let row = required_audit_for(
        actor,
        kind,
        cmd.subject,
        None,
        Some(json!({ "command_id": command.id.to_string() })),
        Some(cmd.reason),
    );
    tx.audit_insert(row).await?;
    tx.commit().await?;
    Ok(command)
}

/// One leased runner pass: claims due commands (pending, or executing with a
/// lapsed lease), delivers each by appending its replay outbox event, and
/// settles it — the durable command makes delivery exactly-once across
/// crashes.
///
/// # Errors
/// Store failures claiming; per-command failures settle `Failed` and do not
/// abort the pass.
pub async fn run_due<S: Store, C: Clock>(
    store: &S,
    clock: &C,
    lease_secs: u64,
    limit: u32,
) -> Result<u32, AppError> {
    let now = clock.now();
    let lease_until =
        now + time::Duration::seconds(lease_secs.min(i64::MAX.cast_unsigned()).cast_signed());
    let mut claim = store.unwind_tx().await?;
    claim.serialize_key("ops-job-runner").await?;
    let due = claim.due_job_commands(now, lease_until, limit).await?;
    claim.commit().await?;
    let mut delivered = 0_u32;
    for command in due {
        let outcome = deliver(store, &command, now).await;
        if settle_delivery(store, &command, outcome).await? {
            delivered += 1;
        }
    }
    Ok(delivered)
}

async fn settle_delivery<S: Store>(
    store: &S,
    command: &OpsJobCommand,
    outcome: Result<(), AppError>,
) -> Result<bool, AppError> {
    let Err(error) = outcome else {
        return Ok(true);
    };
    let mut settle = store.unwind_tx().await?;
    settle
        .serialize_key(&format!("ops-job-settle:{}", command.id))
        .await?;
    settle
        .save_job_command(&failed_command(command, &error))
        .await?;
    settle.commit().await?;
    Ok(false)
}

fn failed_command(command: &OpsJobCommand, error: &AppError) -> OpsJobCommand {
    let mut settled = command.clone();
    settled.status = OpsJobStatus::Failed;
    settled.error = Some(error.to_string());
    settled.lease_expires_at = None;
    settled
}

async fn deliver<S: Store>(
    store: &S,
    command: &OpsJobCommand,
    now: OffsetDateTime,
) -> Result<(), AppError> {
    let aggregate = command
        .subject
        .rsplit(':')
        .next()
        .and_then(|raw| Uuid::parse_str(raw).ok())
        .unwrap_or(command.id);
    let mut tx = store.unwind_tx().await?;
    tx.serialize_key(&format!("ops-job-deliver:{}", command.id))
        .await?;
    tx.append(Event {
        event_type: if command.kind == KIND_REFANOUT {
            "RefanoutRequested"
        } else {
            "JobReplayRequested"
        },
        aggregate_type: "ops_job",
        aggregate_id: aggregate,
        payload: json!({
            "command_id": command.id.to_string(),
            "kind": command.kind,
            "subject": command.subject,
            "requested_at": now.to_string(),
        }),
    })
    .await?;
    let mut settled = command.clone();
    settled.status = OpsJobStatus::Done;
    settled.error = None;
    settled.lease_expires_at = None;
    tx.save_job_command(&settled).await?;
    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::fakes::{FakeClock, InMemoryStore};
    use crate::model::AdminRole;
    use crate::ports::OpsQueries;
    use time::OffsetDateTime;

    fn ops_admin() -> AdminContext {
        AdminContext::Admin {
            token_digest: "digest-ops".into(),
            role: AdminRole::Ops,
        }
    }

    #[tokio::test]
    async fn authorize_is_idempotent_and_audited_then_delivered_exactly_once() {
        let store = InMemoryStore::new();
        let clock = FakeClock::at(OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap());
        let subject = format!("market:{}", uuid::Uuid::new_v4());
        let cmd = OpsJobCmd {
            subject: subject.clone(),
            reason: "stuck fanout".into(),
            idempotency_key: "job-1".into(),
        };
        let command = authorize(&store, &clock, &ops_admin(), KIND_REPLAY_JOB, cmd.clone())
            .await
            .unwrap();
        assert_eq!(command.status, crate::ports::OpsJobStatus::Pending);
        let replay = authorize(&store, &clock, &ops_admin(), KIND_REPLAY_JOB, cmd)
            .await
            .unwrap();
        assert_eq!(replay.id, command.id);
        let audits = store.audit_page(None, 10).await.unwrap();
        assert_eq!(audits.len(), 1);
        assert_eq!(audits[0].action.action, "replay_job");
        assert_eq!(audits[0].action.subject, subject);
        // Machine actors cannot authorize.
        let denied = authorize(
            &store,
            &clock,
            &AdminContext::Machine,
            KIND_REPLAY_JOB,
            OpsJobCmd {
                subject: subject.clone(),
                reason: "r".into(),
                idempotency_key: "job-2".into(),
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(denied, OpsError::App(_)));

        // Delivery: one pass emits the replay event and settles Done.
        let delivered = run_due(&store, &clock, 60, 10).await.unwrap();
        assert_eq!(delivered, 1);
        let events: Vec<_> = store
            .outbox()
            .into_iter()
            .filter(|event| event.event_type == "JobReplayRequested")
            .collect();
        assert_eq!(events.len(), 1);
        // A second pass finds nothing due.
        assert_eq!(run_due(&store, &clock, 60, 10).await.unwrap(), 0);
        assert_eq!(
            store
                .outbox()
                .into_iter()
                .filter(|event| event.event_type == "JobReplayRequested")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn refanout_rides_the_same_command_rail() {
        let store = InMemoryStore::new();
        let clock = FakeClock::at(OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap());
        let command = crate::ops::refanout::authorize_refanout(
            &store,
            &clock,
            &ops_admin(),
            OpsJobCmd {
                subject: format!("market:{}", uuid::Uuid::new_v4()),
                reason: "notifications missed".into(),
                idempotency_key: "rf-1".into(),
            },
        )
        .await
        .unwrap();
        assert_eq!(command.kind, KIND_REFANOUT);
        assert_eq!(run_due(&store, &clock, 60, 10).await.unwrap(), 1);
        assert!(store
            .outbox()
            .iter()
            .any(|event| event.event_type == "RefanoutRequested"));
    }

    #[test]
    fn failed_delivery_projection_clears_the_lease_and_records_the_error() {
        let command = OpsJobCommand {
            id: uuid::Uuid::new_v4(),
            kind: KIND_REPLAY_JOB.into(),
            subject: "market:bad".into(),
            idempotency_key: "failed".into(),
            requested_by: "ops".into(),
            status: OpsJobStatus::Executing,
            attempts: 1,
            lease_expires_at: Some(OffsetDateTime::UNIX_EPOCH),
            error: None,
        };
        let settled = failed_command(
            &command,
            &AppError::Store(crate::error::StoreError::Backend("boom".into())),
        );
        assert_eq!(settled.status, OpsJobStatus::Failed);
        assert!(settled.error.unwrap().contains("boom"));
        assert!(settled.lease_expires_at.is_none());
    }

    #[tokio::test]
    async fn failed_delivery_is_settled_durably() {
        let store = InMemoryStore::new();
        let clock = FakeClock::at(OffsetDateTime::UNIX_EPOCH);
        let command = authorize(
            &store,
            &clock,
            &ops_admin(),
            KIND_REPLAY_JOB,
            OpsJobCmd {
                subject: "market:bad".into(),
                reason: "test failure".into(),
                idempotency_key: "failed-settle".into(),
            },
        )
        .await
        .unwrap();
        assert!(!settle_delivery(
            &store,
            &command,
            Err(AppError::Store(crate::error::StoreError::Backend(
                "boom".into()
            )))
        )
        .await
        .unwrap());
        assert!(settle_delivery(&store, &command, Ok(())).await.unwrap());
    }
}
