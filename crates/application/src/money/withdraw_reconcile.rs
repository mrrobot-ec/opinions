//! Reconciler: mark-unknown, observe-finalized, 2-of-3 archival non-landing.

use serde_json::json;

use super::withdraw_send::WithdrawSigner;
use crate::error::{AppError, StoreError};
use crate::model::Event;
use crate::ports::{
    non_landing_verdict, verify_chain_receipt, ChainReceipt, Clock, Combo, LandingState,
    NonLandingVerdict, OutboundSubject, QuorumObservation, RailIdentity, SignaturePresence,
    WithdrawStore, WithdrawTx, WithdrawalId, WithdrawalRow,
};

/// Reconcile in-flight withdrawals.
pub struct ReconcileWithdraw<'a, S: WithdrawStore, C: Clock> {
    pub store: &'a S,
    pub clock: &'a C,
    pub identity: &'a RailIdentity,
}

impl<S: WithdrawStore, C: Clock> ReconcileWithdraw<'_, S, C> {
    /// W6 → W7. Funds stay Withheld. W3 must use [`Self::recover_w3`]
    /// because the signature lookup has precedence over classification.
    ///
    /// # Errors
    /// Illegal combination or store failure.
    pub async fn mark_unknown(&self, id: WithdrawalId) -> Result<WithdrawalRow, AppError> {
        let user = self.store.withdrawal_user(id).await?;
        let mut tx = self.store.withdraw_tx().await?;
        tx.serialize_key(&format!("withdraw-recon:{}", id.0))
            .await?;
        tx.lock_user(user).await?;
        let mut row = tx.withdrawal_for_update(id).await?;
        ensure_locked_owner(&row, user)?;
        let from = row.combo;
        if from == Combo::W7 {
            return Ok(row);
        }
        if from != Combo::W6 && from != Combo::W3 {
            return Err(AppError::IllegalTransition);
        }
        let payment = tx
            .outbound_by_subject(OutboundSubject::Withdrawal, row.id.0)
            .await?
            .ok_or(StoreError::NotFound("outbound payment"))?;
        let mut attempt = tx
            .live_attempt(payment.id)
            .await?
            .ok_or(StoreError::NotFound("outbound attempt"))?;
        if from == Combo::W3 {
            let lease_expires_at = attempt
                .lease_expires_at
                .ok_or(StoreError::Invariant("W3 attempt has no send lease"))?;
            if self.clock.now() < lease_expires_at {
                return Err(AppError::ProposalConflict("send lease still held"));
            }
            return Err(AppError::ProposalConflict(
                "W3 recovery requires signature lookup",
            ));
        }
        attempt.landing_state = LandingState::Unknown;
        tx.save_attempt(&attempt).await?;
        row.combo = Combo::W7;
        cas(tx.as_mut(), from, &row, "mark-unknown").await?;
        tx.commit().await?;
        Ok(row)
    }

    /// Recover an expired W3 prepared attempt with signature-lookup
    /// precedence. A hit becomes W6; an absent-but-live blockhash stays W3
    /// for same-bytes rebroadcast; expired absence or an unavailable lookup
    /// becomes W7 with funds still Withheld.
    ///
    /// # Errors
    /// Illegal state, live lease, store, or signer failure.
    pub async fn recover_w3(
        &self,
        id: WithdrawalId,
        signer: &dyn WithdrawSigner,
    ) -> Result<WithdrawalRow, AppError> {
        let user = self.store.withdrawal_user(id).await?;
        let mut tx = self.store.withdraw_tx().await?;
        tx.serialize_key(&format!("withdraw-recon:{}", id.0))
            .await?;
        tx.lock_user(user).await?;
        let mut row = tx.withdrawal_for_update(id).await?;
        ensure_locked_owner(&row, user)?;
        if row.combo == Combo::W6 || row.combo == Combo::W7 {
            return Ok(row);
        }
        if row.combo != Combo::W3 {
            return Err(AppError::IllegalTransition);
        }
        let payment = tx
            .outbound_by_subject(OutboundSubject::Withdrawal, row.id.0)
            .await?
            .ok_or(StoreError::NotFound("outbound payment"))?;
        super::withdraw_send::ensure_payment_matches(&payment, &row, self.identity)?;
        let mut attempt = tx
            .live_attempt(payment.id)
            .await?
            .ok_or(StoreError::NotFound("outbound attempt"))?;
        if attempt.landing_state != LandingState::Prepared {
            return Err(StoreError::Invariant("W3 recovery attempt is not prepared").into());
        }
        let lease_expires_at = attempt
            .lease_expires_at
            .ok_or(StoreError::Invariant("W3 attempt has no send lease"))?;
        if self.clock.now() < lease_expires_at {
            return Err(AppError::ProposalConflict("send lease still held"));
        }

        let presence = signer
            .lookup_signature(&attempt.signature)
            .await
            .unwrap_or(SignaturePresence::Unknown);
        let presence_name = match presence {
            SignaturePresence::Present => {
                attempt.landing_state = LandingState::Broadcast;
                attempt.evidence = Some(json!({
                    "purpose": "w3-lease-recovery",
                    "observed_at": self.clock.now(),
                    "signature_presence": "present",
                }));
                tx.save_attempt(&attempt).await?;
                row.combo = Combo::W6;
                row.sent_at.get_or_insert_with(|| self.clock.now());
                cas(tx.as_mut(), Combo::W3, &row, "lease-lookup-hit").await?;
                tx.commit().await?;
                return Ok(row);
            }
            SignaturePresence::Absent => "absent",
            SignaturePresence::Unknown => "unknown",
        };

        let finalized_height = signer.finalized_block_height().await.ok();
        if presence == SignaturePresence::Absent
            && finalized_height.is_some_and(|height| height <= attempt.last_valid_block_height)
        {
            return Err(AppError::ProposalConflict(
                "prepared attempt still requires same-bytes rebroadcast",
            ));
        }
        attempt.landing_state = LandingState::Unknown;
        attempt.evidence = Some(json!({
            "purpose": "w3-lease-recovery",
            "observed_at": self.clock.now(),
            "signature_presence": presence_name,
            "finalized_height": finalized_height,
            "last_valid_block_height": attempt.last_valid_block_height,
        }));
        tx.save_attempt(&attempt).await?;
        row.combo = Combo::W7;
        cas(tx.as_mut(), Combo::W3, &row, "lease-lookup-unknown").await?;
        tx.commit().await?;
        Ok(row)
    }

    /// W6/W7/W8 → W9 after receipt verification.
    ///
    /// # Errors
    /// Receipt mismatch or store failure.
    pub async fn observe_finalized(
        &self,
        id: WithdrawalId,
        receipt: ChainReceipt,
    ) -> Result<WithdrawalRow, AppError> {
        let user = self.store.withdrawal_user(id).await?;
        let mut tx = self.store.withdraw_tx().await?;
        tx.serialize_key(&format!("withdraw-recon:{}", id.0))
            .await?;
        tx.lock_user(user).await?;
        let mut row = tx.withdrawal_for_update(id).await?;
        ensure_locked_owner(&row, user)?;
        let from = row.combo;
        if !matches!(
            from,
            Combo::W6 | Combo::W7 | Combo::W8 | Combo::W9 | Combo::W10
        ) {
            return Err(AppError::IllegalTransition);
        }
        let payment = tx
            .outbound_by_subject(OutboundSubject::Withdrawal, row.id.0)
            .await?
            .ok_or(StoreError::NotFound("outbound payment"))?;
        super::withdraw_send::ensure_payment_matches(&payment, &row, self.identity)?;
        let mut attempts = tx.attempts_for(payment.id).await?;
        let Some(attempt) = attempts
            .iter_mut()
            .find(|attempt| attempt.signature == receipt.signature)
        else {
            return Err(StoreError::NotFound("outbound attempt").into());
        };
        if let Err(reason) = verify_chain_receipt(self.identity, &payment, attempt, &receipt) {
            return Err(AppError::ConfigInvalid {
                key: "receipt".into(),
                reason,
            });
        }
        if matches!(from, Combo::W9 | Combo::W10) {
            if attempt.landing_state != LandingState::Finalized {
                return Err(
                    StoreError::Invariant("finalized withdrawal has no finalized attempt").into(),
                );
            }
            return Ok(row);
        }
        attempt.landing_state = LandingState::Finalized;
        tx.save_attempt(attempt).await?;
        row.combo = Combo::W9;
        cas(tx.as_mut(), from, &row, "observe-finalized").await?;
        tx.commit().await?;
        Ok(row)
    }

    /// W7 (or W8 after proof) → W15 when 2-of-3 prove non-landing.
    ///
    /// # Errors
    /// Predicate unknown, illegal combo, or store failure.
    pub async fn prove_non_landing(
        &self,
        id: WithdrawalId,
        observations: &[QuorumObservation],
    ) -> Result<WithdrawalRow, AppError> {
        let user = self.store.withdrawal_user(id).await?;
        let mut tx = self.store.withdraw_tx().await?;
        tx.serialize_key(&format!("withdraw-recon:{}", id.0))
            .await?;
        tx.lock_user(user).await?;
        let mut row = tx.withdrawal_for_update(id).await?;
        if row.user != user {
            return Err(StoreError::Invariant("withdrawal owner changed after user lock").into());
        }
        let from = row.combo;
        if from == Combo::W15 {
            return Ok(row);
        }
        if from != Combo::W7 && from != Combo::W8 {
            return Err(AppError::IllegalTransition);
        }
        let payment = tx
            .outbound_by_subject(OutboundSubject::Withdrawal, row.id.0)
            .await?
            .ok_or(StoreError::NotFound("outbound payment"))?;
        super::withdraw_send::ensure_payment_matches(&payment, &row, self.identity)?;
        let mut attempt = tx
            .live_attempt(payment.id)
            .await?
            .ok_or(StoreError::NotFound("outbound attempt"))?;
        let verdict = safe_non_landing_verdict(
            attempt.last_valid_block_height,
            observations,
            &self.identity.rpc_endpoints,
        );
        attempt.evidence = Some(json!({
            "observed_at": self.clock.now(),
            "last_valid_block_height": attempt.last_valid_block_height,
            "observations": observations.iter().map(|item| json!({
                "endpoint": item.endpoint,
                "finalized_height": item.finalized_height,
                "signature_present": item.signature_present,
                "pruned": item.pruned,
            })).collect::<Vec<_>>(),
            "verdict": match verdict {
                NonLandingVerdict::DefinitiveFailed => "definitive_failed",
                NonLandingVerdict::Unknown => "unknown",
            },
        }));
        if verdict == NonLandingVerdict::Unknown {
            attempt.landing_state = LandingState::Unknown;
            tx.save_attempt(&attempt).await?;
            if from != Combo::W7 {
                row.combo = Combo::W7;
                cas(tx.as_mut(), from, &row, "non-landing-unknown").await?;
            }
            tx.commit().await?;
            return Ok(row);
        }
        let key = format!("withdraw-release:{}", row.id.0);
        let release = tx.apply_release(row.user, row.amount_micro, &key).await?;
        attempt.landing_state = LandingState::DefinitiveFailed;
        tx.save_attempt(&attempt).await?;
        row.release_tx_id = Some(release);
        row.combo = Combo::W15;
        cas(tx.as_mut(), from, &row, "definitive-fail").await?;
        tx.commit().await?;
        Ok(row)
    }
}

