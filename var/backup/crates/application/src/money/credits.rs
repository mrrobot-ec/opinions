//! Bonus credit lots, fee-allocation algebra, and lazy convert (D32).
//!
//! Progress is the trade's `legs.fee` credit — never notional × bps.
//! Allocations are provisional until the fee's market reaches `Paid`.
//! `finalized` MOVES provisional → finalized (same amount). Voided books
//! never finalize; unwind reverses the provisional rows.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

use domain::ledger::{Currency, Entry, TxnKind};
use domain::money::MicroUsd;

use super::{AllocationFact, AllocationKind, CreditLotRow, GrantClass};
use crate::error::{AppError, StoreError};
use crate::model::{AdminContext, Event, OwnerRef, UserId};
use crate::ports::Store;

/// One lot's remaining redeemable capacity (oldest-first split input).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LotCapacity {
    pub lot_id: Uuid,
    pub remaining_micro: i64,
}

/// Errors that the allocation algebra refuses (never defaulted away).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllocError {
    DuplicateAllocated,
    DuplicateTerminal,
    SourceMissing,
    AmountMismatch,
    ExceedsSource,
    ZeroAmount,
}

/// Deterministic oldest-first split of a trade fee across lots.
#[must_use]
pub fn split_oldest_first(lots: &[LotCapacity], fee_micro: i64) -> Vec<(Uuid, i32, i64)> {
    if fee_micro <= 0 {
        return Vec::new();
    }
    let mut left = fee_micro;
    let mut out = Vec::new();
    for lot in lots {
        if left == 0 {
            break;
        }
        if lot.remaining_micro <= 0 {
            continue;
        }
        let take = left.min(lot.remaining_micro);
        // One allocated fact per lot per trade: split_seq starts at 0.
        out.push((lot.lot_id, 0, take));
        left -= take;
    }
    out
}

/// Live provisional and finalized progress for one lot.
#[must_use]
pub fn lot_progress(facts: &[AllocationFact], lot_id: Uuid) -> (i64, i64) {
    let mut provisional = 0_i64;
    let mut finalized = 0_i64;
    let mut reversed_sources = BTreeSet::new();
    let mut finalized_sources = BTreeSet::new();
    for fact in facts {
        if fact.lot_id != lot_id {
            continue;
        }
        match fact.kind {
            AllocationKind::Allocated => provisional += fact.amount_micro,
            AllocationKind::Finalized => {
                finalized += fact.amount_micro;
                if let Some(src) = fact.source_allocation_id {
                    finalized_sources.insert(src);
                }
            }
            AllocationKind::Reversed => {
                if let Some(src) = fact.source_allocation_id {
                    reversed_sources.insert(src);
                }
            }
        }
    }
    // finalized MOVES: subtract those source amounts from provisional.
    for fact in facts {
        if fact.lot_id != lot_id || fact.kind != AllocationKind::Allocated {
            continue;
        }
        if finalized_sources.contains(&fact.id) || reversed_sources.contains(&fact.id) {
            provisional -= fact.amount_micro;
        }
    }
    (provisional.max(0), finalized)
}

/// Remaining redeemable capacity of a lot (requirement − net allocated).
#[must_use]
pub fn lot_remaining(lot: &CreditLotRow, facts: &[AllocationFact]) -> i64 {
    if lot.converted_at.is_some() {
        return 0;
    }
    let (prov, fin) = lot_progress(facts, lot.id);
    (lot.amount_micro - prov - fin).max(0)
}

/// Net allocated (provisional live + finalized) for one trade.
#[must_use]
pub fn trade_net_allocated(facts: &[AllocationFact], trade_id: Uuid) -> i64 {
    facts
        .iter()
        .filter(|f| f.trade_id == trade_id)
        .fold(0_i64, |sum, fact| match fact.kind {
            AllocationKind::Allocated => sum + fact.amount_micro,
            AllocationKind::Reversed => sum - fact.amount_micro,
            AllocationKind::Finalized => sum,
        })
}

/// Validate a unique `(trade, lot, split_seq)` allocated insert.
///
/// # Errors
/// Duplicate allocated fact or zero amount.
pub fn check_allocate(
    existing: &[AllocationFact],
    trade_id: Uuid,
    lot_id: Uuid,
    split_seq: i32,
    amount_micro: i64,
) -> Result<(), AllocError> {
    if amount_micro <= 0 {
        return Err(AllocError::ZeroAmount);
    }
    if existing.iter().any(|f| {
        f.trade_id == trade_id
            && f.lot_id == lot_id
            && f.split_seq == split_seq
            && f.kind == AllocationKind::Allocated
    }) {
        return Err(AllocError::DuplicateAllocated);
    }
    Ok(())
}

