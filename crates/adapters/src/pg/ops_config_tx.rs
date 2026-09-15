//! PostgreSQL D24 config-plane transaction (wave W1), the D25 fence-point
//! role on the shared write transaction, and the reconciler's watch IO.
//!
//! Lock protocol (one-way graph, codex r2 NEW-1): a config writer takes
//! (proposal row) → exclusive class-4 fences → the singleton
//! `config_generation` row → config rows, and NEVER any trade/vote lock.
//! Trade/vote transactions take their row locks FIRST and the shared fences
//! immediately before their first authoritative write.

use application::error::StoreError;
use application::model::{
    AdminAction, ConfigChange, ConfigEntry, ConfigProposal, Event, MarketId, MarketRow,
    ProposalStatus,
};
use application::ops::config::{FencePoint, OpsWriteSupport, FENCE_CLASS};
use application::ops::reconciler::ConfigWatchIo;
use application::ports::{AuditWrite, Committable, OutboxWriter};
use async_trait::async_trait;
use sqlx::{Postgres, Row, Transaction};
use uuid::Uuid;

use super::rows::{db_error, market_from_row, unique_violation};
use super::store::PgStore;
use super::store::PgTx;

/// The generation-serialized config write transaction.
pub(super) struct PgOpsConfigTx {
    tx: Transaction<'static, Postgres>,
}

impl PgStore {
    pub(super) async fn open_ops_config_tx(&self) -> Result<PgOpsConfigTx, StoreError> {
        Ok(PgOpsConfigTx {
            tx: self.pool_handle().begin().await.map_err(db_error)?,
        })
    }
}

const MARKET_TIMES: &str = r#"
    select m.id, m.slug, m.question, m.status, m.min_votes_to_resolve, m.opens_at,
           m.closes_at, m.tally_hidden_at, m.curator_flagged_at, m.integrity_due_at,
           m.poster_asset_url, m.video_asset_url,
           yes_outcome.id as yes_outcome, no_outcome.id as no_outcome
      from markets m
      join outcomes yes_outcome on yes_outcome.market_id = m.id and yes_outcome.idx = 0
      join outcomes no_outcome on no_outcome.market_id = m.id and no_outcome.idx = 1
     where m.id = $1
"#;

fn proposal_status(value: &str) -> Result<ProposalStatus, StoreError> {
    Ok(match value {
        "pending" => ProposalStatus::Pending,
        "confirmed" => ProposalStatus::Confirmed,
        "rejected" => ProposalStatus::Rejected,
        "expired" => ProposalStatus::Expired,
        _ => return Err(StoreError::Invariant("unknown proposal status")),
    })
}

const fn status_name(status: ProposalStatus) -> &'static str {
    match status {
        ProposalStatus::Pending => "pending",
        ProposalStatus::Confirmed => "confirmed",
        ProposalStatus::Rejected => "rejected",
        ProposalStatus::Expired => "expired",
    }
}

fn role_name(role: application::model::AdminRole) -> &'static str {
    role.name()
}

fn role_of(value: &str) -> Result<application::model::AdminRole, StoreError> {
    use application::model::AdminRole;
    Ok(match value {
        "curator" => AdminRole::Curator,
        "ops" => AdminRole::Ops,
        "finance" => AdminRole::Finance,
        "superadmin" => AdminRole::Superadmin,
        _ => return Err(StoreError::Invariant("unknown admin role")),
    })
}

fn proposal_from_row(row: &sqlx::postgres::PgRow) -> Result<ConfigProposal, StoreError> {
    Ok(ConfigProposal {
        id: row.get("id"),
        idempotency_key: row.get("idempotency_key"),
        patch: row.get("patch"),
        patch_hash: row.get("patch_hash"),
        base_generation: row.get("base_generation"),
        proposer_token_id: row.get("proposer_token_id"),
        proposer_role: role_of(row.get::<String, _>("proposer_role").as_str())?,
        reason: row.get("reason"),
        status: proposal_status(row.get::<String, _>("status").as_str())?,
        expires_at: row.get("expires_at"),
        confirmer_token_id: row.get("confirmer_token_id"),
        resulting_generation: row.get("resulting_generation"),
    })
}

