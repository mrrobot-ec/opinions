//! W3 credit lots / allocations / deposit-machine IO on `PgTx`.

use application::error::{AppError, StoreError};
use application::model::{DepositId, DepositMachineStatus, OwnerRef, UserId};
use application::money::aml::{AmlDirection, AmlLeg, AmlPolicy};
use application::money::credits::lot_ready_to_convert;
use application::money::evaluate_aml;
use application::money::{
    allows_progress, parse_deposit_status, AllocationFact, AllocationKind, ComplianceDecision,
    ConvertCollectReceipt, CreditConversionTxn, CreditIo, CreditLotRow, DepositMachineRow,
    DepositRefundPayment, GrantClass, ObservedDeposit, ReferralPaidMarket,
};
use application::ops::receivable_collection::auto_collect;
use application::ports::{DepositAmlIo, LedgerWriter, ScreenVerdict};
use async_trait::async_trait;
use domain::ledger::{Currency, Entry, TxnKind};
use domain::money::MicroUsd;
use sqlx::Row;
use time::OffsetDateTime;
use uuid::Uuid;

use super::rows::db_error;
use super::store::PgTx;

fn grant_class(raw: &str) -> Result<GrantClass, StoreError> {
    GrantClass::parse(raw)
}

fn allocation_kind(raw: &str) -> Result<AllocationKind, StoreError> {
    AllocationKind::parse(raw)
}

fn credit_lot_from_row(row: &sqlx::postgres::PgRow) -> Result<CreditLotRow, StoreError> {
    Ok(CreditLotRow {
        id: row.try_get("id").map_err(db_error)?,
        user: UserId(row.try_get("user_id").map_err(db_error)?),
        source: row.try_get("source").map_err(db_error)?,
        amount_micro: row.try_get("amount_micro").map_err(db_error)?,
        granted_at: row.try_get("granted_at").map_err(db_error)?,
        grant_class: grant_class(
            row.try_get::<String, _>("grant_class")
                .map_err(db_error)?
                .as_str(),
        )?,
        policy_version: row.try_get("policy_version").map_err(db_error)?,
        converted_at: row.try_get("converted_at").map_err(db_error)?,
    })
}

fn allocation_from_row(row: &sqlx::postgres::PgRow) -> Result<AllocationFact, StoreError> {
    Ok(AllocationFact {
        id: row.try_get("id").map_err(db_error)?,
        trade_id: row.try_get("trade_id").map_err(db_error)?,
        lot_id: row.try_get("lot_id").map_err(db_error)?,
        split_seq: row.try_get("split_seq").map_err(db_error)?,
        amount_micro: row.try_get("amount_micro").map_err(db_error)?,
        kind: allocation_kind(row.try_get::<String, _>("kind").map_err(db_error)?.as_str())?,
        source_allocation_id: row.try_get("source_allocation_id").map_err(db_error)?,
        idempotency_key: row.try_get("idempotency_key").map_err(db_error)?,
    })
}

fn same_allocation(left: &AllocationFact, right: &AllocationFact) -> bool {
    left.trade_id == right.trade_id
        && left.lot_id == right.lot_id
        && left.split_seq == right.split_seq
        && left.amount_micro == right.amount_micro
        && left.kind == right.kind
        && left.source_allocation_id == right.source_allocation_id
}

fn screen_from_row(
    verdict: &str,
    checked: Option<OffsetDateTime>,
    expires: Option<OffsetDateTime>,
    policy: Option<String>,
) -> ScreenVerdict {
    match verdict {
        "hit" => ScreenVerdict::Hit,
        "clear" => match (checked, expires, policy) {
            (Some(checked_at), Some(expires_at), Some(policy_version)) => ScreenVerdict::Clear {
                checked_at,
                expires_at,
                policy_version,
            },
            _ => ScreenVerdict::Indeterminate,
        },
        _ => ScreenVerdict::Indeterminate,
    }
}

fn deposit_machine_from_row(row: &sqlx::postgres::PgRow) -> Result<DepositMachineRow, StoreError> {
    let status = parse_deposit_status(
        row.try_get::<String, _>("status")
            .map_err(db_error)?
            .as_str(),
    )?;
    let source_address: Option<String> = row.try_get("source_address").map_err(db_error)?;
    let dest_address: Option<String> = row.try_get("dest_address").map_err(db_error)?;
    let mint: Option<String> = row.try_get("mint").map_err(db_error)?;
    let rail_fingerprint: Option<String> = row.try_get("rail_fingerprint").map_err(db_error)?;
    let slot: Option<i64> = row.try_get("observed_slot").map_err(db_error)?;
    if status != DepositMachineStatus::AdmittedLegacy
        && (source_address.is_none()
            || dest_address.is_none()
            || mint.is_none()
            || rail_fingerprint.as_deref().is_none_or(str::is_empty)
            || slot.is_none())
    {
        return Err(StoreError::Invariant(
            "machine deposit observation identity incomplete",
        ));
    }
    Ok(DepositMachineRow {
        id: DepositId(row.try_get("id").map_err(db_error)?),
        user: row
            .try_get::<Option<Uuid>, _>("user_id")
            .map_err(db_error)?
            .map(UserId),
        amount: MicroUsd(row.try_get("amount_micro").map_err(db_error)?),
        chain_sig: row.try_get("chain_sig").map_err(db_error)?,
        source_address: source_address.unwrap_or_default(),
        dest_address: dest_address.unwrap_or_default(),
        mint: mint.unwrap_or_default(),
        rail_fingerprint: rail_fingerprint.unwrap_or_default(),
        slot: slot.unwrap_or_default(),
        status,
        suspense_tx_id: row.try_get("suspense_tx_id").map_err(db_error)?,
        admit_tx_id: row.try_get("admit_tx_id").map_err(db_error)?,
        refund_tx_id: row.try_get("refund_tx_id").map_err(db_error)?,
    })
}

