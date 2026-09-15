//! W2 — the real D26 audit sink for the shared write transaction plus the
//! lock-free admin-plane reads (`OpsQueries`).

use application::error::StoreError;
use application::model::{AdminAction, AdminRole, DraftId, MarketId};
use application::ports::{AuditPageRow, OpsQueries, PublicationCommand, PublicationCommandStatus};
use async_trait::async_trait;
use sqlx::Row;
use time::OffsetDateTime;
use uuid::Uuid;

use super::rows::db_error;
use super::store::{PgStore, PgTx};

/// Inserts one audit row inside the caller's open transaction (D26: the
/// SAME transaction as the admin mutation's effect).
pub(super) async fn audit_insert(tx: &mut PgTx, action: AdminAction) -> Result<(), StoreError> {
    sqlx::query(
        "insert into admin_actions (actor_role, actor_token_digest, action, subject, before, after, reason)
         values ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(action.actor_role.name())
    .bind(&action.actor_token_digest)
    .bind(&action.action)
    .bind(&action.subject)
    .bind(action.before)
    .bind(action.after)
    .bind(action.reason)
    .execute(&mut *tx.tx)
    .await
    .map_err(db_error)?;
    Ok(())
}

fn role_from_name(name: &str) -> AdminRole {
    match name {
        "curator" => AdminRole::Curator,
        "ops" => AdminRole::Ops,
        "finance" => AdminRole::Finance,
        _ => AdminRole::Superadmin,
    }
}

pub(super) fn command_status(raw: &str) -> PublicationCommandStatus {
    match raw {
        "pending" => PublicationCommandStatus::Pending,
        "executing" => PublicationCommandStatus::Executing,
        "done" => PublicationCommandStatus::Done,
        _ => PublicationCommandStatus::Failed,
    }
}

pub(super) fn command_from_row(row: &sqlx::postgres::PgRow) -> PublicationCommand {
    PublicationCommand {
        id: row.get("id"),
        draft: DraftId(row.get("draft_id")),
        idempotency_key: row.get("idempotency_key"),
        requested_by: row.get("requested_by"),
        status: command_status(row.get::<String, _>("status").as_str()),
        attempts: row.get("attempts"),
        lease_expires_at: row.get("lease_expires_at"),
        result_market: row.get::<Option<Uuid>, _>("result_market_id").map(MarketId),
        error: row.get("error"),
    }
}

#[async_trait]
impl OpsQueries for PgStore {
    async fn audit_page(
        &self,
        before: Option<OffsetDateTime>,
        limit: u32,
    ) -> Result<Vec<AuditPageRow>, StoreError> {
        let rows = sqlx::query(
            "select id, actor_role, actor_token_digest, action, subject, before, after, reason, at
               from admin_actions
              where ($1::timestamptz is null or at < $1)
              order by at desc, id desc
              limit $2",
        )
        .bind(before)
        .bind(i64::from(limit.clamp(1, 200)))
        .fetch_all(self.pool_handle())
        .await
        .map_err(db_error)?;
        Ok(rows
            .iter()
            .map(|row| AuditPageRow {
                id: row.get("id"),
                at: row.get("at"),
                action: AdminAction {
                    actor_role: role_from_name(row.get::<String, _>("actor_role").as_str()),
                    actor_token_digest: row.get("actor_token_digest"),
                    action: row.get("action"),
                    subject: row.get("subject"),
                    before: row.get("before"),
                    after: row.get("after"),
                    reason: row.get("reason"),
                },
            })
            .collect())
    }

    async fn publication_command_for_draft(
        &self,
        draft: DraftId,
    ) -> Result<Option<PublicationCommand>, StoreError> {
        let row = sqlx::query(
            "select id, draft_id, idempotency_key, requested_by, status, attempts,
                    lease_expires_at, result_market_id, error
               from publication_commands
              where draft_id = $1
              order by created_at desc, id desc
              limit 1",
        )
        .bind(draft.0)
        .fetch_optional(self.pool_handle())
        .await
        .map_err(db_error)?;
        Ok(row.as_ref().map(command_from_row))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_publication_status_is_failed_closed() {
        assert_eq!(command_status("corrupt"), PublicationCommandStatus::Failed);
    }
}