async fn audit_insert_in(
    tx: &mut Transaction<'static, Postgres>,
    action: AdminAction,
) -> Result<(), StoreError> {
    sqlx::query(
        r#"
        insert into admin_actions (actor_role, actor_token_digest, action, subject,
                                   before, after, reason)
        values ($1, $2, $3, $4, $5, $6, $7)
        "#,
    )
    .bind(action.actor_role.name())
    .bind(&action.actor_token_digest)
    .bind(&action.action)
    .bind(&action.subject)
    .bind(&action.before)
    .bind(&action.after)
    .bind(&action.reason)
    .execute(&mut **tx)
    .await
    .map_err(db_error)?;
    Ok(())
}

async fn outbox_append_in(
    tx: &mut Transaction<'static, Postgres>,
    event: Event,
) -> Result<(), StoreError> {
    sqlx::query(
        r#"
        insert into events_outbox (aggregate_type, aggregate_id, event_type, payload)
        values ($1, $2, $3, $4)
        "#,
    )
    .bind(event.aggregate_type)
    .bind(event.aggregate_id)
    .bind(event.event_type)
    .bind(event.payload)
    .execute(&mut **tx)
    .await
    .map_err(db_error)?;
    Ok(())
}

#[async_trait]
impl application::ports::OpsConfigTx for PgOpsConfigTx {
    async fn lock_generation(&mut self) -> Result<i64, StoreError> {
        // THE serialization point: writer B blocks here behind writer A.
        let row =
            sqlx::query("select generation from config_generation where singleton = 1 for update")
                .fetch_optional(&mut *self.tx)
                .await
                .map_err(db_error)?
                .ok_or(StoreError::Invariant("config generation row missing"))?;
        Ok(row.get("generation"))
    }

    async fn config_entries(&mut self) -> Result<Vec<ConfigEntry>, StoreError> {
        let rows = sqlx::query("select key, value from config_entries order by key")
            .fetch_all(&mut *self.tx)
            .await
            .map_err(db_error)?;
        Ok(rows
            .iter()
            .map(|row| ConfigEntry {
                key: row.get("key"),
                value: row.get("value"),
            })
            .collect())
    }

    async fn apply_changes(
        &mut self,
        generation: i64,
        changes: &[ConfigChange],
        applied_by: &str,
    ) -> Result<(), StoreError> {
        sqlx::query("insert into config_generations (generation, applied_by) values ($1, $2)")
            .bind(generation)
            .bind(applied_by)
            .execute(&mut *self.tx)
            .await
            .map_err(db_error)?;
        for change in changes {
            sqlx::query(
                r#"
                insert into config_changes (generation, key, old, new, changed_by)
                values ($1, $2, $3, $4, $5)
                "#,
            )
            .bind(generation)
            .bind(&change.key)
            .bind(&change.old)
            .bind(&change.new)
            .bind(applied_by)
            .execute(&mut *self.tx)
            .await
            .map_err(db_error)?;
            sqlx::query(
                r#"
                insert into config_entries (key, value, updated_at)
                values ($1, $2, now())
                on conflict (key) do update set value = excluded.value, updated_at = now()
                "#,
            )
            .bind(&change.key)
            .bind(&change.new)
            .execute(&mut *self.tx)
            .await
            .map_err(db_error)?;
        }
        sqlx::query(
            r#"
            update config_generation
               set generation = $1, updated_by = $2, updated_at = now()
             where singleton = 1
            "#,
        )
        .bind(generation)
        .bind(applied_by)
        .execute(&mut *self.tx)
        .await
        .map_err(db_error)?;
        Ok(())
    }