/// The D34 AML policy, read from `config_entries`. Every key is REQUIRED and
/// must parse: a missing or non-integer policy key is a typed error, never a
/// permissive default, because the alternative is admitting money under a
/// silently-empty rule set. Bounds mirror the D24 catalog validator.
async fn aml_policy(tx: &mut PgTx) -> Result<AmlPolicy, StoreError> {
    let rows = sqlx::query("select key, value from config_entries")
        .fetch_all(&mut *tx.tx)
        .await
        .map_err(db_error)?;
    let entries: std::collections::HashMap<String, serde_json::Value> = rows
        .into_iter()
        .map(|row| {
            Ok((
                row.try_get("key").map_err(db_error)?,
                row.try_get("value").map_err(db_error)?,
            ))
        })
        .collect::<Result<_, StoreError>>()?;
    let window_hours = super::withdraw_tx::config_i64(&entries, "aml_structuring_window_hours")?;
    let policy = AmlPolicy {
        floor_micro: super::withdraw_tx::config_i64(&entries, "aml_structuring_floor_micro")?,
        threshold_micro: super::withdraw_tx::config_i64(
            &entries,
            "aml_structuring_threshold_micro",
        )?,
        n: super::withdraw_tx::config_i64(&entries, "aml_structuring_n")?,
        window: time::Duration::hours(window_hours),
        deposit_velocity_micro_24h: super::withdraw_tx::config_i64(
            &entries,
            "aml_deposit_velocity_micro_24h",
        )?,
        withdraw_velocity_micro_24h: super::withdraw_tx::config_i64(
            &entries,
            "aml_withdraw_velocity_micro_24h",
        )?,
    };
    if policy.floor_micro <= 0
        || policy.floor_micro >= policy.threshold_micro
        || !(2..=100).contains(&policy.n)
        || !(1..=168).contains(&window_hours)
        || policy.deposit_velocity_micro_24h <= 0
        || policy.withdraw_velocity_micro_24h <= 0
    {
        return Err(StoreError::Invariant("invalid AML policy"));
    }
    Ok(policy)
}

#[async_trait]
impl DepositAmlIo for PgTx {
    /// D34 deposit-side AML, evaluated INSIDE the admission transaction — after
    /// `lock_user`, before any credit — so an open flag holds admission in the
    /// same unit of work rather than after the money has moved.
    ///
    /// Idempotency: the `deposits` row IS the durable candidate binding, so
    /// there is no second place for a candidate to disagree with itself. The
    /// derived leg set EXCLUDES this deposit's own row — it is already
    /// persisted by observation time, and counting it as both history and
    /// candidate would double-count every deposit into the structuring band.
    /// A replay therefore evaluates the identical set, and the `not exists`
    /// guard keeps at most one open flag per (user, rule). A call whose
    /// arguments disagree with the persisted row is a typed conflict, never a
    /// second differently-shaped candidate for the same deposit.
    ///
    /// Returns whether the user has ANY open flag afterwards: the caller holds
    /// admission on `true`, so an unexpected state resolves toward holding.
    async fn evaluate_deposit_aml_candidate(
        &mut self,
        deposit: DepositId,
        user: UserId,
        source_address: &str,
        amount_micro: i64,
        at: OffsetDateTime,
    ) -> Result<bool, StoreError> {
        let bound = sqlx::query(
            "select user_id, coalesce(source_address, '') as source_address, amount_micro
               from deposits where id = $1",
        )
        .bind(deposit.0)
        .fetch_optional(&mut *self.tx)
        .await
        .map_err(db_error)?
        .ok_or(StoreError::NotFound("deposit"))?;
        let bound_user: Option<Uuid> = bound.try_get("user_id").map_err(db_error)?;
        let bound_source: String = bound.try_get("source_address").map_err(db_error)?;
        let bound_amount: i64 = bound.try_get("amount_micro").map_err(db_error)?;
        if bound_user != Some(user.0)
            || bound_source != source_address
            || bound_amount != amount_micro
        {
            return Err(StoreError::Conflict("deposit aml candidate"));
        }

        let policy = aml_policy(self).await?;
        let since = at - policy.window;
        let rows = sqlx::query(
            r"select id, user_id, coalesce(source_address, 'deposit') as dest,
                      amount_micro, created_at as at, 'deposit' as direction
                 from deposits
                where created_at >= $1 and status <> 'refunded' and user_id is not null
                  and id <> $4
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
        .bind(source_address)
        .bind(deposit.0)
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
            id: deposit.0,
            user,
            dest: source_address.to_string(),
            amount_micro,
            at,
            direction: AmlDirection::Deposit,
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
        sqlx::query_scalar(
            "select exists(select 1 from aml_flags where user_id = $1 and status = 'open')",
        )
        .bind(user.0)
        .fetch_one(&mut *self.tx)
        .await
        .map_err(db_error)
    }
}

#[async_trait]
impl CreditIo for PgTx {
    async fn lock_credit_lots(&mut self, user: UserId) -> Result<(), StoreError> {
        sqlx::query("select pg_advisory_xact_lock(6, hashtext($1))")
            .bind(user.0.to_string())
            .execute(&mut *self.tx)
            .await
            .map_err(db_error)?;
        Ok(())
    }

    async fn insert_credit_lot(&mut self, lot: &CreditLotRow) -> Result<Uuid, StoreError> {
        sqlx::query(
            r#"
            insert into credit_grant_lots
                (id, user_id, source, amount_micro, granted_at, grant_class, policy_version)
            values ($1, $2, $3, $4, $5, $6, $7)
            "#,
        )
        .bind(lot.id)
        .bind(lot.user.0)
        .bind(&lot.source)
        .bind(lot.amount_micro)
        .bind(lot.granted_at)
        .bind(lot.grant_class.as_str())
        .bind(&lot.policy_version)
        .execute(&mut *self.tx)
        .await
        .map_err(db_error)?;
        Ok(lot.id)
    }

    async fn lots_for_user(&mut self, user: UserId) -> Result<Vec<CreditLotRow>, StoreError> {
        let rows = sqlx::query(
            r#"
            select id, user_id, source, amount_micro, granted_at, grant_class,
                   policy_version, converted_at
              from credit_grant_lots
             where user_id = $1
             order by granted_at, id
             for no key update
            "#,
        )
        .bind(user.0)
        .fetch_all(&mut *self.tx)
        .await
        .map_err(db_error)?;
        rows.iter().map(credit_lot_from_row).collect()
    }

    async fn lot_by_idempotency(&mut self, key: &str) -> Result<Option<CreditLotRow>, StoreError> {
        let row = sqlx::query(
            r#"
            select id, user_id, source, amount_micro, granted_at, grant_class,
                   policy_version, converted_at
              from credit_grant_lots
             where idempotency_key = $1
            "#,
        )
        .bind(key)
        .fetch_optional(&mut *self.tx)
        .await
        .map_err(db_error)?;
        row.as_ref().map(credit_lot_from_row).transpose()
    }

