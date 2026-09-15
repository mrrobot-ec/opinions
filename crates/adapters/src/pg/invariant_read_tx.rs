//! W2 — D27 invariant snapshot: one `REPEATABLE READ, READ ONLY` transaction
//! with a statement timeout; every identity reads the same MVCC snapshot, so
//! the suite is wholly-before-or-after any concurrent commit.

use application::error::StoreError;
use application::model::{
    AccountBalanceRow, EscrowHistoryRow, MarketId, ReceivableReconRow, TxnSumRow,
    WithdrawalAttributionRow,
};
use application::ports::InvariantReadTx;
use async_trait::async_trait;
use domain::ledger::OwnerType;
use sqlx::{Postgres, Row, Transaction};
use time::OffsetDateTime;
use uuid::Uuid;

use super::rows::db_error;
use super::store::PgStore;

pub(super) struct PgInvariantTx {
    tx: Transaction<'static, Postgres>,
}

pub(super) async fn open(store: &PgStore) -> Result<PgInvariantTx, StoreError> {
    let mut tx = store.pool_handle().begin().await.map_err(db_error)?;
    sqlx::query("set transaction isolation level repeatable read read only")
        .execute(&mut *tx)
        .await
        .map_err(db_error)?;
    sqlx::query("set local statement_timeout = '30s'")
        .execute(&mut *tx)
        .await
        .map_err(db_error)?;
    Ok(PgInvariantTx { tx })
}

fn owner_type_from(raw: &str) -> Result<OwnerType, StoreError> {
    super::rows::owner_type(raw)
}

#[async_trait]
impl InvariantReadTx for PgInvariantTx {
    async fn as_of(&mut self) -> Result<OffsetDateTime, StoreError> {
        sqlx::query_scalar("select now()")
            .fetch_one(&mut *self.tx)
            .await
            .map_err(db_error)
    }

    /// Grouped by `(txn_id, currency)`, matching the `ledger_entries_balanced`
    /// trigger (`migrations/0002_ledger_triggers.sql:6-13`) rather than the
    /// weaker aggregate. A transaction that nets to zero overall while being
    /// non-zero in `usdc` and `usdc_credit` separately is unbalanced, and a
    /// currency-blind `group by txn_id` would report it as clean. One offending
    /// (txn, currency) pair yields one row, so a transaction broken in both
    /// currencies is reported twice — the sweep only asks whether the list is
    /// empty.
    async fn unbalanced_txns(&mut self) -> Result<Vec<TxnSumRow>, StoreError> {
        let rows = sqlx::query(
            "select e.txn_id, sum(e.amount_micro)::bigint as sum_micro
               from ledger_entries e
               join ledger_accounts a on a.id = e.account_id
              group by e.txn_id, a.currency
             having sum(e.amount_micro) <> 0",
        )
        .fetch_all(&mut *self.tx)
        .await
        .map_err(db_error)?;
        Ok(rows
            .iter()
            .map(|row| TxnSumRow {
                txn: row.get("txn_id"),
                sum_micro: row.get("sum_micro"),
            })
            .collect())
    }

    /// Carries `currency` because external mirroring is an identity WITHIN
    /// each currency, not across the aggregate ledger: without it, equal and
    /// opposite `usdc` / `usdc_credit` drift cancels and identity 3 passes over
    /// a broken ledger. `migrations/0002_ledger_triggers.sql` makes that state
    /// uncommittable through the normal write path, but this sweep is the
    /// backstop for the cases that bypass it — schema drift, a restore missing
    /// the trigger, privileged corruption — so it must not be weaker than the
    /// trigger it backstops.
    async fn account_balances(&mut self) -> Result<Vec<AccountBalanceRow>, StoreError> {
        // `sum(bigint)` is `numeric` in PostgreSQL, and the row type is `i128`
        // precisely because one account may aggregate individually valid `i64`
        // entries past the `i64` range. Casting to `::bigint` here would raise
        // inside Postgres on exactly the corrupt ledger this sweep exists to
        // report, so the sum crosses as exact `text` and is parsed into `i128`.
        // A value past `i128` is itself an invariant breach, not a decode bug.
        let rows = sqlx::query(
            "select a.owner_type, a.currency,
                    coalesce(sum(e.amount_micro), 0)::text as balance_micro
               from ledger_accounts a
               left join ledger_entries e on e.account_id = a.id
              group by a.id, a.owner_type, a.currency",
        )
        .fetch_all(&mut *self.tx)
        .await
        .map_err(db_error)?;
        Ok(rows
            .iter()
            .map(|row| {
                Ok::<_, StoreError>(AccountBalanceRow {
                    owner_type: owner_type_from(row.get::<String, _>("owner_type").as_str())?,
                    currency: super::rows::currency(row.get::<String, _>("currency").as_str())?,
                    balance_micro: row
                        .get::<String, _>("balance_micro")
                        .parse::<i128>()
                        .map_err(|_| StoreError::Invariant("account balance exceeds i128"))?,
                })
            })
            .collect::<Result<Vec<_>, _>>()?)
    }

