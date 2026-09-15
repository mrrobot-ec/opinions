//! W2 — D30 unwind/receivables/manual-ops write surface on the shared
//! `PgTx`. Together with the roles the trade/resolve files already
//! implement, `PgTx` satisfies the `UnwindTx` composition alias.

use application::error::StoreError;
use application::model::{
    MarketId, MarketUnwind, PositionRow, ProposalStatus, Receivable, ReceivableMovement,
    ReceivableMovementKind, UnwindStage, UserId, WithdrawalEligibilityView,
};
use application::ports::{
    LedgerEntryFacts, LedgerTxnFacts, OpsJobCommand, OpsJobStatus, OpsWriteIo,
    ReceivableCollectionIo, ReceivableOutstanding, RemedialCreditProposal, WriteOffProposal,
};
use async_trait::async_trait;
use domain::ledger::TxnKind;
use domain::money::{MicroShares, MicroUsd};
use sqlx::Row;
use time::OffsetDateTime;
use uuid::Uuid;

use super::rows::db_error;
use super::store::{PgStore, PgTx};

fn stage_name(stage: UnwindStage) -> &'static str {
    match stage {
        UnwindStage::Proposed => "proposed",
        UnwindStage::Confirmed => "confirmed",
        UnwindStage::Applied => "applied",
        UnwindStage::Rejected => "rejected",
        UnwindStage::Expired => "expired",
    }
}

fn stage_from(raw: &str) -> UnwindStage {
    match raw {
        "proposed" => UnwindStage::Proposed,
        "confirmed" => UnwindStage::Confirmed,
        "applied" => UnwindStage::Applied,
        "rejected" => UnwindStage::Rejected,
        _ => UnwindStage::Expired,
    }
}

fn status_name(status: ProposalStatus) -> &'static str {
    match status {
        ProposalStatus::Pending => "pending",
        ProposalStatus::Confirmed => "confirmed",
        ProposalStatus::Rejected => "rejected",
        ProposalStatus::Expired => "expired",
    }
}

fn status_from(raw: &str) -> ProposalStatus {
    match raw {
        "pending" => ProposalStatus::Pending,
        "confirmed" => ProposalStatus::Confirmed,
        "rejected" => ProposalStatus::Rejected,
        _ => ProposalStatus::Expired,
    }
}

fn job_status_name(status: OpsJobStatus) -> &'static str {
    match status {
        OpsJobStatus::Pending => "pending",
        OpsJobStatus::Executing => "executing",
        OpsJobStatus::Done => "done",
        OpsJobStatus::Failed => "failed",
    }
}

fn job_status_from(raw: &str) -> OpsJobStatus {
    match raw {
        "pending" => OpsJobStatus::Pending,
        "executing" => OpsJobStatus::Executing,
        "done" => OpsJobStatus::Done,
        _ => OpsJobStatus::Failed,
    }
}

fn kind_from(raw: &str) -> TxnKind {
    match raw {
        "deposit" => TxnKind::Deposit,
        "trade" => TxnKind::Trade,
        "payout" => TxnKind::Payout,
        "withdrawal" => TxnKind::Withdrawal,
        "seed" => TxnKind::Seed,
        "credit_grant" => TxnKind::CreditGrant,
        "credit_convert" => TxnKind::CreditConvert,
        _ => TxnKind::Reversal,
    }
}

fn owner_from(owner_type: &str, owner_id: Option<Uuid>) -> application::model::OwnerRef {
    use application::model::OwnerRef;
    match (owner_type, owner_id) {
        ("user", Some(id)) => OwnerRef::User(UserId(id)),
        ("escrow", Some(id)) => OwnerRef::MarketEscrow(MarketId(id)),
        ("pool", Some(id)) => OwnerRef::MarketPool(MarketId(id)),
        ("fees", _) => OwnerRef::Fees,
        ("house", _) => OwnerRef::House,
        _ => OwnerRef::External,
    }
}