    async fn remember_lot_idempotency(&mut self, key: &str, lot: Uuid) -> Result<(), StoreError> {
        let result = sqlx::query(
            "update credit_grant_lots set idempotency_key = $2 where id = $1 and idempotency_key is null",
        )
        .bind(lot)
        .bind(key)
        .execute(&mut *self.tx)
        .await;
        match result {
            Ok(result) if result.rows_affected() == 1 => Ok(()),
            Ok(_) => Err(StoreError::Invariant(
                "credit lot idempotency mapping missing",
            )),
            Err(error) if super::rows::unique_violation(&error) => {
                Err(StoreError::Conflict("credit lot idempotency"))
            }
            Err(error) => Err(db_error(error)),
        }
    }

    async fn mark_lot_converted(
        &mut self,
        lot: Uuid,
        at: OffsetDateTime,
    ) -> Result<(), StoreError> {
        let result = sqlx::query(
            "update credit_grant_lots set converted_at = $2 where id = $1 and converted_at is null",
        )
        .bind(lot)
        .bind(at)
        .execute(&mut *self.tx)
        .await
        .map_err(db_error)?;
        if result.rows_affected() == 1 {
            Ok(())
        } else {
            Err(StoreError::Conflict("credit lot conversion"))
        }
    }

    async fn insert_allocation(&mut self, fact: &AllocationFact) -> Result<(), StoreError> {
        let replay = sqlx::query(
            r#"
            select id, trade_id, lot_id, split_seq, amount_micro, kind,
                   source_allocation_id, idempotency_key
              from credit_fee_allocations
             where idempotency_key = $1
            "#,
        )
        .bind(&fact.idempotency_key)
        .fetch_optional(&mut *self.tx)
        .await
        .map_err(db_error)?;
        if let Some(row) = replay {
            let existing = allocation_from_row(&row)?;
            return if same_allocation(&existing, fact) {
                Ok(())
            } else {
                Err(StoreError::Conflict("credit allocation idempotency"))
            };
        }

        match fact.kind {
            AllocationKind::Allocated => {
                if fact.source_allocation_id.is_some() {
                    return Err(StoreError::Invariant(
                        "allocated credit fact names a source",
                    ));
                }
                let fee_micro: i64 =
                    sqlx::query_scalar("select fee_micro from trades where id = $1 for update")
                        .bind(fact.trade_id)
                        .fetch_optional(&mut *self.tx)
                        .await
                        .map_err(db_error)?
                        .ok_or(StoreError::NotFound("trade"))?;
                let trade_progress: i64 = sqlx::query_scalar(
                    r#"
                    select coalesce(sum(
                        case a.kind when 'allocated' then a.amount_micro
                                    when 'reversed' then -a.amount_micro
                                    else 0 end
                    ), 0)::bigint
                      from credit_fee_allocations a
                     where a.trade_id = $1
                    "#,
                )
                .bind(fact.trade_id)
                .fetch_one(&mut *self.tx)
                .await
                .map_err(db_error)?;
                let next_trade = trade_progress
                    .checked_add(fact.amount_micro)
                    .ok_or(StoreError::Invariant("credit allocation overflow"))?;
                if next_trade > fee_micro {
                    return Err(StoreError::Invariant("credit allocation exceeds trade fee"));
                }

                let lot_amount: i64 = sqlx::query_scalar(
                    "select amount_micro from credit_grant_lots where id = $1 for update",
                )
                .bind(fact.lot_id)
                .fetch_optional(&mut *self.tx)
                .await
                .map_err(db_error)?
                .ok_or(StoreError::NotFound("credit lot"))?;
                let lot_progress: i64 = sqlx::query_scalar(
                    r#"
                    select coalesce(sum(
                        case a.kind when 'allocated' then a.amount_micro
                                    when 'reversed' then -a.amount_micro
                                    else 0 end
                    ), 0)::bigint
                      from credit_fee_allocations a
                     where a.lot_id = $1
                    "#,
                )
                .bind(fact.lot_id)
                .fetch_one(&mut *self.tx)
                .await
                .map_err(db_error)?;
                let next_lot = lot_progress
                    .checked_add(fact.amount_micro)
                    .ok_or(StoreError::Invariant("credit allocation overflow"))?;
                if next_lot > lot_amount {
                    return Err(StoreError::Invariant(
                        "credit allocation exceeds lot requirement",
                    ));
                }
            }
            AllocationKind::Finalized | AllocationKind::Reversed => {
                let source_id = fact.source_allocation_id.ok_or(StoreError::Invariant(
                    "terminal credit allocation missing source",
                ))?;
                let source = sqlx::query(
                    r#"
                    select id, trade_id, lot_id, split_seq, amount_micro, kind,
                           source_allocation_id, idempotency_key
                      from credit_fee_allocations
                     where id = $1
                     for update
                    "#,
                )
                .bind(source_id)
                .fetch_optional(&mut *self.tx)
                .await
                .map_err(db_error)?
                .ok_or(StoreError::NotFound("credit allocation source"))?;
                let source = allocation_from_row(&source)?;
                if source.kind != AllocationKind::Allocated
                    || source.trade_id != fact.trade_id
                    || source.lot_id != fact.lot_id
                    || source.split_seq != fact.split_seq
                    || source.amount_micro != fact.amount_micro
                {
                    return Err(StoreError::Invariant(
                        "terminal credit allocation does not exactly move source",
                    ));
                }
                let terminal_exists: bool = sqlx::query_scalar(
                    "select exists(select 1 from credit_fee_allocations where source_allocation_id = $1)",
                )
                .bind(source_id)
                .fetch_one(&mut *self.tx)
                .await
                .map_err(db_error)?;
                if terminal_exists {
                    return Err(StoreError::Conflict("credit allocation terminal"));
                }
            }
        }

        let inserted = sqlx::query(
            r#"
            insert into credit_fee_allocations
                (id, trade_id, lot_id, split_seq, amount_micro, kind,
                 source_allocation_id, idempotency_key)
            values ($1, $2, $3, $4, $5, $6, $7, $8)
            "#,
        )
        .bind(fact.id)
        .bind(fact.trade_id)
        .bind(fact.lot_id)
        .bind(fact.split_seq)
        .bind(fact.amount_micro)
        .bind(fact.kind.as_str())
        .bind(fact.source_allocation_id)
        .bind(&fact.idempotency_key)
        .execute(&mut *self.tx)
        .await;
        match inserted {
            Ok(_) => Ok(()),
            Err(error) if super::rows::unique_violation(&error) => {
                Err(StoreError::Conflict("credit allocation"))
            }
            Err(error) => Err(db_error(error)),
        }
    }

