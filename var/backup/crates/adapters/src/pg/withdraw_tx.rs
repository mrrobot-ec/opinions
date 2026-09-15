//! Postgres withdrawal unit of work (D31). Standalone of `PgTx` so W1 does
//! not depend on W3-owned `store.rs`.

use application::error::StoreError;
use application::model::{
    AdminAction, Event, NewNotification, ProposalStatus, UserId, WithdrawalReviewState,
    WithdrawalSendState, WithdrawalStatus,
};
use application::money::{evaluate_aml, AmlDirection, AmlLeg, AmlPolicy};
use application::ports::{
    AuditWrite, Combo, Committable, DestWarmth, IdempotencyGuard, MoneyProposal, OpenReceivable,
    OutboundAttemptRow, OutboundIo, OutboundPaymentRow, OutboundSubject, OutboxWriter,
    ScreenVerdict, UserLockGuard, UserMoneyView, WithdrawIo, WithdrawLimits, WithdrawStore,
    WithdrawTx, WithdrawalId, WithdrawalReceipt, WithdrawalRow,
};
use async_trait::async_trait;
use serde_json::Value;
use sqlx::postgres::PgPool;
use sqlx::{PgConnection, Row};
use std::collections::HashMap;
use time::OffsetDateTime;
use uuid::Uuid;

use super::outbound_tx::{
    insert_attempt as insert_attempt_row, insert_payment, parse_landing, payment_by_subject,
    save_attempt as save_attempt_row,
};
use super::rows::db_error;

/// Factory over a pool.
#[derive(Clone)]
pub struct PgWithdrawStore {
    pool: PgPool,
}

impl PgWithdrawStore {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

pub(super) struct PgWithdrawTx {
    tx: sqlx::Transaction<'static, sqlx::Postgres>,
}

impl PgWithdrawTx {
    pub(super) fn new(tx: sqlx::Transaction<'static, sqlx::Postgres>) -> Self {
        Self { tx }
    }
}

#[async_trait]
impl WithdrawStore for PgWithdrawStore {
    async fn withdraw_tx(&self) -> Result<Box<dyn WithdrawTx + '_>, StoreError> {
        let tx = self.pool.begin().await.map_err(db_error)?;
        Ok(Box::new(PgWithdrawTx::new(tx)))
    }

    async fn withdrawal_user(&self, id: WithdrawalId) -> Result<UserId, StoreError> {
        lock_free_withdrawal_user(&self.pool, id).await
    }

    async fn lookup_fingerprint(
        &self,
        fingerprint: &str,
    ) -> Result<Option<WithdrawalReceipt>, StoreError> {
        let payload: Option<Value> = sqlx::query_scalar(
            "select fingerprint::jsonb from request_fingerprints where idempotency_key = $1",
        )
        .bind(format!("withdraw-fp:{fingerprint}"))
        .fetch_optional(&self.pool)
        .await
        .map_err(db_error)?;
        payload.as_ref().map_or(Ok(None), decode_receipt)
    }
}

// `PgStore` is registered by the frozen integration module. The standalone
// adapter contract includes this file directly under `cfg(test)`, where that
// sibling type intentionally does not exist.
#[cfg(not(test))]
#[async_trait]
impl WithdrawStore for super::store::PgStore {
    async fn withdraw_tx(&self) -> Result<Box<dyn WithdrawTx + '_>, StoreError> {
        let tx = self.pool_handle().begin().await.map_err(db_error)?;
        Ok(Box::new(PgWithdrawTx::new(tx)))
    }

    async fn withdrawal_user(&self, id: WithdrawalId) -> Result<UserId, StoreError> {
        lock_free_withdrawal_user(self.pool_handle(), id).await
    }

    async fn lookup_fingerprint(
        &self,
        fingerprint: &str,
    ) -> Result<Option<WithdrawalReceipt>, StoreError> {
        PgWithdrawStore::new(self.pool_handle().clone())
            .lookup_fingerprint(fingerprint)
            .await
    }
}

async fn lock_free_withdrawal_user(pool: &PgPool, id: WithdrawalId) -> Result<UserId, StoreError> {
    sqlx::query_scalar("select user_id from withdrawals where id = $1")
        .bind(id.0)
        .fetch_optional(pool)
        .await
        .map_err(db_error)?
        .map(UserId)
        .ok_or(StoreError::NotFound("withdrawal"))
}

#[async_trait]
impl IdempotencyGuard for PgWithdrawTx {
    async fn serialize_key(&mut self, key: &str) -> Result<(), StoreError> {
        sqlx::query("select pg_advisory_xact_lock(1, hashtext($1))")
            .bind(key)
            .execute(&mut *self.tx)
            .await
            .map_err(db_error)?;
        Ok(())
    }
}

#[async_trait]
impl UserLockGuard for PgWithdrawTx {
    async fn lock_user(&mut self, user: UserId) -> Result<(), StoreError> {
        sqlx::query("select pg_advisory_xact_lock(2, hashtext($1))")
            .bind(user.0.to_string())
            .execute(&mut *self.tx)
            .await
            .map_err(db_error)?;
        Ok(())
    }
}