    async fn changes_for_key_since(
        &mut self,
        key: &str,
        generation: i64,
    ) -> Result<Vec<ConfigChange>, StoreError> {
        // Rides the (key, generation) index (D25a probe shape).
        let rows = sqlx::query(
            r#"
            select key, old, new from config_changes
             where key = $1 and generation > $2
             order by generation
            "#,
        )
        .bind(key)
        .bind(generation)
        .fetch_all(&mut *self.tx)
        .await
        .map_err(db_error)?;
        Ok(rows
            .iter()
            .map(|row| ConfigChange {
                key: row.get("key"),
                old: row.get("old"),
                new: row.get("new"),
            })
            .collect())
    }

    async fn insert_proposal(&mut self, proposal: ConfigProposal) -> Result<(), StoreError> {
        let result = sqlx::query(
            r#"
            insert into config_change_proposals
                (id, idempotency_key, patch, patch_hash, base_generation,
                 proposer_token_id, proposer_role, reason, status, expires_at)
            values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
            "#,
        )
        .bind(proposal.id)
        .bind(&proposal.idempotency_key)
        .bind(&proposal.patch)
        .bind(&proposal.patch_hash)
        .bind(proposal.base_generation)
        .bind(&proposal.proposer_token_id)
        .bind(role_name(proposal.proposer_role))
        .bind(&proposal.reason)
        .bind(status_name(proposal.status))
        .bind(proposal.expires_at)
        .execute(&mut *self.tx)
        .await;
        match result {
            Ok(_) => Ok(()),
            Err(error) if unique_violation(&error) => {
                Err(StoreError::Conflict("config proposal idempotency key"))
            }
            Err(error) => Err(db_error(error)),
        }
    }

    async fn proposal_for_update(&mut self, id: Uuid) -> Result<ConfigProposal, StoreError> {
        let row = sqlx::query("select * from config_change_proposals where id = $1 for update")
            .bind(id)
            .fetch_optional(&mut *self.tx)
            .await
            .map_err(db_error)?
            .ok_or(StoreError::NotFound("config proposal"))?;
        proposal_from_row(&row)
    }

    async fn save_proposal(&mut self, proposal: &ConfigProposal) -> Result<(), StoreError> {
        sqlx::query(
            r#"
            update config_change_proposals
               set status = $2, confirmer_token_id = $3, resulting_generation = $4,
                   settled_at = case when $2 in ('confirmed','rejected','expired')
                                     then now() else settled_at end
             where id = $1
            "#,
        )
        .bind(proposal.id)
        .bind(status_name(proposal.status))
        .bind(&proposal.confirmer_token_id)
        .bind(proposal.resulting_generation)
        .execute(&mut *self.tx)
        .await
        .map_err(db_error)?;
        Ok(())
    }
}

#[async_trait]
impl OpsWriteSupport for PgOpsConfigTx {
    async fn acquire_exclusive_fences(&mut self, namespaces: &[String]) -> Result<(), StoreError> {
        for namespace in namespaces {
            sqlx::query("select pg_advisory_xact_lock($1, hashtext($2))")
                .bind(FENCE_CLASS)
                .bind(namespace)
                .execute(&mut *self.tx)
                .await
                .map_err(db_error)?;
        }
        Ok(())
    }

    async fn market_times(&mut self, market: MarketId) -> Result<Option<MarketRow>, StoreError> {
        // Deliberately lock-free: a row lock here would close the one-way
        // lock graph into a trade-vs-pause deadlock cycle.
        let row = sqlx::query(MARKET_TIMES)
            .bind(market.0)
            .fetch_optional(&mut *self.tx)
            .await
            .map_err(db_error)?;
        row.as_ref().map(market_from_row).transpose()
    }

    async fn proposal_by_idempotency_key(
        &mut self,
        key: &str,
    ) -> Result<Option<ConfigProposal>, StoreError> {
        let row = sqlx::query("select * from config_change_proposals where idempotency_key = $1")
            .bind(key)
            .fetch_optional(&mut *self.tx)
            .await
            .map_err(db_error)?;
        row.as_ref().map(proposal_from_row).transpose()
    }