    async fn allocations_for_lot(&mut self, lot: Uuid) -> Result<Vec<AllocationFact>, StoreError> {
        let rows = sqlx::query(
            r#"
            select id, trade_id, lot_id, split_seq, amount_micro, kind,
                   source_allocation_id, idempotency_key
              from credit_fee_allocations
             where lot_id = $1
             order by split_seq, id
             for update
            "#,
        )
        .bind(lot)
        .fetch_all(&mut *self.tx)
        .await
        .map_err(db_error)?;
        rows.iter().map(allocation_from_row).collect()
    }

    async fn live_allocations_for_trade(
        &mut self,
        trade: Uuid,
    ) -> Result<Vec<AllocationFact>, StoreError> {
        let rows = sqlx::query(
            r#"
            select id, trade_id, lot_id, split_seq, amount_micro, kind,
                   source_allocation_id, idempotency_key
              from credit_fee_allocations a
             where a.trade_id = $1
               and a.kind = 'allocated'
               and not exists (
                   select 1
                     from credit_fee_allocations terminal
                    where terminal.source_allocation_id = a.id
               )
             order by a.lot_id, a.split_seq, a.id
             for update of a
            "#,
        )
        .bind(trade)
        .fetch_all(&mut *self.tx)
        .await
        .map_err(db_error)?;
        rows.iter().map(allocation_from_row).collect()
    }

    async fn bonus_reserve_balance(&mut self) -> Result<i64, StoreError> {
        let reserve = self.account(OwnerRef::BonusReserve, Currency::Usdc).await?;
        let locked: Option<Uuid> =
            sqlx::query_scalar("select id from ledger_accounts where id = $1 for update")
                .bind(reserve.0)
                .fetch_optional(&mut *self.tx)
                .await
                .map_err(db_error)?;
        if locked != Some(reserve.0) {
            return Err(StoreError::Invariant("bonus reserve account missing"));
        }
        let bal: Option<i64> = sqlx::query_scalar(
            r#"
            select coalesce(sum(e.amount_micro), 0)::bigint
              from ledger_accounts a
              left join ledger_entries e on e.account_id = a.id
             where a.owner_type = 'bonus_reserve' and a.currency = 'usdc'
            "#,
        )
        .fetch_optional(&mut *self.tx)
        .await
        .map_err(db_error)?;
        Ok(bal.unwrap_or(0))
    }

    async fn remaining_real_money_promise(&mut self) -> Result<i64, StoreError> {
        sqlx::query_scalar(
            r#"
            select coalesce(sum(amount_micro), 0)::bigint
              from credit_grant_lots
             where grant_class = 'real_money' and converted_at is null
            "#,
        )
        .fetch_one(&mut *self.tx)
        .await
        .map_err(db_error)
    }

    async fn bonus_minted_since(&mut self, since: OffsetDateTime) -> Result<i64, StoreError> {
        sqlx::query_scalar(
            r#"
            select coalesce(sum(amount_micro), 0)::bigint
              from credit_grant_lots
             where granted_at >= $1
            "#,
        )
        .bind(since)
        .fetch_one(&mut *self.tx)
        .await
        .map_err(db_error)
    }

    async fn observe_deposit(
        &mut self,
        obs: &ObservedDeposit,
        suspense_tx: Uuid,
        rail_fingerprint: &str,
    ) -> Result<DepositId, StoreError> {
        let id = DepositId(Uuid::new_v4());
        sqlx::query(
            r#"
            insert into deposits
                (id, user_id, chain_sig, amount_micro, status, source_address,
                 dest_address, mint, rail_fingerprint, observed_slot, suspense_tx_id,
                 machine_status)
            values ($1, $2, $3, $4, 'observed_finalized', $5, $6, $7, $8, $9, $10,
                    'observed_finalized')
            "#,
        )
        .bind(id.0)
        .bind(obs.user.map(|u| u.0))
        .bind(&obs.chain_sig)
        .bind(obs.amount.0)
        .bind(&obs.source_address)
        .bind(&obs.dest_address)
        .bind(&obs.mint)
        .bind(rail_fingerprint)
        .bind(obs.slot)
        .bind(suspense_tx)
        .execute(&mut *self.tx)
        .await
        .map_err(db_error)?;
        Ok(id)
    }

    async fn deposit_machine_by_sig(
        &mut self,
        sig: &str,
    ) -> Result<Option<DepositMachineRow>, StoreError> {
        let row = sqlx::query(
            r#"
            select id, user_id, chain_sig, amount_micro, source_address,
                   dest_address, mint, rail_fingerprint, observed_slot,
                   coalesce(machine_status, status) as status,
                   suspense_tx_id, admit_tx_id, refund_tx_id
              from deposits
             where chain_sig = $1
               and machine_status is not null
               and machine_status <> 'quarantined_legacy'
            "#,
        )
        .bind(sig)
        .fetch_optional(&mut *self.tx)
        .await
        .map_err(db_error)?;
        row.as_ref().map(deposit_machine_from_row).transpose()
    }

    async fn deposit_machine_by_id(
        &mut self,
        id: DepositId,
    ) -> Result<Option<DepositMachineRow>, StoreError> {
        let signature: Option<String> = sqlx::query_scalar(
            r#"
            select chain_sig
              from deposits
             where id = $1
               and machine_status is not null
               and machine_status <> 'quarantined_legacy'
            "#,
        )
        .bind(id.0)
        .fetch_optional(&mut *self.tx)
        .await
        .map_err(db_error)?;
        match signature {
            Some(signature) => self.deposit_machine_by_sig(&signature).await,
            None => Ok(None),
        }
    }

    async fn cas_deposit_status(
        &mut self,
        id: DepositId,
        from: DepositMachineStatus,
        to: DepositMachineStatus,
    ) -> Result<bool, StoreError> {
        if !application::credit_deposit::legal_deposit_transition(from, to) {
            return Err(StoreError::Invariant("illegal deposit transition"));
        }
        let result = sqlx::query(
            r#"
            update deposits
               set status = $3, machine_status = $3
             where id = $1
               and coalesce(machine_status, status) = $2
            "#,
        )
        .bind(id.0)
        .bind(application::money::deposit_status_name(from))
        .bind(application::money::deposit_status_name(to))
        .execute(&mut *self.tx)
        .await
        .map_err(db_error)?;
        Ok(result.rows_affected() == 1)
    }