#[async_trait]
impl OutboxWriter for PgWithdrawTx {
    async fn append(&mut self, event: Event) -> Result<(), StoreError> {
        sqlx::query(
            r"insert into events_outbox (aggregate_type, aggregate_id, event_type, payload)
               values ($1, $2, $3, $4)",
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

    async fn append_batch(&mut self, events: &[Event]) -> Result<(), StoreError> {
        for event in events {
            self.append(event.clone()).await?;
        }
        Ok(())
    }
}

#[async_trait]
impl AuditWrite for PgWithdrawTx {
    async fn audit_insert(&mut self, action: AdminAction) -> Result<(), StoreError> {
        sqlx::query(
            r"insert into admin_actions
                (actor_role, actor_token_digest, action, subject, before, after, reason)
               values ($1,$2,$3,$4,$5,$6,$7)",
        )
        .bind(action.actor_role.name())
        .bind(action.actor_token_digest)
        .bind(action.action)
        .bind(action.subject)
        .bind(action.before)
        .bind(action.after)
        .bind(action.reason)
        .execute(&mut *self.tx)
        .await
        .map_err(db_error)?;
        Ok(())
    }
}

#[async_trait]
impl Committable for PgWithdrawTx {
    async fn commit(self: Box<Self>) -> Result<(), StoreError> {
        self.tx.commit().await.map_err(db_error)
    }
}

#[async_trait]
impl WithdrawIo for PgWithdrawTx {
    async fn withheld_balance(&mut self) -> Result<i64, StoreError> {
        let id = withheld_account(&mut self.tx).await?;
        let bal: i64 = sqlx::query_scalar(
            "select coalesce(sum(amount_micro),0)::bigint from ledger_entries where account_id = $1",
        )
        .bind(id)
        .fetch_one(&mut *self.tx)
        .await
        .map_err(db_error)?;
        if bal < 0 {
            return Err(StoreError::Invariant("withheld account is negative"));
        }
        Ok(bal)
    }

    async fn user_cash(&mut self, user: UserId) -> Result<i64, StoreError> {
        let id = user_account(&mut self.tx, user).await?;
        let bal: i64 = sqlx::query_scalar(
            "select coalesce(sum(amount_micro),0)::bigint from ledger_entries where account_id = $1",
        )
        .bind(id)
        .fetch_one(&mut *self.tx)
        .await
        .map_err(db_error)?;
        if bal < 0 {
            return Err(StoreError::Invariant("user cash account is negative"));
        }
        Ok(bal)
    }

    async fn user_money_view(&mut self, user: UserId) -> Result<UserMoneyView, StoreError> {
        let row = sqlx::query("select status, kyc_tier from users where id = $1")
            .bind(user.0)
            .fetch_optional(&mut *self.tx)
            .await
            .map_err(db_error)?
            .ok_or(StoreError::NotFound("user"))?;
        let status: String = row.try_get("status").map_err(db_error)?;
        let kyc_tier: i32 = row.try_get("kyc_tier").map_err(db_error)?;
        // `cooling_off_until` is only the earliest time a dual-control lift
        // may occur; the exclusion remains active until `lifted_at` is set.
        // `UserMoneyView` carries an until-shaped compatibility field, so map
        // an active row to a stable far-future sentinel rather than silently
        // expiring it when the cooling-off interval elapses.
        let until: Option<OffsetDateTime> = sqlx::query_scalar(
            r"select '9999-12-31 23:59:59+00'::timestamptz
                 from self_exclusions
                where user_id = $1 and lifted_at is null and starts_at <= now()
                order by starts_at desc, id desc limit 1",
        )
        .bind(user.0)
        .fetch_optional(&mut *self.tx)
        .await
        .map_err(db_error)?;
        Ok(UserMoneyView {
            status: application::money::statuses::parse_user_status(&status)?,
            kyc_tier,
            self_excluded_until: until,
        })
    }

    async fn limits(&mut self) -> Result<WithdrawLimits, StoreError> {
        let rows = sqlx::query("select key, value from config_entries")
            .fetch_all(&mut *self.tx)
            .await
            .map_err(db_error)?;
        let entries: HashMap<String, Value> = rows
            .into_iter()
            .map(|row| {
                Ok((
                    row.try_get("key").map_err(db_error)?,
                    row.try_get("value").map_err(db_error)?,
                ))
            })
            .collect::<Result<_, StoreError>>()?;
        let limits = WithdrawLimits {
            min_micro: config_i64(&entries, "withdraw_min_micro")?,
            max_micro: config_i64(&entries, "withdraw_max_micro")?,
            daily_micro: config_i64(&entries, "withdraw_daily_limit_micro")?,
            auto_approve_micro: config_i64(&entries, "withdraw_auto_approve_micro")?,
            dual_control_micro: config_i64(&entries, "withdraw_dual_control_micro")?,
            dest_warm_floor_micro: config_i64(&entries, "dest_warm_floor_micro")?,
            dest_warm_age_hours: config_i64(&entries, "dest_warm_age_hours")?,
            dest_daily_micro: config_i64(&entries, "dest_daily_limit_micro")?,
            hot_daily_micro: config_i64(&entries, "hot_wallet_daily_limit_micro")?,
            withdraw_kyc_tier: config_i64(&entries, "withdraw_kyc_tier")?,
            pause_withdrawals: config_bool(&entries, "pause_withdrawals")?,
            approve_daily_cap_micro: config_i64(&entries, "withdraw_approve_daily_cap_micro")?,
        };
        validate_limits(limits)?;
        Ok(limits)
    }

    async fn persist_screening(
        &mut self,
        user: UserId,
        context: &str,
        verdict: &ScreenVerdict,
    ) -> Result<(), StoreError> {
        let (name, checked, expires, policy) = match verdict {
            ScreenVerdict::Clear {
                checked_at,
                expires_at,
                policy_version,
            } => (
                "clear",
                Some(*checked_at),
                Some(*expires_at),
                Some(policy_version.as_str()),
            ),
            ScreenVerdict::Hit => ("hit", None, None, None),
            ScreenVerdict::Indeterminate => ("indeterminate", None, None, None),
        };
        sqlx::query(
            r"insert into sanction_screenings
                (user_id, context, verdict, checked_at, expires_at, policy_version)
               values ($1,$2,$3,coalesce($4, now()),$5,$6)",
        )
        .bind(user.0)
        .bind(context)
        .bind(name)
        .bind(checked)
        .bind(expires)
        .bind(policy)
        .execute(&mut *self.tx)
        .await
        .map_err(db_error)?;
        Ok(())
    }

    async fn latest_screening(
        &mut self,
        user: UserId,
        context: &str,
    ) -> Result<Option<ScreenVerdict>, StoreError> {
        let row = sqlx::query(
            r"select verdict, expires_at, policy_version, checked_at
                 from sanction_screenings
                where user_id = $1 and context = $2
                order by checked_at desc limit 1",
        )
        .bind(user.0)
        .bind(context)
        .fetch_optional(&mut *self.tx)
        .await
        .map_err(db_error)?;
        Ok(row.map(|row| {
            let verdict: String = row.try_get("verdict").unwrap_or_default();
            match verdict.as_str() {
                "clear" => ScreenVerdict::Clear {
                    checked_at: row
                        .try_get("checked_at")
                        .unwrap_or(OffsetDateTime::UNIX_EPOCH),
                    expires_at: row
                        .try_get("expires_at")
                        .ok()
                        .flatten()
                        .unwrap_or(OffsetDateTime::UNIX_EPOCH),
                    policy_version: row.try_get("policy_version").unwrap_or_default(),
                },
                "hit" => ScreenVerdict::Hit,
                _ => ScreenVerdict::Indeterminate,
            }
        }))
    }

    async fn open_aml_flag_count(&mut self, user: UserId) -> Result<u32, StoreError> {
        // Request-time remote facts are persisted before `lock_user` by D31.
        // Once the caller owns that user lock, materialize a sanctions Hit as
        // the durable account-level compliance hold exactly once.
        sqlx::query(
            r"with latest_fact as (
                 select id, context, verdict
                   from sanction_screenings
                  where user_id = $1 and context = 'withdraw'
                  order by checked_at desc, id desc limit 1
               )
               insert into aml_flags
                 (id, user_id, rule, window_label, evidence, status)
               select gen_random_uuid(), $1, 'sanctions_hit', h.context,
                      jsonb_build_object('screening_id', h.id), 'open'
                 from latest_fact h
                where h.verdict = 'hit'
                  and not exists (
                  select 1 from aml_flags
                   where user_id = $1 and rule = 'sanctions_hit' and status = 'open'
                )",
        )
        .bind(user.0)
        .execute(&mut *self.tx)
        .await
        .map_err(db_error)?;
        let n: i64 = sqlx::query_scalar(
            "select count(*) from aml_flags where user_id = $1 and status = 'open'",
        )
        .bind(user.0)
        .fetch_one(&mut *self.tx)
        .await
        .map_err(db_error)?;
        u32::try_from(n).map_err(|_| StoreError::Invariant("AML flag count overflow"))
    }

    async fn evaluate_withdraw_aml_candidate(
        &mut self,
        id: WithdrawalId,
        user: UserId,
        dest: &str,
        amount_micro: i64,
        at: OffsetDateTime,
    ) -> Result<(), StoreError> {
        let config_rows = sqlx::query("select key, value from config_entries")
            .fetch_all(&mut *self.tx)
            .await
            .map_err(db_error)?;
        let entries: HashMap<String, Value> = config_rows
            .into_iter()
            .map(|row| {
                Ok((
                    row.try_get("key").map_err(db_error)?,
                    row.try_get("value").map_err(db_error)?,
                ))
            })
            .collect::<Result<_, StoreError>>()?;
        let window_hours = config_i64(&entries, "aml_structuring_window_hours")?;
        let floor_micro = config_i64(&entries, "aml_structuring_floor_micro")?;
        let threshold_micro = config_i64(&entries, "aml_structuring_threshold_micro")?;
        let n = config_i64(&entries, "aml_structuring_n")?;
        let deposit_velocity_micro_24h = config_i64(&entries, "aml_deposit_velocity_micro_24h")?;
        let withdraw_velocity_micro_24h = config_i64(&entries, "aml_withdraw_velocity_micro_24h")?;
        if floor_micro <= 0
            || floor_micro >= threshold_micro
            || !(2..=100).contains(&n)
            || !(1..=168).contains(&window_hours)
            || deposit_velocity_micro_24h <= 0
            || withdraw_velocity_micro_24h <= 0
        {
            return Err(StoreError::Invariant("invalid AML policy"));
        }
        let policy = AmlPolicy {
            floor_micro,
            threshold_micro,
            n,
            window: time::Duration::hours(window_hours),
            deposit_velocity_micro_24h,
            withdraw_velocity_micro_24h,
        };
        let since = at - policy.window;
        let rows = sqlx::query(
            r"select id, user_id, coalesce(source_address, 'deposit') as dest,
                      amount_micro, created_at as at, 'deposit' as direction
                 from deposits
                where created_at >= $1 and status <> 'refunded' and user_id is not null
                  and (user_id = $2 or source_address = $3)
               union all
               select id, user_id, dest_address as dest,
                      amount_micro, requested_at as at, 'withdrawal' as direction
                 from withdrawals
                where requested_at >= $1 and status not in ('denied','failed')
                  and (user_id = $2 or dest_address = $3)
               union all
               select id, user_id, 'convert:' || id::text as dest,
                      amount_micro, converted_at as at, 'withdrawal' as direction
                 from credit_grant_lots
                where converted_at is not null and converted_at >= $1 and user_id = $2",
        )
        .bind(since)
        .bind(user.0)
        .bind(dest)
        .fetch_all(&mut *self.tx)
        .await
        .map_err(db_error)?;
        let existing = rows
            .into_iter()
            .map(|row| {
                let direction: String = row.try_get("direction").map_err(db_error)?;
                Ok(AmlLeg {
                    id: row.try_get("id").map_err(db_error)?,
                    user: UserId(row.try_get("user_id").map_err(db_error)?),
                    dest: row.try_get("dest").map_err(db_error)?,
                    amount_micro: row.try_get("amount_micro").map_err(db_error)?,
                    at: row.try_get("at").map_err(db_error)?,
                    direction: if direction == "deposit" {
                        AmlDirection::Deposit
                    } else {
                        AmlDirection::Withdrawal
                    },
                })
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        let candidate = AmlLeg {
            id: id.0,
            user,
            dest: dest.to_string(),
            amount_micro,
            at,
            direction: AmlDirection::Withdrawal,
        };
        let evaluation = evaluate_aml(&existing, &candidate, &policy);
        for kind in evaluation.flags {
            sqlx::query(
                r"insert into aml_flags
                     (id,user_id,rule,window_label,evidence,status,at)
                   select $1,$2,$3,$4,$5,'open',$6
                    where not exists (
                      select 1 from aml_flags
                       where user_id=$2 and rule=$3 and status='open'
                    )",
            )
            .bind(Uuid::new_v4())
            .bind(user.0)
            .bind(kind.as_str())
            .bind(format!("{}h", policy.window.whole_hours()))
            .bind(serde_json::json!({
                "structuring_user": evaluation.structuring_user,
                "structuring_dest": evaluation.structuring_dest,
                "deposit_velocity": evaluation.deposit_velocity,
                "withdraw_velocity": evaluation.withdraw_velocity,
                "leg_id": candidate.id,
            }))
            .bind(at)
            .execute(&mut *self.tx)
            .await
            .map_err(db_error)?;
        }
        Ok(())
    }

    async fn persist_intent(&mut self, receipt: &WithdrawalReceipt) -> Result<(), StoreError> {
        let fingerprint = application::ports::intent_fingerprint(
            receipt.user,
            receipt.amount_micro,
            &receipt.dest,
        );
        let payload = encode_receipt(receipt);
        let inserted = sqlx::query(
            r"insert into request_fingerprints (idempotency_key, fingerprint)
               values ($1, $2)
               on conflict (idempotency_key) do nothing",
        )
        .bind(format!("withdraw-fp:{fingerprint}"))
        .bind(payload.to_string())
        .execute(&mut *self.tx)
        .await
        .map_err(db_error)?;
        if inserted.rows_affected() != 1 {
            return Err(StoreError::Conflict("withdraw intent fingerprint"));
        }
        Ok(())
    }

    async fn lookup_fingerprint_tx(
        &mut self,
        fingerprint: &str,
    ) -> Result<Option<WithdrawalReceipt>, StoreError> {
        let payload: Option<String> = sqlx::query_scalar(
            "select fingerprint from request_fingerprints where idempotency_key = $1",
        )
        .bind(format!("withdraw-fp:{fingerprint}"))
        .fetch_optional(&mut *self.tx)
        .await
        .map_err(db_error)?;
        payload
            .map(|raw| {
                let value = serde_json::from_str(&raw)
                    .map_err(|_| StoreError::Invariant("intent receipt is not JSON"))?;
                decode_receipt(&value)
            })
            .transpose()
            .map(Option::flatten)
    }

    async fn lookup_idempotency(&mut self, key: &str) -> Result<Option<String>, StoreError> {
        sqlx::query_scalar(
            "select fingerprint from request_fingerprints where idempotency_key = $1",
        )
        .bind(key)
        .fetch_optional(&mut *self.tx)
        .await
        .map_err(db_error)
    }

    async fn persist_idempotency(
        &mut self,
        key: &str,
        fingerprint: &str,
    ) -> Result<(), StoreError> {
        let inserted = sqlx::query(
            r"insert into request_fingerprints (idempotency_key, fingerprint)
               values ($1, $2)
               on conflict (idempotency_key) do nothing",
        )
        .bind(key)
        .bind(fingerprint)
        .execute(&mut *self.tx)
        .await
        .map_err(db_error)?;
        if inserted.rows_affected() != 1 {
            let existing: String = sqlx::query_scalar(
                "select fingerprint from request_fingerprints where idempotency_key = $1",
            )
            .bind(key)
            .fetch_one(&mut *self.tx)
            .await
            .map_err(db_error)?;
            if existing != fingerprint {
                return Err(StoreError::Conflict("withdraw idempotency key"));
            }
        }
        Ok(())
    }

    async fn insert_withdrawal(&mut self, row: &WithdrawalRow) -> Result<(), StoreError> {
        sqlx::query(
            r"
            insert into withdrawals
              (id, user_id, dest_address, amount_micro, status, review_state, send_state,
               hold_tx_id, release_tx_id, settle_tx_id, request_fingerprint, risk_reasons,
               requested_at, decided_at, sent_at, settled_at)
            values ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16)
            ",
        )
        .bind(row.id.0)
        .bind(row.user.0)
        .bind(&row.dest)
        .bind(row.amount_micro)
        .bind(status_name(row.combo.status))
        .bind(review_name(row.combo.review))
        .bind(send_name(row.combo.send))
        .bind(row.hold_tx_id)
        .bind(row.release_tx_id)
        .bind(row.settle_tx_id)
        .bind(&row.request_fingerprint)
        .bind(serde_json::json!(row.risk_reasons))
        .bind(row.requested_at)
        .bind(row.decided_at)
        .bind(row.sent_at)
        .bind(row.settled_at)
        .execute(&mut *self.tx)
        .await
        .map_err(db_error)?;
        Ok(())
    }

    async fn withdrawal_for_update(
        &mut self,
        id: WithdrawalId,
    ) -> Result<WithdrawalRow, StoreError> {
        let row = sqlx::query("select * from withdrawals where id = $1 for update")
            .bind(id.0)
            .fetch_optional(&mut *self.tx)
            .await
            .map_err(db_error)?
            .ok_or(StoreError::NotFound("withdrawal"))?;
        row_from_pg(&row)
    }

    async fn cas_withdrawal(
        &mut self,
        id: WithdrawalId,
        expected: Combo,
        next: &WithdrawalRow,
    ) -> Result<bool, StoreError> {
        let result = sqlx::query(
            r"
            update withdrawals
               set status = $2, review_state = $3, send_state = $4,
                   release_tx_id = $5, settle_tx_id = $6, risk_reasons = $7,
                   decided_at = $8, sent_at = $9, settled_at = $10
             where id = $1
               and status = $11 and review_state = $12 and send_state = $13
            ",
        )
        .bind(id.0)
        .bind(status_name(next.combo.status))
        .bind(review_name(next.combo.review))
        .bind(send_name(next.combo.send))
        .bind(next.release_tx_id)
        .bind(next.settle_tx_id)
        .bind(serde_json::json!(next.risk_reasons))
        .bind(next.decided_at)
        .bind(next.sent_at)
        .bind(next.settled_at)
        .bind(status_name(expected.status))
        .bind(review_name(expected.review))
        .bind(send_name(expected.send))
        .execute(&mut *self.tx)
        .await
        .map_err(db_error)?;
        Ok(result.rows_affected() == 1)
    }

    async fn append_withdrawal_event(
        &mut self,
        withdrawal: WithdrawalId,
        kind: &str,
        actor: &str,
        payload: Value,
    ) -> Result<(), StoreError> {
        sqlx::query(
            r"insert into withdrawal_events (withdrawal_id, kind, actor, payload)
               values ($1,$2,$3,$4)",
        )
        .bind(withdrawal.0)
        .bind(kind)
        .bind(actor)
        .bind(payload)
        .execute(&mut *self.tx)
        .await
        .map_err(db_error)?;
        Ok(())
    }

    async fn list_withdrawals(&mut self) -> Result<Vec<WithdrawalRow>, StoreError> {
        let rows = sqlx::query("select * from withdrawals")
            .fetch_all(&mut *self.tx)
            .await
            .map_err(db_error)?;
        rows.iter().map(row_from_pg).collect()
    }

    async fn window_sum_user(
        &mut self,
        user: UserId,
        since: OffsetDateTime,
    ) -> Result<i64, StoreError> {
        window_sum(
            &mut self.tx,
            "user_id = $1 and requested_at >= $2 and status not in ('denied','failed')",
            user.0,
            since,
        )
        .await
    }

    async fn window_sum_dest(
        &mut self,
        dest: &str,
        since: OffsetDateTime,
    ) -> Result<i64, StoreError> {
        let n: i64 = sqlx::query_scalar(
            r"select coalesce(sum(amount_micro),0)::bigint from withdrawals
                where dest_address = $1 and requested_at >= $2
                  and status not in ('denied','failed')",
        )
        .bind(dest)
        .bind(since)
        .fetch_one(&mut *self.tx)
        .await
        .map_err(db_error)?;
        Ok(n)
    }

    async fn window_sum_hot(&mut self, since: OffsetDateTime) -> Result<i64, StoreError> {
        let n: i64 = sqlx::query_scalar(
            r"select coalesce(sum(amount_micro),0)::bigint from withdrawals
                where requested_at >= $1 and status not in ('denied','failed')",
        )
        .bind(since)
        .fetch_one(&mut *self.tx)
        .await
        .map_err(db_error)?;
        Ok(n)
    }

    async fn dest_warmth(&mut self, dest: &str) -> Result<DestWarmth, StoreError> {
        let dust_floor = self.limits().await?.min_micro;
        let settled: i64 = sqlx::query_scalar(
            r"select coalesce(sum(amount_micro),0)::bigint from withdrawals
                where dest_address = $1 and status = 'settled' and amount_micro >= $2",
        )
        .bind(dest)
        .bind(dust_floor)
        .fetch_one(&mut *self.tx)
        .await
        .map_err(db_error)?;
        let first: Option<OffsetDateTime> = sqlx::query_scalar(
            r"select min(settled_at) from withdrawals
                where dest_address = $1 and status = 'settled' and amount_micro >= $2",
        )
        .bind(dest)
        .bind(dust_floor)
        .fetch_one(&mut *self.tx)
        .await
        .map_err(db_error)?;
        let users: i64 = sqlx::query_scalar(
            "select count(distinct user_id) from withdrawals \
              where dest_address = $1 and status = 'settled' and amount_micro >= $2",
        )
        .bind(dest)
        .bind(dust_floor)
        .fetch_one(&mut *self.tx)
        .await
        .map_err(db_error)?;
        let is_refund_dest: bool = sqlx::query_scalar(
            r"select exists(
                   select 1 from outbound_payments
                    where subject = 'deposit_refund' and dest = $1
                   union all
                   select 1 from deposits where source_address = $1
               )",
        )
        .bind(dest)
        .fetch_one(&mut *self.tx)
        .await
        .map_err(db_error)?;
        Ok(DestWarmth {
            settled_micro: settled,
            first_settled_at: first,
            distinct_users: u32::try_from(users)
                .map_err(|_| StoreError::Invariant("destination user count overflow"))?,
            is_refund_dest,
        })
    }

    async fn dest_was_settled_for_user(
        &mut self,
        user: UserId,
        dest: &str,
    ) -> Result<bool, StoreError> {
        let n: i64 = sqlx::query_scalar(
            r"select count(*) from withdrawals
                where user_id = $1 and dest_address = $2 and status = 'settled'",
        )
        .bind(user.0)
        .bind(dest)
        .fetch_one(&mut *self.tx)
        .await
        .map_err(db_error)?;
        Ok(n > 0)
    }

    async fn dest_is_observation_source_for_user(
        &mut self,
        user: UserId,
        dest: &str,
    ) -> Result<bool, StoreError> {
        sqlx::query_scalar(
            r"select exists(
                   select 1 from deposits
                    where user_id = $1 and source_address = $2
                      and machine_status is not null
               )",
        )
        .bind(user.0)
        .bind(dest)
        .fetch_one(&mut *self.tx)
        .await
        .map_err(db_error)
    }

