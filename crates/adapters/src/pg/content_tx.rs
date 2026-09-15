//! PostgreSQL content transaction implementation.

use application::error::StoreError;
use application::model::{
    ArtifactKind, DraftId, DraftRow, DraftStatus, Event, MarketId, PublishStage, UserId,
};
use application::ports::ContentTx;
use async_trait::async_trait;
use domain::drafting::{DraftSource, DraftSpec, DraftTier};
use domain::money::{BasisPoints, MicroUsd};
use sqlx::postgres::PgRow;
use sqlx::Row;
use time::{Date, OffsetDateTime};

use super::rows::{db_error, unique_violation};
use super::store::{PgStore, PgTx};

pub(super) async fn open(store: &PgStore) -> Result<Box<dyn ContentTx + '_>, StoreError> {
    Ok(Box::new(store.tx().await?))
}

#[async_trait]
impl ContentTx for PgTx {
    async fn lock_admission(&mut self) -> Result<(), StoreError> {
        advisory(&mut self.tx, "content:admission").await
    }

    async fn lock_publication(&mut self, draft: DraftId) -> Result<(), StoreError> {
        advisory(&mut self.tx, &format!("publish-now:{}", draft.0)).await
    }

    async fn lock_publication_queue(&mut self) -> Result<(), StoreError> {
        advisory(&mut self.tx, "publication-commands").await
    }