fn unwind_from_row(row: &sqlx::postgres::PgRow) -> MarketUnwind {
    MarketUnwind {
        market: MarketId(row.get("market_id")),
        unwind_key: row.get("unwind_key"),
        stage: stage_from(row.get::<String, _>("stage").as_str()),
        proposer_token_id: row.get("proposer_token_id"),
        confirmer_token_id: row.get("confirmer_token_id"),
        reason: row.get("reason"),
        confirm_not_before: row.get("confirm_not_before"),
        reversal_txn: row.get("reversal_txn_id"),
    }
}

fn write_off_from_row(row: &sqlx::postgres::PgRow) -> WriteOffProposal {
    WriteOffProposal {
        id: row.get("id"),
        receivable: row.get("receivable_id"),
        idempotency_key: row.get("idempotency_key"),
        amount_micro: row.get("amount_micro"),
        proposer_token_id: row.get("proposer_token_id"),
        confirmer_token_id: row.get("confirmer_token_id"),
        reason: row.get("reason"),
        status: status_from(row.get::<String, _>("status").as_str()),
        confirm_not_before: row.get("confirm_not_before"),
    }
}

fn remedial_from_row(row: &sqlx::postgres::PgRow) -> RemedialCreditProposal {
    RemedialCreditProposal {
        id: row.get("id"),
        market: MarketId(row.get("market_id")),
        user: UserId(row.get("user_id")),
        idempotency_key: row.get("idempotency_key"),
        amount_micro: row.get("amount_micro"),
        proposer_token_id: row.get("proposer_token_id"),
        confirmer_token_id: row.get("confirmer_token_id"),
        reason: row.get("reason"),
        status: status_from(row.get::<String, _>("status").as_str()),
        confirm_not_before: row.get("confirm_not_before"),
    }
}

fn job_from_row(row: &sqlx::postgres::PgRow) -> OpsJobCommand {
    OpsJobCommand {
        id: row.get("id"),
        kind: row.get("kind"),
        subject: row.get("subject"),
        idempotency_key: row.get("idempotency_key"),
        requested_by: row.get("requested_by"),
        status: job_status_from(row.get::<String, _>("status").as_str()),
        attempts: row.get("attempts"),
        lease_expires_at: row.get("lease_expires_at"),
        error: row.get("error"),
    }
}

const RECEIVABLE_SELECT: &str = r"
    select r.id, r.market_id, r.user_id, r.origin_reversal_txn_id, r.opened_micro,
           (r.opened_micro
            - coalesce((select sum(m.amount_micro) from receivable_movements m
                         where m.receivable_id = r.id and m.kind in ('collected', 'written_off')), 0))::bigint
               as outstanding_micro
      from receivables r
";

fn receivable_from_row(row: &sqlx::postgres::PgRow) -> ReceivableOutstanding {
    ReceivableOutstanding {
        receivable: Receivable {
            id: row.get("id"),
            market: MarketId(row.get("market_id")),
            user: UserId(row.get("user_id")),
            origin_reversal_txn: row.get("origin_reversal_txn_id"),
            opened_micro: row.get("opened_micro"),
        },
        outstanding_micro: row.get("outstanding_micro"),
    }
}

#[async_trait]
impl ReceivableCollectionIo for PgTx {
    async fn open_receivables_for_user(
        &mut self,
        user: UserId,
    ) -> Result<Vec<ReceivableOutstanding>, StoreError> {
        let sql = format!(
            "{RECEIVABLE_SELECT} where r.user_id = $1
             order by r.created_at, r.id"
        );
        let rows = sqlx::query(&sql)
            .bind(user.0)
            .fetch_all(&mut *self.tx)
            .await
            .map_err(db_error)?;
        Ok(rows
            .iter()
            .map(receivable_from_row)
            .filter(|row| row.outstanding_micro > 0)
            .collect())
    }

