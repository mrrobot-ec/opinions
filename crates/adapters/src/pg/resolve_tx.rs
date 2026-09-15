use application::error::StoreError;
use application::model::{
    Holding, HoldingOwner, MarketId, OutcomeId, ReputationRow, UserId, VoteFact, VoteScoreUpdate,
};
use application::money::{AllocationFact, AllocationKind, CreditIo};
use application::ports::SettlementIo;
use async_trait::async_trait;
use domain::money::{MicroShares, MicroUsd};
use sqlx::Row;

use super::rows::{db_error, market_state_name, side_from_idx};
use super::store::PgTx;

// D34/D31 dual-control proposals. The money-effect transaction gets the SAME
// `money_command_proposals` authority W2's admin surface writes — one shared
// implementation, so a proposal CAS and its economic effect commit atomically
// instead of racing across two transactions (codex-p7r3 B4).
#[async_trait]
impl application::ports::ReferralPaidFinalize for PgTx {
    async fn referral_relevant_users(
        &mut self,
        market: MarketId,
    ) -> Result<Vec<UserId>, StoreError> {
        let referees = CreditIo::referral_referees_for_paid_market(self, market).await?;
        let mut users = Vec::with_capacity(referees.len().saturating_mul(2));
        for referee in referees {
            let Some(bind) = CreditIo::referral_bind_for_referee(self, referee).await? else {
                continue;
            };
            let (referrer, bound_referee, _) = CreditIo::referral_bind_parties(self, bind)
                .await?
                .ok_or(StoreError::Invariant(
                "referral bind index points to no row",
            ))?;
            if bound_referee != referee || referrer == referee {
                continue;
            }
            users.extend([referrer, referee]);
        }
        users.sort_unstable_by_key(|user| user.0);
        users.dedup();
        Ok(users)
    }

    async fn grant_referrals_on_paid(&mut self, market: MarketId) -> Result<u32, StoreError> {
        let granted_at: time::OffsetDateTime =
            sqlx::query_scalar("select coalesce(settled_at, now()) from markets where id = $1")
                .bind(market.0)
                .fetch_optional(&mut *self.tx)
                .await
                .map_err(db_error)?
                .ok_or(StoreError::NotFound("market"))?;
        let referees = CreditIo::referral_referees_for_paid_market(self, market).await?;
        let mut granted = 0_u32;
        for referee in referees {
            if application::money::referrals::grant_referral_on_paid_in_tx(
                self, referee, market, granted_at,
            )
            .await?
            {
                granted = granted
                    .checked_add(1)
                    .ok_or(StoreError::Invariant("referral grant count overflow"))?;
            }
        }
        Ok(granted)
    }
}

#[async_trait]
impl application::ports::MoneyProposalIo for PgTx {
    async fn insert_proposal(
        &mut self,
        proposal: application::money::MoneyProposal,
    ) -> Result<application::money::MoneyProposal, StoreError> {
        super::compliance_tx::proposal_insert(&mut self.tx, proposal).await
    }

    async fn get_proposal_by_replay(
        &mut self,
        replay_key: &str,
    ) -> Result<Option<application::money::MoneyProposal>, StoreError> {
        super::compliance_tx::proposal_by_replay(&mut self.tx, replay_key).await
    }

    async fn get_proposal(
        &mut self,
        id: uuid::Uuid,
    ) -> Result<application::money::MoneyProposal, StoreError> {
        super::compliance_tx::proposal_by_id(&mut self.tx, id).await
    }

    async fn confirm_proposal(
        &mut self,
        id: uuid::Uuid,
        confirmer: &str,
        now: time::OffsetDateTime,
    ) -> Result<application::money::MoneyProposal, StoreError> {
        let _ = now;
        super::compliance_tx::proposal_confirm(&mut self.tx, id, confirmer).await
    }
}

#[async_trait]
impl application::ports::FeeAllocationFinalize for PgTx {
    async fn finalize_market_fee_allocations(
        &mut self,
        market: MarketId,
    ) -> Result<u32, StoreError> {
        move_market_allocations(self, market, AllocationKind::Finalized).await
    }
}