    async fn apply_hold(
        &mut self,
        user: UserId,
        amount_micro: i64,
        key: &str,
    ) -> Result<Uuid, StoreError> {
        let user_account = user_account(&mut self.tx, user).await?;
        let withheld = withheld_account(&mut self.tx).await?;
        two_leg(
            &mut self.tx,
            key,
            "withdrawal",
            user_account,
            -amount_micro,
            withheld,
            amount_micro,
        )
        .await
    }

    async fn apply_release(
        &mut self,
        user: UserId,
        amount_micro: i64,
        key: &str,
    ) -> Result<Uuid, StoreError> {
        let withheld = withheld_account(&mut self.tx).await?;
        let user_account = user_account(&mut self.tx, user).await?;
        two_leg(
            &mut self.tx,
            key,
            "reversal",
            withheld,
            -amount_micro,
            user_account,
            amount_micro,
        )
        .await
    }

    async fn apply_settle(&mut self, amount_micro: i64, key: &str) -> Result<Uuid, StoreError> {
        let withheld = withheld_account(&mut self.tx).await?;
        let external = external_account(&mut self.tx).await?;
        two_leg(
            &mut self.tx,
            key,
            "withdrawal",
            withheld,
            -amount_micro,
            external,
            amount_micro,
        )
        .await
    }

