//! D30 — receivable collection: self-serve auto-collection at deposit time
//! (same transaction as the deposit) and the dual-controlled write-off
//! (codex r4 NEW-2: a single-principal economic transfer carries the full
//! unwind-grade protocol — one principal can never forgive a receivable).

use domain::ledger::{Entry, TxnKind};
use serde_json::json;
use uuid::Uuid;

use crate::error::AppError;
use crate::model::{
    AdminContext, MarketId, OwnerRef, ProposalStatus, RealizationFact, RealizationSource,
    ReceivableMovement, ReceivableMovementKind, UserId,
};
use crate::ports::{Clock, DepositTx, Store, WriteOffProposal};

use super::audit::{principal_digest, required_audit_for, OpsError, OpsPolicy};

/// Auto-collects `min(cash_after_deposit, Σ open receivables)` INSIDE the
/// caller's deposit transaction (grok r3 NEW-5). Returns the collected total
/// (0 when the user owes nothing). One cash transaction `user → house` keyed
/// off the deposit's idempotency key; one `Collected` movement per receivable
/// touched, oldest first.
///
/// # Errors
/// Ledger and store failures; the caller's transaction rolls back whole.
pub async fn auto_collect(
    tx: &mut (dyn DepositTx + '_),
    user: UserId,
    deposit_key: &str,
    actor: &str,
) -> Result<i64, AppError> {
    let open = tx.open_receivables_for_user(user).await?;
    let owed: i64 = open.iter().map(|row| row.outstanding_micro).sum();
    if owed == 0 {
        return Ok(0);
    }
    let user_account = tx
        .account(OwnerRef::User(user), domain::ledger::Currency::Usdc)
        .await?;
    let cash = tx.account_balance(user_account).await?.0.max(0);
    let collect = owed.min(cash);
    if collect == 0 {
        return Ok(0);
    }
    let house = tx
        .account(OwnerRef::House, domain::ledger::Currency::Usdc)
        .await?;
    let cash_txn = tx
        .ledger_apply(
            TxnKind::Reversal,
            &format!("recv-collect:{deposit_key}"),
            &[
                Entry {
                    account: user_account,
                    amount: domain::money::MicroUsd(-collect),
                },
                Entry {
                    account: house,
                    amount: domain::money::MicroUsd(collect),
                },
            ],
        )
        .await?;
    let mut remaining = collect;
    for row in open {
        if remaining == 0 {
            break;
        }
        let take = remaining.min(row.outstanding_micro);
        remaining -= take;
        tx.insert_receivable_movement(ReceivableMovement {
            id: Uuid::new_v4(),
            receivable: row.receivable.id,
            kind: ReceivableMovementKind::Collected,
            amount_micro: take,
            actor: actor.to_string(),
            cash_txn: Some(cash_txn),
            idempotency_key: format!("recv-collect:{deposit_key}:{}", row.receivable.id),
        })
        .await?;
    }
    Ok(collect)
}

#[derive(Debug, Clone)]
pub struct WriteOffCmd {
    pub receivable: Uuid,
    pub reason: String,
    pub idempotency_key: String,
}

/// Dual-controlled receivable write-off (propose → distinct-principal
/// confirm after ≥T delay; both audited; capped per item and per day).
pub struct WriteOffReceivable<'a, S: Store, C: Clock> {
    pub store: &'a S,
    pub clock: &'a C,
    pub policy: OpsPolicy,
    pub actor: AdminContext,
}