/// Validate a terminal child (`finalized` XOR `reversed`) of a live source.
///
/// # Errors
/// Missing source, duplicate terminal, or amount that is not an exact move.
pub fn check_terminal(
    existing: &[AllocationFact],
    source_id: Uuid,
    amount_micro: i64,
) -> Result<(), AllocError> {
    if amount_micro <= 0 {
        return Err(AllocError::ZeroAmount);
    }
    let source = existing
        .iter()
        .find(|f| f.id == source_id && f.kind == AllocationKind::Allocated)
        .ok_or(AllocError::SourceMissing)?;
    if amount_micro > source.amount_micro {
        return Err(AllocError::ExceedsSource);
    }
    if amount_micro != source.amount_micro {
        return Err(AllocError::AmountMismatch);
    }
    if existing.iter().any(|f| {
        f.source_allocation_id == Some(source_id)
            && matches!(f.kind, AllocationKind::Finalized | AllocationKind::Reversed)
    }) {
        return Err(AllocError::DuplicateTerminal);
    }
    Ok(())
}

/// Remaining cash redemption promise across unconverted `real_money` lots.
#[must_use]
pub fn remaining_real_money_promise(lots: &[CreditLotRow], facts: &[AllocationFact]) -> i64 {
    lots.iter()
        .filter(|lot| lot.grant_class == GrantClass::RealMoney && lot.converted_at.is_none())
        .map(|lot| {
            let (_, finalized) = lot_progress(facts, lot.id);
            // Coverage is the stamped cash promise still outstanding — the
            // unconverted lot amount, not the un-finalized remainder. An
            // earned conversion must never fail, so reserve covers the
            // whole unconverted lot until convert debits it.
            let _ = finalized;
            lot.amount_micro
        })
        .sum()
}

/// A lot is convertible when finalized progress covers the stamped amount.
#[must_use]
pub fn lot_ready_to_convert(lot: &CreditLotRow, facts: &[AllocationFact]) -> bool {
    if lot.converted_at.is_some() || lot.grant_class != GrantClass::RealMoney {
        return false;
    }
    let (_, finalized) = lot_progress(facts, lot.id);
    finalized >= lot.amount_micro
}

/// Build the allocated facts for one trade fee (oldest lots first).
///
/// # Errors
/// Duplicate allocated key.
pub fn plan_trade_allocations(
    lots: &[CreditLotRow],
    facts: &[AllocationFact],
    trade_id: Uuid,
    fee_micro: i64,
) -> Result<Vec<AllocationFact>, AllocError> {
    let mut capacities: Vec<LotCapacity> = lots
        .iter()
        .filter(|lot| lot.converted_at.is_none())
        .map(|lot| LotCapacity {
            lot_id: lot.id,
            remaining_micro: lot_remaining(lot, facts),
        })
        .collect();
    capacities.sort_by_key(|capacity| {
        (
            lots.iter()
                .find(|lot| lot.id == capacity.lot_id)
                .map_or(OffsetDateTime::UNIX_EPOCH, |lot| lot.granted_at),
            capacity.lot_id,
        )
    });
    let mut planned = Vec::new();
    for (lot_id, split_seq, amount) in split_oldest_first(&capacities, fee_micro) {
        check_allocate(facts, trade_id, lot_id, split_seq, amount)?;
        planned.push(AllocationFact {
            id: Uuid::new_v4(),
            trade_id,
            lot_id,
            split_seq,
            amount_micro: amount,
            kind: AllocationKind::Allocated,
            source_allocation_id: None,
            idempotency_key: format!("alloc:{trade_id}:{lot_id}:{split_seq}"),
        });
    }
    Ok(planned)
}

/// Grant command.
#[derive(Debug, Clone)]
pub struct GrantCreditCmd {
    pub user: UserId,
    pub amount: MicroUsd,
    pub source: String,
    pub grant_class: GrantClass,
    pub policy_version: String,
    pub idempotency_key: String,
    pub granted_at: OffsetDateTime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GrantReceipt {
    pub lot_id: Uuid,
    pub ledger_txn: Uuid,
    pub replayed: bool,
}

/// `GrantCredit` — House(UsdcCredit) → User(UsdcCredit) with reserve-at-grant.
pub struct GrantCredit<'a, S: Store> {
    pub store: &'a S,
}