    async fn insert_publication_command(
        &mut self,
        command: application::ports::PublicationCommand,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "insert into publication_commands
                 (id, draft_id, idempotency_key, requested_by, status, attempts,
                  lease_expires_at, result_market_id, error)
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        )
        .bind(command.id)
        .bind(command.draft.0)
        .bind(&command.idempotency_key)
        .bind(&command.requested_by)
        .bind(publication_status_name(command.status))
        .bind(command.attempts)
        .bind(command.lease_expires_at)
        .bind(command.result_market.map(|m| m.0))
        .bind(&command.error)
        .execute(&mut *self.tx)
        .await
        .map_err(|error| match error {
            sqlx::Error::Database(db) if db.is_unique_violation() => {
                if db.constraint() == Some("publication_commands_open_draft_idx") {
                    StoreError::Conflict("publication command")
                } else {
                    StoreError::Conflict("publication command key")
                }
            }
            other => db_error(other),
        })?;
        Ok(())
    }

    async fn publication_command_by_draft(
        &mut self,
        draft: DraftId,
    ) -> Result<Option<application::ports::PublicationCommand>, StoreError> {
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "{PUBLICATION_COMMAND_SELECT} where draft_id = $1
             order by created_at desc, id desc limit 1"
        )))
        .bind(draft.0)
        .fetch_optional(&mut *self.tx)
        .await
        .map_err(db_error)?;
        Ok(row.as_ref().map(super::ops_audit_tx::command_from_row))
    }

    async fn publication_command_by_key(
        &mut self,
        key: &str,
    ) -> Result<Option<application::ports::PublicationCommand>, StoreError> {
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "{PUBLICATION_COMMAND_SELECT} where idempotency_key = $1"
        )))
        .bind(key)
        .fetch_optional(&mut *self.tx)
        .await
        .map_err(db_error)?;
        Ok(row.as_ref().map(super::ops_audit_tx::command_from_row))
    }

    async fn due_publication_commands(
        &mut self,
        now: OffsetDateTime,
        limit: u32,
    ) -> Result<Vec<application::ports::PublicationCommand>, StoreError> {
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "{PUBLICATION_COMMAND_SELECT}
              where status = 'pending'
                 or (status = 'executing'
                     and (lease_expires_at is null or lease_expires_at <= $1))
              order by created_at, id
              limit $2
              for update skip locked"
        )))
        .bind(now)
        .bind(i64::from(limit))
        .fetch_all(&mut *self.tx)
        .await
        .map_err(db_error)?;
        Ok(rows
            .iter()
            .map(super::ops_audit_tx::command_from_row)
            .collect())
    }

    async fn save_publication_command(
        &mut self,
        command: &application::ports::PublicationCommand,
    ) -> Result<(), StoreError> {
        let updated = sqlx::query(
            "update publication_commands
                set status = $2, attempts = $3, lease_expires_at = $4,
                    result_market_id = $5, error = $6, updated_at = now()
              where id = $1",
        )
        .bind(command.id)
        .bind(publication_status_name(command.status))
        .bind(command.attempts)
        .bind(command.lease_expires_at)
        .bind(command.result_market.map(|m| m.0))
        .bind(&command.error)
        .execute(&mut *self.tx)
        .await
        .map_err(db_error)?;
        if updated.rows_affected() == 0 {
            return Err(StoreError::Invariant(
                "save for unknown publication command",
            ));
        }
        Ok(())
    }

    async fn lock_slot_tier(&mut self, tier: DraftTier) -> Result<(), StoreError> {
        advisory(&mut self.tx, &format!("content:slot:{}", tier_name(tier))).await
    }

    async fn lock_lp_kill_switch(&mut self) -> Result<(), StoreError> {
        sqlx::query("select pg_advisory_xact_lock(3, hashtext('lp_kill'))")
            .execute(&mut *self.tx)
            .await
            .map_err(db_error)?;
        Ok(())
    }

    async fn pending_draft_count(&mut self) -> Result<u32, StoreError> {
        let count: i64 = sqlx::query_scalar(
            "select count(*)::bigint from market_drafts where status = 'pending'",
        )
        .fetch_one(&mut *self.tx)
        .await
        .map_err(db_error)?;
        u32::try_from(count).map_err(|_| StoreError::Invariant("pending draft count overflow"))
    }

    async fn list_drafts(&mut self, limit: u32) -> Result<Vec<DraftRow>, StoreError> {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "{DRAFT_SELECT} order by publish_at nulls last, created_at, id limit $1"
        )))
        .bind(i64::from(limit))
        .fetch_all(&mut *self.tx)
        .await
        .map_err(db_error)?
        .iter()
        .map(draft_from_row)
        .collect()
    }

    async fn insert_draft(&mut self, draft: DraftRow) -> Result<DraftId, StoreError> {
        let result = sqlx::query(
            r#"insert into market_drafts
                  (id, source, fallback_from, tier, status, publish_stage,
                   published_market_id, question, description, video_script, slug,
                   seed_micro, fee_bps, min_votes_to_resolve, open_secs,
                   hidden_window_secs, publish_at, expires_at, created_at, updated_at)
               values
                  ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$19)"#,
        )
        .bind(draft.id.0)
        .bind(source_name(draft.source))
        .bind(draft.fallback_from.map(source_name))
        .bind(tier_name(draft.spec.tier))
        .bind(status_name(draft.status))
        .bind(draft.publish_stage.map(stage_name))
        .bind(draft.published_market.map(|market| market.0))
        .bind(&draft.spec.question)
        .bind(&draft.spec.description)
        .bind(&draft.spec.video_script)
        .bind(&draft.spec.slug)
        .bind(draft.spec.seed.0)
        .bind(i32::from(draft.spec.fee.0))
        .bind(draft.spec.min_votes_to_resolve)
        .bind(
            i64::try_from(draft.spec.open_secs)
                .map_err(|_| StoreError::Invariant("open seconds overflow"))?,
        )
        .bind(
            i64::try_from(draft.spec.hidden_window_secs)
                .map_err(|_| StoreError::Invariant("hidden seconds overflow"))?,
        )
        .bind(draft.publish_at)
        .bind(draft.expires_at)
        .bind(draft.created_at)
        .execute(&mut *self.tx)
        .await;
        match result {
            Ok(_) => Ok(draft.id),
            Err(error) if unique_violation(&error) => Err(StoreError::Conflict("draft")),
            Err(error) => Err(db_error(error)),
        }
    }

    async fn draft_for_update(&mut self, draft: DraftId) -> Result<DraftRow, StoreError> {
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "{DRAFT_SELECT} where id = $1 for update"
        )))
        .bind(draft.0)
        .fetch_optional(&mut *self.tx)
        .await
        .map_err(db_error)?
        .ok_or(StoreError::NotFound("draft"))?;
        draft_from_row(&row)
    }

    async fn save_draft(&mut self, draft: &DraftRow) -> Result<(), StoreError> {
        let result = sqlx::query(
            r#"update market_drafts set
                   source=$2, fallback_from=$3, tier=$4, status=$5, publish_stage=$6,
                   published_market_id=$7, question=$8, description=$9,
                   video_script=$10, slug=$11, seed_micro=$12, fee_bps=$13,
                   min_votes_to_resolve=$14, open_secs=$15, hidden_window_secs=$16,
                   publish_at=$17, expires_at=$18, updated_at=now()
               where id=$1"#,
        )
        .bind(draft.id.0)
        .bind(source_name(draft.source))
        .bind(draft.fallback_from.map(source_name))
        .bind(tier_name(draft.spec.tier))
        .bind(status_name(draft.status))
        .bind(draft.publish_stage.map(stage_name))
        .bind(draft.published_market.map(|market| market.0))
        .bind(&draft.spec.question)
        .bind(&draft.spec.description)
        .bind(&draft.spec.video_script)
        .bind(&draft.spec.slug)
        .bind(draft.spec.seed.0)
        .bind(i32::from(draft.spec.fee.0))
        .bind(draft.spec.min_votes_to_resolve)
        .bind(
            i64::try_from(draft.spec.open_secs)
                .map_err(|_| StoreError::Invariant("open seconds overflow"))?,
        )
        .bind(
            i64::try_from(draft.spec.hidden_window_secs)
                .map_err(|_| StoreError::Invariant("hidden seconds overflow"))?,
        )
        .bind(draft.publish_at)
        .bind(draft.expires_at)
        .execute(&mut *self.tx)
        .await;
        match result {
            Ok(result) if result.rows_affected() == 1 => Ok(()),
            Ok(_) => Err(StoreError::NotFound("draft")),
            Err(error) if unique_violation(&error) => Err(StoreError::Conflict("draft slot")),
            Err(error) => Err(db_error(error)),
        }
    }

    async fn lock_budget_day(&mut self, day: Date) -> Result<(), StoreError> {
        advisory(&mut self.tx, &format!("content:budget:{day}")).await
    }

    async fn reserved_seed_for_day(&mut self, day: Date) -> Result<i64, StoreError> {
        sqlx::query_scalar(
            r#"select coalesce(sum(seed_micro), 0)::bigint
                 from market_drafts
                where status in ('approved', 'published')
                  and publish_at >= (($1::date)::timestamp at time zone 'UTC')
                  and publish_at < ((($1::date + 1)::timestamp) at time zone 'UTC')"#,
        )
        .bind(day)
        .fetch_one(&mut *self.tx)
        .await
        .map_err(db_error)
    }

    async fn lp_pnl_sum(
        &mut self,
        since: OffsetDateTime,
        until: OffsetDateTime,
    ) -> Result<i64, StoreError> {
        sqlx::query_scalar(
            r#"select coalesce(sum(lp_pnl_micro), 0)::bigint from markets
                where settled_at >= $1 and settled_at < $2 and lp_pnl_micro is not null"#,
        )
        .bind(since)
        .bind(until)
        .fetch_one(&mut *self.tx)
        .await
        .map_err(db_error)
    }

    async fn slot_is_reserved(
        &mut self,
        tier: DraftTier,
        at: OffsetDateTime,
    ) -> Result<bool, StoreError> {
        sqlx::query_scalar(
            "select exists(select 1 from market_drafts where tier=$1 and publish_at=$2)",
        )
        .bind(tier_name(tier))
        .bind(at)
        .fetch_one(&mut *self.tx)
        .await
        .map_err(db_error)
    }

    async fn due_drafts(
        &mut self,
        now: OffsetDateTime,
        limit: u32,
    ) -> Result<Vec<DraftId>, StoreError> {
        sqlx::query_scalar(
            r#"select id from market_drafts
                where status='approved' and publish_at <= $1
                order by publish_at, created_at, id limit $2"#,
        )
        .bind(now)
        .bind(i64::from(limit))
        .fetch_all(&mut *self.tx)
        .await
        .map(|ids| ids.into_iter().map(DraftId).collect())
        .map_err(db_error)
    }

    async fn due_expiries(
        &mut self,
        now: OffsetDateTime,
        limit: u32,
    ) -> Result<Vec<DraftId>, StoreError> {
        sqlx::query_scalar(
            r#"select id from market_drafts
                where status='pending' and expires_at <= $1
                order by expires_at, id limit $2"#,
        )
        .bind(now)
        .bind(i64::from(limit))
        .fetch_all(&mut *self.tx)
        .await
        .map(|ids| ids.into_iter().map(DraftId).collect())
        .map_err(db_error)
    }

    async fn reserve_market_id(&mut self, draft: DraftId) -> Result<MarketId, StoreError> {
        sqlx::query_scalar(
            r#"update market_drafts
                  set published_market_id=coalesce(published_market_id, gen_random_uuid()), updated_at=now()
                where id=$1 returning published_market_id"#,
        )
        .bind(draft.0)
        .fetch_optional(&mut *self.tx)
        .await
        .map_err(db_error)?
        .map(MarketId)
            .ok_or(StoreError::NotFound("draft"))
    }

    async fn set_market_question(
        &mut self,
        market: MarketId,
        question: &str,
    ) -> Result<(), StoreError> {
        let result = sqlx::query("update markets set question=$2 where id=$1")
            .bind(market.0)
            .bind(question)
            .execute(&mut *self.tx)
            .await
            .map_err(db_error)?;
        if result.rows_affected() == 1 {
            Ok(())
        } else {
            Err(StoreError::NotFound("market"))
        }
    }

    async fn enqueue_artifact_jobs(
        &mut self,
        draft: DraftId,
        market: MarketId,
        kinds: &[ArtifactKind],
        available_at: OffsetDateTime,
    ) -> Result<(), StoreError> {
        for kind in kinds {
            sqlx::query(
                r#"insert into video_jobs
                      (market_id, tier, status, kind, draft_id, available_at, updated_at)
                   values ($1,$2,'queued',$3,$4,$5,$5)
                   on conflict (draft_id, kind) where draft_id is not null do nothing"#,
            )
            .bind(market.0)
            .bind(if *kind == ArtifactKind::MarketVideo {
                "hero"
            } else {
                "template"
            })
            .bind(kind_name(*kind))
            .bind(draft.0)
            .bind(available_at)
            .execute(&mut *self.tx)
            .await
            .map_err(db_error)?;
        }
        Ok(())
    }

    async fn claim_unfilled_slot(&mut self, slot: OffsetDateTime) -> Result<bool, StoreError> {
        let key = format!("content:unfilled:{}", slot.unix_timestamp());
        advisory(&mut self.tx, &key).await?;
        let exists: bool = sqlx::query_scalar(
            r#"select
                 exists(select 1 from events_outbox
                         where event_type='SlotUnfilled' and payload->>'slot_unix'=$1)
                 or exists(select 1 from market_drafts
                           where tier='flash' and publish_at=$2
                             and status in ('approved','published'))"#,
        )
        .bind(slot.unix_timestamp().to_string())
        .bind(slot)
        .fetch_one(&mut *self.tx)
        .await
        .map_err(db_error)?;
        Ok(!exists)
    }

    async fn record_reviewer(&mut self, draft: DraftId, user: UserId) -> Result<(), StoreError> {
        let result = sqlx::query("update market_drafts set reviewed_by=$2 where id=$1")
            .bind(draft.0)
            .bind(user.0)
            .execute(&mut *self.tx)
            .await
            .map_err(db_error)?;
        if result.rows_affected() == 1 {
            Ok(())
        } else {
            Err(StoreError::NotFound("draft"))
        }
    }

    async fn append(&mut self, event: Event) -> Result<(), StoreError> {
        sqlx::query(
            r#"insert into events_outbox (aggregate_type, aggregate_id, event_type, payload)
               values ($1,$2,$3,$4)"#,
        )
        .bind(event.aggregate_type)
        .bind(event.aggregate_id)
        .bind(event.event_type)
        .bind(event.payload)
        .execute(&mut *self.tx)
        .await
        .map_err(db_error)?;
        Ok(())
    }
}