    async fn apply_user_to_house(
        &mut self,
        user: UserId,
        amount_micro: i64,
        key: &str,
    ) -> Result<Uuid, StoreError> {
        let user_account = user_account(&mut self.tx, user).await?;
        let house = house_account(&mut self.tx).await?;
        two_leg(
            &mut self.tx,
            key,
            "reversal",
            user_account,
            -amount_micro,
            house,
            amount_micro,
        )
        .await
    }

    async fn open_receivables(&mut self, user: UserId) -> Result<Vec<OpenReceivable>, StoreError> {
        let rows = sqlx::query(
            r"
            select r.id,
                   (r.opened_micro - coalesce((select sum(m.amount_micro)
                                from receivable_movements m where m.receivable_id = r.id
                                  and m.kind in ('collected','written_off')),0))::bigint as outstanding
              from receivables r
             where r.user_id = $1
             order by r.created_at, r.id
            ",
        )
        .bind(user.0)
        .fetch_all(&mut *self.tx)
        .await
        .map_err(db_error)?;
        rows.into_iter()
            .map(|row| {
                let outstanding: i64 = row.try_get("outstanding").map_err(db_error)?;
                let id = row.try_get("id").map_err(db_error)?;
                Ok((outstanding > 0).then_some(OpenReceivable {
                    id,
                    outstanding_micro: outstanding,
                }))
            })
            .filter_map(Result::transpose)
            .collect()
    }