    async fn insert_receivable_movement(
        &mut self,
        movement: ReceivableMovement,
    ) -> Result<(), StoreError> {
        let kind = match movement.kind {
            ReceivableMovementKind::Opened => "opened",
            ReceivableMovementKind::Collected => "collected",
            ReceivableMovementKind::WrittenOff => "written_off",
        };
        // The movement's audit anchor: the most recent audit fact for the
        // receivable subject, falling back to the market subject (the unwind
        // confirm that opened the receivable) — visible in THIS transaction.
        let inserted = sqlx::query(
            "insert into receivable_movements
                 (id, receivable_id, kind, amount_micro, actor, audit_id, cash_txn_id, idempotency_key)
             select $1, $2, $3, $4, $5,
                    coalesce(
                        (select a.id from admin_actions a
                          where a.subject = 'receivable:' || $2::text
                          order by a.at desc, a.id desc limit 1),
                        (select a.id from admin_actions a
                          join receivables r on r.id = $2
                         where a.subject = 'market:' || r.market_id::text
                         order by a.at desc, a.id desc limit 1)
                    ),
                    $6, $7
             on conflict (idempotency_key) do nothing",
        )
        .bind(movement.id)
        .bind(movement.receivable)
        .bind(kind)
        .bind(movement.amount_micro)
        .bind(&movement.actor)
        .bind(movement.cash_txn)
        .bind(&movement.idempotency_key)
        .execute(&mut *self.tx)
        .await
        .map_err(db_error)?;
        if inserted.rows_affected() == 0 {
            return Err(StoreError::Conflict("receivable movement key"));
        }
        Ok(())
    }

    async fn account_balance(
        &mut self,
        account: domain::ledger::AccountId,
    ) -> Result<MicroUsd, StoreError> {
        let balance: i64 = sqlx::query_scalar(
            "select coalesce(sum(amount_micro), 0)::bigint
               from ledger_entries where account_id = $1",
        )
        .bind(account.0)
        .fetch_one(&mut *self.tx)
        .await
        .map_err(db_error)?;
        Ok(MicroUsd(balance))
    }
}

#[async_trait]
#[allow(clippy::too_many_lines)]
impl OpsWriteIo for PgTx {
    async fn unwind_for_update(
        &mut self,
        market: MarketId,
    ) -> Result<Option<MarketUnwind>, StoreError> {
        let row = sqlx::query(
            "select market_id, unwind_key, stage, proposer_token_id, confirmer_token_id,
                    reason, confirm_not_before, reversal_txn_id
               from market_unwinds where market_id = $1 for update",
        )
        .bind(market.0)
        .fetch_optional(&mut *self.tx)
        .await
        .map_err(db_error)?;
        Ok(row.as_ref().map(unwind_from_row))
    }

    async fn insert_unwind(&mut self, unwind: MarketUnwind) -> Result<(), StoreError> {
        sqlx::query(
            "insert into market_unwinds
                 (market_id, unwind_key, stage, proposer_token_id, reason, confirm_not_before)
             values ($1, $2, $3, $4, $5, $6)",
        )
        .bind(unwind.market.0)
        .bind(&unwind.unwind_key)
        .bind(stage_name(unwind.stage))
        .bind(&unwind.proposer_token_id)
        .bind(&unwind.reason)
        .bind(unwind.confirm_not_before)
        .execute(&mut *self.tx)
        .await
        .map_err(|error| match error {
            sqlx::Error::Database(db) if db.is_unique_violation() => {
                StoreError::Conflict("market unwind")
            }
            other => db_error(other),
        })?;
        Ok(())
    }

    async fn save_unwind(&mut self, unwind: &MarketUnwind) -> Result<(), StoreError> {
        let updated = sqlx::query(
            "update market_unwinds
                set stage = $2, confirmer_token_id = $3, reversal_txn_id = $4,
                    settled_at = case when $2 in ('applied', 'rejected', 'expired')
                                      then now() else settled_at end
              where market_id = $1",
        )
        .bind(unwind.market.0)
        .bind(stage_name(unwind.stage))
        .bind(&unwind.confirmer_token_id)
        .bind(unwind.reversal_txn)
        .execute(&mut *self.tx)
        .await
        .map_err(db_error)?;
        if updated.rows_affected() == 0 {
            return Err(StoreError::Invariant("save for unknown unwind"));
        }
        Ok(())
    }