const PUBLICATION_COMMAND_SELECT: &str = r"
    select id, draft_id, idempotency_key, requested_by, status, attempts,
           lease_expires_at, result_market_id, error
      from publication_commands
";

fn publication_status_name(status: application::ports::PublicationCommandStatus) -> &'static str {
    use application::ports::PublicationCommandStatus as S;
    match status {
        S::Pending => "pending",
        S::Executing => "executing",
        S::Done => "done",
        S::Failed => "failed",
    }
}

async fn advisory(
    tx: &mut sqlx::Transaction<'static, sqlx::Postgres>,
    key: &str,
) -> Result<(), StoreError> {
    sqlx::query("select pg_advisory_xact_lock(3, hashtext($1))")
        .bind(key)
        .execute(&mut **tx)
        .await
        .map_err(db_error)?;
    Ok(())
}

const DRAFT_SELECT: &str = r#"
    select id, source, fallback_from, tier, status, publish_stage,
           published_market_id, question, description, video_script, slug,
           seed_micro, fee_bps, min_votes_to_resolve, open_secs,
           hidden_window_secs, publish_at, expires_at, created_at
      from market_drafts
"#;

fn draft_from_row(row: &PgRow) -> Result<DraftRow, StoreError> {
    let fee = u16::try_from(row.try_get::<i32, _>("fee_bps").map_err(db_error)?)
        .map_err(|_| StoreError::Invariant("draft fee out of range"))?;
    let open = u64::try_from(row.try_get::<i64, _>("open_secs").map_err(db_error)?)
        .map_err(|_| StoreError::Invariant("draft open seconds out of range"))?;
    let hidden = u64::try_from(
        row.try_get::<i64, _>("hidden_window_secs")
            .map_err(db_error)?,
    )
    .map_err(|_| StoreError::Invariant("draft hidden seconds out of range"))?;
    let source = parse_source(&row.try_get::<String, _>("source").map_err(db_error)?)?;
    let fallback_from = row
        .try_get::<Option<String>, _>("fallback_from")
        .map_err(db_error)?
        .as_deref()
        .map(parse_source)
        .transpose()?;
    let tier = parse_tier(&row.try_get::<String, _>("tier").map_err(db_error)?)?;
    Ok(DraftRow {
        id: DraftId(row.try_get("id").map_err(db_error)?),
        spec: DraftSpec {
            question: row.try_get("question").map_err(db_error)?,
            description: row.try_get("description").map_err(db_error)?,
            video_script: row.try_get("video_script").map_err(db_error)?,
            slug: row.try_get("slug").map_err(db_error)?,
            tier,
            seed: MicroUsd(row.try_get("seed_micro").map_err(db_error)?),
            fee: BasisPoints(fee),
            min_votes_to_resolve: row.try_get("min_votes_to_resolve").map_err(db_error)?,
            open_secs: open,
            hidden_window_secs: hidden,
        },
        source,
        fallback_from,
        status: parse_status(&row.try_get::<String, _>("status").map_err(db_error)?)?,
        publish_stage: row
            .try_get::<Option<String>, _>("publish_stage")
            .map_err(db_error)?
            .as_deref()
            .map(parse_stage)
            .transpose()?,
        published_market: row
            .try_get::<Option<uuid::Uuid>, _>("published_market_id")
            .map_err(db_error)?
            .map(MarketId),
        publish_at: row.try_get("publish_at").map_err(db_error)?,
        expires_at: row.try_get("expires_at").map_err(db_error)?,
        created_at: row.try_get("created_at").map_err(db_error)?,
    })
}