    async fn changes_in_generation(
        &mut self,
        generation: i64,
    ) -> Result<Vec<ConfigChange>, StoreError> {
        let rows = sqlx::query(
            "select key, old, new from config_changes where generation = $1 order by key",
        )
        .bind(generation)
        .fetch_all(&mut *self.tx)
        .await
        .map_err(db_error)?;
        Ok(rows
            .iter()
            .map(|row| ConfigChange {
                key: row.get("key"),
                old: row.get("old"),
                new: row.get("new"),
            })
            .collect())
    }
}

#[async_trait]
impl AuditWrite for PgOpsConfigTx {
    async fn audit_insert(&mut self, action: AdminAction) -> Result<(), StoreError> {
        audit_insert_in(&mut self.tx, action).await
    }
}

#[async_trait]
impl OutboxWriter for PgOpsConfigTx {
    async fn append(&mut self, event: Event) -> Result<(), StoreError> {
        outbox_append_in(&mut self.tx, event).await
    }

    async fn append_batch(&mut self, events: &[Event]) -> Result<(), StoreError> {
        for event in events {
            outbox_append_in(&mut self.tx, event.clone()).await?;
        }
        Ok(())
    }
}

#[async_trait]
impl Committable for PgOpsConfigTx {
    async fn commit(self: Box<Self>) -> Result<(), StoreError> {
        self.tx.commit().await.map_err(db_error)
    }
}

// ---------------------------------------------------------------------------
// D25 fence point on the shared write transaction
// ---------------------------------------------------------------------------

#[async_trait]
impl FencePoint for PgTx {
    async fn acquire_shared_fences(&mut self, namespaces: &[String]) -> Result<(), StoreError> {
        for namespace in namespaces {
            sqlx::query("select pg_advisory_xact_lock_shared($1, hashtext($2))")
                .bind(FENCE_CLASS)
                .bind(namespace)
                .execute(&mut *self.tx)
                .await
                .map_err(db_error)?;
        }
        Ok(())
    }

    async fn fence_config_value(
        &mut self,
        key: &str,
    ) -> Result<Option<serde_json::Value>, StoreError> {
        let row = sqlx::query("select value from config_entries where key = $1")
            .bind(key)
            .fetch_optional(&mut *self.tx)
            .await
            .map_err(db_error)?;
        Ok(row.map(|r| r.get("value")))
    }

    async fn fence_changes_since(
        &mut self,
        generation: i64,
    ) -> Result<(i64, Option<Vec<ConfigChange>>), StoreError> {
        let head = sqlx::query(
            r#"
            select g.generation as current,
                   (select min(generation) from config_generations) as watermark
              from config_generation g where g.singleton = 1
            "#,
        )
        .fetch_optional(&mut *self.tx)
        .await
        .map_err(db_error)?
        .ok_or(StoreError::Invariant("config generation row missing"))?;
        let current: i64 = head.get("current");
        let watermark: i64 = head.try_get("watermark").unwrap_or(1);
        if generation + 1 < watermark {
            return Ok((current, None)); // history pruned: caller goes conservative
        }
        let rows = sqlx::query(
            r#"
            select key, old, new from config_changes
             where generation > $1 order by generation, key
            "#,
        )
        .bind(generation)
        .fetch_all(&mut *self.tx)
        .await
        .map_err(db_error)?;
        Ok((
            current,
            Some(
                rows.iter()
                    .map(|row| ConfigChange {
                        key: row.get("key"),
                        old: row.get("old"),
                        new: row.get("new"),
                    })
                    .collect(),
            ),
        ))
    }

    async fn request_fingerprint(&mut self, key: &str) -> Result<Option<String>, StoreError> {
        let row =
            sqlx::query("select fingerprint from request_fingerprints where idempotency_key = $1")
                .bind(key)
                .fetch_optional(&mut *self.tx)
                .await
                .map_err(db_error)?;
        Ok(row.map(|r| r.get("fingerprint")))
    }