    async fn mark_admitted(&mut self, id: DepositId, admit_tx: Uuid) -> Result<(), StoreError> {
        let result = sqlx::query(
            "update deposits set admit_tx_id = $2 where id = $1 and refund_tx_id is null and admit_tx_id is null",
        )
            .bind(id.0)
            .bind(admit_tx)
            .execute(&mut *self.tx)
            .await
            .map_err(db_error)?;
        if result.rows_affected() == 1 {
            Ok(())
        } else {
            Err(StoreError::Conflict("deposit admission transaction"))
        }
    }

    async fn mark_refunded(&mut self, id: DepositId, refund_tx: Uuid) -> Result<(), StoreError> {
        let result = sqlx::query(
            "update deposits set refund_tx_id = $2 where id = $1 and admit_tx_id is null and refund_tx_id is null",
        )
            .bind(id.0)
            .bind(refund_tx)
            .execute(&mut *self.tx)
            .await
            .map_err(db_error)?;
        if result.rows_affected() == 1 {
            Ok(())
        } else {
            Err(StoreError::Conflict("deposit refund transaction"))
        }
    }

    async fn insert_refund_payment(
        &mut self,
        deposit: DepositId,
        dest: &str,
        amount: i64,
        rail_fp: &str,
    ) -> Result<Uuid, StoreError> {
        let id = Uuid::new_v4();
        sqlx::query(
            r#"
            insert into outbound_payments (id, subject, subject_id, dest, amount_micro, rail_fingerprint)
            values ($1, 'deposit_refund', $2, $3, $4, $5)
            "#,
        )
        .bind(id)
        .bind(deposit.0)
        .bind(dest)
        .bind(amount)
        .bind(rail_fp)
        .execute(&mut *self.tx)
        .await
        .map_err(db_error)?;
        Ok(id)
    }

    async fn refund_payment_for_deposit(
        &mut self,
        deposit: DepositId,
    ) -> Result<Option<DepositRefundPayment>, StoreError> {
        let row = sqlx::query(
            r#"
            select id, subject_id, dest, amount_micro, rail_fingerprint
              from outbound_payments
             where subject = 'deposit_refund' and subject_id = $1
            "#,
        )
        .bind(deposit.0)
        .fetch_optional(&mut *self.tx)
        .await
        .map_err(db_error)?;
        row.map(|row| {
            Ok(DepositRefundPayment {
                id: row.try_get("id").map_err(db_error)?,
                deposit: DepositId(row.try_get("subject_id").map_err(db_error)?),
                dest: row.try_get("dest").map_err(db_error)?,
                amount_micro: row.try_get("amount_micro").map_err(db_error)?,
                rail_fingerprint: row.try_get("rail_fingerprint").map_err(db_error)?,
            })
        })
        .transpose()
    }

    async fn user_status(&mut self, user: UserId) -> Result<String, StoreError> {
        sqlx::query_scalar("select coalesce(status, 'active') from users where id = $1")
            .bind(user.0)
            .fetch_optional(&mut *self.tx)
            .await
            .map_err(db_error)?
            .ok_or(StoreError::NotFound("user"))
    }

    async fn user_kyc_tier(&mut self, user: UserId) -> Result<i32, StoreError> {
        sqlx::query_scalar("select kyc_tier from users where id = $1")
            .bind(user.0)
            .fetch_optional(&mut *self.tx)
            .await
            .map_err(db_error)?
            .ok_or(StoreError::NotFound("user"))
    }

    async fn fresh_clear(
        &mut self,
        user: UserId,
        context: &str,
        now: OffsetDateTime,
    ) -> Result<bool, StoreError> {
        let row = sqlx::query(
            r#"
            select verdict, checked_at, expires_at, policy_version
              from sanction_screenings
             where user_id = $1 and context = $2
             order by checked_at desc
             limit 1
            "#,
        )
        .bind(user.0)
        .bind(context)
        .fetch_optional(&mut *self.tx)
        .await
        .map_err(db_error)?;
        let Some(row) = row else {
            return Ok(false);
        };
        let verdict = screen_from_row(
            row.try_get::<String, _>("verdict")
                .map_err(db_error)?
                .as_str(),
            row.try_get("checked_at").map_err(db_error)?,
            row.try_get("expires_at").map_err(db_error)?,
            row.try_get("policy_version").map_err(db_error)?,
        );
        Ok(allows_progress(&verdict, now))
    }

    async fn self_excluded(
        &mut self,
        user: UserId,
        now: OffsetDateTime,
    ) -> Result<bool, StoreError> {
        let until: Option<OffsetDateTime> = sqlx::query_scalar(
            r#"
            select cooling_off_until
              from self_exclusions
             where user_id = $1 and lifted_at is null
             order by starts_at desc
             limit 1
            "#,
        )
        .bind(user.0)
        .fetch_optional(&mut *self.tx)
        .await
        .map_err(db_error)?;
        Ok(until.is_some_and(|until| until > now))
    }

    async fn deposit_limit_micro(&mut self, user: UserId) -> Result<Option<i64>, StoreError> {
        sqlx::query_scalar(
            r#"
            select case
                     when pending_limit_micro is not null
                      and pending_effective_at is not null
                      and pending_effective_at <= now()
                     then pending_limit_micro
                     else limit_micro
                   end
              from user_deposit_limits
             where user_id = $1
            "#,
        )
        .bind(user.0)
        .fetch_optional(&mut *self.tx)
        .await
        .map_err(db_error)
    }

    /// OPTIONAL feature flags only (`feature_referrals`): absent or non-boolean
    /// means "off", which is the safe direction for a feature that gates
    /// granting money. Never use this for a policy value whose absence must
    /// block a money path — that is [`Self::required_config_flag`].
    async fn config_flag(&mut self, key: &str) -> Result<bool, StoreError> {
        let value: Option<serde_json::Value> =
            sqlx::query_scalar("select value from config_entries where key = $1")
                .bind(key)
                .fetch_optional(&mut *self.tx)
                .await
                .map_err(db_error)?;
        Ok(value.and_then(|v| v.as_bool()).unwrap_or(false))
    }