    async fn record_reversal(
        &mut self,
        reversed_entry: Uuid,
        reversal_txn: Uuid,
        market: MarketId,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "insert into ledger_entry_reversals (reversed_entry_id, reversal_txn_id, market_id)
             values ($1, $2, $3)",
        )
        .bind(reversed_entry)
        .bind(reversal_txn)
        .bind(market.0)
        .execute(&mut *self.tx)
        .await
        .map_err(|error| match error {
            sqlx::Error::Database(db) if db.is_unique_violation() => {
                StoreError::Conflict("reversal lineage")
            }
            other => db_error(other),
        })?;
        Ok(())
    }

    async fn insert_receivable(&mut self, receivable: Receivable) -> Result<(), StoreError> {
        sqlx::query(
            "insert into receivables (id, market_id, user_id, origin_reversal_txn_id, opened_micro)
             values ($1, $2, $3, $4, $5)",
        )
        .bind(receivable.id)
        .bind(receivable.market.0)
        .bind(receivable.user.0)
        .bind(receivable.origin_reversal_txn)
        .bind(receivable.opened_micro)
        .execute(&mut *self.tx)
        .await
        .map_err(|error| match error {
            sqlx::Error::Database(db) if db.is_unique_violation() => {
                StoreError::Conflict("receivable")
            }
            other => db_error(other),
        })?;
        Ok(())
    }

    async fn open_receivables_total(&mut self) -> Result<i64, StoreError> {
        sqlx::query_scalar(
            "select coalesce(sum(outstanding_micro), 0)::bigint from (
                select r.opened_micro
                       - coalesce((select sum(m.amount_micro) from receivable_movements m
                                    where m.receivable_id = r.id
                                      and m.kind in ('collected', 'written_off')), 0)
                           as outstanding_micro
                  from receivables r) open
             where outstanding_micro > 0",
        )
        .fetch_one(&mut *self.tx)
        .await
        .map_err(db_error)
    }

    async fn market_ledger_txns(
        &mut self,
        market: MarketId,
    ) -> Result<Vec<LedgerTxnFacts>, StoreError> {
        let rows = sqlx::query(
            "select t.id as txn_id, t.kind, t.created_at,
                    e.account_id, e.amount_micro, a.owner_type, a.owner_id
               from ledger_transactions t
               join ledger_entries e on e.txn_id = t.id
               join ledger_accounts a on a.id = e.account_id
              where t.id in (
                    select distinct e2.txn_id
                      from ledger_entries e2
                      join ledger_accounts a2 on a2.id = e2.account_id
                     where a2.owner_type in ('escrow', 'pool') and a2.owner_id = $1)
              order by t.created_at, t.id, e.id",
        )
        .bind(market.0)
        .fetch_all(&mut *self.tx)
        .await
        .map_err(db_error)?;
        let mut facts: Vec<LedgerTxnFacts> = Vec::new();
        for row in &rows {
            let txn: Uuid = row.get("txn_id");
            if facts.last().map(|f| f.txn) != Some(txn) {
                facts.push(LedgerTxnFacts {
                    txn,
                    kind: kind_from(row.get::<String, _>("kind").as_str()),
                    entries: Vec::new(),
                });
            }
            let Some(fact) = facts.last_mut() else {
                return Err(StoreError::Invariant("txn facts grouping"));
            };
            fact.entries.push(LedgerEntryFacts {
                account: domain::ledger::AccountId(row.get("account_id")),
                owner: owner_from(
                    row.get::<String, _>("owner_type").as_str(),
                    row.get("owner_id"),
                ),
                amount_micro: row.get("amount_micro"),
            });
        }
        Ok(facts)
    }

    async fn positions_for_market(
        &mut self,
        market: MarketId,
    ) -> Result<Vec<PositionRow>, StoreError> {
        let rows = sqlx::query(
            "select p.user_id, p.outcome_id, p.shares_micro, p.cost_micro, p.realized_pnl_micro
               from positions p
               join outcomes o on o.id = p.outcome_id
              where o.market_id = $1
              order by p.user_id, p.outcome_id
              for update of p",
        )
        .bind(market.0)
        .fetch_all(&mut *self.tx)
        .await
        .map_err(db_error)?;
        Ok(rows
            .iter()
            .map(|row| PositionRow {
                user: UserId(row.get("user_id")),
                outcome: application::model::OutcomeId(row.get("outcome_id")),
                shares: MicroShares(row.get("shares_micro")),
                cost: MicroUsd(row.get("cost_micro")),
                realized_pnl: MicroUsd(row.get("realized_pnl_micro")),
            })
            .collect())
    }

    async fn receivable_by_id(
        &mut self,
        id: Uuid,
    ) -> Result<Option<ReceivableOutstanding>, StoreError> {
        let sql = format!("{RECEIVABLE_SELECT} where r.id = $1");
        let row = sqlx::query(&sql)
            .bind(id)
            .fetch_optional(&mut *self.tx)
            .await
            .map_err(db_error)?;
        Ok(row.as_ref().map(receivable_from_row))
    }

    async fn write_off_for_update(
        &mut self,
        id: Uuid,
    ) -> Result<Option<WriteOffProposal>, StoreError> {
        let row = sqlx::query(
            "select id, receivable_id, idempotency_key, amount_micro, proposer_token_id,
                    confirmer_token_id, reason, status, confirm_not_before
               from receivable_write_off_proposals where id = $1 for update",
        )
        .bind(id)
        .fetch_optional(&mut *self.tx)
        .await
        .map_err(db_error)?;
        Ok(row.as_ref().map(write_off_from_row))
    }

    async fn write_off_by_key(
        &mut self,
        key: &str,
    ) -> Result<Option<WriteOffProposal>, StoreError> {
        let row = sqlx::query(
            "select id, receivable_id, idempotency_key, amount_micro, proposer_token_id,
                    confirmer_token_id, reason, status, confirm_not_before
               from receivable_write_off_proposals where idempotency_key = $1 for update",
        )
        .bind(key)
        .fetch_optional(&mut *self.tx)
        .await
        .map_err(db_error)?;
        Ok(row.as_ref().map(write_off_from_row))
    }

    async fn insert_write_off(&mut self, proposal: WriteOffProposal) -> Result<(), StoreError> {
        sqlx::query(
            "insert into receivable_write_off_proposals
                 (id, receivable_id, idempotency_key, amount_micro, proposer_token_id,
                  reason, status, confirm_not_before)
             values ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(proposal.id)
        .bind(proposal.receivable)
        .bind(&proposal.idempotency_key)
        .bind(proposal.amount_micro)
        .bind(&proposal.proposer_token_id)
        .bind(&proposal.reason)
        .bind(status_name(proposal.status))
        .bind(proposal.confirm_not_before)
        .execute(&mut *self.tx)
        .await
        .map_err(|error| match error {
            sqlx::Error::Database(db) if db.is_unique_violation() => {
                StoreError::Conflict("write-off proposal")
            }
            other => db_error(other),
        })?;
        Ok(())
    }

    async fn save_write_off(&mut self, proposal: &WriteOffProposal) -> Result<(), StoreError> {
        let updated = sqlx::query(
            "update receivable_write_off_proposals
                set status = $2, confirmer_token_id = $3,
                    settled_at = case when $2 in ('confirmed', 'rejected', 'expired')
                                      then now() else settled_at end
              where id = $1",
        )
        .bind(proposal.id)
        .bind(status_name(proposal.status))
        .bind(&proposal.confirmer_token_id)
        .execute(&mut *self.tx)
        .await
        .map_err(db_error)?;
        if updated.rows_affected() == 0 {
            return Err(StoreError::Invariant("save for unknown write-off"));
        }
        Ok(())
    }

    async fn written_off_since(&mut self, since: OffsetDateTime) -> Result<i64, StoreError> {
        sqlx::query_scalar(
            "select coalesce(sum(amount_micro), 0)::bigint
               from receivable_movements
              where kind = 'written_off' and created_at >= $1",
        )
        .bind(since)
        .fetch_one(&mut *self.tx)
        .await
        .map_err(db_error)
    }

    async fn remedial_for_update(
        &mut self,
        id: Uuid,
    ) -> Result<Option<RemedialCreditProposal>, StoreError> {
        let row = sqlx::query(
            "select id, market_id, user_id, idempotency_key, amount_micro, proposer_token_id,
                    confirmer_token_id, reason, status, confirm_not_before
               from remedial_credit_proposals where id = $1 for update",
        )
        .bind(id)
        .fetch_optional(&mut *self.tx)
        .await
        .map_err(db_error)?;
        Ok(row.as_ref().map(remedial_from_row))
    }

    async fn remedial_by_key(
        &mut self,
        key: &str,
    ) -> Result<Option<RemedialCreditProposal>, StoreError> {
        let row = sqlx::query(
            "select id, market_id, user_id, idempotency_key, amount_micro, proposer_token_id,
                    confirmer_token_id, reason, status, confirm_not_before
               from remedial_credit_proposals where idempotency_key = $1 for update",
        )
        .bind(key)
        .fetch_optional(&mut *self.tx)
        .await
        .map_err(db_error)?;
        Ok(row.as_ref().map(remedial_from_row))
    }

    async fn insert_remedial(
        &mut self,
        proposal: RemedialCreditProposal,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "insert into remedial_credit_proposals
                 (id, market_id, user_id, idempotency_key, amount_micro, proposer_token_id,
                  reason, status, confirm_not_before)
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        )
        .bind(proposal.id)
        .bind(proposal.market.0)
        .bind(proposal.user.0)
        .bind(&proposal.idempotency_key)
        .bind(proposal.amount_micro)
        .bind(&proposal.proposer_token_id)
        .bind(&proposal.reason)
        .bind(status_name(proposal.status))
        .bind(proposal.confirm_not_before)
        .execute(&mut *self.tx)
        .await
        .map_err(|error| match error {
            sqlx::Error::Database(db) if db.is_unique_violation() => {
                StoreError::Conflict("remedial proposal")
            }
            other => db_error(other),
        })?;
        Ok(())
    }

    async fn save_remedial(&mut self, proposal: &RemedialCreditProposal) -> Result<(), StoreError> {
        let updated = sqlx::query(
            "update remedial_credit_proposals
                set status = $2, confirmer_token_id = $3,
                    settled_at = case when $2 in ('confirmed', 'rejected', 'expired')
                                      then now() else settled_at end
              where id = $1",
        )
        .bind(proposal.id)
        .bind(status_name(proposal.status))
        .bind(&proposal.confirmer_token_id)
        .execute(&mut *self.tx)
        .await
        .map_err(db_error)?;
        if updated.rows_affected() == 0 {
            return Err(StoreError::Invariant("save for unknown remedial"));
        }
        Ok(())
    }

    async fn remedial_credited_for_market(&mut self, market: MarketId) -> Result<i64, StoreError> {
        sqlx::query_scalar(
            "select coalesce(sum(amount_micro), 0)::bigint
               from remedial_credit_proposals
              where market_id = $1 and status = 'confirmed'",
        )
        .bind(market.0)
        .fetch_one(&mut *self.tx)
        .await
        .map_err(db_error)
    }

    async fn remedial_credited_since(&mut self, since: OffsetDateTime) -> Result<i64, StoreError> {
        sqlx::query_scalar(
            "select coalesce(sum(amount_micro), 0)::bigint
               from remedial_credit_proposals
              where status = 'confirmed' and settled_at >= $1",
        )
        .bind(since)
        .fetch_one(&mut *self.tx)
        .await
        .map_err(db_error)
    }

    async fn insert_job_command(&mut self, command: OpsJobCommand) -> Result<(), StoreError> {
        sqlx::query(
            "insert into ops_job_commands
                 (id, kind, subject, idempotency_key, requested_by, status, attempts,
                  lease_expires_at, error)
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        )
        .bind(command.id)
        .bind(&command.kind)
        .bind(&command.subject)
        .bind(&command.idempotency_key)
        .bind(&command.requested_by)
        .bind(job_status_name(command.status))
        .bind(command.attempts)
        .bind(command.lease_expires_at)
        .bind(&command.error)
        .execute(&mut *self.tx)
        .await
        .map_err(|error| match error {
            sqlx::Error::Database(db) if db.is_unique_violation() => {
                StoreError::Conflict("ops job command")
            }
            other => db_error(other),
        })?;
        Ok(())
    }

    async fn job_command_by_key(&mut self, key: &str) -> Result<Option<OpsJobCommand>, StoreError> {
        let row = sqlx::query(
            "select id, kind, subject, idempotency_key, requested_by, status, attempts,
                    lease_expires_at, error
               from ops_job_commands where idempotency_key = $1 for update",
        )
        .bind(key)
        .fetch_optional(&mut *self.tx)
        .await
        .map_err(db_error)?;
        Ok(row.as_ref().map(job_from_row))
    }

    async fn due_job_commands(
        &mut self,
        now: OffsetDateTime,
        lease_until: OffsetDateTime,
        limit: u32,
    ) -> Result<Vec<OpsJobCommand>, StoreError> {
        let rows = sqlx::query(
            "update ops_job_commands
                set status = 'executing', lease_expires_at = $2, attempts = attempts + 1,
                    updated_at = now()
              where id in (
                    select id from ops_job_commands
                     where status = 'pending'
                        or (status = 'executing'
                            and (lease_expires_at is null or lease_expires_at <= $1))
                     order by created_at, id
                     limit $3
                     for update skip locked)
              returning id, kind, subject, idempotency_key, requested_by, status, attempts,
                        lease_expires_at, error",
        )
        .bind(now)
        .bind(lease_until)
        .bind(i64::from(limit))
        .fetch_all(&mut *self.tx)
        .await
        .map_err(db_error)?;
        Ok(rows.iter().map(job_from_row).collect())
    }

    async fn save_job_command(&mut self, command: &OpsJobCommand) -> Result<(), StoreError> {
        let updated = sqlx::query(
            "update ops_job_commands
                set status = $2, lease_expires_at = $3, error = $4, attempts = $5,
                    updated_at = now()
              where id = $1",
        )
        .bind(command.id)
        .bind(job_status_name(command.status))
        .bind(command.lease_expires_at)
        .bind(&command.error)
        .bind(command.attempts)
        .execute(&mut *self.tx)
        .await
        .map_err(db_error)?;
        if updated.rows_affected() == 0 {
            return Err(StoreError::Invariant("save for unknown job command"));
        }
        Ok(())
    }

    async fn set_user_created_at(
        &mut self,
        user: UserId,
        at: OffsetDateTime,
    ) -> Result<(), StoreError> {
        let updated = sqlx::query("update users set created_at = $2 where id = $1")
            .bind(user.0)
            .bind(at)
            .execute(&mut *self.tx)
            .await
            .map_err(db_error)?;
        if updated.rows_affected() == 0 {
            return Err(StoreError::NotFound("user"));
        }
        Ok(())
    }

    async fn seed_reputation(
        &mut self,
        user: UserId,
        rep_micro: i64,
        tier: u8,
    ) -> Result<(), StoreError> {
        let updated = sqlx::query(
            "insert into reputation (user_id, rep_micro, tier)
             values ($1, $2, $3)
             on conflict (user_id) do update set rep_micro = $2, tier = $3",
        )
        .bind(user.0)
        .bind(rep_micro)
        .bind(i16::from(tier))
        .execute(&mut *self.tx)
        .await
        .map_err(db_error)?;
        if updated.rows_affected() == 0 {
            return Err(StoreError::NotFound("user"));
        }
        Ok(())
    }
}