    async fn insert_receivable_movement(
        &mut self,
        receivable: Uuid,
        amount_micro: i64,
        actor: &str,
        cash_txn: Uuid,
        key: &str,
    ) -> Result<(), StoreError> {
        let inserted = sqlx::query(
            r"insert into receivable_movements
                (id, receivable_id, kind, amount_micro, actor, audit_id, cash_txn_id, idempotency_key)
               select $1,$2,'collected',$3,$4,
                      coalesce(
                        (select a.id from admin_actions a
                          where a.subject = 'receivable:' || $2::text
                          order by a.at desc, a.id desc limit 1),
                        (select a.id from admin_actions a
                          join receivables r on r.id = $2
                         where a.subject = 'market:' || r.market_id::text
                         order by a.at desc, a.id desc limit 1)
                      ),$5,$6
               on conflict (idempotency_key) do nothing",
        )
        .bind(Uuid::new_v4())
        .bind(receivable)
        .bind(amount_micro)
        .bind(actor)
        .bind(cash_txn)
        .bind(key)
        .execute(&mut *self.tx)
        .await
        .map_err(db_error)?;
        if inserted.rows_affected() != 1 {
            return Err(StoreError::Conflict("receivable movement key"));
        }
        Ok(())
    }

    async fn insert_proposal(&mut self, proposal: &MoneyProposal) -> Result<(), StoreError> {
        sqlx::query(
            r"insert into money_command_proposals
                (id, kind, subject_id, payload_hash, proposer_token_id, confirmer_token_id,
                 reason, status, confirm_not_before, expires_at, replay_key)
               values ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)",
        )
        .bind(proposal.id)
        .bind(&proposal.kind)
        .bind(proposal.subject_id)
        .bind(&proposal.payload_hash)
        .bind(&proposal.proposer_token_id)
        .bind(&proposal.confirmer_token_id)
        .bind(&proposal.reason)
        .bind(proposal_status_name(proposal.status))
        .bind(proposal.confirm_not_before)
        .bind(proposal.expires_at)
        .bind(&proposal.replay_key)
        .execute(&mut *self.tx)
        .await
        .map_err(db_error)?;
        Ok(())
    }

    async fn proposal_by_replay(&mut self, key: &str) -> Result<Option<MoneyProposal>, StoreError> {
        load_proposal(
            &mut self.tx,
            "select * from money_command_proposals where replay_key = $1",
            key,
        )
        .await
    }

    async fn open_proposal_for(
        &mut self,
        subject: Uuid,
        kind: &str,
    ) -> Result<Option<MoneyProposal>, StoreError> {
        let row = sqlx::query(
            "select * from money_command_proposals \
              where subject_id = $1 and kind = $2 and status = 'pending'",
        )
        .bind(subject)
        .bind(kind)
        .fetch_optional(&mut *self.tx)
        .await
        .map_err(db_error)?;
        row.as_ref().map(proposal_from_pg).transpose()
    }

    async fn save_proposal(&mut self, proposal: &MoneyProposal) -> Result<(), StoreError> {
        let saved = sqlx::query(
            r"update money_command_proposals
                  set status = $2, confirmer_token_id = $3
                where id = $1 and status = 'pending'",
        )
        .bind(proposal.id)
        .bind(proposal_status_name(proposal.status))
        .bind(&proposal.confirmer_token_id)
        .execute(&mut *self.tx)
        .await
        .map_err(db_error)?;
        if saved.rows_affected() != 1 {
            return Err(StoreError::Conflict("money proposal changed"));
        }
        Ok(())
    }

    async fn finance_approve_sum_since(
        &mut self,
        since: OffsetDateTime,
    ) -> Result<i64, StoreError> {
        let n: i64 = sqlx::query_scalar(
            r"select coalesce(sum((payload->>'amount_micro')::bigint),0)::bigint
                 from withdrawal_events
                where kind = 'finance-approve' and at >= $1",
        )
        .bind(since)
        .fetch_one(&mut *self.tx)
        .await
        .map_err(db_error)?;
        Ok(n)
    }

    async fn record_finance_approve(&mut self, _amount_micro: i64) -> Result<(), StoreError> {
        Err(StoreError::Invariant(
            "finance approval must be recorded against a withdrawal",
        ))
    }

    async fn record_event(&mut self, event: Event) -> Result<i64, StoreError> {
        let seq: i64 = sqlx::query_scalar(
            r"insert into events_outbox (aggregate_type, aggregate_id, event_type, payload)
               values ($1,$2,$3,$4) returning seq",
        )
        .bind(event.aggregate_type)
        .bind(event.aggregate_id)
        .bind(event.event_type)
        .bind(event.payload)
        .fetch_one(&mut *self.tx)
        .await
        .map_err(db_error)?;
        Ok(seq)
    }

    async fn insert_notification(
        &mut self,
        notification: NewNotification,
    ) -> Result<(), StoreError> {
        sqlx::query(
            r"insert into notifications (user_id, type, market_id, payload, source_seq, created_at)
               values ($1,$2,$3,$4,$5,$6)",
        )
        .bind(notification.user.0)
        .bind(notification.notification_type)
        .bind(notification.market.map(|m| m.0))
        .bind(notification.payload)
        .bind(notification.source_seq)
        .bind(notification.created_at)
        .execute(&mut *self.tx)
        .await
        .map_err(db_error)?;
        Ok(())
    }

    async fn lock_cap(&mut self, name: &str) -> Result<(), StoreError> {
        sqlx::query("select pg_advisory_xact_lock(4, hashtext($1))")
            .bind(name)
            .execute(&mut *self.tx)
            .await
            .map_err(db_error)?;
        Ok(())
    }
}

#[async_trait]
impl OutboundIo for PgWithdrawTx {
    async fn insert_outbound_payment(
        &mut self,
        payment: &OutboundPaymentRow,
    ) -> Result<(), StoreError> {
        insert_payment(&mut self.tx, payment).await
    }

    async fn outbound_by_subject(
        &mut self,
        subject: OutboundSubject,
        subject_id: Uuid,
    ) -> Result<Option<OutboundPaymentRow>, StoreError> {
        payment_by_subject(&mut self.tx, subject, subject_id).await
    }

    async fn insert_attempt(&mut self, attempt: &OutboundAttemptRow) -> Result<(), StoreError> {
        insert_attempt_row(&mut self.tx, attempt).await
    }

    async fn live_attempt(
        &mut self,
        payment_id: Uuid,
    ) -> Result<Option<OutboundAttemptRow>, StoreError> {
        sqlx::query("select id from outbound_payments where id = $1 for update")
            .bind(payment_id)
            .fetch_optional(&mut *self.tx)
            .await
            .map_err(db_error)?
            .ok_or(StoreError::NotFound("outbound payment"))?;
        let rows = sqlx::query(
            r"select * from outbound_send_attempts
                where payment_id = $1
                  and landing_state in ('prepared','broadcast','unknown')
                order by attempt_number desc limit 2",
        )
        .bind(payment_id)
        .fetch_all(&mut *self.tx)
        .await
        .map_err(db_error)?;
        if rows.len() > 1 {
            return Err(StoreError::Invariant("multiple live outbound attempts"));
        }
        rows.first().map(attempt_from_pg).transpose()
    }

    async fn save_attempt(&mut self, attempt: &OutboundAttemptRow) -> Result<(), StoreError> {
        save_attempt_row(&mut self.tx, attempt).await
    }

    async fn attempts_for(
        &mut self,
        payment_id: Uuid,
    ) -> Result<Vec<OutboundAttemptRow>, StoreError> {
        let rows = sqlx::query(
            "select * from outbound_send_attempts where payment_id = $1 order by attempt_number",
        )
        .bind(payment_id)
        .fetch_all(&mut *self.tx)
        .await
        .map_err(db_error)?;
        rows.iter().map(attempt_from_pg).collect()
    }
}

pub(super) fn config_i64(entries: &HashMap<String, Value>, key: &str) -> Result<i64, StoreError> {
    let value = entries
        .get(key)
        .ok_or(StoreError::Invariant("withdraw config key missing"))?;
    value
        .as_i64()
        .or_else(|| value.as_str().and_then(|raw| raw.parse().ok()))
        .ok_or_else(|| StoreError::Backend(format!("withdraw config {key} is not an integer")))
}