#[async_trait]
impl application::ports::FeeAllocationReverse for PgTx {
    async fn reverse_market_fee_allocations(
        &mut self,
        market: MarketId,
    ) -> Result<u32, StoreError> {
        move_market_allocations(self, market, AllocationKind::Reversed).await
    }
}

fn terminal_allocation_prefix(kind: AllocationKind) -> Result<&'static str, StoreError> {
    match kind {
        AllocationKind::Finalized => Ok("final"),
        AllocationKind::Reversed => Ok("rev"),
        AllocationKind::Allocated => Err(StoreError::Invariant(
            "allocated is not a terminal credit fact",
        )),
    }
}

async fn move_market_allocations(
    tx: &mut PgTx,
    market: MarketId,
    terminal_kind: AllocationKind,
) -> Result<u32, StoreError> {
    let rows = sqlx::query(
        r#"
        select a.id, a.trade_id, a.lot_id, a.split_seq, a.amount_micro
          from credit_fee_allocations a
          join trades t on t.id = a.trade_id
         where t.market_id = $1
           and a.kind = 'allocated'
           and not exists (
               select 1 from credit_fee_allocations terminal
                where terminal.source_allocation_id = a.id
           )
         order by a.trade_id, a.lot_id, a.split_seq, a.id
         for update of a
        "#,
    )
    .bind(market.0)
    .fetch_all(&mut *tx.tx)
    .await
    .map_err(db_error)?;
    let mut moved = 0_u32;
    for row in rows {
        let source: uuid::Uuid = row.try_get("id").map_err(db_error)?;
        let trade_id: uuid::Uuid = row.try_get("trade_id").map_err(db_error)?;
        let lot_id: uuid::Uuid = row.try_get("lot_id").map_err(db_error)?;
        let split_seq: i32 = row.try_get("split_seq").map_err(db_error)?;
        let amount_micro: i64 = row.try_get("amount_micro").map_err(db_error)?;
        let prefix = terminal_allocation_prefix(terminal_kind)?;
        CreditIo::insert_allocation(
            tx,
            &AllocationFact {
                id: uuid::Uuid::new_v4(),
                trade_id,
                lot_id,
                split_seq,
                amount_micro,
                kind: terminal_kind,
                source_allocation_id: Some(source),
                idempotency_key: format!("{prefix}:{trade_id}:{source}"),
            },
        )
        .await?;
        moved = moved
            .checked_add(1)
            .ok_or(StoreError::Invariant("credit allocation count overflow"))?;
    }
    Ok(moved)
}

#[async_trait]
impl SettlementIo for PgTx {
    async fn holdings(&mut self, market: MarketId) -> Result<Vec<Holding>, StoreError> {
        let pool_account: uuid::Uuid = sqlx::query_scalar(
            r#"
            select id from ledger_accounts
             where owner_type = 'pool' and owner_id = $1 and currency = 'usdc'
            "#,
        )
        .bind(market.0)
        .fetch_optional(&mut *self.tx)
        .await
        .map_err(db_error)?
        .ok_or(StoreError::NotFound("market pool account"))?;
        let rows = sqlx::query(
            r#"
            select la.id as account_id, p.user_id, p.outcome_id, o.idx, p.shares_micro
              from positions p
              join outcomes o on o.id = p.outcome_id
              join ledger_accounts la
                on la.owner_type = 'user' and la.owner_id = p.user_id and la.currency = 'usdc'
             where o.market_id = $1 and p.shares_micro > 0
            union all
            select $2::uuid as account_id, null::uuid as user_id, o.id as outcome_id,
                   o.idx, pr.reserve_micro_shares as shares_micro
              from pool_reserves pr
              join outcomes o on o.id = pr.outcome_id and o.market_id = pr.market_id
             where pr.market_id = $1 and pr.reserve_micro_shares > 0
             order by account_id, idx
            "#,
        )
        .bind(market.0)
        .bind(pool_account)
        .fetch_all(&mut *self.tx)
        .await
        .map_err(db_error)?;
        rows.iter()
            .map(|row| {
                let user: Option<uuid::Uuid> = row.try_get("user_id").map_err(db_error)?;
                Ok(Holding {
                    account: domain::ledger::AccountId(
                        row.try_get("account_id").map_err(db_error)?,
                    ),
                    owner: user.map_or(HoldingOwner::Pool, |id| HoldingOwner::User(UserId(id))),
                    outcome: OutcomeId(row.try_get("outcome_id").map_err(db_error)?),
                    side: side_from_idx(row.try_get("idx").map_err(db_error)?)?,
                    shares: MicroShares(row.try_get("shares_micro").map_err(db_error)?),
                })
            })
            .collect()
    }

