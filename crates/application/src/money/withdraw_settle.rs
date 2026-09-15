//! Receipt verification and W9→W10 settle (`Withheld → External`,
//! keyed `withdraw-settle:<id>`).

use serde_json::json;

use crate::error::AppError;
use crate::model::{Event, NewNotification};
use crate::ports::{
    identity_a_holds, identity_b_holds, identity_c_holds, identity_d_holds, verify_chain_receipt,
    ChainReceipt, Clock, Combo, LandingState, OutboundSubject, RailIdentity, WithdrawStore,
    WithdrawalId, WithdrawalRow,
};

/// Settle a finalized withdrawal.
pub struct SettleWithdraw<'a, S: WithdrawStore, C: Clock> {
    pub store: &'a S,
    pub clock: &'a C,
    pub identity: &'a RailIdentity,
}

impl<S: WithdrawStore, C: Clock> SettleWithdraw<'_, S, C> {
    /// # Errors
    /// Receipt mismatch, illegal combination, or store failure.
    pub async fn execute(
        &self,
        id: WithdrawalId,
        receipt: ChainReceipt,
    ) -> Result<WithdrawalRow, AppError> {
        let user = self.store.withdrawal_user(id).await?;
        let mut tx = self.store.withdraw_tx().await?;
        tx.serialize_key(&format!("withdraw-settle:{}", id.0))
            .await?;
        tx.lock_user(user).await?;
        let mut row = tx.withdrawal_for_update(id).await?;
        if row.user != user {
            return Err(crate::error::StoreError::Invariant(
                "withdrawal owner changed after user lock",
            )
            .into());
        }
        if row.combo != Combo::W9 && row.combo != Combo::W10 {
            return Err(AppError::IllegalTransition);
        }
        let payment = tx
            .outbound_by_subject(OutboundSubject::Withdrawal, row.id.0)
            .await?
            .ok_or(crate::error::StoreError::NotFound("outbound payment"))?;
        super::withdraw_send::ensure_payment_matches(&payment, &row, self.identity)?;
        let attempts = tx.attempts_for(payment.id).await?;
        let attempt = attempts
            .iter()
            .find(|item| {
                item.landing_state == LandingState::Finalized && item.signature == receipt.signature
            })
            .ok_or(crate::error::StoreError::NotFound("finalized attempt"))?;
        if let Err(reason) = verify_chain_receipt(self.identity, &payment, attempt, &receipt) {
            return Err(AppError::ConfigInvalid {
                key: "receipt".into(),
                reason,
            });
        }
        if row.combo == Combo::W10 {
            return Ok(row);
        }
        let key = format!("withdraw-settle:{}", row.id.0);
        let settle_tx = match tx.apply_settle(row.amount_micro, &key).await {
            Ok(txn) => txn,
            Err(crate::error::StoreError::DuplicateKey) => {
                // The ledger leg and W9→W10 CAS share this transaction. Seeing
                // the key while the locked row is still W9 means prior state is
                // corrupt; claiming success would strand the row at W9.
                return Err(crate::error::StoreError::Invariant(
                    "settle key exists while withdrawal is W9",
                )
                .into());
            }
            Err(err) => return Err(err.into()),
        };
        row.settle_tx_id = Some(settle_tx);
        row.settled_at = Some(self.clock.now());
        row.combo = Combo::W10;
        if !tx.cas_withdrawal(id, Combo::W9, &row).await? {
            return Err(AppError::IllegalTransition);
        }
        tx.append_withdrawal_event(id, "settle", "machine", json!({"to": "W10"}))
            .await?;
        let seq = tx
            .record_event(Event {
                event_type: "WithdrawalSettled",
                aggregate_type: "withdrawal",
                aggregate_id: id.0,
                payload: json!({
                    "user_id": row.user.0.to_string(),
                    "amount_micro": row.amount_micro,
                    "dest": row.dest,
                    "signature": receipt.signature,
                }),
            })
            .await?;
        tx.insert_notification(NewNotification {
            user: row.user,
            notification_type: "withdrawal_settled".into(),
            market: None,
            payload: json!({
                "withdrawal_id": id.0.to_string(),
                "amount_micro": row.amount_micro,
            }),
            source_seq: seq,
            created_at: self.clock.now(),
        })
        .await?;
        let rows = tx.list_withdrawals().await?;
        let withheld = tx.withheld_balance().await?;
        debug_assert!(identity_a_holds(withheld, &rows));
        debug_assert!(identity_b_holds(&rows));
        debug_assert!(identity_c_holds(&rows));
        debug_assert!(identity_d_holds(&attempts, true));
        tx.commit().await?;
        Ok(row)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::too_many_lines, clippy::unwrap_used)]

    use super::*;
    use crate::model::{AdminContext, AdminRole, UserStatus};
    use crate::ports::withdraw_decide::{decide_machine, DecideCmd, DecideWithdraw};
    use crate::ports::withdraw_fakes::{
        dest_a, test_rail, FakeRails, FakeScreen, FakeWithdrawStore,
    };
    use crate::ports::withdraw_request::{RequestClock, RequestWithdraw};
    use crate::ports::withdraw_send::{FakeSigner, SendWithdraw};
    use crate::ports::RequestWithdrawCmd;
    use crate::ports::WithdrawStore;
    use crate::ports::{identity_a_holds, identity_e_replay_ok, ScreenVerdict};
    use std::net::IpAddr;
    use time::Duration;

    fn finance() -> AdminContext {
        AdminContext::Admin {
            token_digest: "fin-a".into(),
            role: AdminRole::Finance,
        }
    }

    async fn to_w9(store: &FakeWithdrawStore, identity: &RailIdentity) -> (WithdrawalId, String) {
        let user = store.seed_user(UserStatus::Active, 2);
        store.credit(user, 20_000_000);
        let now = store.now();
        let screen = FakeScreen {
            verdict: ScreenVerdict::Clear {
                checked_at: now,
                expires_at: now + Duration::hours(24),
                policy_version: "1".into(),
            },
            fail: false,
        };
        let receipt = RequestWithdraw {
            store,
            clock: &RequestClock(now),
            geo: &screen,
            sanctions: &screen,
        }
        .execute(RequestWithdrawCmd {
            user,
            amount_micro: 5_000_000,
            dest: dest_a(),
            client_ip: Some(IpAddr::from([203, 0, 113, 2])),
            idempotency_key: Some(format!("set-{}", user.0)),
        })
        .await
        .unwrap();
        let id = receipt.id.unwrap();
        decide_machine(store, &RequestClock(now), id).await.unwrap();
        DecideWithdraw {
            store,
            clock: &RequestClock(now),
            actor: finance(),
        }
        .execute(DecideCmd::FinanceApprove {
            id,
            reason: "go".into(),
        })
        .await
        .unwrap();
        let rails = FakeRails::default();
        let signer = FakeSigner::new("sig-settle");
        SendWithdraw {
            store,
            clock: &RequestClock(now),
            rails: &rails,
            signer: &signer,
            identity,
        }
        .execute(id)
        .await
        .unwrap();
        {
            let mut tx = store.withdraw_tx().await.unwrap();
            let mut row = tx.withdrawal_for_update(id).await.unwrap();
            let from = row.combo;
            row.combo = Combo::W9;
            tx.cas_withdrawal(id, from, &row).await.unwrap();
            let mut attempt = store.attempts().into_iter().next().unwrap();
            attempt.landing_state = LandingState::Finalized;
            tx.save_attempt(&attempt).await.unwrap();
            tx.commit().await.unwrap();
        }
        (id, "sig-settle".into())
    }

    #[tokio::test]
    async fn settle_moves_withheld_to_external_and_notifies() {
        let store = FakeWithdrawStore::new();
        let identity = test_rail();
        let (id, signature) = to_w9(&store, &identity).await;
        let before = store.withheld();
        let rails = FakeRails::default();
        let signer = FakeSigner::new("unused-terminal-replay");
        assert_eq!(
            SendWithdraw {
                store: &store,
                clock: &RequestClock(store.now()),
                rails: &rails,
                signer: &signer,
                identity: &identity,
            }
            .execute(id)
            .await
            .unwrap()
            .combo,
            Combo::W9
        );
        let row = SettleWithdraw {
            store: &store,
            clock: &RequestClock(store.now()),
            identity: &identity,
        }
        .execute(
            id,
            ChainReceipt {
                signature,
                mint: identity.usdc_mint.clone(),
                source: identity.treasury_token_account.clone(),
                dest_token_account: dest_a(),
                delta_micro: 5_000_000,
                commitment: "finalized".into(),
            },
        )
        .await
        .unwrap();
        assert_eq!(row.combo, Combo::W10);
        assert_eq!(store.withheld(), before - 5_000_000);
        assert_eq!(
            store.notifications()[0].notification_type,
            "withdrawal_settled"
        );
        assert!(identity_a_holds(store.withheld(), &store.withdrawals()));
        assert!(identity_b_holds(&store.withdrawals()));
        assert!(identity_c_holds(&store.withdrawals()));
        assert_eq!(
            SendWithdraw {
                store: &store,
                clock: &RequestClock(store.now()),
                rails: &rails,
                signer: &signer,
                identity: &identity,
            }
            .execute(id)
            .await
            .unwrap()
            .combo,
            Combo::W10
        );
        let replay = SettleWithdraw {
            store: &store,
            clock: &RequestClock(store.now()),
            identity: &identity,
        }
        .execute(
            id,
            ChainReceipt {
                signature: "sig-settle".into(),
                mint: identity.usdc_mint.clone(),
                source: identity.treasury_token_account.clone(),
                dest_token_account: dest_a(),
                delta_micro: 5_000_000,
                commitment: "finalized".into(),
            },
        )
        .await
        .unwrap();
        assert_eq!(replay.combo, Combo::W10);
        assert_eq!(store.withheld(), before - 5_000_000);
        let mismatched_replay = SettleWithdraw {
            store: &store,
            clock: &RequestClock(store.now()),
            identity: &identity,
        }
        .execute(
            id,
            ChainReceipt {
                signature: "different-signature".into(),
                mint: identity.usdc_mint.clone(),
                source: identity.treasury_token_account.clone(),
                dest_token_account: dest_a(),
                delta_micro: 5_000_000,
                commitment: "finalized".into(),
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(
            mismatched_replay,
            AppError::Store(crate::error::StoreError::NotFound("finalized attempt"))
        ));
        let _ = identity_e_replay_ok;
    }

    #[tokio::test]
    async fn bad_receipt_fields_are_rejected() {
        let store = FakeWithdrawStore::new();
        let identity = test_rail();
        let (id, signature) = to_w9(&store, &identity).await;
        let err = SettleWithdraw {
            store: &store,
            clock: &RequestClock(store.now()),
            identity: &identity,
        }
        .execute(
            id,
            ChainReceipt {
                signature,
                mint: "wrong".into(),
                source: identity.treasury_token_account.clone(),
                dest_token_account: dest_a(),
                delta_micro: 5_000_000,
                commitment: "finalized".into(),
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(err, AppError::ConfigInvalid { .. }));
    }

    #[tokio::test]
    async fn settle_refuses_illegal_state_and_a_stranded_ledger_key() {
        let store = FakeWithdrawStore::new();
        let identity = test_rail();
        let (id, signature) = to_w9(&store, &identity).await;
        let receipt = ChainReceipt {
            signature,
            mint: identity.usdc_mint.clone(),
            source: identity.treasury_token_account.clone(),
            dest_token_account: dest_a(),
            delta_micro: 5_000_000,
            commitment: "finalized".into(),
        };
        {
            let mut tx = store.withdraw_tx().await.unwrap();
            let mut row = tx.withdrawal_for_update(id).await.unwrap();
            row.combo = Combo::W6;
            tx.cas_withdrawal(id, Combo::W9, &row).await.unwrap();
            tx.commit().await.unwrap();
        }
        assert_eq!(
            SettleWithdraw {
                store: &store,
                clock: &RequestClock(store.now()),
                identity: &identity,
            }
            .execute(id, receipt.clone())
            .await
            .unwrap_err(),
            AppError::IllegalTransition
        );
        {
            let mut tx = store.withdraw_tx().await.unwrap();
            let mut row = tx.withdrawal_for_update(id).await.unwrap();
            row.combo = Combo::W9;
            tx.cas_withdrawal(id, Combo::W6, &row).await.unwrap();
            tx.apply_settle(5_000_000, &format!("withdraw-settle:{}", id.0))
                .await
                .unwrap();
            tx.commit().await.unwrap();
        }
        let error = SettleWithdraw {
            store: &store,
            clock: &RequestClock(store.now()),
            identity: &identity,
        }
        .execute(id, receipt)
        .await
        .unwrap_err();
        assert!(matches!(
            error,
            AppError::Store(crate::error::StoreError::Invariant(
                "settle key exists while withdrawal is W9"
            ))
        ));
    }

    #[tokio::test]
    async fn settle_owner_backend_and_cas_failures_leave_w9_intact() {
        fn receipt(identity: &RailIdentity, signature: String) -> ChainReceipt {
            ChainReceipt {
                signature,
                mint: identity.usdc_mint.clone(),
                source: identity.treasury_token_account.clone(),
                dest_token_account: dest_a(),
                delta_micro: 5_000_000,
                commitment: "finalized".into(),
            }
        }

        let identity = test_rail();
        let owner_store = FakeWithdrawStore::new();
        let (id, signature) = to_w9(&owner_store, &identity).await;
        let wrong_user = owner_store.seed_user(UserStatus::Active, 2);
        owner_store.override_withdrawal_user(id, wrong_user);
        let err = SettleWithdraw {
            store: &owner_store,
            clock: &RequestClock(owner_store.now()),
            identity: &identity,
        }
        .execute(id, receipt(&identity, signature))
        .await
        .unwrap_err();
        assert!(matches!(
            err,
            AppError::Store(crate::error::StoreError::Invariant(_))
        ));

        let backend_store = FakeWithdrawStore::new();
        let (id, signature) = to_w9(&backend_store, &identity).await;
        backend_store.fail_next_settle();
        let err = SettleWithdraw {
            store: &backend_store,
            clock: &RequestClock(backend_store.now()),
            identity: &identity,
        }
        .execute(id, receipt(&identity, signature))
        .await
        .unwrap_err();
        assert!(matches!(
            err,
            AppError::Store(crate::error::StoreError::Backend(_))
        ));
        assert_eq!(backend_store.withdrawals()[0].combo, Combo::W9);

        let cas_store = FakeWithdrawStore::new();
        let (id, signature) = to_w9(&cas_store, &identity).await;
        let before = cas_store.withheld();
        cas_store.force_next_cas_miss();
        assert_eq!(
            SettleWithdraw {
                store: &cas_store,
                clock: &RequestClock(cas_store.now()),
                identity: &identity,
            }
            .execute(id, receipt(&identity, signature))
            .await
            .unwrap_err(),
            AppError::IllegalTransition
        );
        assert_eq!(cas_store.withheld(), before);
        assert_eq!(cas_store.withdrawals()[0].combo, Combo::W9);
    }
}