impl<S: Store> GrantCredit<'_, S> {
    /// # Errors
    /// Insufficient `BonusReserve` coverage, store failures.
    pub async fn execute(&self, cmd: GrantCreditCmd) -> Result<GrantReceipt, AppError> {
        self.execute_as(cmd, &AdminContext::Machine).await
    }

    /// # Errors
    /// As [`Self::execute`].
    #[allow(clippy::too_many_lines)]
    pub async fn execute_as(
        &self,
        cmd: GrantCreditCmd,
        actor: &AdminContext,
    ) -> Result<GrantReceipt, AppError> {
        let mut tx = self.store.credit_convert_tx().await?;
        tx.serialize_key(&cmd.idempotency_key).await?;
        if let Some(txn) = tx.txn_by_key(&cmd.idempotency_key).await? {
            let lot =
                tx.lot_by_idempotency(&cmd.idempotency_key)
                    .await?
                    .ok_or(StoreError::Invariant(
                        "credit grant transaction has no grant lot",
                    ))?;
            if lot.user != cmd.user
                || lot.amount_micro != cmd.amount.0
                || lot.source != cmd.source
                || lot.grant_class != cmd.grant_class
                || lot.policy_version != cmd.policy_version
                || lot.granted_at != cmd.granted_at
            {
                return Err(AppError::IdempotencyConflict);
            }
            return Ok(GrantReceipt {
                lot_id: lot.id,
                ledger_txn: txn,
                replayed: true,
            });
        }
        tx.lock_user(cmd.user).await?;
        tx.convert_then_collect(
            cmd.user,
            cmd.granted_at,
            &format!("credit-grant-lock:{}", cmd.idempotency_key),
        )
        .await?;
        crate::money::enforcement::enforce_money_mutation(
            tx.as_mut(),
            cmd.user,
            crate::money::enforcement::MoneyMutation::CreditGrant,
            cmd.amount.0,
            cmd.granted_at,
        )
        .await?;
        tx.lock_credit_lots(cmd.user).await?;
        // The reserve singleton is also the global grant-cap mutex. Every
        // grant class takes it, so sweeps grants cannot race around the cap.
        let reserve = tx.bonus_reserve_balance().await?;
        let mint_cap = tx
            .config_i64("bonus_mint_daily_cap_micro")
            .await?
            .unwrap_or(500_000_000);
        let since = cmd.granted_at - time::Duration::hours(24);
        let minted = tx.bonus_minted_since(since).await?;
        let next_minted = minted.checked_add(cmd.amount.0).ok_or(AppError::Overflow)?;
        if mint_cap == 0 || next_minted > mint_cap {
            return Err(AppError::MoneyForbidden("bonus mint cap"));
        }
        if cmd.grant_class == GrantClass::RealMoney {
            let promised = tx.remaining_real_money_promise().await?;
            let next = promised
                .checked_add(cmd.amount.0)
                .ok_or(AppError::Overflow)?;
            if reserve < next {
                return Err(AppError::InsufficientBonusReserve {
                    reserve_micro: reserve,
                    promised_micro: next,
                });
            }
        }
        let house = tx.account(OwnerRef::House, Currency::UsdcCredit).await?;
        let user = tx
            .account(OwnerRef::User(cmd.user), Currency::UsdcCredit)
            .await?;
        let ledger_txn = tx
            .ledger_apply(
                TxnKind::CreditGrant,
                &cmd.idempotency_key,
                &[
                    Entry {
                        account: house,
                        amount: MicroUsd(-cmd.amount.0),
                    },
                    Entry {
                        account: user,
                        amount: cmd.amount,
                    },
                ],
            )
            .await?;
        let lot_id = tx
            .insert_credit_lot(&CreditLotRow {
                id: Uuid::new_v4(),
                user: cmd.user,
                source: cmd.source.clone(),
                amount_micro: cmd.amount.0,
                granted_at: cmd.granted_at,
                grant_class: cmd.grant_class,
                policy_version: cmd.policy_version.clone(),
                converted_at: None,
            })
            .await?;
        tx.remember_lot_idempotency(&cmd.idempotency_key, lot_id)
            .await?;
        tx.append(Event {
            event_type: "CreditGranted",
            aggregate_type: "user",
            aggregate_id: cmd.user.0,
            payload: json!({
                "lot_id": lot_id.to_string(),
                "amount_micro": cmd.amount.0,
                "source": cmd.source,
                "grant_class": cmd.grant_class.as_str(),
            }),
        })
        .await?;
        let _ = actor;
        tx.commit().await?;
        Ok(GrantReceipt {
            lot_id,
            ledger_txn,
            replayed: false,
        })
    }
}