    async fn unpaired_payment_facts(&mut self) -> Result<Vec<Uuid>, StoreError> {
        // Identity 4 extension: legacy admitted rows retain their direct
        // External→User carve-out. Every new observation is exactly
        // External→DepositSuspense, and each terminal leg is exactly either
        // DepositSuspense→User or DepositSuspense→External. Refund payment
        // attribution is source-locked as part of the same fact check.
        let rows = sqlx::query_scalar::<_, Uuid>(
            r#"
            select d.id
              from deposits d
             where d.machine_status is not null
               and d.machine_status <> 'quarantined_legacy'
               and case coalesce(d.machine_status, d.status)
               when 'admitted_legacy' then
                    d.admit_tx_id is null
                 or (select count(*) from ledger_entries e where e.txn_id = d.admit_tx_id) <> 2
                 or coalesce((select sum(e.amount_micro)
                                from ledger_entries e join ledger_accounts a on a.id = e.account_id
                               where e.txn_id = d.admit_tx_id and a.owner_type = 'external'), 0)
                    <> -d.amount_micro
                 or coalesce((select sum(e.amount_micro)
                                from ledger_entries e join ledger_accounts a on a.id = e.account_id
                               where e.txn_id = d.admit_tx_id
                                 and a.owner_type = 'user' and a.owner_id = d.user_id), 0)
                    <> d.amount_micro
               else
                    d.suspense_tx_id is null
                 or (select count(*) from ledger_entries e where e.txn_id = d.suspense_tx_id) <> 2
                 or coalesce((select sum(e.amount_micro)
                                from ledger_entries e join ledger_accounts a on a.id = e.account_id
                               where e.txn_id = d.suspense_tx_id and a.owner_type = 'external'), 0)
                    <> -d.amount_micro
                 or coalesce((select sum(e.amount_micro)
                                from ledger_entries e join ledger_accounts a on a.id = e.account_id
                               where e.txn_id = d.suspense_tx_id
                                 and a.owner_type = 'deposit_suspense'), 0)
                    <> d.amount_micro
                 or case coalesce(d.machine_status, d.status)
                      when 'admitted' then
                           d.admit_tx_id is null or d.refund_tx_id is not null
                        or (select count(*) from ledger_entries e where e.txn_id = d.admit_tx_id) <> 2
                        or coalesce((select sum(e.amount_micro)
                                       from ledger_entries e join ledger_accounts a on a.id = e.account_id
                                      where e.txn_id = d.admit_tx_id
                                        and a.owner_type = 'deposit_suspense'), 0)
                           <> -d.amount_micro
                        or coalesce((select sum(e.amount_micro)
                                       from ledger_entries e join ledger_accounts a on a.id = e.account_id
                                      where e.txn_id = d.admit_tx_id
                                        and a.owner_type = 'user' and a.owner_id = d.user_id), 0)
                           <> d.amount_micro
                      when 'refunded' then
                           d.refund_tx_id is null or d.admit_tx_id is not null
                        or (select count(*) from ledger_entries e where e.txn_id = d.refund_tx_id) <> 2
                        or coalesce((select sum(e.amount_micro)
                                       from ledger_entries e join ledger_accounts a on a.id = e.account_id
                                      where e.txn_id = d.refund_tx_id
                                        and a.owner_type = 'deposit_suspense'), 0)
                           <> -d.amount_micro
                        or coalesce((select sum(e.amount_micro)
                                       from ledger_entries e join ledger_accounts a on a.id = e.account_id
                                      where e.txn_id = d.refund_tx_id
                                        and a.owner_type = 'external'), 0)
                           <> d.amount_micro
                        or not exists (
                            select 1 from outbound_payments p
                             where p.subject = 'deposit_refund'
                               and p.subject_id = d.id
                               and p.dest = d.source_address
                               and p.amount_micro = d.amount_micro
                        )
                      when 'refund_approved' then
                           d.admit_tx_id is not null or d.refund_tx_id is not null
                        or not exists (
                            select 1 from outbound_payments p
                             where p.subject = 'deposit_refund'
                               and p.subject_id = d.id
                               and p.dest = d.source_address
                               and p.amount_micro = d.amount_micro
                        )
                      when 'refund_sending' then
                           d.admit_tx_id is not null or d.refund_tx_id is not null
                        or not exists (
                            select 1 from outbound_payments p
                             where p.subject = 'deposit_refund'
                               and p.subject_id = d.id
                               and p.dest = d.source_address
                               and p.amount_micro = d.amount_micro
                        )
                      else d.admit_tx_id is not null or d.refund_tx_id is not null
                    end
             end
            "#,
        )
        .fetch_all(&mut *self.tx)
        .await
        .map_err(db_error)?;
        Ok(rows)
    }