    async fn escrow_balance(&mut self, market: MarketId) -> Result<MicroUsd, StoreError> {
        let row = sqlx::query(
            r#"
            select la.id,
                   coalesce((select sum(le.amount_micro)::bigint from ledger_entries le
                              where le.account_id = la.id), 0)::bigint as balance_micro
              from ledger_accounts la
             where la.owner_type = 'escrow' and la.owner_id = $1 and la.currency = 'usdc'
             for update of la
            "#,
        )
        .bind(market.0)
        .fetch_optional(&mut *self.tx)
        .await
        .map_err(db_error)?
        .ok_or(StoreError::NotFound("market escrow account"))?;
        Ok(MicroUsd(row.try_get("balance_micro").map_err(db_error)?))
    }

    async fn write_outcome_resolution(
        &mut self,
        outcome: OutcomeId,
        final_bps: u16,
        redemption: MicroUsd,
    ) -> Result<(), StoreError> {
        let result = sqlx::query(
            "update outcomes set final_vote_bps = $2, redemption_micro = $3 where id = $1",
        )
        .bind(outcome.0)
        .bind(i32::from(final_bps))
        .bind(redemption.0)
        .execute(&mut *self.tx)
        .await
        .map_err(db_error)?;
        affected_one(result.rows_affected(), "outcome")
    }

    async fn vote_facts(&mut self, market: MarketId) -> Result<Vec<VoteFact>, StoreError> {
        let rows = sqlx::query(
            r#"
            select v.id, v.user_id, o.idx, v.crowd_guess_pct
              from votes v
              join outcomes o on o.id = v.outcome_id and o.market_id = v.market_id
             where v.market_id = $1 order by v.seq
            "#,
        )
        .bind(market.0)
        .fetch_all(&mut *self.tx)
        .await
        .map_err(db_error)?;
        rows.iter()
            .map(|row| {
                let guess: i32 = row.try_get("crowd_guess_pct").map_err(db_error)?;
                Ok(VoteFact {
                    vote_id: row.try_get("id").map_err(db_error)?,
                    user: UserId(row.try_get("user_id").map_err(db_error)?),
                    side: side_from_idx(row.try_get("idx").map_err(db_error)?)?,
                    crowd_guess_pct: u8::try_from(guess)
                        .map_err(|_| StoreError::Invariant("invalid crowd guess"))?,
                })
            })
            .collect()
    }

    async fn save_vote_scores(&mut self, batch: &[VoteScoreUpdate]) -> Result<(), StoreError> {
        if batch.is_empty() {
            return Ok(());
        }
        let ids: Vec<uuid::Uuid> = batch.iter().map(|row| row.vote_id).collect();
        let accuracy: Vec<i32> = batch
            .iter()
            .map(|row| i32::from(row.score.accuracy_bp))
            .collect();
        let majority: Vec<i32> = batch
            .iter()
            .map(|row| i32::from(row.score.majority_bp))
            .collect();
        let scores: Vec<i32> = batch
            .iter()
            .map(|row| i32::from(row.score.score_bp))
            .collect();
        sqlx::query(
            r#"
            insert into vote_scores (vote_id, accuracy_bp, majority_bp, score_bp)
            select * from unnest($1::uuid[], $2::int[], $3::int[], $4::int[])
            on conflict (vote_id) do update set
                accuracy_bp = excluded.accuracy_bp,
                majority_bp = excluded.majority_bp,
                score_bp = excluded.score_bp
            "#,
        )
        .bind(ids)
        .bind(accuracy)
        .bind(majority)
        .bind(scores)
        .execute(&mut *self.tx)
        .await
        .map_err(db_error)?;
        Ok(())
    }