impl<S: Store, C: Clock> WriteOffReceivable<'_, S, C> {
    /// # Errors
    /// `NotFound` for an unknown receivable; [`OpsError::OverCap`] past the
    /// per-item cap; conflicts for duplicate proposals.
    pub async fn propose(&self, cmd: WriteOffCmd) -> Result<WriteOffProposal, OpsError> {
        let proposer = principal_digest(&self.actor)?.to_string();
        let mut tx = self.store.unwind_tx().await?;
        tx.serialize_key(&format!("write-off:{}", cmd.receivable))
            .await?;
        if let Some(existing) = tx.write_off_by_key(&cmd.idempotency_key).await? {
            return Ok(existing);
        }
        let row = tx
            .receivable_by_id(cmd.receivable)
            .await?
            .ok_or(AppError::Store(crate::error::StoreError::NotFound(
                "receivable",
            )))?;
        if row.outstanding_micro <= 0 {
            return Err(OpsError::App(AppError::ProposalConflict(
                "receivable is not open",
            )));
        }
        if row.outstanding_micro > self.policy.writeoff_per_item_cap_micro {
            return Err(OpsError::OverCap {
                cap_micro: self.policy.writeoff_per_item_cap_micro,
            });
        }
        let now = self.clock.now();
        let proposal = WriteOffProposal {
            id: Uuid::new_v4(),
            receivable: cmd.receivable,
            idempotency_key: cmd.idempotency_key.clone(),
            amount_micro: row.outstanding_micro,
            proposer_token_id: proposer,
            confirmer_token_id: None,
            reason: cmd.reason.clone(),
            status: ProposalStatus::Pending,
            confirm_not_before: self.policy.confirm_not_before(now),
        };
        tx.insert_write_off(proposal.clone()).await?;
        let row = required_audit_for(
            &self.actor,
            "receivable_write_off_propose",
            format!("receivable:{}", cmd.receivable),
            None,
            Some(json!({ "amount_micro": proposal.amount_micro })),
            Some(cmd.reason),
        );
        tx.audit_insert(row).await?;
        tx.commit().await?;
        Ok(proposal)
    }

    /// # Errors
    /// [`AppError::ProposalConflict`] for same-principal, early, expired or
    /// settled confirms; [`OpsError::OverCap`] past the daily cap.
    #[allow(clippy::too_many_lines)]
    pub async fn confirm(&self, cmd: WriteOffCmd) -> Result<WriteOffProposal, OpsError> {
        let confirmer = principal_digest(&self.actor)?.to_string();
        let mut tx = self.store.unwind_tx().await?;
        tx.serialize_key(&format!("write-off:{}", cmd.receivable))
            .await?;
        let mut proposal = tx
            .write_off_by_key(&cmd.idempotency_key)
            .await?
            .ok_or(AppError::ProposalConflict("no write-off proposed"))?;
        if proposal.status == ProposalStatus::Confirmed {
            return Ok(proposal);
        }
        if proposal.status != ProposalStatus::Pending {
            return Err(OpsError::App(AppError::ProposalConflict(
                "write-off is not pending",
            )));
        }
        if proposal.proposer_token_id == confirmer {
            return Err(OpsError::App(AppError::ProposalConflict(
                "confirm requires a distinct principal",
            )));
        }
        let now = self.clock.now();
        if now < proposal.confirm_not_before {
            return Err(OpsError::App(AppError::ProposalConflict(
                "dual-control delay has not elapsed",
            )));
        }
        if now > self.policy.expires_at(proposal.confirm_not_before) {
            proposal.status = ProposalStatus::Expired;
            tx.save_write_off(&proposal).await?;
            tx.commit().await?;
            return Err(OpsError::App(AppError::ProposalConflict(
                "write-off expired",
            )));
        }
        let written_today = tx.written_off_since(day_start(now)).await?;
        if written_today.saturating_add(proposal.amount_micro)
            > self.policy.writeoff_daily_cap_micro
        {
            return Err(OpsError::OverCap {
                cap_micro: self.policy.writeoff_daily_cap_micro,
            });
        }
        let row = tx
            .receivable_by_id(proposal.receivable)
            .await?
            .ok_or(AppError::Store(crate::error::StoreError::NotFound(
                "receivable",
            )))?;
        let amount = proposal.amount_micro.min(row.outstanding_micro);
        if amount <= 0 {
            return Err(OpsError::App(AppError::ProposalConflict(
                "receivable is no longer open",
            )));
        }
        tx.insert_receivable_movement(ReceivableMovement {
            id: Uuid::new_v4(),
            receivable: proposal.receivable,
            kind: ReceivableMovementKind::WrittenOff,
            amount_micro: amount,
            actor: confirmer.clone(),
            cash_txn: None,
            idempotency_key: format!("write-off:{}", proposal.id),
        })
        .await?;
        // Compensating realization fact (insert-once on the origin reversal
        // transaction; the linked movement is the authoritative subledger
        // record). The market's YES outcome anchors the fact.
        let market_row = tx
            .market_for_update(row.receivable.market)
            .await
            .map_err(AppError::from)?;
        tx.insert_realization(&RealizationFact {
            user: row.receivable.user,
            market: row.receivable.market,
            outcome: market_row.yes_outcome,
            source: RealizationSource::Void,
            realized_delta: domain::money::MicroUsd(amount),
            payout: domain::money::MicroUsd(0),
            ledger_txn: row.receivable.origin_reversal_txn,
            created_at: now,
        })
        .await?;
        proposal.status = ProposalStatus::Confirmed;
        proposal.confirmer_token_id = Some(confirmer);
        tx.save_write_off(&proposal).await?;
        let audit = required_audit_for(
            &self.actor,
            "receivable_write_off_confirm",
            format!("receivable:{}", proposal.receivable),
            Some(json!({ "outstanding_micro": row.outstanding_micro })),
            Some(json!({ "written_off_micro": amount })),
            Some(cmd.reason),
        );
        tx.audit_insert(audit).await?;
        tx.commit().await?;
        Ok(proposal)
    }
}