fn config_bool(entries: &HashMap<String, Value>, key: &str) -> Result<bool, StoreError> {
    let value = entries
        .get(key)
        .ok_or(StoreError::Invariant("withdraw config key missing"))?;
    value
        .as_bool()
        .or_else(|| value.as_str().and_then(|raw| raw.parse().ok()))
        .ok_or_else(|| StoreError::Backend(format!("withdraw config {key} is not a boolean")))
}

fn validate_limits(limits: WithdrawLimits) -> Result<(), StoreError> {
    let auto_valid = limits.auto_approve_micro == 0
        || (limits.min_micro <= limits.auto_approve_micro
            && limits.auto_approve_micro < limits.dest_warm_floor_micro
            && limits.auto_approve_micro <= limits.dual_control_micro);
    let ordered = limits.min_micro > 0
        && auto_valid
        && limits.dual_control_micro <= limits.max_micro
        && limits.max_micro <= limits.dest_daily_micro
        && limits.dest_daily_micro <= limits.daily_micro
        && limits.daily_micro <= limits.hot_daily_micro
        && limits.dest_warm_age_hours > 0
        && (0..=2).contains(&limits.withdraw_kyc_tier)
        && limits.approve_daily_cap_micro > 0;
    if !ordered {
        return Err(StoreError::Invariant("invalid withdrawal limits"));
    }
    Ok(())
}

async fn window_sum(
    tx: &mut PgConnection,
    pred: &str,
    user: Uuid,
    since: OffsetDateTime,
) -> Result<i64, StoreError> {
    let sql = format!("select coalesce(sum(amount_micro),0)::bigint from withdrawals where {pred}");
    sqlx::query_scalar(&sql)
        .bind(user)
        .bind(since)
        .fetch_one(tx)
        .await
        .map_err(db_error)
}

async fn two_leg(
    tx: &mut PgConnection,
    key: &str,
    kind: &str,
    a: Uuid,
    a_amt: i64,
    b: Uuid,
    b_amt: i64,
) -> Result<Uuid, StoreError> {
    let existing: Option<Uuid> =
        sqlx::query_scalar("select id from ledger_transactions where idempotency_key = $1")
            .bind(key)
            .fetch_optional(&mut *tx)
            .await
            .map_err(db_error)?;
    if let Some(_id) = existing {
        return Err(StoreError::DuplicateKey);
    }
    let id = Uuid::new_v4();
    sqlx::query("insert into ledger_transactions (id, kind, idempotency_key) values ($1,$2,$3)")
        .bind(id)
        .bind(kind)
        .bind(key)
        .execute(&mut *tx)
        .await
        .map_err(db_error)?;
    for (account, amount) in [(a, a_amt), (b, b_amt)] {
        sqlx::query(
            "insert into ledger_entries (txn_id, account_id, amount_micro) values ($1,$2,$3)",
        )
        .bind(id)
        .bind(account)
        .bind(amount)
        .execute(&mut *tx)
        .await
        .map_err(db_error)?;
    }
    let _ = existing;
    Ok(id)
}

async fn user_account(tx: &mut PgConnection, user: UserId) -> Result<Uuid, StoreError> {
    sqlx::query(
        r"insert into ledger_accounts (owner_type, owner_id, currency)
           values ('user', $1, 'usdc')
           on conflict (owner_type, owner_id, currency)
             where owner_type in ('user','pool','escrow') do nothing",
    )
    .bind(user.0)
    .execute(&mut *tx)
    .await
    .map_err(db_error)?;
    sqlx::query_scalar(
        "select id from ledger_accounts where owner_type = 'user' and owner_id = $1 and currency = 'usdc'",
    )
    .bind(user.0)
    .fetch_one(tx)
    .await
    .map_err(db_error)
}

async fn singleton(tx: &mut PgConnection, owner: &str) -> Result<Uuid, StoreError> {
    sqlx::query(
        r"insert into ledger_accounts (owner_type, owner_id, currency)
           values ($1, null, 'usdc')
           on conflict (owner_type, currency)
             where owner_type in ('fees','house','withheld','deposit_suspense','bonus_reserve')
             do nothing",
    )
    .bind(owner)
    .execute(&mut *tx)
    .await
    .map_err(db_error)?;
    sqlx::query_scalar("select id from ledger_accounts where owner_type = $1 and currency = 'usdc'")
        .bind(owner)
        .fetch_one(tx)
        .await
        .map_err(db_error)
}

async fn withheld_account(tx: &mut PgConnection) -> Result<Uuid, StoreError> {
    singleton(tx, "withheld").await
}
async fn external_account(tx: &mut PgConnection) -> Result<Uuid, StoreError> {
    sqlx::query(
        r"insert into ledger_accounts (owner_type, owner_id, currency)
           values ('external', null, 'usdc')
           on conflict (currency) where owner_type = 'external' do nothing",
    )
    .execute(&mut *tx)
    .await
    .map_err(db_error)?;
    sqlx::query_scalar(
        "select id from ledger_accounts where owner_type = 'external' and currency = 'usdc'",
    )
    .fetch_one(tx)
    .await
    .map_err(db_error)
}
async fn house_account(tx: &mut PgConnection) -> Result<Uuid, StoreError> {
    singleton(tx, "house").await
}

const fn status_name(status: WithdrawalStatus) -> &'static str {
    match status {
        WithdrawalStatus::Queued => "queued",
        WithdrawalStatus::RiskHold => "risk_hold",
        WithdrawalStatus::Sent => "sent",
        WithdrawalStatus::Settled => "settled",
        WithdrawalStatus::Denied => "denied",
        WithdrawalStatus::Failed => "failed",
    }
}
const fn review_name(review: WithdrawalReviewState) -> &'static str {
    match review {
        WithdrawalReviewState::Screening => "screening",
        WithdrawalReviewState::ReviewRequired => "review_required",
        WithdrawalReviewState::ApprovalProposed => "approval_proposed",
        WithdrawalReviewState::Approved => "approved",
    }
}
const fn send_name(send: WithdrawalSendState) -> &'static str {
    match send {
        WithdrawalSendState::Unsent => "unsent",
        WithdrawalSendState::Sending => "sending",
        WithdrawalSendState::Broadcast => "broadcast",
        WithdrawalSendState::Finalized => "finalized",
        WithdrawalSendState::DefinitiveFailed => "definitive_failed",
        WithdrawalSendState::Unknown => "unknown",
    }
}

fn parse_status(value: &str) -> Result<WithdrawalStatus, StoreError> {
    match value {
        "queued" => Ok(WithdrawalStatus::Queued),
        "risk_hold" => Ok(WithdrawalStatus::RiskHold),
        "sent" => Ok(WithdrawalStatus::Sent),
        "settled" => Ok(WithdrawalStatus::Settled),
        "denied" => Ok(WithdrawalStatus::Denied),
        "failed" => Ok(WithdrawalStatus::Failed),
        _ => Err(StoreError::Invariant("unknown withdrawal status")),
    }
}
fn parse_review(value: &str) -> Result<WithdrawalReviewState, StoreError> {
    match value {
        "screening" => Ok(WithdrawalReviewState::Screening),
        "review_required" => Ok(WithdrawalReviewState::ReviewRequired),
        "approval_proposed" => Ok(WithdrawalReviewState::ApprovalProposed),
        "approved" => Ok(WithdrawalReviewState::Approved),
        _ => Err(StoreError::Invariant("unknown withdrawal review state")),
    }
}
fn parse_send(value: &str) -> Result<WithdrawalSendState, StoreError> {
    match value {
        "unsent" => Ok(WithdrawalSendState::Unsent),
        "sending" => Ok(WithdrawalSendState::Sending),
        "broadcast" => Ok(WithdrawalSendState::Broadcast),
        "finalized" => Ok(WithdrawalSendState::Finalized),
        "definitive_failed" => Ok(WithdrawalSendState::DefinitiveFailed),
        "unknown" => Ok(WithdrawalSendState::Unknown),
        _ => Err(StoreError::Invariant("unknown withdrawal send state")),
    }
}