pub(super) fn safe_non_landing_verdict(
    last_valid: i64,
    observations: &[QuorumObservation],
    expected_endpoints: &[String],
) -> NonLandingVerdict {
    let endpoints: std::collections::BTreeSet<_> = observations
        .iter()
        .map(|observation| observation.endpoint.as_str())
        .collect();
    let expected: std::collections::BTreeSet<_> =
        expected_endpoints.iter().map(String::as_str).collect();
    if observations.len() != 3
        || endpoints.len() != 3
        || endpoints != expected
        || observations
            .iter()
            .any(|observation| observation.signature_present == Some(true))
    {
        return NonLandingVerdict::Unknown;
    }
    non_landing_verdict(last_valid, observations)
}

fn ensure_locked_owner(row: &WithdrawalRow, user: crate::model::UserId) -> Result<(), AppError> {
    if row.user != user {
        return Err(StoreError::Invariant("withdrawal owner changed after user lock").into());
    }
    Ok(())
}

async fn cas(
    tx: &mut dyn WithdrawTx,
    from: Combo,
    row: &WithdrawalRow,
    kind: &str,
) -> Result<(), AppError> {
    if !tx.cas_withdrawal(row.id, from, row).await? {
        return Err(AppError::IllegalTransition);
    }
    tx.append_withdrawal_event(row.id, kind, "machine", json!({"to": row.combo.label()}))
        .await?;
    let _ = tx
        .record_event(Event {
            event_type: "WithdrawalReconciled",
            aggregate_type: "withdrawal",
            aggregate_id: row.id.0,
            payload: json!({"to": row.combo.label()}),
        })
        .await?;
    Ok(())
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
    use crate::ports::ScreenVerdict;
    use std::net::IpAddr;
    use time::Duration;

    fn finance() -> AdminContext {
        AdminContext::Admin {
            token_digest: "fin-a".into(),
            role: AdminRole::Finance,
        }
    }

    async fn to_w6(store: &FakeWithdrawStore, identity: &RailIdentity) -> WithdrawalId {
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
            client_ip: Some(IpAddr::from([203, 0, 113, 3])),
            idempotency_key: Some(format!("r-{}", user.0)),
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
        let signer = FakeSigner::new("sig-recon");
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
        id
    }

    fn fail_obs(endpoint: &str) -> QuorumObservation {
        QuorumObservation {
            endpoint: endpoint.into(),
            finalized_height: Some(9_999),
            signature_present: Some(false),
            pruned: false,
        }
    }

    #[tokio::test]
    async fn mark_unknown_then_observe_or_definitive_fail() {
        let store = FakeWithdrawStore::new();
        let identity = test_rail();
        let id = to_w6(&store, &identity).await;
        let unknown = ReconcileWithdraw {
            store: &store,
            clock: &RequestClock(store.now()),
            identity: &identity,
        }
        .mark_unknown(id)
        .await
        .unwrap();
        assert_eq!(unknown.combo, Combo::W7);
        assert_eq!(store.withheld(), 5_000_000);
        let unknown_replay = ReconcileWithdraw {
            store: &store,
            clock: &RequestClock(store.now()),
            identity: &identity,
        }
        .mark_unknown(id)
        .await
        .unwrap();
        assert_eq!(unknown_replay.combo, Combo::W7);

        let chain_receipt = ChainReceipt {
            signature: "sig-recon".into(),
            mint: identity.usdc_mint.clone(),
            source: identity.treasury_token_account.clone(),
            dest_token_account: dest_a(),
            delta_micro: 5_000_000,
            commitment: "finalized".into(),
        };
        let observed = ReconcileWithdraw {
            store: &store,
            clock: &RequestClock(store.now()),
            identity: &identity,
        }
        .observe_finalized(id, chain_receipt.clone())
        .await
        .unwrap();
        assert_eq!(observed.combo, Combo::W9);
        let replay = ReconcileWithdraw {
            store: &store,
            clock: &RequestClock(store.now()),
            identity: &identity,
        }
        .observe_finalized(id, chain_receipt)
        .await
        .unwrap();
        assert_eq!(replay.combo, Combo::W9);
    }

    #[tokio::test]
    async fn reconcile_cas_paths_lock_the_user_before_the_withdrawal_row() {
        let store = FakeWithdrawStore::new();
        let identity = test_rail();
        let id = to_w6(&store, &identity).await;
        store.enforce_withdraw_lock_order();
        let reconciler = ReconcileWithdraw {
            store: &store,
            clock: &RequestClock(store.now()),
            identity: &identity,
        };

        let unknown = reconciler.mark_unknown(id).await.unwrap();
        assert_eq!(unknown.combo, Combo::W7);
        let finalized = reconciler
            .observe_finalized(
                id,
                ChainReceipt {
                    signature: "sig-recon".into(),
                    mint: identity.usdc_mint.clone(),
                    source: identity.treasury_token_account.clone(),
                    dest_token_account: dest_a(),
                    delta_micro: 5_000_000,
                    commitment: "finalized".into(),
                },
            )
            .await
            .unwrap();
        assert_eq!(finalized.combo, Combo::W9);
    }

    #[tokio::test]
    async fn a_live_w3_lease_cannot_be_stolen_by_the_reconciler() {
        let store = FakeWithdrawStore::new();
        let identity = test_rail();
        let id = to_w6(&store, &identity).await;
        {
            let mut tx = store.withdraw_tx().await.unwrap();
            let mut row = tx.withdrawal_for_update(id).await.unwrap();
            row.combo = Combo::W3;
            row.sent_at = None;
            tx.cas_withdrawal(id, Combo::W6, &row).await.unwrap();
            let mut attempt = store.attempts().into_iter().next().unwrap();
            attempt.landing_state = LandingState::Prepared;
            attempt.lease_expires_at = Some(store.now() + Duration::seconds(30));
            tx.save_attempt(&attempt).await.unwrap();
            tx.commit().await.unwrap();
        }
        let held = ReconcileWithdraw {
            store: &store,
            clock: &RequestClock(store.now()),
            identity: &identity,
        }
        .mark_unknown(id)
        .await
        .unwrap_err();
        assert!(matches!(
            held,
            AppError::ProposalConflict("send lease still held")
        ));

        let signer = FakeSigner::new("sig-recon");
        signer.set_finalized_height(1_001);
        let unknown = ReconcileWithdraw {
            store: &store,
            clock: &RequestClock(store.now() + Duration::seconds(30)),
            identity: &identity,
        }
        .recover_w3(id, &signer)
        .await
        .unwrap();
        assert_eq!(unknown.combo, Combo::W7);
    }

    #[tokio::test]
    async fn an_expired_w3_signature_hit_recovers_to_broadcast_not_unknown() {
        let store = FakeWithdrawStore::new();
        let identity = test_rail();
        let id = to_w6(&store, &identity).await;
        {
            let mut tx = store.withdraw_tx().await.unwrap();
            let mut row = tx.withdrawal_for_update(id).await.unwrap();
            row.combo = Combo::W3;
            row.sent_at = None;
            tx.cas_withdrawal(id, Combo::W6, &row).await.unwrap();
            let mut attempt = store.attempts().into_iter().next().unwrap();
            attempt.landing_state = LandingState::Prepared;
            attempt.lease_expires_at = Some(store.now());
            tx.save_attempt(&attempt).await.unwrap();
            tx.commit().await.unwrap();
        }
        let signer = FakeSigner::new("sig-recon");
        signer.set_presence(crate::ports::SignaturePresence::Present);

        let recovered = ReconcileWithdraw {
            store: &store,
            clock: &RequestClock(store.now()),
            identity: &identity,
        }
        .recover_w3(id, &signer)
        .await
        .unwrap();

        assert_eq!(recovered.combo, Combo::W6);
        assert_eq!(store.attempts()[0].landing_state, LandingState::Broadcast);
    }

    #[tokio::test]
    async fn w3_recovery_requires_a_prepared_attempt() {
        let store = FakeWithdrawStore::new();
        let identity = test_rail();
        let id = to_w6(&store, &identity).await;
        {
            let mut tx = store.withdraw_tx().await.unwrap();
            let mut row = tx.withdrawal_for_update(id).await.unwrap();
            row.combo = Combo::W3;
            tx.cas_withdrawal(id, Combo::W6, &row).await.unwrap();
            let mut attempt = store.attempts().into_iter().next().unwrap();
            attempt.landing_state = LandingState::Broadcast;
            attempt.lease_expires_at = Some(store.now());
            tx.save_attempt(&attempt).await.unwrap();
            tx.commit().await.unwrap();
        }
        let signer = FakeSigner::new("sig-recon");
        signer.set_presence(crate::ports::SignaturePresence::Present);

        let result = ReconcileWithdraw {
            store: &store,
            clock: &RequestClock(store.now()),
            identity: &identity,
        }
        .recover_w3(id, &signer)
        .await;

        assert!(matches!(
            result,
            Err(AppError::Store(StoreError::Invariant(_)))
        ));
    }

    #[tokio::test]
    async fn two_of_three_releases_hold_on_definitive_fail() {
        let store = FakeWithdrawStore::new();
        let identity = test_rail();
        let id = to_w6(&store, &identity).await;
        ReconcileWithdraw {
            store: &store,
            clock: &RequestClock(store.now()),
            identity: &identity,
        }
        .mark_unknown(id)
        .await
        .unwrap();
        let before = store.withheld();
        let failed = ReconcileWithdraw {
            store: &store,
            clock: &RequestClock(store.now()),
            identity: &identity,
        }
        .prove_non_landing(
            id,
            &identity
                .rpc_endpoints
                .iter()
                .map(|endpoint| fail_obs(endpoint))
                .collect::<Vec<_>>(),
        )
        .await
        .unwrap();
        assert_eq!(failed.combo, Combo::W15);
        assert_eq!(store.withheld(), before - 5_000_000);
        let failed_replay = ReconcileWithdraw {
            store: &store,
            clock: &RequestClock(store.now()),
            identity: &identity,
        }
        .prove_non_landing(id, &[])
        .await
        .unwrap();
        assert_eq!(failed_replay.combo, Combo::W15);

        let id2 = to_w6(&store, &identity).await;
        ReconcileWithdraw {
            store: &store,
            clock: &RequestClock(store.now()),
            identity: &identity,
        }
        .mark_unknown(id2)
        .await
        .unwrap();
        let pruned = QuorumObservation {
            endpoint: identity.rpc_endpoints[2].clone(),
            finalized_height: Some(9_999),
            signature_present: Some(false),
            pruned: true,
        };
        let stayed = ReconcileWithdraw {
            store: &store,
            clock: &RequestClock(store.now()),
            identity: &identity,
        }
        .prove_non_landing(
            id2,
            &[
                fail_obs(&identity.rpc_endpoints[0]),
                fail_obs(&identity.rpc_endpoints[1]),
                pruned,
            ],
        )
        .await
        .unwrap();
        assert_eq!(stayed.combo, Combo::W7);
    }

    #[tokio::test]
    async fn a_present_signature_vetoes_non_landing_and_evidence_is_complete() {
        let store = FakeWithdrawStore::new();
        let identity = test_rail();
        let id = to_w6(&store, &identity).await;
        ReconcileWithdraw {
            store: &store,
            clock: &RequestClock(store.now()),
            identity: &identity,
        }
        .mark_unknown(id)
        .await
        .unwrap();
        let present = QuorumObservation {
            endpoint: identity.rpc_endpoints[2].clone(),
            finalized_height: Some(9_999),
            signature_present: Some(true),
            pruned: false,
        };
        let stayed = ReconcileWithdraw {
            store: &store,
            clock: &RequestClock(store.now()),
            identity: &identity,
        }
        .prove_non_landing(
            id,
            &[
                fail_obs(&identity.rpc_endpoints[0]),
                fail_obs(&identity.rpc_endpoints[1]),
                present,
            ],
        )
        .await
        .unwrap();
        assert_eq!(stayed.combo, Combo::W7);
        let evidence = store.attempts()[0].evidence.clone().unwrap();
        let observations = evidence
            .get("observations")
            .and_then(serde_json::Value::as_array)
            .unwrap();
        assert_eq!(observations.len(), 3);
        assert_eq!(observations[0]["finalized_height"], 9_999);
        assert_eq!(observations[2]["signature_present"], true);
    }

    #[tokio::test]
    async fn unpinned_endpoints_can_never_release_the_hold() {
        let store = FakeWithdrawStore::new();
        let identity = test_rail();
        let id = to_w6(&store, &identity).await;
        ReconcileWithdraw {
            store: &store,
            clock: &RequestClock(store.now()),
            identity: &identity,
        }
        .mark_unknown(id)
        .await
        .unwrap();
        let stayed = ReconcileWithdraw {
            store: &store,
            clock: &RequestClock(store.now()),
            identity: &identity,
        }
        .prove_non_landing(id, &[fail_obs("x"), fail_obs("y"), fail_obs("z")])
        .await
        .unwrap();
        assert_eq!(stayed.combo, Combo::W7);
        assert_eq!(store.withheld(), 5_000_000);
    }

    #[tokio::test]
    async fn reconcile_invalid_replay_and_w8_unknown_edges_are_explicit() {
        let store = FakeWithdrawStore::new();
        let identity = test_rail();
        let id = to_w6(&store, &identity).await;
        let reconciler = ReconcileWithdraw {
            store: &store,
            clock: &RequestClock(store.now()),
            identity: &identity,
        };
        assert_eq!(
            reconciler
                .recover_w3(id, &FakeSigner::new("unused"))
                .await
                .unwrap()
                .combo,
            Combo::W6
        );
        assert_eq!(
            reconciler.prove_non_landing(id, &[]).await.unwrap_err(),
            AppError::IllegalTransition
        );

        let unknown = reconciler.mark_unknown(id).await.unwrap();
        assert_eq!(unknown.combo, Combo::W7);
        assert_eq!(
            reconciler
                .recover_w3(id, &FakeSigner::new("unused"))
                .await
                .unwrap()
                .combo,
            Combo::W7
        );
        let missing = reconciler
            .observe_finalized(
                id,
                ChainReceipt {
                    signature: "missing".into(),
                    mint: identity.usdc_mint.clone(),
                    source: identity.treasury_token_account.clone(),
                    dest_token_account: dest_a(),
                    delta_micro: 5_000_000,
                    commitment: "finalized".into(),
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(missing, AppError::Store(StoreError::NotFound(_))));
        let bad = reconciler
            .observe_finalized(
                id,
                ChainReceipt {
                    signature: "sig-recon".into(),
                    mint: "wrong".into(),
                    source: identity.treasury_token_account.clone(),
                    dest_token_account: dest_a(),
                    delta_micro: 5_000_000,
                    commitment: "finalized".into(),
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(bad, AppError::ConfigInvalid { .. }));

        {
            let mut tx = store.withdraw_tx().await.unwrap();
            let mut row = tx.withdrawal_for_update(id).await.unwrap();
            row.combo = Combo::W8;
            tx.cas_withdrawal(id, Combo::W7, &row).await.unwrap();
            tx.commit().await.unwrap();
        }
        let stayed = reconciler.prove_non_landing(id, &[]).await.unwrap();
        assert_eq!(stayed.combo, Combo::W7);

        let duplicate = fail_obs(&identity.rpc_endpoints[0]);
        assert_eq!(
            safe_non_landing_verdict(
                1_000,
                &[duplicate.clone(), duplicate.clone(), duplicate],
                &identity.rpc_endpoints,
            ),
            NonLandingVerdict::Unknown
        );
    }

    #[tokio::test]
    async fn expired_w3_absence_demands_same_bytes_and_a_lease() {
        let store = FakeWithdrawStore::new();
        let identity = test_rail();
        let id = to_w6(&store, &identity).await;
        {
            let mut tx = store.withdraw_tx().await.unwrap();
            let mut row = tx.withdrawal_for_update(id).await.unwrap();
            row.combo = Combo::W3;
            tx.cas_withdrawal(id, Combo::W6, &row).await.unwrap();
            let mut attempt = store.attempts().into_iter().next().unwrap();
            attempt.landing_state = LandingState::Prepared;
            attempt.lease_expires_at = Some(store.now());
            tx.save_attempt(&attempt).await.unwrap();
            tx.commit().await.unwrap();
        }
        let signer = FakeSigner::new("sig-recon");
        let reconciler = ReconcileWithdraw {
            store: &store,
            clock: &RequestClock(store.now()),
            identity: &identity,
        };
        assert!(matches!(
            reconciler.recover_w3(id, &signer).await,
            Err(AppError::ProposalConflict(
                "prepared attempt still requires same-bytes rebroadcast"
            ))
        ));
        assert!(matches!(
            reconciler.mark_unknown(id).await,
            Err(AppError::ProposalConflict(
                "W3 recovery requires signature lookup"
            ))
        ));
        {
            let mut tx = store.withdraw_tx().await.unwrap();
            let mut attempt = store.attempts().into_iter().next().unwrap();
            attempt.lease_expires_at = None;
            tx.save_attempt(&attempt).await.unwrap();
            tx.commit().await.unwrap();
        }
        assert!(matches!(
            reconciler.recover_w3(id, &signer).await,
            Err(AppError::Store(StoreError::Invariant(_)))
        ));
    }

    #[tokio::test]
    async fn reconcile_owner_transition_attempt_and_cas_invariants_are_total() {
        fn receipt(identity: &RailIdentity) -> ChainReceipt {
            ChainReceipt {
                signature: "sig-recon".into(),
                mint: identity.usdc_mint.clone(),
                source: identity.treasury_token_account.clone(),
                dest_token_account: dest_a(),
                delta_micro: 5_000_000,
                commitment: "finalized".into(),
            }
        }

        let identity = test_rail();
        let invalid_store = FakeWithdrawStore::new();
        let invalid_id = to_w6(&invalid_store, &identity).await;
        let reconciler = ReconcileWithdraw {
            store: &invalid_store,
            clock: &RequestClock(invalid_store.now()),
            identity: &identity,
        };
        reconciler
            .observe_finalized(invalid_id, receipt(&identity))
            .await
            .unwrap();
        assert_eq!(
            reconciler.mark_unknown(invalid_id).await.unwrap_err(),
            AppError::IllegalTransition
        );
        assert_eq!(
            reconciler
                .recover_w3(invalid_id, &FakeSigner::new("unused"))
                .await
                .unwrap_err(),
            AppError::IllegalTransition
        );
        let mut tx = invalid_store.withdraw_tx().await.unwrap();
        let mut attempt = invalid_store.attempts().into_iter().next().unwrap();
        attempt.landing_state = LandingState::Broadcast;
        tx.save_attempt(&attempt).await.unwrap();
        tx.commit().await.unwrap();
        let err = reconciler
            .observe_finalized(invalid_id, receipt(&identity))
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Store(StoreError::Invariant(_))));

        let observe_store = FakeWithdrawStore::new();
        let observe_id = to_w6(&observe_store, &identity).await;
        let mut tx = observe_store.withdraw_tx().await.unwrap();
        let mut row = tx.withdrawal_for_update(observe_id).await.unwrap();
        row.combo = Combo::W3;
        tx.cas_withdrawal(observe_id, Combo::W6, &row)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        assert_eq!(
            ReconcileWithdraw {
                store: &observe_store,
                clock: &RequestClock(observe_store.now()),
                identity: &identity,
            }
            .observe_finalized(observe_id, receipt(&identity))
            .await
            .unwrap_err(),
            AppError::IllegalTransition
        );

        let live_store = FakeWithdrawStore::new();
        let live_id = to_w6(&live_store, &identity).await;
        let mut tx = live_store.withdraw_tx().await.unwrap();
        let mut row = tx.withdrawal_for_update(live_id).await.unwrap();
        row.combo = Combo::W3;
        tx.cas_withdrawal(live_id, Combo::W6, &row).await.unwrap();
        let mut attempt = live_store.attempts().into_iter().next().unwrap();
        attempt.landing_state = LandingState::Prepared;
        attempt.lease_expires_at = Some(live_store.now() + Duration::seconds(1));
        tx.save_attempt(&attempt).await.unwrap();
        tx.commit().await.unwrap();
        assert!(matches!(
            ReconcileWithdraw {
                store: &live_store,
                clock: &RequestClock(live_store.now()),
                identity: &identity,
            }
            .recover_w3(live_id, &FakeSigner::new("unused"))
            .await,
            Err(AppError::ProposalConflict("send lease still held"))
        ));

        let unknown_store = FakeWithdrawStore::new();
        let unknown_id = to_w6(&unknown_store, &identity).await;
        let mut tx = unknown_store.withdraw_tx().await.unwrap();
        let mut row = tx.withdrawal_for_update(unknown_id).await.unwrap();
        row.combo = Combo::W3;
        tx.cas_withdrawal(unknown_id, Combo::W6, &row)
            .await
            .unwrap();
        let mut attempt = unknown_store.attempts().into_iter().next().unwrap();
        attempt.landing_state = LandingState::Prepared;
        attempt.lease_expires_at = Some(unknown_store.now());
        tx.save_attempt(&attempt).await.unwrap();
        tx.commit().await.unwrap();
        let signer = FakeSigner::new("unused");
        signer.set_presence(SignaturePresence::Unknown);
        let row = ReconcileWithdraw {
            store: &unknown_store,
            clock: &RequestClock(unknown_store.now()),
            identity: &identity,
        }
        .recover_w3(unknown_id, &signer)
        .await
        .unwrap();
        assert_eq!(row.combo, Combo::W7);

        let owner_store = FakeWithdrawStore::new();
        let owner_id = to_w6(&owner_store, &identity).await;
        let wrong = owner_store.seed_user(UserStatus::Active, 2);
        owner_store.override_withdrawal_user(owner_id, wrong);
        let err = ReconcileWithdraw {
            store: &owner_store,
            clock: &RequestClock(owner_store.now()),
            identity: &identity,
        }
        .mark_unknown(owner_id)
        .await
        .unwrap_err();
        assert!(matches!(err, AppError::Store(StoreError::Invariant(_))));

        let prove_store = FakeWithdrawStore::new();
        let prove_id = to_w6(&prove_store, &identity).await;
        ReconcileWithdraw {
            store: &prove_store,
            clock: &RequestClock(prove_store.now()),
            identity: &identity,
        }
        .mark_unknown(prove_id)
        .await
        .unwrap();
        let wrong = prove_store.seed_user(UserStatus::Active, 2);
        prove_store.override_withdrawal_user(prove_id, wrong);
        let err = ReconcileWithdraw {
            store: &prove_store,
            clock: &RequestClock(prove_store.now()),
            identity: &identity,
        }
        .prove_non_landing(prove_id, &[])
        .await
        .unwrap_err();
        assert!(matches!(err, AppError::Store(StoreError::Invariant(_))));

        let cas_store = FakeWithdrawStore::new();
        let cas_id = to_w6(&cas_store, &identity).await;
        cas_store.force_next_cas_miss();
        assert_eq!(
            ReconcileWithdraw {
                store: &cas_store,
                clock: &RequestClock(cas_store.now()),
                identity: &identity,
            }
            .mark_unknown(cas_id)
            .await
            .unwrap_err(),
            AppError::IllegalTransition
        );
    }
}