/// Group facts by lot for coverage reporting.
#[must_use]
pub fn coverage_by_lot(
    lots: &[CreditLotRow],
    facts: &[AllocationFact],
) -> BTreeMap<Uuid, (i64, i64)> {
    lots.iter()
        .map(|lot| (lot.id, lot_progress(facts, lot.id)))
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::model::OwnerRef;
    use crate::ports::Store;
    use domain::ledger::{Currency, Entry, TxnKind};
    use domain::money::MicroUsd;

    fn lot(id: u128, amount: i64, at: i64) -> CreditLotRow {
        CreditLotRow {
            id: Uuid::from_u128(id),
            user: UserId(Uuid::from_u128(1)),
            source: "signup".into(),
            amount_micro: amount,
            granted_at: OffsetDateTime::from_unix_timestamp(at).unwrap(),
            grant_class: GrantClass::RealMoney,
            policy_version: "1".into(),
            converted_at: None,
        }
    }

    fn allocated(id: u128, trade: u128, lot_id: u128, seq: i32, amount: i64) -> AllocationFact {
        AllocationFact {
            id: Uuid::from_u128(id),
            trade_id: Uuid::from_u128(trade),
            lot_id: Uuid::from_u128(lot_id),
            split_seq: seq,
            amount_micro: amount,
            kind: AllocationKind::Allocated,
            source_allocation_id: None,
            idempotency_key: format!("a-{id}"),
        }
    }

    #[test]
    fn split_is_oldest_first_and_stops_at_fee() {
        let lots = [
            LotCapacity {
                lot_id: Uuid::from_u128(1),
                remaining_micro: 3,
            },
            LotCapacity {
                lot_id: Uuid::from_u128(2),
                remaining_micro: 10,
            },
        ];
        let plan = split_oldest_first(&lots, 5);
        assert_eq!(
            plan,
            vec![(Uuid::from_u128(1), 0, 3), (Uuid::from_u128(2), 0, 2)]
        );
        assert!(split_oldest_first(&lots, 0).is_empty());
    }

    #[test]
    fn allocation_helpers_cover_closed_edge_cases() {
        let lot_id = Uuid::from_u128(1);
        let other_lot = Uuid::from_u128(2);
        let trade_id = Uuid::from_u128(3);
        let source = allocated(10, 3, 1, 0, 5);
        let other = allocated(11, 4, 2, 0, 7);
        let finalized = AllocationFact {
            id: Uuid::from_u128(12),
            trade_id,
            lot_id,
            split_seq: 0,
            amount_micro: 5,
            kind: AllocationKind::Finalized,
            source_allocation_id: Some(source.id),
            idempotency_key: "finalized".into(),
        };
        let reversed = AllocationFact {
            id: Uuid::from_u128(13),
            trade_id,
            lot_id,
            split_seq: 0,
            amount_micro: 5,
            kind: AllocationKind::Reversed,
            source_allocation_id: Some(source.id),
            idempotency_key: "reversed".into(),
        };

        assert_eq!(
            split_oldest_first(
                &[
                    LotCapacity {
                        lot_id,
                        remaining_micro: 0,
                    },
                    LotCapacity {
                        lot_id: other_lot,
                        remaining_micro: 2,
                    },
                ],
                1,
            ),
            vec![(other_lot, 0, 1)]
        );
        assert_eq!(lot_progress(&[other], lot_id), (0, 0));

        let mut converted = lot(1, 5, 1);
        converted.converted_at = Some(OffsetDateTime::UNIX_EPOCH);
        assert_eq!(lot_remaining(&converted, &[]), 0);
        assert!(!lot_ready_to_convert(&converted, &[]));
        let mut sweeps = lot(2, 5, 1);
        sweeps.grant_class = GrantClass::Sweeps;
        assert!(!lot_ready_to_convert(&sweeps, &[]));

        assert_eq!(
            trade_net_allocated(&[source.clone(), finalized], trade_id),
            5
        );
        assert_eq!(
            trade_net_allocated(&[source.clone(), reversed], trade_id),
            0
        );
        assert_eq!(
            check_terminal(std::slice::from_ref(&source), source.id, 0),
            Err(AllocError::ZeroAmount)
        );
        assert_eq!(
            check_terminal(std::slice::from_ref(&source), source.id, 6),
            Err(AllocError::ExceedsSource)
        );
        assert_eq!(coverage_by_lot(&[lot(1, 5, 1)], &[source])[&lot_id], (5, 0));
    }

    #[test]
    fn equal_time_lots_split_by_stable_lot_identity() {
        let lots = [lot(2, 10, 1), lot(1, 10, 1)];
        let planned = plan_trade_allocations(&lots, &[], Uuid::from_u128(9), 5).unwrap();
        assert_eq!(planned.len(), 1);
        assert_eq!(planned[0].lot_id, Uuid::from_u128(1));
    }

    #[test]
    fn finalize_moves_provisional_and_does_not_add_progress() {
        let src = allocated(10, 1, 1, 0, 5_000_000);
        let fin = AllocationFact {
            id: Uuid::from_u128(11),
            trade_id: Uuid::from_u128(1),
            lot_id: Uuid::from_u128(1),
            split_seq: 0,
            amount_micro: 5_000_000,
            kind: AllocationKind::Finalized,
            source_allocation_id: Some(src.id),
            idempotency_key: "f".into(),
        };
        let (prov, fin_amt) = lot_progress(&[src.clone(), fin], Uuid::from_u128(1));
        assert_eq!(prov, 0, "finalize MOVES, it does not add");
        assert_eq!(fin_amt, 5_000_000);
        assert_eq!(trade_net_allocated(&[src], Uuid::from_u128(1)), 5_000_000);
    }

    #[test]
    fn wash_volume_without_finalize_does_not_convert() {
        let lot = lot(1, 5_000_000, 1);
        let fee = allocated(10, 1, 1, 0, 5_000_000);
        assert!(
            !lot_ready_to_convert(&lot, &[fee]),
            "provisional is not convert"
        );
        let (_, fin) = lot_progress(&[allocated(10, 1, 1, 0, 5_000_000)], lot.id);
        assert_eq!(fin, 0);
    }

    #[test]
    fn voided_reverse_leaves_no_finalized_progress() {
        let src = allocated(10, 1, 1, 0, 5_000_000);
        let rev = AllocationFact {
            id: Uuid::from_u128(12),
            trade_id: Uuid::from_u128(1),
            lot_id: Uuid::from_u128(1),
            split_seq: 0,
            amount_micro: 5_000_000,
            kind: AllocationKind::Reversed,
            source_allocation_id: Some(src.id),
            idempotency_key: "r".into(),
        };
        let lot = lot(1, 5_000_000, 1);
        assert!(!lot_ready_to_convert(&lot, &[src.clone(), rev.clone()]));
        let (prov, fin) = lot_progress(&[src, rev], lot.id);
        assert_eq!(prov, 0);
        assert_eq!(fin, 0);
    }

    #[test]
    fn terminal_is_xor_and_must_name_the_exact_source() {
        let src = allocated(10, 1, 1, 0, 5);
        assert!(check_terminal(std::slice::from_ref(&src), src.id, 5).is_ok());
        assert_eq!(
            check_terminal(std::slice::from_ref(&src), src.id, 4),
            Err(AllocError::AmountMismatch)
        );
        assert_eq!(
            check_terminal(std::slice::from_ref(&src), Uuid::from_u128(99), 5),
            Err(AllocError::SourceMissing)
        );
        let fin = AllocationFact {
            id: Uuid::from_u128(11),
            trade_id: src.trade_id,
            lot_id: src.lot_id,
            split_seq: 0,
            amount_micro: 5,
            kind: AllocationKind::Finalized,
            source_allocation_id: Some(src.id),
            idempotency_key: "f".into(),
        };
        assert_eq!(
            check_terminal(&[src, fin], Uuid::from_u128(10), 5),
            Err(AllocError::DuplicateTerminal)
        );
    }

    #[test]
    fn unique_trade_lot_split_is_enforced() {
        let src = allocated(10, 1, 1, 0, 5);
        assert_eq!(
            check_allocate(&[src], Uuid::from_u128(1), Uuid::from_u128(1), 0, 5),
            Err(AllocError::DuplicateAllocated)
        );
        assert!(check_allocate(&[], Uuid::from_u128(1), Uuid::from_u128(1), 0, 5).is_ok());
        assert_eq!(
            check_allocate(&[], Uuid::from_u128(1), Uuid::from_u128(1), 0, 0),
            Err(AllocError::ZeroAmount)
        );
    }

    #[test]
    fn reserve_coverage_counts_unconverted_real_money_lots() {
        let mut lots = vec![lot(1, 5_000_000, 1), lot(2, 2_000_000, 2)];
        lots[1].grant_class = GrantClass::Sweeps;
        assert_eq!(remaining_real_money_promise(&lots, &[]), 5_000_000);
        lots[0].converted_at = Some(OffsetDateTime::UNIX_EPOCH);
        assert_eq!(remaining_real_money_promise(&lots, &[]), 0);
    }

    #[test]
    fn plan_allocates_actual_fee_not_notional() {
        let lots = [lot(1, 5_000_000, 1)];
        let planned = plan_trade_allocations(&lots, &[], Uuid::from_u128(9), 1_000_000).unwrap();
        assert_eq!(planned.len(), 1);
        assert_eq!(planned[0].amount_micro, 1_000_000);
        assert_eq!(planned[0].kind, AllocationKind::Allocated);
    }

    async fn fund_reserve_and_house_credit(
        store: &crate::fakes::InMemoryStore,
        reserve: i64,
        house_credit: i64,
    ) {
        use crate::ports::Store;
        let mut tx = store.credit_convert_tx().await.unwrap();
        let ext = tx
            .account(OwnerRef::External, Currency::Usdc)
            .await
            .unwrap();
        let res = tx
            .account(OwnerRef::BonusReserve, Currency::Usdc)
            .await
            .unwrap();
        tx.ledger_apply(
            TxnKind::Seed,
            "bonus-reserve-topup",
            &[
                Entry {
                    account: ext,
                    amount: MicroUsd(-reserve),
                },
                Entry {
                    account: res,
                    amount: MicroUsd(reserve),
                },
            ],
        )
        .await
        .unwrap();
        let ext_c = tx
            .account(OwnerRef::External, Currency::UsdcCredit)
            .await
            .unwrap();
        let house_c = tx
            .account(OwnerRef::House, Currency::UsdcCredit)
            .await
            .unwrap();
        tx.ledger_apply(
            TxnKind::CreditGrant,
            "house-credit-seed",
            &[
                Entry {
                    account: ext_c,
                    amount: MicroUsd(-house_credit),
                },
                Entry {
                    account: house_c,
                    amount: MicroUsd(house_credit),
                },
            ],
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
    }

    #[tokio::test]
    async fn grant_without_reserve_is_refused() {
        let store = crate::fakes::InMemoryStore::new();
        let user =
            crate::fakes::InMemoryStore::add_user(&store, "g", OffsetDateTime::UNIX_EPOCH, 0);
        let err = GrantCredit { store: &store }
            .execute(GrantCreditCmd {
                user,
                amount: MicroUsd(5_000_000),
                source: "signup".into(),
                grant_class: GrantClass::RealMoney,
                policy_version: "1".into(),
                idempotency_key: "g1".into(),
                granted_at: OffsetDateTime::UNIX_EPOCH,
            })
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::InsufficientBonusReserve { .. }));
    }

    #[tokio::test]
    async fn grant_call_site_enforces_the_locked_user_gate() {
        let store = crate::fakes::InMemoryStore::new();
        fund_reserve_and_house_credit(&store, 10_000_000, 10_000_000).await;
        let user = crate::fakes::InMemoryStore::add_user(
            &store,
            "blocked-grant",
            OffsetDateTime::UNIX_EPOCH,
            0,
        );
        store.set_money_user(user, "banned", 2);
        let before = store.snapshot();
        let err = GrantCredit { store: &store }
            .execute(GrantCreditCmd {
                user,
                amount: MicroUsd(5_000_000),
                source: "signup".into(),
                grant_class: GrantClass::RealMoney,
                policy_version: "1".into(),
                idempotency_key: "blocked-grant".into(),
                granted_at: OffsetDateTime::UNIX_EPOCH,
            })
            .await
            .unwrap_err();
        assert_eq!(err, AppError::MoneyForbidden("user banned"));
        assert_eq!(before, store.snapshot());
    }

    #[tokio::test]
    async fn grant_replay_rejects_a_different_immutable_lot_shape() {
        let store = crate::fakes::InMemoryStore::new();
        fund_reserve_and_house_credit(&store, 10_000_000, 10_000_000).await;
        let user = crate::fakes::InMemoryStore::add_user(
            &store,
            "grant-replay-shape",
            OffsetDateTime::UNIX_EPOCH,
            0,
        );
        let command = GrantCreditCmd {
            user,
            amount: MicroUsd(5_000_000),
            source: "signup".into(),
            grant_class: GrantClass::RealMoney,
            policy_version: "1".into(),
            idempotency_key: "grant-replay-shape".into(),
            granted_at: OffsetDateTime::UNIX_EPOCH,
        };
        let first = GrantCredit { store: &store }
            .execute(command.clone())
            .await
            .unwrap();
        let replay = GrantCredit { store: &store }
            .execute(command.clone())
            .await
            .unwrap();
        assert!(replay.replayed);
        assert_eq!(replay.lot_id, first.lot_id);
        assert_eq!(replay.ledger_txn, first.ledger_txn);
        let mut changed = command;
        changed.amount = MicroUsd(4_000_000);
        assert_eq!(
            GrantCredit { store: &store }
                .execute(changed)
                .await
                .unwrap_err(),
            AppError::IdempotencyConflict
        );

        let original = GrantCreditCmd {
            user,
            amount: MicroUsd(5_000_000),
            source: "signup".into(),
            grant_class: GrantClass::RealMoney,
            policy_version: "1".into(),
            idempotency_key: "grant-replay-shape".into(),
            granted_at: OffsetDateTime::UNIX_EPOCH,
        };
        let mut variants = Vec::new();
        let mut source = original.clone();
        source.source = "different".into();
        variants.push(source);
        let mut class = original.clone();
        class.grant_class = GrantClass::Sweeps;
        variants.push(class);
        let mut policy = original.clone();
        policy.policy_version = "2".into();
        variants.push(policy);
        let mut timestamp = original;
        timestamp.granted_at += time::Duration::seconds(1);
        variants.push(timestamp);
        for variant in variants {
            assert_eq!(
                GrantCredit { store: &store }
                    .execute(variant)
                    .await
                    .unwrap_err(),
                AppError::IdempotencyConflict
            );
        }
    }

    #[tokio::test]
    async fn sweeps_grant_does_not_consume_cash_reserve_promise() {
        let store = crate::fakes::InMemoryStore::new();
        fund_reserve_and_house_credit(&store, 1, 5_000_000).await;
        let user = crate::fakes::InMemoryStore::add_user(
            &store,
            "sweeps-grant",
            OffsetDateTime::UNIX_EPOCH,
            0,
        );
        let receipt = GrantCredit { store: &store }
            .execute(GrantCreditCmd {
                user,
                amount: MicroUsd(5_000_000),
                source: "promotion".into(),
                grant_class: GrantClass::Sweeps,
                policy_version: "1".into(),
                idempotency_key: "sweeps-grant".into(),
                granted_at: OffsetDateTime::UNIX_EPOCH,
            })
            .await
            .unwrap();
        assert!(!receipt.replayed);
        let mut tx = store.credit_convert_tx().await.unwrap();
        assert_eq!(tx.remaining_real_money_promise().await.unwrap(), 0);
        assert_eq!(tx.bonus_reserve_balance().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn concurrent_grants_cannot_overbook_one_reserve_dollar() {
        let store = crate::fakes::InMemoryStore::new();
        fund_reserve_and_house_credit(&store, 5_000_000, 10_000_000).await;
        let a = crate::fakes::InMemoryStore::add_user(
            &store,
            "reserve-a",
            OffsetDateTime::UNIX_EPOCH,
            0,
        );
        let b = crate::fakes::InMemoryStore::add_user(
            &store,
            "reserve-b",
            OffsetDateTime::UNIX_EPOCH,
            0,
        );
        let grant_a = GrantCredit { store: &store };
        let grant_b = GrantCredit { store: &store };
        let make_cmd = |user, key: &str| GrantCreditCmd {
            user,
            amount: MicroUsd(5_000_000),
            source: "signup".into(),
            grant_class: GrantClass::RealMoney,
            policy_version: "1".into(),
            idempotency_key: key.into(),
            granted_at: OffsetDateTime::UNIX_EPOCH,
        };
        let (left, right) = tokio::join!(
            grant_a.execute(make_cmd(a, "reserve-a")),
            grant_b.execute(make_cmd(b, "reserve-b"))
        );
        assert_eq!(usize::from(left.is_ok()) + usize::from(right.is_ok()), 1);
        assert!(left.is_ok() || matches!(left, Err(AppError::InsufficientBonusReserve { .. })));
        assert!(right.is_ok() || matches!(right, Err(AppError::InsufficientBonusReserve { .. })));
        let mut tx = store.credit_convert_tx().await.unwrap();
        assert_eq!(tx.remaining_real_money_promise().await.unwrap(), 5_000_000);
        assert_eq!(tx.bonus_reserve_balance().await.unwrap(), 5_000_000);
    }

    #[tokio::test]
    async fn grants_serialize_on_and_never_exceed_the_daily_mint_cap() {
        let store = crate::fakes::InMemoryStore::new();
        fund_reserve_and_house_credit(&store, 20_000_000, 20_000_000).await;
        store.set_money_i64("bonus_mint_daily_cap_micro", 5_000_000);
        let a =
            crate::fakes::InMemoryStore::add_user(&store, "mint-a", OffsetDateTime::UNIX_EPOCH, 0);
        let b =
            crate::fakes::InMemoryStore::add_user(&store, "mint-b", OffsetDateTime::UNIX_EPOCH, 0);
        let grant = GrantCredit { store: &store };
        grant
            .execute(GrantCreditCmd {
                user: a,
                amount: MicroUsd(5_000_000),
                source: "signup".into(),
                grant_class: GrantClass::RealMoney,
                policy_version: "1".into(),
                idempotency_key: "mint-a".into(),
                granted_at: OffsetDateTime::UNIX_EPOCH,
            })
            .await
            .unwrap();
        let error = grant
            .execute(GrantCreditCmd {
                user: b,
                amount: MicroUsd(1),
                source: "signup".into(),
                grant_class: GrantClass::RealMoney,
                policy_version: "1".into(),
                idempotency_key: "mint-b".into(),
                granted_at: OffsetDateTime::UNIX_EPOCH,
            })
            .await
            .unwrap_err();
        assert_eq!(error, AppError::MoneyForbidden("bonus mint cap"));
    }

    #[tokio::test]
    async fn grant_then_provisional_fee_does_not_convert() {
        let store = crate::fakes::InMemoryStore::new();
        fund_reserve_and_house_credit(&store, 10_000_000, 10_000_000).await;
        let user =
            crate::fakes::InMemoryStore::add_user(&store, "g2", OffsetDateTime::UNIX_EPOCH, 0);
        let grant = GrantCredit { store: &store }
            .execute(GrantCreditCmd {
                user,
                amount: MicroUsd(5_000_000),
                source: "signup".into(),
                grant_class: GrantClass::RealMoney,
                policy_version: "1".into(),
                idempotency_key: "g2".into(),
                granted_at: OffsetDateTime::UNIX_EPOCH,
            })
            .await
            .unwrap();
        let mut tx = store.credit_convert_tx().await.unwrap();
        tx.insert_allocation(&allocated(10, 1, grant.lot_id.as_u128(), 0, 5_000_000))
            .await
            .unwrap();
        // allocated() uses lot_id as u128; rebuild with the real lot id.
        let _ = tx;
        let mut tx = store.credit_convert_tx().await.unwrap();
        tx.insert_allocation(&AllocationFact {
            id: Uuid::from_u128(10),
            trade_id: Uuid::from_u128(1),
            lot_id: grant.lot_id,
            split_seq: 0,
            amount_micro: 5_000_000,
            kind: AllocationKind::Allocated,
            source_allocation_id: None,
            idempotency_key: "wash".into(),
        })
        .await
        .unwrap();
        tx.commit().await.unwrap();
        let mut tx = store.credit_convert_tx().await.unwrap();
        let receipt = tx
            .convert_then_collect(user, OffsetDateTime::UNIX_EPOCH, "lock")
            .await
            .unwrap();
        assert_eq!(receipt.lots_converted, 0, "provisional never converts");
        assert_eq!(store.balance_of(OwnerRef::User(user), Currency::Usdc), None);
    }

    #[tokio::test]
    async fn finalized_then_lock_user_converts() {
        let store = crate::fakes::InMemoryStore::new();
        fund_reserve_and_house_credit(&store, 10_000_000, 10_000_000).await;
        let user =
            crate::fakes::InMemoryStore::add_user(&store, "g3", OffsetDateTime::UNIX_EPOCH, 0);
        let grant = GrantCredit { store: &store }
            .execute(GrantCreditCmd {
                user,
                amount: MicroUsd(5_000_000),
                source: "signup".into(),
                grant_class: GrantClass::RealMoney,
                policy_version: "1".into(),
                idempotency_key: "g3".into(),
                granted_at: OffsetDateTime::UNIX_EPOCH,
            })
            .await
            .unwrap();
        let src = AllocationFact {
            id: Uuid::from_u128(10),
            trade_id: Uuid::from_u128(1),
            lot_id: grant.lot_id,
            split_seq: 0,
            amount_micro: 5_000_000,
            kind: AllocationKind::Allocated,
            source_allocation_id: None,
            idempotency_key: "a".into(),
        };
        let fin = AllocationFact {
            id: Uuid::from_u128(11),
            trade_id: Uuid::from_u128(1),
            lot_id: grant.lot_id,
            split_seq: 0,
            amount_micro: 5_000_000,
            kind: AllocationKind::Finalized,
            source_allocation_id: Some(src.id),
            idempotency_key: "f".into(),
        };
        let mut tx = store.credit_convert_tx().await.unwrap();
        tx.insert_allocation(&src).await.unwrap();
        tx.insert_allocation(&fin).await.unwrap();
        tx.commit().await.unwrap();
        let mut tx = store.credit_convert_tx().await.unwrap();
        let receipt = tx
            .convert_then_collect(user, OffsetDateTime::UNIX_EPOCH, "lock")
            .await
            .unwrap();
        tx.commit().await.unwrap();
        assert_eq!(receipt.lots_converted, 1);
        assert_eq!(receipt.conversion_txns.len(), 1);
        assert!(!receipt.conversion_txns[0].replayed);
        let original_txns = receipt.conversion_txns[0];
        let mut replay_tx = store.credit_convert_tx().await.unwrap();
        let replay = replay_tx
            .convert_then_collect(user, OffsetDateTime::UNIX_EPOCH, "lock-replay")
            .await
            .unwrap();
        replay_tx.commit().await.unwrap();
        assert_eq!(replay.lots_converted, 0);
        assert_eq!(replay.conversion_txns.len(), 1);
        assert!(replay.conversion_txns[0].replayed);
        assert_eq!(
            replay.conversion_txns[0].retire_txn,
            original_txns.retire_txn
        );
        assert_eq!(replay.conversion_txns[0].pay_txn, original_txns.pay_txn);
        assert_eq!(
            store.balance_of(OwnerRef::User(user), Currency::Usdc),
            Some(MicroUsd(5_000_000))
        );
    }

    #[tokio::test]
    async fn next_credit_grant_user_lock_converts_ready_lot_before_grant() {
        let store = crate::fakes::InMemoryStore::new();
        fund_reserve_and_house_credit(&store, 10_000_000, 10_000_000).await;
        let user = crate::fakes::InMemoryStore::add_user(
            &store,
            "grant-lock-convert",
            OffsetDateTime::UNIX_EPOCH,
            0,
        );
        let first = GrantCredit { store: &store }
            .execute(GrantCreditCmd {
                user,
                amount: MicroUsd(5_000_000),
                source: "signup".into(),
                grant_class: GrantClass::RealMoney,
                policy_version: "1".into(),
                idempotency_key: "grant-lock-convert:first".into(),
                granted_at: OffsetDateTime::UNIX_EPOCH,
            })
            .await
            .unwrap();
        let allocated = AllocationFact {
            id: Uuid::new_v4(),
            trade_id: Uuid::new_v4(),
            lot_id: first.lot_id,
            split_seq: 0,
            amount_micro: 5_000_000,
            kind: AllocationKind::Allocated,
            source_allocation_id: None,
            idempotency_key: "grant-lock-convert:allocated".into(),
        };
        let finalized = AllocationFact {
            id: Uuid::new_v4(),
            trade_id: allocated.trade_id,
            lot_id: first.lot_id,
            split_seq: 0,
            amount_micro: 5_000_000,
            kind: AllocationKind::Finalized,
            source_allocation_id: Some(allocated.id),
            idempotency_key: "grant-lock-convert:finalized".into(),
        };
        let mut facts = store.credit_convert_tx().await.unwrap();
        facts.insert_allocation(&allocated).await.unwrap();
        facts.insert_allocation(&finalized).await.unwrap();
        facts.commit().await.unwrap();

        GrantCredit { store: &store }
            .execute(GrantCreditCmd {
                user,
                amount: MicroUsd(1_000_000),
                source: "retention".into(),
                grant_class: GrantClass::RealMoney,
                policy_version: "1".into(),
                idempotency_key: "grant-lock-convert:second".into(),
                granted_at: OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(1),
            })
            .await
            .unwrap();

        assert_eq!(
            store.balance_of(OwnerRef::User(user), Currency::Usdc),
            Some(MicroUsd(5_000_000))
        );
    }

    #[tokio::test]
    async fn voided_reverse_never_converts() {
        let lot = lot(1, 5_000_000, 1);
        let src = allocated(10, 1, 1, 0, 5_000_000);
        let rev = AllocationFact {
            id: Uuid::from_u128(12),
            trade_id: Uuid::from_u128(1),
            lot_id: Uuid::from_u128(1),
            split_seq: 0,
            amount_micro: 5_000_000,
            kind: AllocationKind::Reversed,
            source_allocation_id: Some(src.id),
            idempotency_key: "r".into(),
        };
        assert!(!lot_ready_to_convert(&lot, &[src, rev]));
    }
}