    /// MANDATORY boolean policy (e.g. `pause_deposits`). A missing row and a
    /// wrong-typed value are BOTH typed errors: mapping either to `false` would
    /// silently un-pause deposits the moment the catalog row went missing or
    /// was written as `"false"` instead of `false`. The two cases are
    /// distinguished in the error text so an operator can tell "never seeded"
    /// from "seeded wrong", but neither is permissive.
    async fn required_config_flag(&mut self, key: &str) -> Result<bool, StoreError> {
        let value: Option<serde_json::Value> =
            sqlx::query_scalar("select value from config_entries where key = $1")
                .bind(key)
                .fetch_optional(&mut *self.tx)
                .await
                .map_err(db_error)?;
        match value {
            None => Err(StoreError::Invariant("required money config missing")),
            Some(value) => value
                .as_bool()
                .ok_or(StoreError::Invariant("required money config malformed")),
        }
    }

    async fn config_i64(&mut self, key: &str) -> Result<Option<i64>, StoreError> {
        let value: Option<serde_json::Value> =
            sqlx::query_scalar("select value from config_entries where key = $1")
                .bind(key)
                .fetch_optional(&mut *self.tx)
                .await
                .map_err(db_error)?;
        Ok(value.and_then(|v| v.as_i64()))
    }

    async fn config_text(&mut self, key: &str) -> Result<Option<String>, StoreError> {
        let value: Option<serde_json::Value> =
            sqlx::query_scalar("select value from config_entries where key = $1")
                .bind(key)
                .fetch_optional(&mut *self.tx)
                .await
                .map_err(db_error)?;
        Ok(value.and_then(|value| value.as_str().map(ToOwned::to_owned)))
    }

    async fn insert_compliance_decision(
        &mut self,
        decision: ComplianceDecision,
    ) -> Result<(), StoreError> {
        sqlx::query(
            r#"
            insert into compliance_decisions
                (id, subject_type, subject_id, kind, actor, at, payload)
            values ($1, $2, $3, $4, $5, $6, $7)
            "#,
        )
        .bind(decision.id)
        .bind(decision.subject_type)
        .bind(decision.subject_id)
        .bind(decision.kind)
        .bind(decision.actor)
        .bind(decision.at)
        .bind(decision.payload)
        .execute(&mut *self.tx)
        .await
        .map_err(db_error)?;
        Ok(())
    }

    async fn phone_verified(
        &mut self,
        user: UserId,
        _now: OffsetDateTime,
    ) -> Result<bool, StoreError> {
        let found: bool = sqlx::query_scalar(
            "select exists(select 1 from phone_verifications where user_id = $1 and verified_at is not null)",
        )
        .bind(user.0)
        .fetch_one(&mut *self.tx)
        .await
        .map_err(db_error)?;
        Ok(found)
    }

    async fn referral_code_for_user(&mut self, user: UserId) -> Result<Option<String>, StoreError> {
        sqlx::query_scalar("select code from referral_codes where user_id = $1")
            .bind(user.0)
            .fetch_optional(&mut *self.tx)
            .await
            .map_err(db_error)
    }

    async fn referral_code_owner(&mut self, code: &str) -> Result<Option<UserId>, StoreError> {
        Ok(
            sqlx::query_scalar::<_, Uuid>("select user_id from referral_codes where code = $1")
                .bind(code)
                .fetch_optional(&mut *self.tx)
                .await
                .map_err(db_error)?
                .map(UserId),
        )
    }

    async fn insert_referral_code(&mut self, user: UserId, code: &str) -> Result<(), StoreError> {
        let inserted =
            sqlx::query("insert into referral_codes (id, user_id, code) values ($1, $2, $3)")
                .bind(Uuid::new_v4())
                .bind(user.0)
                .bind(code)
                .execute(&mut *self.tx)
                .await;
        match inserted {
            Ok(_) => Ok(()),
            Err(error) if super::rows::unique_violation(&error) => {
                Err(StoreError::Conflict("referral code"))
            }
            Err(error) => Err(db_error(error)),
        }
    }

    async fn insert_referral_bind(
        &mut self,
        referrer: UserId,
        referee: UserId,
        bind_key: &str,
    ) -> Result<Uuid, StoreError> {
        let id = Uuid::new_v4();
        let inserted = sqlx::query(
            "insert into referral_binds (id, referrer_id, referee_id, bind_key) values ($1, $2, $3, $4)",
        )
        .bind(id)
        .bind(referrer.0)
        .bind(referee.0)
        .bind(bind_key)
        .execute(&mut *self.tx)
        .await;
        match inserted {
            Ok(_) => Ok(id),
            Err(error) if super::rows::unique_violation(&error) => {
                Err(StoreError::Conflict("referral bind"))
            }
            Err(error) => Err(db_error(error)),
        }
    }

    async fn referral_bind_by_key(&mut self, bind_key: &str) -> Result<Option<Uuid>, StoreError> {
        sqlx::query_scalar("select id from referral_binds where bind_key = $1")
            .bind(bind_key)
            .fetch_optional(&mut *self.tx)
            .await
            .map_err(db_error)
    }

    async fn referral_bind_parties(
        &mut self,
        bind: Uuid,
    ) -> Result<Option<(UserId, UserId, String)>, StoreError> {
        let row = sqlx::query(
            "select referrer_id, referee_id, bind_key from referral_binds where id = $1",
        )
        .bind(bind)
        .fetch_optional(&mut *self.tx)
        .await
        .map_err(db_error)?;
        row.map(|row| {
            Ok((
                UserId(row.try_get("referrer_id").map_err(db_error)?),
                UserId(row.try_get("referee_id").map_err(db_error)?),
                row.try_get("bind_key").map_err(db_error)?,
            ))
        })
        .transpose()
    }

    async fn referral_bind_for_referee(
        &mut self,
        referee: UserId,
    ) -> Result<Option<Uuid>, StoreError> {
        sqlx::query_scalar("select id from referral_binds where referee_id = $1")
            .bind(referee.0)
            .fetch_optional(&mut *self.tx)
            .await
            .map_err(db_error)
    }