    async fn voter_ids(&mut self, market: MarketId) -> Result<Vec<UserId>, StoreError> {
        sqlx::query_scalar("select user_id from votes where market_id = $1")
            .bind(market.0)
            .fetch_all(&mut *self.tx)
            .await
            .map(|rows| rows.into_iter().map(UserId).collect())
            .map_err(db_error)
    }

    async fn reps_for_update(
        &mut self,
        users_sorted: &[UserId],
    ) -> Result<Vec<ReputationRow>, StoreError> {
        if users_sorted.is_empty() {
            return Ok(Vec::new());
        }
        let ids: Vec<uuid::Uuid> = users_sorted.iter().map(|user| user.0).collect();
        sqlx::query(
            r#"select pg_advisory_xact_lock(2, hashtext(user_id::text))
                 from unnest($1::uuid[]) as user_id order by user_id"#,
        )
        .bind(&ids)
        .execute(&mut *self.tx)
        .await
        .map_err(db_error)?;
        let rows = sqlx::query(
            "select user_id, rep_micro, tier from reputation where user_id = any($1) order by user_id for update",
        )
        .bind(&ids)
        .fetch_all(&mut *self.tx)
        .await
        .map_err(db_error)?;
        if rows.len() != ids.len() {
            return Err(StoreError::Invariant("voter without reputation row"));
        }
        rows.iter()
            .map(|row| {
                Ok(ReputationRow {
                    user: UserId(row.try_get("user_id").map_err(db_error)?),
                    rep_micro: row.try_get("rep_micro").map_err(db_error)?,
                    tier: u8::try_from(row.try_get::<i32, _>("tier").map_err(db_error)?)
                        .map_err(|_| StoreError::Invariant("invalid reputation tier"))?,
                })
            })
            .collect()
    }

    async fn save_reps(&mut self, batch: &[ReputationRow]) -> Result<(), StoreError> {
        if batch.is_empty() {
            return Ok(());
        }
        let ids: Vec<uuid::Uuid> = batch.iter().map(|row| row.user.0).collect();
        let reps: Vec<i64> = batch.iter().map(|row| row.rep_micro).collect();
        let tiers: Vec<i32> = batch.iter().map(|row| i32::from(row.tier)).collect();
        let result = sqlx::query(
            r#"update reputation r set rep_micro = data.rep_micro,
                   tier = data.tier, updated_at = now()
                 from (select * from unnest($1::uuid[], $2::bigint[], $3::int[])
                       as x(user_id, rep_micro, tier)) data
                where r.user_id = data.user_id"#,
        )
        .bind(ids)
        .bind(reps)
        .bind(tiers)
        .execute(&mut *self.tx)
        .await
        .map_err(db_error)?;
        if result.rows_affected() != u64::try_from(batch.len()).unwrap_or(u64::MAX) {
            return Err(StoreError::Invariant("reputation update cardinality"));
        }
        Ok(())
    }

    async fn set_market_state(
        &mut self,
        market: MarketId,
        state: domain::market::MarketState,
    ) -> Result<(), StoreError> {
        set_market_state(self, market, state).await
    }

    async fn open_interest(&mut self, market: MarketId) -> Result<MicroUsd, StoreError> {
        sqlx::query_scalar(
            r#"
            select coalesce(sum(p.cost_micro), 0)::bigint
              from positions p join outcomes o on o.id = p.outcome_id
             where o.market_id = $1
            "#,
        )
        .bind(market.0)
        .fetch_one(&mut *self.tx)
        .await
        .map(MicroUsd)
        .map_err(db_error)
    }

    async fn flag_curator_needed(&mut self, market: MarketId) -> Result<bool, StoreError> {
        let result: Option<uuid::Uuid> = sqlx::query_scalar(
            r#"
            update markets set curator_flagged_at = now()
             where id = $1 and status = 'resolving' and curator_flagged_at is null
            returning id
            "#,
        )
        .bind(market.0)
        .fetch_optional(&mut *self.tx)
        .await
        .map_err(db_error)?;
        Ok(result.is_some())
    }