    async fn escrow_history(&mut self) -> Result<Vec<EscrowHistoryRow>, StoreError> {
        let rows = sqlx::query(
            "select m.id as market_id, m.collateral_at_close_micro,
                    coalesce((select sum(e.amount_micro)
                                from ledger_entries e
                                join ledger_accounts a on a.id = e.account_id
                               where a.owner_type = 'escrow' and a.owner_id = m.id), 0)::bigint
                        as residual_micro
               from markets m
              order by m.id",
        )
        .fetch_all(&mut *self.tx)
        .await
        .map_err(db_error)?;
        Ok(rows
            .iter()
            .map(|row| EscrowHistoryRow {
                market: MarketId(row.get("market_id")),
                residual_micro: row.get("residual_micro"),
                collateral_at_close_micro: row.get("collateral_at_close_micro"),
            })
            .collect())
    }

    async fn duplicate_job_effects(&mut self) -> Result<Vec<String>, StoreError> {
        // Unique indexes make duplicates structurally impossible; the checks
        // still RUN so a dropped index cannot silently void identity 6.
        let mut duplicates: Vec<String> = sqlx::query_scalar::<_, String>(
            "select 'ledger:' || idempotency_key
               from ledger_transactions group by idempotency_key having count(*) > 1",
        )
        .fetch_all(&mut *self.tx)
        .await
        .map_err(db_error)?;
        let commands: Vec<String> = sqlx::query_scalar::<_, String>(
            "select 'publication:' || draft_id::text
               from publication_commands where status in ('pending', 'executing')
              group by draft_id having count(*) > 1",
        )
        .fetch_all(&mut *self.tx)
        .await
        .map_err(db_error)?;
        duplicates.extend(commands);
        Ok(duplicates)
    }

    async fn suspense_liability_micro(&mut self) -> Result<i64, StoreError> {
        let liabilities: i64 = sqlx::query_scalar(
            r#"
            select coalesce(sum(amount_micro), 0)::bigint
              from deposits
             where coalesce(machine_status, status) in (
                 'observed_finalized', 'admission_pending', 'compliance_hold',
                 'refund_approved', 'refund_sending'
             )
            "#,
        )
        .fetch_one(&mut *self.tx)
        .await
        .map_err(db_error)?;
        Ok(liabilities)
    }

    async fn bonus_reserve_and_promise(&mut self) -> Result<(i64, i64), StoreError> {
        let reserve: i64 = sqlx::query_scalar(
            r#"
            select coalesce(sum(e.amount_micro), 0)::bigint
              from ledger_accounts a
              left join ledger_entries e on e.account_id = a.id
             where a.owner_type = 'bonus_reserve' and a.currency = 'usdc'
            "#,
        )
        .fetch_one(&mut *self.tx)
        .await
        .map_err(db_error)?;
        let promised: i64 = sqlx::query_scalar(
            r#"
            select coalesce(sum(amount_micro), 0)::bigint
              from credit_grant_lots
             where grant_class = 'real_money' and converted_at is null
            "#,
        )
        .fetch_one(&mut *self.tx)
        .await
        .map_err(db_error)?;
        Ok((reserve, promised))
    }