    async fn first_paid_market_for_user(
        &mut self,
        user: UserId,
    ) -> Result<Option<ReferralPaidMarket>, StoreError> {
        let row = sqlx::query(
            r#"
            select t.market_id, sum(t.collateral_micro)::bigint as notional_micro
              from trades t
              join markets m on m.id = t.market_id
             where t.user_id = $1 and m.status = 'paid'
             group by t.market_id, m.settled_at
             order by m.settled_at nulls last, t.market_id
             limit 1
            "#,
        )
        .bind(user.0)
        .fetch_optional(&mut *self.tx)
        .await
        .map_err(db_error)?;
        row.map(|row| {
            Ok(ReferralPaidMarket {
                market: application::model::MarketId(row.try_get("market_id").map_err(db_error)?),
                notional_micro: row.try_get("notional_micro").map_err(db_error)?,
            })
        })
        .transpose()
    }

    async fn referral_referees_for_paid_market(
        &mut self,
        market: application::model::MarketId,
    ) -> Result<Vec<UserId>, StoreError> {
        let rows: Vec<Uuid> = sqlx::query_scalar(
            r#"
            select distinct b.referee_id
              from referral_binds b
              join trades t on t.user_id = b.referee_id
             where t.market_id = $1
             order by b.referee_id
            "#,
        )
        .bind(market.0)
        .fetch_all(&mut *self.tx)
        .await
        .map_err(db_error)?;
        Ok(rows.into_iter().map(UserId).collect())
    }

    async fn convert_then_collect(
        &mut self,
        user: UserId,
        now: OffsetDateTime,
        key: &str,
    ) -> Result<ConvertCollectReceipt, AppError> {
        self.lock_credit_lots(user).await?;
        let lots = self.lots_for_user(user).await?;
        let mut converted_micro = 0_i64;
        let mut lots_converted = 0_u32;
        let mut conversion_txns = Vec::new();
        for lot in lots {
            let convert_key = format!("credit-convert:{}", lot.id);
            let pay_key = format!("credit-convert-pay:{}", lot.id);
            let retire_txn = self.txn_by_key(&convert_key).await?;
            let pay_txn = self.txn_by_key(&pay_key).await?;
            match (retire_txn, pay_txn) {
                (Some(retire_txn), Some(pay_txn)) => {
                    conversion_txns.push(CreditConversionTxn {
                        lot_id: lot.id,
                        retire_txn,
                        pay_txn,
                        replayed: true,
                    });
                    continue;
                }
                (None, None) => {}
                _ => {
                    return Err(StoreError::Invariant(
                        "credit conversion has only one ledger transaction",
                    )
                    .into())
                }
            }
            let facts = self.allocations_for_lot(lot.id).await?;
            if !lot_ready_to_convert(&lot, &facts) {
                continue;
            }
            let user_credit = self
                .account(OwnerRef::User(user), Currency::UsdcCredit)
                .await?;
            let house_credit = self.account(OwnerRef::House, Currency::UsdcCredit).await?;
            let reserve = self.account(OwnerRef::BonusReserve, Currency::Usdc).await?;
            let user_cash = self.account(OwnerRef::User(user), Currency::Usdc).await?;
            let retire_txn = self
                .ledger_apply(
                    TxnKind::CreditConvert,
                    &convert_key,
                    &[
                        Entry {
                            account: user_credit,
                            amount: MicroUsd(-lot.amount_micro),
                        },
                        Entry {
                            account: house_credit,
                            amount: MicroUsd(lot.amount_micro),
                        },
                    ],
                )
                .await?;
            let pay_txn = self
                .ledger_apply(
                    TxnKind::CreditConvert,
                    &pay_key,
                    &[
                        Entry {
                            account: reserve,
                            amount: MicroUsd(-lot.amount_micro),
                        },
                        Entry {
                            account: user_cash,
                            amount: MicroUsd(lot.amount_micro),
                        },
                    ],
                )
                .await?;
            self.mark_lot_converted(lot.id, now).await?;
            converted_micro += lot.amount_micro;
            lots_converted += 1;
            conversion_txns.push(CreditConversionTxn {
                lot_id: lot.id,
                retire_txn,
                pay_txn,
                replayed: false,
            });
        }
        let collected_micro =
            auto_collect(self, user, key, &format!("machine:convert:{key}")).await?;
        Ok(ConvertCollectReceipt {
            converted_micro,
            collected_micro,
            lots_converted,
            conversion_txns,
        })
    }
}

#[async_trait]
impl application::ports::OutboundIo for PgTx {
    async fn insert_outbound_payment(
        &mut self,
        payment: &application::ports::OutboundPaymentRow,
    ) -> Result<(), StoreError> {
        super::outbound_tx::insert_payment(&mut self.tx, payment).await
    }

    async fn outbound_by_subject(
        &mut self,
        subject: application::ports::OutboundSubject,
        subject_id: Uuid,
    ) -> Result<Option<application::ports::OutboundPaymentRow>, StoreError> {
        super::outbound_tx::payment_by_subject(&mut self.tx, subject, subject_id).await
    }

    async fn insert_attempt(
        &mut self,
        attempt: &application::ports::OutboundAttemptRow,
    ) -> Result<(), StoreError> {
        super::outbound_tx::insert_attempt(&mut self.tx, attempt).await
    }

    async fn live_attempt(
        &mut self,
        payment_id: Uuid,
    ) -> Result<Option<application::ports::OutboundAttemptRow>, StoreError> {
        sqlx::query("select id from outbound_payments where id = $1 for update")
            .bind(payment_id)
            .fetch_optional(&mut *self.tx)
            .await
            .map_err(db_error)?
            .ok_or(StoreError::NotFound("outbound payment"))?;
        let rows = sqlx::query(
            r#"
            select * from outbound_send_attempts
             where payment_id = $1
               and landing_state in ('prepared','broadcast','unknown')
             order by attempt_number desc
             limit 2
            "#,
        )
        .bind(payment_id)
        .fetch_all(&mut *self.tx)
        .await
        .map_err(db_error)?;
        if rows.len() > 1 {
            return Err(StoreError::Invariant("multiple live outbound attempts"));
        }
        rows.first().map(outbound_attempt_from_row).transpose()
    }

    async fn save_attempt(
        &mut self,
        attempt: &application::ports::OutboundAttemptRow,
    ) -> Result<(), StoreError> {
        super::outbound_tx::save_attempt(&mut self.tx, attempt).await
    }