fn row_from_pg(row: &sqlx::postgres::PgRow) -> Result<WithdrawalRow, StoreError> {
    let status: String = row.try_get("status").map_err(db_error)?;
    let review: String = row.try_get("review_state").map_err(db_error)?;
    let send: String = row.try_get("send_state").map_err(db_error)?;
    let reasons: Value = row.try_get("risk_reasons").map_err(db_error)?;
    let reason_items = reasons.as_array().ok_or(StoreError::Invariant(
        "withdrawal risk reasons are not an array",
    ))?;
    let risk_reasons = reason_items
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(ToOwned::to_owned)
                .ok_or(StoreError::Invariant("withdrawal risk reason is not text"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let combo = checked_combo(&status, &review, &send)?;
    Ok(WithdrawalRow {
        id: WithdrawalId(row.try_get("id").map_err(db_error)?),
        user: UserId(row.try_get("user_id").map_err(db_error)?),
        dest: row.try_get("dest_address").map_err(db_error)?,
        amount_micro: row.try_get("amount_micro").map_err(db_error)?,
        combo,
        hold_tx_id: row.try_get("hold_tx_id").map_err(db_error)?,
        release_tx_id: row.try_get("release_tx_id").map_err(db_error)?,
        settle_tx_id: row.try_get("settle_tx_id").map_err(db_error)?,
        request_fingerprint: row.try_get("request_fingerprint").map_err(db_error)?,
        risk_reasons,
        requested_at: row.try_get("requested_at").map_err(db_error)?,
        decided_at: row.try_get("decided_at").map_err(db_error)?,
        sent_at: row.try_get("sent_at").map_err(db_error)?,
        settled_at: row.try_get("settled_at").map_err(db_error)?,
    })
}

fn checked_combo(status: &str, review: &str, send: &str) -> Result<Combo, StoreError> {
    let combo = Combo::of(
        parse_status(status)?,
        parse_review(review)?,
        parse_send(send)?,
    );
    if !combo.is_legal() {
        return Err(StoreError::Invariant("illegal withdrawal combination"));
    }
    Ok(combo)
}

fn attempt_from_pg(row: &sqlx::postgres::PgRow) -> Result<OutboundAttemptRow, StoreError> {
    let landing: String = row.try_get("landing_state").map_err(db_error)?;
    Ok(OutboundAttemptRow {
        id: row.try_get("id").map_err(db_error)?,
        payment_id: row.try_get("payment_id").map_err(db_error)?,
        attempt_number: row.try_get("attempt_number").map_err(db_error)?,
        replaces_attempt_id: row.try_get("replaces_attempt_id").map_err(db_error)?,
        signed_tx_bytes: row.try_get("signed_tx_bytes").map_err(db_error)?,
        signature: row.try_get("signature").map_err(db_error)?,
        last_valid_block_height: row.try_get("last_valid_block_height").map_err(db_error)?,
        landing_state: parse_landing(&landing)?,
        lease_expires_at: row.try_get("lease_expires_at").map_err(db_error)?,
        evidence: row.try_get("evidence").ok(),
    })
}

fn proposal_status_name(status: ProposalStatus) -> &'static str {
    match status {
        ProposalStatus::Pending => "pending",
        ProposalStatus::Confirmed => "confirmed",
        ProposalStatus::Rejected => "rejected",
        ProposalStatus::Expired => "expired",
    }
}

fn parse_proposal_status(status: &str) -> Result<ProposalStatus, StoreError> {
    match status {
        "pending" => Ok(ProposalStatus::Pending),
        "confirmed" => Ok(ProposalStatus::Confirmed),
        "rejected" => Ok(ProposalStatus::Rejected),
        "expired" => Ok(ProposalStatus::Expired),
        _ => Err(StoreError::Invariant("unknown proposal status")),
    }
}

async fn load_proposal<'q, T>(
    tx: &mut PgConnection,
    sql: &'q str,
    key: T,
) -> Result<Option<MoneyProposal>, StoreError>
where
    T: sqlx::Encode<'q, sqlx::Postgres> + sqlx::Type<sqlx::Postgres> + Send + 'q,
{
    let row = sqlx::query(sql)
        .bind(key)
        .fetch_optional(tx)
        .await
        .map_err(db_error)?;
    row.as_ref().map(proposal_from_pg).transpose()
}

fn proposal_from_pg(row: &sqlx::postgres::PgRow) -> Result<MoneyProposal, StoreError> {
    let status: String = row.try_get("status").map_err(db_error)?;
    Ok(MoneyProposal {
        id: row.try_get("id").map_err(db_error)?,
        kind: row.try_get("kind").map_err(db_error)?,
        subject_id: row.try_get("subject_id").map_err(db_error)?,
        payload_hash: row.try_get("payload_hash").map_err(db_error)?,
        proposer_token_id: row.try_get("proposer_token_id").map_err(db_error)?,
        confirmer_token_id: row.try_get("confirmer_token_id").map_err(db_error)?,
        reason: row.try_get("reason").map_err(db_error)?,
        status: parse_proposal_status(&status)?,
        confirm_not_before: row.try_get("confirm_not_before").map_err(db_error)?,
        expires_at: row.try_get("expires_at").map_err(db_error)?,
        replay_key: row.try_get("replay_key").map_err(db_error)?,
    })
}

fn encode_receipt(receipt: &WithdrawalReceipt) -> Value {
    serde_json::json!({
        "id": receipt.id.map(|id| id.0.to_string()),
        "user": receipt.user.0.to_string(),
        "dest": receipt.dest,
        "amount_micro": receipt.amount_micro,
        "combo": receipt.combo.and_then(Combo::label),
        "refused": receipt.refused,
        "refuse_code": receipt.refuse_code,
        "refuse_message": receipt.refuse_message,
        "hold_tx_id": receipt.hold_tx_id.map(|id| id.to_string()),
    })
}

fn decode_receipt(value: &Value) -> Result<Option<WithdrawalReceipt>, StoreError> {
    let user = value
        .get("user")
        .and_then(Value::as_str)
        .and_then(|raw| Uuid::parse_str(raw).ok())
        .ok_or(StoreError::Invariant("intent receipt user"))?;
    let id = value
        .get("id")
        .and_then(Value::as_str)
        .map(|raw| Uuid::parse_str(raw).map(WithdrawalId))
        .transpose()
        .map_err(|_| StoreError::Invariant("intent receipt id"))?;
    let dest = value
        .get("dest")
        .and_then(Value::as_str)
        .ok_or(StoreError::Invariant("intent receipt dest"))?
        .to_string();
    let amount_micro = value
        .get("amount_micro")
        .and_then(Value::as_i64)
        .ok_or(StoreError::Invariant("intent receipt amount"))?;
    let combo = value
        .get("combo")
        .and_then(Value::as_str)
        .map(parse_combo)
        .transpose()?;
    let hold_tx_id = value
        .get("hold_tx_id")
        .and_then(Value::as_str)
        .map(Uuid::parse_str)
        .transpose()
        .map_err(|_| StoreError::Invariant("intent receipt hold id"))?;
    let refused = value
        .get("refused")
        .and_then(Value::as_bool)
        .ok_or(StoreError::Invariant("intent receipt refusal flag"))?;
    if amount_micro <= 0
        || refused == id.is_some()
        || refused == combo.is_some()
        || refused == hold_tx_id.is_some()
    {
        return Err(StoreError::Invariant("intent receipt shape"));
    }
    Ok(Some(WithdrawalReceipt {
        id,
        user: UserId(user),
        dest,
        amount_micro,
        combo,
        hold_tx_id,
        replayed: false,
        refused,
        refuse_code: value
            .get("refuse_code")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        refuse_message: value
            .get("refuse_message")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
    }))
}