const fn source_name(value: DraftSource) -> &'static str {
    match value {
        DraftSource::Template => "template",
        DraftSource::Llm => "llm",
    }
}
fn parse_source(value: &str) -> Result<DraftSource, StoreError> {
    match value {
        "template" => Ok(DraftSource::Template),
        "llm" => Ok(DraftSource::Llm),
        _ => Err(StoreError::Invariant("unknown draft source")),
    }
}
const fn tier_name(value: DraftTier) -> &'static str {
    match value {
        DraftTier::Daily => "daily",
        DraftTier::Flash => "flash",
    }
}
fn parse_tier(value: &str) -> Result<DraftTier, StoreError> {
    match value {
        "daily" => Ok(DraftTier::Daily),
        "flash" => Ok(DraftTier::Flash),
        _ => Err(StoreError::Invariant("unknown draft tier")),
    }
}
const fn status_name(value: DraftStatus) -> &'static str {
    match value {
        DraftStatus::Pending => "pending",
        DraftStatus::Approved => "approved",
        DraftStatus::Rejected => "rejected",
        DraftStatus::Published => "published",
        DraftStatus::Expired => "expired",
    }
}
fn parse_status(value: &str) -> Result<DraftStatus, StoreError> {
    match value {
        "pending" => Ok(DraftStatus::Pending),
        "approved" => Ok(DraftStatus::Approved),
        "rejected" => Ok(DraftStatus::Rejected),
        "published" => Ok(DraftStatus::Published),
        "expired" => Ok(DraftStatus::Expired),
        _ => Err(StoreError::Invariant("unknown draft status")),
    }
}
const fn stage_name(value: PublishStage) -> &'static str {
    match value {
        PublishStage::Claimed => "claimed",
        PublishStage::Seeded => "seeded",
        PublishStage::Live => "live",
        PublishStage::JobsEnqueued => "jobs_enqueued",
        PublishStage::Published => "published",
    }
}
fn parse_stage(value: &str) -> Result<PublishStage, StoreError> {
    match value {
        "claimed" => Ok(PublishStage::Claimed),
        "seeded" => Ok(PublishStage::Seeded),
        "live" => Ok(PublishStage::Live),
        "jobs_enqueued" => Ok(PublishStage::JobsEnqueued),
        "published" => Ok(PublishStage::Published),
        _ => Err(StoreError::Invariant("unknown publish stage")),
    }
}
const fn kind_name(value: ArtifactKind) -> &'static str {
    match value {
        ArtifactKind::MarketVideo => "market_video",
        ArtifactKind::Poster => "poster",
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;

    #[test]
    fn content_database_vocabularies_are_total() {
        for value in [DraftSource::Template, DraftSource::Llm] {
            assert_eq!(parse_source(source_name(value)), Ok(value));
        }
        for value in [DraftTier::Daily, DraftTier::Flash] {
            assert_eq!(parse_tier(tier_name(value)), Ok(value));
        }
        for value in [
            DraftStatus::Pending,
            DraftStatus::Approved,
            DraftStatus::Rejected,
            DraftStatus::Published,
            DraftStatus::Expired,
        ] {
            assert_eq!(parse_status(status_name(value)), Ok(value));
        }
        for value in [
            PublishStage::Claimed,
            PublishStage::Seeded,
            PublishStage::Live,
            PublishStage::JobsEnqueued,
            PublishStage::Published,
        ] {
            assert_eq!(parse_stage(stage_name(value)), Ok(value));
        }
        assert_eq!(kind_name(ArtifactKind::MarketVideo), "market_video");
        assert_eq!(kind_name(ArtifactKind::Poster), "poster");
        assert!(parse_source("bad").is_err());
        assert!(parse_tier("bad").is_err());
        assert!(parse_status("bad").is_err());
        assert!(parse_stage("bad").is_err());
        for (status, raw) in [
            (
                application::ports::PublicationCommandStatus::Pending,
                "pending",
            ),
            (
                application::ports::PublicationCommandStatus::Executing,
                "executing",
            ),
            (application::ports::PublicationCommandStatus::Done, "done"),
            (
                application::ports::PublicationCommandStatus::Failed,
                "failed",
            ),
        ] {
            assert_eq!(publication_status_name(status), raw);
        }
    }

    #[tokio::test]
    async fn postgres_passes_content_transaction_contract() {
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL for content contract");
        let store = PgStore::connect(&url).await.unwrap();
        application::contract::content::content_tx_contract(&store).await;
        application::contract::content::curation_saga_contract(&store).await;
    }
}