    async fn clear_curator_flag(&mut self, market: MarketId) -> Result<(), StoreError> {
        let result = sqlx::query("update markets set curator_flagged_at = null where id = $1")
            .bind(market.0)
            .execute(&mut *self.tx)
            .await
            .map_err(db_error)?;
        affected_one(result.rows_affected(), "market")
    }

    async fn integrity_report(
        &mut self,
        market: MarketId,
    ) -> Result<Option<application::model::IntegrityReportRow>, StoreError> {
        super::integrity_tx::read_report(self, market).await
    }

    async fn set_integrity_due_at(
        &mut self,
        market: MarketId,
        due_at: Option<time::OffsetDateTime>,
    ) -> Result<(), StoreError> {
        let result = sqlx::query("update markets set integrity_due_at = $2 where id = $1")
            .bind(market.0)
            .bind(due_at)
            .execute(&mut *self.tx)
            .await
            .map_err(db_error)?;
        affected_one(result.rows_affected(), "market")
    }

    async fn pool_seeded_micro(&mut self, market: MarketId) -> Result<MicroUsd, StoreError> {
        sqlx::query_scalar("select seeded_micro from pools where market_id = $1")
            .bind(market.0)
            .fetch_optional(&mut *self.tx)
            .await
            .map_err(db_error)?
            .map(MicroUsd)
            .ok_or(StoreError::NotFound("pool"))
    }

    async fn set_lp_result(
        &mut self,
        market: MarketId,
        pnl: MicroUsd,
        settled_at: time::OffsetDateTime,
    ) -> Result<(), StoreError> {
        let result =
            sqlx::query("update markets set lp_pnl_micro = $2, settled_at = $3 where id = $1")
                .bind(market.0)
                .bind(pnl.0)
                .bind(settled_at)
                .execute(&mut *self.tx)
                .await
                .map_err(db_error)?;
        affected_one(result.rows_affected(), "market")
    }

    async fn set_collateral_at_close(
        &mut self,
        market: MarketId,
        collateral: MicroUsd,
    ) -> Result<(), StoreError> {
        let result = sqlx::query("update markets set collateral_at_close_micro = $2 where id = $1")
            .bind(market.0)
            .bind(collateral.0)
            .execute(&mut *self.tx)
            .await
            .map_err(db_error)?;
        affected_one(result.rows_affected(), "market")
    }
}

pub(super) async fn set_market_state(
    tx: &mut PgTx,
    market: MarketId,
    state: domain::market::MarketState,
) -> Result<(), StoreError> {
    let result = sqlx::query("update markets set status = $2 where id = $1")
        .bind(market.0)
        .bind(market_state_name(state))
        .execute(&mut *tx.tx)
        .await
        .map_err(db_error)?;
    affected_one(result.rows_affected(), "market")
}

fn affected_one(rows: u64, entity: &'static str) -> Result<(), StoreError> {
    match rows {
        1 => Ok(()),
        0 => Err(StoreError::NotFound(entity)),
        _ => Err(StoreError::Invariant(
            "write unexpectedly affected multiple rows",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn affected_row_contract_distinguishes_absence_and_cardinality_bugs() {
        assert_eq!(affected_one(1, "market"), Ok(()));
        assert_eq!(
            affected_one(0, "market"),
            Err(StoreError::NotFound("market"))
        );
        assert_eq!(
            affected_one(2, "market"),
            Err(StoreError::Invariant(
                "write unexpectedly affected multiple rows"
            ))
        );
    }

    #[test]
    fn only_terminal_allocation_kinds_receive_idempotency_prefixes() {
        assert_eq!(
            terminal_allocation_prefix(AllocationKind::Finalized),
            Ok("final")
        );
        assert_eq!(
            terminal_allocation_prefix(AllocationKind::Reversed),
            Ok("rev")
        );
        assert_eq!(
            terminal_allocation_prefix(AllocationKind::Allocated),
            Err(StoreError::Invariant(
                "allocated is not a terminal credit fact"
            ))
        );
    }
}