pub(super) async fn withdrawal_eligibility(
    store: &PgStore,
    user: UserId,
) -> Result<WithdrawalEligibilityView, StoreError> {
    let exists: bool = sqlx::query_scalar("select exists(select 1 from users where id = $1)")
        .bind(user.0)
        .fetch_one(store.pool_handle())
        .await
        .map_err(db_error)?;
    if !exists {
        return Err(StoreError::NotFound("user"));
    }
    let cash: i64 = sqlx::query_scalar(
        "select coalesce(sum(e.amount_micro), 0)::bigint
           from ledger_entries e
           join ledger_accounts a on a.id = e.account_id
          where a.owner_type = 'user' and a.owner_id = $1 and a.currency = 'usdc'",
    )
    .bind(user.0)
    .fetch_one(store.pool_handle())
    .await
    .map_err(db_error)?;
    let open: i64 = sqlx::query_scalar(
        "select coalesce(sum(outstanding_micro), 0)::bigint from (
            select r.opened_micro
                   - coalesce((select sum(m.amount_micro) from receivable_movements m
                                where m.receivable_id = r.id
                                  and m.kind in ('collected', 'written_off')), 0)
                       as outstanding_micro
              from receivables r where r.user_id = $1) open
         where outstanding_micro > 0",
    )
    .bind(user.0)
    .fetch_one(store.pool_handle())
    .await
    .map_err(db_error)?;
    Ok(WithdrawalEligibilityView {
        user,
        cash_micro: cash,
        open_receivables_micro: open,
        eligible: open == 0,
    })
}