    async fn save_request_fingerprint(
        &mut self,
        key: &str,
        fingerprint: &str,
    ) -> Result<(), StoreError> {
        sqlx::query(
            r#"
            insert into request_fingerprints (idempotency_key, fingerprint)
            values ($1, $2) on conflict (idempotency_key) do nothing
            "#,
        )
        .bind(key)
        .bind(fingerprint)
        .execute(&mut *self.tx)
        .await
        .map_err(db_error)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Reconciler watch IO (wake-ups only; wholesale reload)
// ---------------------------------------------------------------------------

#[async_trait]
impl ConfigWatchIo for PgStore {
    async fn drain_wakeups(&self) -> Result<bool, StoreError> {
        let mut tx = self.pool_handle().begin().await.map_err(db_error)?;
        let cursor = sqlx::query(
            "select last_seq from outbox_cursors where consumer = 'config_reconciler' for update",
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(db_error)?
        .ok_or(StoreError::Invariant("config_reconciler cursor missing"))?;
        let last_seq: i64 = cursor.get("last_seq");
        let head = sqlx::query(
            r#"
            select coalesce(max(seq), $1) as head,
                   count(*) filter (where event_type = 'ConfigChanged') as config_events
              from events_outbox where seq > $1
            "#,
        )
        .bind(last_seq)
        .fetch_one(&mut *tx)
        .await
        .map_err(db_error)?;
        let head_seq: i64 = head.get("head");
        let config_events: i64 = head.get("config_events");
        if head_seq > last_seq {
            sqlx::query(
                "update outbox_cursors set last_seq = $1 where consumer = 'config_reconciler'",
            )
            .bind(head_seq)
            .execute(&mut *tx)
            .await
            .map_err(db_error)?;
        }
        tx.commit().await.map_err(db_error)?;
        Ok(config_events > 0)
    }

    async fn load_snapshot(&self) -> Result<(i64, Vec<ConfigEntry>), StoreError> {
        // One transaction: the generation and the entry set are consistent.
        let mut tx = self.pool_handle().begin().await.map_err(db_error)?;
        let generation: i64 =
            sqlx::query("select generation from config_generation where singleton = 1")
                .fetch_optional(&mut *tx)
                .await
                .map_err(db_error)?
                .ok_or(StoreError::Invariant("config generation row missing"))?
                .get("generation");
        let rows = sqlx::query("select key, value from config_entries order by key")
            .fetch_all(&mut *tx)
            .await
            .map_err(db_error)?;
        tx.commit().await.map_err(db_error)?;
        Ok((
            generation,
            rows.iter()
                .map(|row| ConfigEntry {
                    key: row.get("key"),
                    value: row.get("value"),
                })
                .collect(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use application::model::{AdminRole, ProposalStatus};

    use super::*;

    #[test]
    fn config_database_vocabularies_are_total() {
        for (status, raw) in [
            (ProposalStatus::Pending, "pending"),
            (ProposalStatus::Confirmed, "confirmed"),
            (ProposalStatus::Rejected, "rejected"),
            (ProposalStatus::Expired, "expired"),
        ] {
            assert_eq!(status_name(status), raw);
            assert_eq!(proposal_status(raw), Ok(status));
        }
        assert_eq!(
            proposal_status("unknown"),
            Err(StoreError::Invariant("unknown proposal status"))
        );
        for (role, raw) in [
            (AdminRole::Curator, "curator"),
            (AdminRole::Ops, "ops"),
            (AdminRole::Finance, "finance"),
            (AdminRole::Superadmin, "superadmin"),
        ] {
            assert_eq!(role_name(role), raw);
            assert_eq!(role_of(raw), Ok(role));
        }
        assert_eq!(
            role_of("unknown"),
            Err(StoreError::Invariant("unknown admin role"))
        );
    }
}