    async fn withdrawal_attribution(
        &mut self,
    ) -> Result<Vec<WithdrawalAttributionRow>, StoreError> {
        let rows = sqlx::query(
            r#"
            select w.id,
                   w.amount_micro,
                   (w.status in ('queued', 'risk_hold', 'sent')
                    and w.release_tx_id is null and w.settle_tx_id is null) as active,
                   (
                     w.hold_tx_id is not null
                     and (select count(*) from ledger_entries e
                           where e.txn_id = w.hold_tx_id) = 2
                     and coalesce((select sum(e.amount_micro)
                                     from ledger_entries e
                                     join ledger_accounts a on a.id = e.account_id
                                    where e.txn_id = w.hold_tx_id
                                      and a.owner_type = 'user'
                                      and a.owner_id = w.user_id
                                      and a.currency = 'usdc'), 0) = -w.amount_micro
                     and coalesce((select sum(e.amount_micro)
                                     from ledger_entries e
                                     join ledger_accounts a on a.id = e.account_id
                                    where e.txn_id = w.hold_tx_id
                                      and a.owner_type = 'withheld'
                                      and a.currency = 'usdc'), 0) = w.amount_micro
                   ) as has_hold,
                   (
                     w.release_tx_id is not null
                     and (select count(*) from ledger_entries e
                           where e.txn_id = w.release_tx_id) = 2
                     and coalesce((select sum(e.amount_micro)
                                     from ledger_entries e
                                     join ledger_accounts a on a.id = e.account_id
                                    where e.txn_id = w.release_tx_id
                                      and a.owner_type = 'withheld'
                                      and a.currency = 'usdc'), 0) = -w.amount_micro
                     and coalesce((select sum(e.amount_micro)
                                     from ledger_entries e
                                     join ledger_accounts a on a.id = e.account_id
                                    where e.txn_id = w.release_tx_id
                                      and a.owner_type = 'user'
                                      and a.owner_id = w.user_id
                                      and a.currency = 'usdc'), 0) = w.amount_micro
                   ) as has_release,
                   (
                     w.settle_tx_id is not null
                     and (select count(*) from ledger_entries e
                           where e.txn_id = w.settle_tx_id) = 2
                     and coalesce((select sum(e.amount_micro)
                                     from ledger_entries e
                                     join ledger_accounts a on a.id = e.account_id
                                    where e.txn_id = w.settle_tx_id
                                      and a.owner_type = 'withheld'
                                      and a.currency = 'usdc'), 0) = -w.amount_micro
                     and coalesce((select sum(e.amount_micro)
                                     from ledger_entries e
                                     join ledger_accounts a on a.id = e.account_id
                                    where e.txn_id = w.settle_tx_id
                                      and a.owner_type = 'external'
                                      and a.currency = 'usdc'), 0) = w.amount_micro
                   ) as has_settle,
                   coalesce((
                     select count(*)::bigint
                       from outbound_payments p
                       join outbound_send_attempts a on a.payment_id = p.id
                      where p.subject = 'withdrawal'
                        and p.subject_id = w.id
                        and a.landing_state = 'finalized'
                   ), 0)::bigint as finalized_attempts
              from withdrawals w
             order by w.id
            "#,
        )
        .fetch_all(&mut *self.tx)
        .await
        .map_err(db_error)?;
        rows.iter()
            .map(|row| {
                let finalized: i64 = row.try_get("finalized_attempts").map_err(db_error)?;
                Ok(WithdrawalAttributionRow {
                    withdrawal: row.try_get("id").map_err(db_error)?,
                    amount_micro: row.try_get("amount_micro").map_err(db_error)?,
                    active: row.try_get("active").map_err(db_error)?,
                    has_hold: row.try_get("has_hold").map_err(db_error)?,
                    has_release: row.try_get("has_release").map_err(db_error)?,
                    has_settle: row.try_get("has_settle").map_err(db_error)?,
                    finalized_attempts: u32::try_from(finalized)
                        .map_err(|_| StoreError::Invariant("finalized attempt count overflow"))?,
                })
            })
            .collect()
    }

    async fn receivable_reconciliation(&mut self) -> Result<Vec<ReceivableReconRow>, StoreError> {
        let rows = sqlx::query(
            "select r.origin_reversal_txn_id,
                    sum(r.opened_micro)::bigint as opened_micro,
                    coalesce((select -sum(e.amount_micro)
                                from ledger_entries e
                                join ledger_accounts a on a.id = e.account_id
                               where e.txn_id = r.origin_reversal_txn_id
                                 and a.owner_type = 'house' and e.amount_micro < 0), 0)::bigint
                        as house_shortfall_micro,
                    coalesce(sum((select sum(m.amount_micro)
                                    from receivable_movements m
                                   where m.receivable_id = r.id and m.kind = 'collected')), 0)::bigint
                        as collected_micro,
                    coalesce(sum((select sum(m.amount_micro)
                                    from receivable_movements m
                                   where m.receivable_id = r.id and m.kind = 'written_off')), 0)::bigint
                        as written_off_micro
               from receivables r
              group by r.origin_reversal_txn_id",
        )
        .fetch_all(&mut *self.tx)
        .await
        .map_err(db_error)?;
        Ok(rows
            .iter()
            .map(|row| ReceivableReconRow {
                origin_reversal_txn: row.get("origin_reversal_txn_id"),
                opened_micro: row.get("opened_micro"),
                house_shortfall_micro: row.get("house_shortfall_micro"),
                collected_micro: row.get("collected_micro"),
                written_off_micro: row.get("written_off_micro"),
            })
            .collect())
    }
}