#[cfg(test)]
mod tests {
    use application::model::{AdminRole, OwnerRef};

    use super::*;

    #[test]
    fn ops_database_vocabularies_are_total() {
        for (stage, raw) in [
            (UnwindStage::Proposed, "proposed"),
            (UnwindStage::Confirmed, "confirmed"),
            (UnwindStage::Applied, "applied"),
            (UnwindStage::Rejected, "rejected"),
            (UnwindStage::Expired, "expired"),
        ] {
            assert_eq!(stage_name(stage), raw);
            assert_eq!(stage_from(raw), stage);
        }
        assert_eq!(stage_from("unknown"), UnwindStage::Expired);

        for (status, raw) in [
            (ProposalStatus::Pending, "pending"),
            (ProposalStatus::Confirmed, "confirmed"),
            (ProposalStatus::Rejected, "rejected"),
            (ProposalStatus::Expired, "expired"),
        ] {
            assert_eq!(status_name(status), raw);
            assert_eq!(status_from(raw), status);
        }
        assert_eq!(status_from("unknown"), ProposalStatus::Expired);

        for (status, raw) in [
            (OpsJobStatus::Pending, "pending"),
            (OpsJobStatus::Executing, "executing"),
            (OpsJobStatus::Done, "done"),
            (OpsJobStatus::Failed, "failed"),
        ] {
            assert_eq!(job_status_name(status), raw);
            assert_eq!(job_status_from(raw), status);
        }
        assert_eq!(job_status_from("unknown"), OpsJobStatus::Failed);

        for (raw, kind) in [
            ("deposit", TxnKind::Deposit),
            ("trade", TxnKind::Trade),
            ("payout", TxnKind::Payout),
            ("withdrawal", TxnKind::Withdrawal),
            ("seed", TxnKind::Seed),
            ("credit_grant", TxnKind::CreditGrant),
            ("credit_convert", TxnKind::CreditConvert),
            ("reversal", TxnKind::Reversal),
        ] {
            assert_eq!(kind_from(raw), kind);
        }

        let id = Uuid::new_v4();
        for (raw, owner_id, expected) in [
            ("user", Some(id), OwnerRef::User(UserId(id))),
            ("escrow", Some(id), OwnerRef::MarketEscrow(MarketId(id))),
            ("pool", Some(id), OwnerRef::MarketPool(MarketId(id))),
            ("fees", None, OwnerRef::Fees),
            ("house", None, OwnerRef::House),
            ("external", None, OwnerRef::External),
        ] {
            assert_eq!(owner_from(raw, owner_id), expected);
        }
        assert_eq!(AdminRole::Ops.name(), "ops");
    }
}