    async fn attempts_for(
        &mut self,
        payment_id: Uuid,
    ) -> Result<Vec<application::ports::OutboundAttemptRow>, StoreError> {
        let rows = sqlx::query(
            "select * from outbound_send_attempts where payment_id = $1 order by attempt_number",
        )
        .bind(payment_id)
        .fetch_all(&mut *self.tx)
        .await
        .map_err(db_error)?;
        rows.iter().map(outbound_attempt_from_row).collect()
    }
}

fn outbound_attempt_from_row(
    row: &sqlx::postgres::PgRow,
) -> Result<application::ports::OutboundAttemptRow, StoreError> {
    let landing: String = row.try_get("landing_state").map_err(db_error)?;
    Ok(application::ports::OutboundAttemptRow {
        id: row.try_get("id").map_err(db_error)?,
        payment_id: row.try_get("payment_id").map_err(db_error)?,
        attempt_number: row.try_get("attempt_number").map_err(db_error)?,
        replaces_attempt_id: row.try_get("replaces_attempt_id").map_err(db_error)?,
        signed_tx_bytes: row.try_get("signed_tx_bytes").map_err(db_error)?,
        signature: row.try_get("signature").map_err(db_error)?,
        last_valid_block_height: row.try_get("last_valid_block_height").map_err(db_error)?,
        landing_state: super::outbound_tx::parse_landing(&landing)?,
        lease_expires_at: row.try_get("lease_expires_at").map_err(db_error)?,
        evidence: row.try_get("evidence").map_err(db_error)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allocation() -> AllocationFact {
        AllocationFact {
            id: Uuid::new_v4(),
            trade_id: Uuid::new_v4(),
            lot_id: Uuid::new_v4(),
            split_seq: 3,
            amount_micro: 17,
            kind: AllocationKind::Allocated,
            source_allocation_id: None,
            idempotency_key: "allocation-original".into(),
        }
    }

    #[test]
    fn allocation_replay_identity_uses_every_economic_field() {
        let original = allocation();
        let mut replay = original.clone();
        replay.id = Uuid::new_v4();
        replay.idempotency_key = "allocation-replay".into();
        assert!(same_allocation(&original, &replay));

        let mut changed = replay.clone();
        changed.trade_id = Uuid::new_v4();
        assert!(!same_allocation(&original, &changed));
        changed = replay.clone();
        changed.lot_id = Uuid::new_v4();
        assert!(!same_allocation(&original, &changed));
        changed = replay.clone();
        changed.split_seq += 1;
        assert!(!same_allocation(&original, &changed));
        changed = replay.clone();
        changed.amount_micro += 1;
        assert!(!same_allocation(&original, &changed));
        changed = replay.clone();
        changed.kind = AllocationKind::Finalized;
        assert!(!same_allocation(&original, &changed));
        changed = replay;
        changed.source_allocation_id = Some(Uuid::new_v4());
        assert!(!same_allocation(&original, &changed));
    }

    #[test]
    fn screening_rows_require_complete_clear_evidence() {
        let checked_at = OffsetDateTime::UNIX_EPOCH;
        let expires_at = checked_at + time::Duration::hours(1);
        assert_eq!(screen_from_row("hit", None, None, None), ScreenVerdict::Hit);
        assert_eq!(
            screen_from_row(
                "clear",
                Some(checked_at),
                Some(expires_at),
                Some("policy-1".into()),
            ),
            ScreenVerdict::Clear {
                checked_at,
                expires_at,
                policy_version: "policy-1".into(),
            }
        );
        assert_eq!(
            screen_from_row("clear", Some(checked_at), Some(expires_at), None),
            ScreenVerdict::Indeterminate
        );
        assert_eq!(
            screen_from_row("mystery", None, None, None),
            ScreenVerdict::Indeterminate
        );
    }

    async fn machine_row(
        pool: &sqlx::PgPool,
        status: &str,
        complete_identity: bool,
    ) -> Result<sqlx::postgres::PgRow, sqlx::Error> {
        let source = complete_identity.then_some("source-address");
        let dest = complete_identity.then_some("destination-address");
        let mint = complete_identity.then_some("mint-address");
        let rail = complete_identity.then_some("rail-v1");
        let slot = complete_identity.then_some(41_i64);
        sqlx::query(
            r#"
            select $1::uuid as id, $2::uuid as user_id, $3::text as chain_sig,
                   17::bigint as amount_micro, $4::text as source_address,
                   $5::text as dest_address, $6::text as mint,
                   $7::text as rail_fingerprint, $8::bigint as observed_slot,
                   $9::text as status, null::uuid as suspense_tx_id,
                   null::uuid as admit_tx_id, null::uuid as refund_tx_id
            "#,
        )
        .bind(Uuid::new_v4())
        .bind(Uuid::new_v4())
        .bind(format!("sig-{}", Uuid::new_v4()))
        .bind(source)
        .bind(dest)
        .bind(mint)
        .bind(rail)
        .bind(slot)
        .bind(status)
        .fetch_one(pool)
        .await
    }

    #[tokio::test]
    async fn row_decoders_reject_corrupt_credit_and_deposit_shapes(
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let url = std::env::var("DATABASE_URL")?;
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await?;

        let corrupt_lot = sqlx::query(
            r#"
            select $1::uuid as id, $2::uuid as user_id, 'source'::text as source,
                   17::bigint as amount_micro, now() as granted_at,
                   'counterfeit'::text as grant_class, 'policy-1'::text as policy_version,
                   null::timestamptz as converted_at
            "#,
        )
        .bind(Uuid::new_v4())
        .bind(Uuid::new_v4())
        .fetch_one(&pool)
        .await?;
        assert_eq!(
            credit_lot_from_row(&corrupt_lot),
            Err(StoreError::Invariant("unknown grant class"))
        );

        let unknown_status = machine_row(&pool, "invented_status", true).await?;
        assert_eq!(
            deposit_machine_from_row(&unknown_status),
            Err(StoreError::Invariant("unknown deposit machine status"))
        );
        let incomplete = machine_row(&pool, "observed_finalized", false).await?;
        assert_eq!(
            deposit_machine_from_row(&incomplete),
            Err(StoreError::Invariant(
                "machine deposit observation identity incomplete"
            ))
        );

        let legacy =
            deposit_machine_from_row(&machine_row(&pool, "admitted_legacy", false).await?)?;
        assert_eq!(legacy.status, DepositMachineStatus::AdmittedLegacy);
        assert!(legacy.source_address.is_empty());
        assert!(legacy.dest_address.is_empty());
        assert!(legacy.mint.is_empty());
        assert!(legacy.rail_fingerprint.is_empty());
        assert_eq!(legacy.slot, 0);

        pool.close().await;
        Ok(())
    }
}