/// UTC midnight for the daily-cap window.
#[must_use]
pub fn day_start(now: time::OffsetDateTime) -> time::OffsetDateTime {
    now.replace_time(time::Time::MIDNIGHT)
}

/// Marker helper so unwind reversals and reports can name a market's
/// receivable subjects uniformly.
#[must_use]
pub fn receivable_subject(market: MarketId) -> String {
    format!("market:{}", market.0)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::fakes::{FakeClock, InMemoryStore};
    use crate::model::{AdminRole, Receivable};
    use crate::ports::{OpsQueries, Store, WithdrawalEligibility};
    use domain::money::MicroUsd;
    use time::{Duration, OffsetDateTime};

    fn finance() -> AdminContext {
        AdminContext::Admin {
            token_digest: "digest-finance".into(),
            role: AdminRole::Finance,
        }
    }

    fn superadmin() -> AdminContext {
        AdminContext::Admin {
            token_digest: "digest-superadmin".into(),
            role: AdminRole::Superadmin,
        }
    }

    /// Opens a receivable directly through the port (the unwind tests cover
    /// the full reversal path; here the subledger is the subject).
    async fn open_receivable(store: &InMemoryStore, opened: i64) -> (UserId, Uuid) {
        let user = store.add_user(
            "debtor",
            OffsetDateTime::from_unix_timestamp(1_600_000_000).unwrap(),
            0,
        );
        let market = store
            .add_market(
                "recv-market",
                domain::market::MarketState::Voided,
                OffsetDateTime::UNIX_EPOCH,
                OffsetDateTime::UNIX_EPOCH,
                domain::money::MicroShares(1_000_000),
                domain::money::BasisPoints(0),
            )
            .unwrap()
            .id;
        let receivable = Receivable {
            id: Uuid::new_v4(),
            market,
            user,
            origin_reversal_txn: Uuid::new_v4(),
            opened_micro: opened,
        };
        let mut tx = store.unwind_tx().await.unwrap();
        tx.insert_receivable(receivable).await.unwrap();
        tx.insert_receivable_movement(ReceivableMovement {
            id: Uuid::new_v4(),
            receivable: receivable.id,
            kind: ReceivableMovementKind::Opened,
            amount_micro: opened,
            actor: "test".into(),
            cash_txn: None,
            idempotency_key: format!("open:{}", receivable.id),
        })
        .await
        .unwrap();
        tx.commit().await.unwrap();
        (user, receivable.id)
    }

    fn uc<'a>(
        store: &'a InMemoryStore,
        clock: &'a FakeClock,
        actor: AdminContext,
    ) -> WriteOffReceivable<'a, InMemoryStore, FakeClock> {
        WriteOffReceivable {
            store,
            clock,
            policy: OpsPolicy::default(),
            actor,
        }
    }

    fn cmd(receivable: Uuid, key: &str) -> WriteOffCmd {
        WriteOffCmd {
            receivable,
            reason: "uncollectable".to_string(),
            idempotency_key: key.to_string(),
        }
    }

    #[tokio::test]
    async fn one_principal_can_never_forgive_a_receivable() {
        let store = InMemoryStore::new();
        let clock = FakeClock::at(OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap());
        let (user, receivable) = open_receivable(&store, 40_000_000).await;
        let proposal = uc(&store, &clock, finance())
            .propose(cmd(receivable, "wo-1"))
            .await
            .unwrap();
        assert_eq!(proposal.status, crate::model::ProposalStatus::Pending);
        clock.advance(Duration::seconds(61));
        // The SAME token cannot confirm its own proposal.
        let same = uc(&store, &clock, finance())
            .confirm(cmd(receivable, "wo-1"))
            .await
            .unwrap_err();
        assert_eq!(
            same,
            OpsError::App(AppError::ProposalConflict(
                "confirm requires a distinct principal"
            ))
        );
        // The debtor still owes; nothing moved.
        let eligibility = crate::ports::WithdrawalEligibility::withdrawal_eligibility(&store, user)
            .await
            .unwrap();
        assert_eq!(eligibility.open_receivables_micro, 40_000_000);
        // A DISTINCT principal confirms; the movement + audits land.
        let confirmed = uc(&store, &clock, superadmin())
            .confirm(cmd(receivable, "wo-1"))
            .await
            .unwrap();
        assert_eq!(confirmed.status, crate::model::ProposalStatus::Confirmed);
        let after = crate::ports::WithdrawalEligibility::withdrawal_eligibility(&store, user)
            .await
            .unwrap();
        assert_eq!(after.open_receivables_micro, 0);
        assert!(after.eligible);
        let audits = store.audit_page(None, 10).await.unwrap();
        let actions: Vec<&str> = audits
            .iter()
            .map(|row| row.action.action.as_str())
            .collect();
        assert!(actions.contains(&"receivable_write_off_propose"));
        assert!(actions.contains(&"receivable_write_off_confirm"));
        // Idempotent confirmation replays.
        let replay = uc(&store, &clock, superadmin())
            .confirm(cmd(receivable, "wo-1"))
            .await
            .unwrap();
        assert_eq!(replay.status, crate::model::ProposalStatus::Confirmed);
    }

    #[tokio::test]
    async fn write_off_caps_and_delay_are_pinned() {
        let store = InMemoryStore::new();
        let clock = FakeClock::at(OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap());
        // Per-item cap: $500 pinned — a $600 receivable cannot even propose.
        let (_, too_big) = open_receivable(&store, 600_000_000).await;
        let denied = uc(&store, &clock, finance())
            .propose(cmd(too_big, "wo-big"))
            .await
            .unwrap_err();
        assert_eq!(
            denied,
            OpsError::OverCap {
                cap_micro: 500_000_000
            }
        );
        // Delay: an in-window confirm is refused.
        let store2 = InMemoryStore::new();
        let (_, receivable) = open_receivable(&store2, 10_000_000).await;
        uc(&store2, &clock, finance())
            .propose(cmd(receivable, "wo-2"))
            .await
            .unwrap();
        let early = uc(&store2, &clock, superadmin())
            .confirm(cmd(receivable, "wo-2"))
            .await
            .unwrap_err();
        assert_eq!(
            early,
            OpsError::App(AppError::ProposalConflict(
                "dual-control delay has not elapsed"
            ))
        );
        // Expiry: past confirm_not_before + TTL the proposal expires.
        clock.advance(Duration::seconds(61 + 901));
        let expired = uc(&store2, &clock, superadmin())
            .confirm(cmd(receivable, "wo-2"))
            .await
            .unwrap_err();
        assert_eq!(
            expired,
            OpsError::App(AppError::ProposalConflict("write-off expired"))
        );
        assert!(matches!(
            uc(&store2, &clock, superadmin())
                .confirm(cmd(receivable, "wo-2"))
                .await,
            Err(OpsError::App(AppError::ProposalConflict(
                "write-off is not pending"
            )))
        ));
    }

    #[tokio::test]
    async fn partial_collection_keeps_outstanding_derived() {
        let store = InMemoryStore::new();
        let (user, receivable) = open_receivable(&store, 50_000_000).await;
        store.fund_user(user, MicroUsd(20_000_000)).unwrap();
        let mut tx = store.deposit_tx().await.unwrap();
        tx.serialize_key("collect-1").await.unwrap();
        let collected = auto_collect(tx.as_mut(), user, "collect-1", "machine:test")
            .await
            .unwrap();
        tx.commit().await.unwrap();
        assert_eq!(collected, 20_000_000, "collects only available cash");
        let view = crate::ports::WithdrawalEligibility::withdrawal_eligibility(&store, user)
            .await
            .unwrap();
        assert_eq!(view.open_receivables_micro, 30_000_000);
        assert_eq!(view.cash_micro, 0);
        assert!(!view.eligible);
        let _ = receivable;
    }

    #[tokio::test]
    async fn movements_are_append_only_with_unique_keys() {
        let store = InMemoryStore::new();
        let (_, receivable) = open_receivable(&store, 10_000_000).await;
        let mut tx = store.unwind_tx().await.unwrap();
        let movement = ReceivableMovement {
            id: Uuid::new_v4(),
            receivable,
            kind: ReceivableMovementKind::Collected,
            amount_micro: 1_000_000,
            actor: "test".into(),
            cash_txn: None,
            idempotency_key: "dup-key".into(),
        };
        tx.insert_receivable_movement(movement.clone())
            .await
            .unwrap();
        let duplicate = tx
            .insert_receivable_movement(ReceivableMovement {
                id: Uuid::new_v4(),
                ..movement
            })
            .await
            .unwrap_err();
        assert_eq!(
            duplicate,
            crate::error::StoreError::Conflict("receivable movement key")
        );
    }

    #[tokio::test]
    async fn zero_cash_replay_closed_and_daily_cap_paths_are_explicit() {
        let store = InMemoryStore::new();
        let clock = FakeClock::at(OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap());
        let (user, receivable) = open_receivable(&store, 10_000_000).await;
        store.fund_user(user, MicroUsd(1_000_000)).unwrap();
        let mut collect = store.deposit_tx().await.unwrap();
        assert_eq!(
            auto_collect(collect.as_mut(), user, "first", "machine:test")
                .await
                .unwrap(),
            1_000_000
        );
        collect.commit().await.unwrap();
        let mut empty = store.deposit_tx().await.unwrap();
        assert_eq!(
            auto_collect(empty.as_mut(), user, "empty", "machine:test")
                .await
                .unwrap(),
            0
        );

        let proposal = uc(&store, &clock, finance())
            .propose(cmd(receivable, "wo-replay"))
            .await
            .unwrap();
        let replay = uc(&store, &clock, finance())
            .propose(cmd(receivable, "wo-replay"))
            .await
            .unwrap();
        assert_eq!(proposal.id, replay.id);
        clock.advance(Duration::seconds(61));
        let capped = WriteOffReceivable {
            store: &store,
            clock: &clock,
            policy: OpsPolicy {
                writeoff_daily_cap_micro: 0,
                ..OpsPolicy::default()
            },
            actor: superadmin(),
        }
        .confirm(cmd(receivable, "wo-replay"))
        .await
        .unwrap_err();
        assert!(matches!(capped, OpsError::OverCap { cap_micro: 0 }));

        store.fund_user(user, MicroUsd(9_000_000)).unwrap();
        let mut paid = store.deposit_tx().await.unwrap();
        assert_eq!(
            auto_collect(paid.as_mut(), user, "paid", "machine:test")
                .await
                .unwrap(),
            9_000_000
        );
        paid.commit().await.unwrap();
        assert!(matches!(
            uc(&store, &clock, superadmin())
                .confirm(cmd(receivable, "wo-replay"))
                .await,
            Err(OpsError::App(AppError::ProposalConflict(
                "receivable is no longer open"
            )))
        ));
        assert!(matches!(
            uc(&store, &clock, finance())
                .propose(cmd(receivable, "wo-closed"))
                .await,
            Err(OpsError::App(AppError::ProposalConflict(
                "receivable is not open"
            )))
        ));
        assert_eq!(
            receivable_subject(crate::model::MarketId(uuid::Uuid::nil())),
            format!("market:{}", uuid::Uuid::nil())
        );
    }

    #[tokio::test]
    async fn collection_stops_once_cash_is_exhausted_before_later_receivables() {
        let store = InMemoryStore::new();
        let (user, _) = open_receivable(&store, 3_000_000).await;
        let market = store
            .add_market(
                "recv-market-two",
                domain::market::MarketState::Voided,
                OffsetDateTime::UNIX_EPOCH,
                OffsetDateTime::UNIX_EPOCH,
                domain::money::MicroShares(1),
                domain::money::BasisPoints(0),
            )
            .unwrap()
            .id;
        let second = Receivable {
            id: Uuid::new_v4(),
            market,
            user,
            origin_reversal_txn: Uuid::new_v4(),
            opened_micro: 5_000_000,
        };
        let mut tx = store.unwind_tx().await.unwrap();
        tx.insert_receivable(second).await.unwrap();
        tx.insert_receivable_movement(ReceivableMovement {
            id: Uuid::new_v4(),
            receivable: second.id,
            kind: ReceivableMovementKind::Opened,
            amount_micro: second.opened_micro,
            actor: "test".into(),
            cash_txn: None,
            idempotency_key: "second-open".into(),
        })
        .await
        .unwrap();
        tx.commit().await.unwrap();
        let mut view = store.deposit_tx().await.unwrap();
        let first_owed = view.open_receivables_for_user(user).await.unwrap()[0].outstanding_micro;
        drop(view);
        store.fund_user(user, MicroUsd(first_owed)).unwrap();
        let mut collect = store.deposit_tx().await.unwrap();
        assert_eq!(
            auto_collect(collect.as_mut(), user, "one-row", "machine:test")
                .await
                .unwrap(),
            first_owed
        );
        collect.commit().await.unwrap();
        assert!(
            store
                .withdrawal_eligibility(user)
                .await
                .unwrap()
                .open_receivables_micro
                > 0
        );
    }
}