fn parse_combo(label: &str) -> Result<Combo, StoreError> {
    match label {
        "W1" => Ok(Combo::W1),
        "W2" => Ok(Combo::W2),
        "W3" => Ok(Combo::W3),
        "W4" => Ok(Combo::W4),
        "W5" => Ok(Combo::W5),
        "W6" => Ok(Combo::W6),
        "W7" => Ok(Combo::W7),
        "W8" => Ok(Combo::W8),
        "W9" => Ok(Combo::W9),
        "W10" => Ok(Combo::W10),
        "W11" => Ok(Combo::W11),
        "W12" => Ok(Combo::W12),
        "W13" => Ok(Combo::W13),
        "W14" => Ok(Combo::W14),
        "W15" => Ok(Combo::W15),
        _ => Err(StoreError::Invariant("unknown withdrawal combination")),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn combo_names_match_0011() {
        for (status, name) in [
            (WithdrawalStatus::Queued, "queued"),
            (WithdrawalStatus::RiskHold, "risk_hold"),
            (WithdrawalStatus::Sent, "sent"),
            (WithdrawalStatus::Settled, "settled"),
            (WithdrawalStatus::Denied, "denied"),
            (WithdrawalStatus::Failed, "failed"),
        ] {
            assert_eq!(status_name(status), name);
            assert_eq!(parse_status(name).unwrap(), status);
        }
        for (review, name) in [
            (WithdrawalReviewState::Screening, "screening"),
            (WithdrawalReviewState::ReviewRequired, "review_required"),
            (WithdrawalReviewState::ApprovalProposed, "approval_proposed"),
            (WithdrawalReviewState::Approved, "approved"),
        ] {
            assert_eq!(review_name(review), name);
            assert_eq!(parse_review(name).unwrap(), review);
        }
        for (send, name) in [
            (WithdrawalSendState::Unsent, "unsent"),
            (WithdrawalSendState::Sending, "sending"),
            (WithdrawalSendState::Broadcast, "broadcast"),
            (WithdrawalSendState::Finalized, "finalized"),
            (WithdrawalSendState::DefinitiveFailed, "definitive_failed"),
            (WithdrawalSendState::Unknown, "unknown"),
        ] {
            assert_eq!(send_name(send), name);
            assert_eq!(parse_send(name).unwrap(), send);
        }
        assert!(parse_status("mystery").is_err());
        assert!(parse_review("mystery").is_err());
        assert!(parse_send("mystery").is_err());
        assert!(parse_proposal_status("mystery").is_err());
        for (status, name) in [
            (ProposalStatus::Pending, "pending"),
            (ProposalStatus::Confirmed, "confirmed"),
            (ProposalStatus::Rejected, "rejected"),
            (ProposalStatus::Expired, "expired"),
        ] {
            assert_eq!(proposal_status_name(status), name);
            assert_eq!(parse_proposal_status(name).unwrap(), status);
        }

        for combo in [
            Combo::W1,
            Combo::W2,
            Combo::W3,
            Combo::W4,
            Combo::W5,
            Combo::W6,
            Combo::W7,
            Combo::W8,
            Combo::W9,
            Combo::W10,
            Combo::W11,
            Combo::W12,
            Combo::W13,
            Combo::W14,
            Combo::W15,
        ] {
            assert_eq!(parse_combo(combo.label().unwrap()).unwrap(), combo);
        }
        assert!(parse_combo("W16").is_err());
        assert_eq!(
            checked_combo("queued", "screening", "unsent").unwrap(),
            Combo::W1
        );
        assert!(checked_combo("queued", "screening", "sending").is_err());
        assert!(checked_combo("corrupt", "screening", "unsent").is_err());
    }

    #[test]
    fn catalog_helpers_and_cross_key_validation_fail_closed() {
        let mut entries = HashMap::new();
        entries.insert("integer".into(), serde_json::json!(42));
        entries.insert("integer-text".into(), serde_json::json!("43"));
        entries.insert("boolean".into(), serde_json::json!(true));
        entries.insert("boolean-text".into(), serde_json::json!("false"));
        entries.insert("bad".into(), serde_json::json!([]));
        assert_eq!(config_i64(&entries, "integer").unwrap(), 42);
        assert_eq!(config_i64(&entries, "integer-text").unwrap(), 43);
        assert!(config_i64(&entries, "missing").is_err());
        assert!(config_i64(&entries, "bad").is_err());
        assert!(config_bool(&entries, "boolean").unwrap());
        assert!(!config_bool(&entries, "boolean-text").unwrap());
        assert!(config_bool(&entries, "missing").is_err());
        assert!(config_bool(&entries, "bad").is_err());

        let seed = WithdrawLimits::seed();
        assert!(validate_limits(seed).is_ok());
        assert!(validate_limits(WithdrawLimits {
            auto_approve_micro: 0,
            ..seed
        })
        .is_ok());
        for invalid in [
            WithdrawLimits {
                min_micro: 0,
                ..seed
            },
            WithdrawLimits {
                auto_approve_micro: seed.min_micro - 1,
                ..seed
            },
            WithdrawLimits {
                auto_approve_micro: seed.dest_warm_floor_micro,
                ..seed
            },
            WithdrawLimits {
                dual_control_micro: seed.max_micro + 1,
                ..seed
            },
            WithdrawLimits {
                max_micro: seed.dest_daily_micro + 1,
                ..seed
            },
            WithdrawLimits {
                dest_daily_micro: seed.daily_micro + 1,
                ..seed
            },
            WithdrawLimits {
                daily_micro: seed.hot_daily_micro + 1,
                ..seed
            },
            WithdrawLimits {
                dest_warm_age_hours: 0,
                ..seed
            },
            WithdrawLimits {
                withdraw_kyc_tier: 3,
                ..seed
            },
            WithdrawLimits {
                approve_daily_cap_micro: 0,
                ..seed
            },
        ] {
            assert!(validate_limits(invalid).is_err());
        }
    }

    #[test]
    fn receipt_codec_covers_accepted_refused_and_corrupt_shapes() {
        let user = UserId(Uuid::new_v4());
        let accepted = WithdrawalReceipt {
            id: Some(WithdrawalId(Uuid::new_v4())),
            user,
            dest: "dest".into(),
            amount_micro: 5_000_000,
            combo: Some(Combo::W1),
            hold_tx_id: Some(Uuid::new_v4()),
            replayed: false,
            refused: false,
            refuse_code: None,
            refuse_message: None,
        };
        assert_eq!(
            decode_receipt(&encode_receipt(&accepted)).unwrap(),
            Some(accepted)
        );

        let refused = WithdrawalReceipt {
            id: None,
            user,
            dest: "dest".into(),
            amount_micro: 5_000_000,
            combo: None,
            hold_tx_id: None,
            replayed: false,
            refused: true,
            refuse_code: Some("paused".into()),
            refuse_message: Some("paused".into()),
        };
        assert_eq!(
            decode_receipt(&encode_receipt(&refused)).unwrap(),
            Some(refused)
        );

        for corrupt in [
            serde_json::json!({}),
            serde_json::json!({
                "user": user.0, "id": "not-uuid", "dest": "dest", "amount_micro": 1,
                "combo": "W1", "hold_tx_id": Uuid::new_v4(), "refused": false
            }),
            serde_json::json!({
                "user": user.0, "id": Uuid::new_v4(), "dest": "dest", "amount_micro": 1,
                "combo": "W16", "hold_tx_id": Uuid::new_v4(), "refused": false
            }),
            serde_json::json!({
                "user": user.0, "id": Uuid::new_v4(), "dest": "dest", "amount_micro": 1,
                "combo": "W1", "hold_tx_id": "not-uuid", "refused": false
            }),
            serde_json::json!({
                "user": user.0, "id": Uuid::new_v4(), "dest": "dest", "amount_micro": 0,
                "combo": "W1", "hold_tx_id": Uuid::new_v4(), "refused": false
            }),
        ] {
            assert!(decode_receipt(&corrupt).is_err());
        }
    }
}
