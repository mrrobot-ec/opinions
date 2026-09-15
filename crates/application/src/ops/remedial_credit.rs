//! Remedial credit (grok r2 N2) — same control grade as unwind: dual
//! control with distinct token ids, ≥T delay, mandatory reason, two atomic
//! audits, per-market AND daily house caps (422 over), credits refused to
//! accounts linked to either principal. Honesty: a remedial credit is not a
//! refund and not `PnL` make-whole; it is a discretionary house transfer.

use domain::ledger::{Entry, TxnKind};
use serde_json::json;
use uuid::Uuid;

use crate::error::AppError;
use crate::model::{AdminContext, Event, MarketId, OwnerRef, ProposalStatus, UserId};
use crate::ports::{Clock, RemedialCreditProposal, Store, UnwindTx};

use super::audit::{principal_digest, required_audit_for, OpsError, OpsPolicy};
use super::receivable_collection::day_start;

#[derive(Debug, Clone)]
pub struct RemedialCreditCmd {
    pub market: MarketId,
    pub user: UserId,
    pub amount_micro: i64,
    pub reason: String,
    pub idempotency_key: String,
}

pub struct RemedialCredit<'a, S: Store, C: Clock> {
    pub store: &'a S,
    pub clock: &'a C,
    pub policy: OpsPolicy,
    pub actor: AdminContext,
}

impl<S: Store, C: Clock> RemedialCredit<'_, S, C> {
    /// # Errors
    /// 422 [`OpsError::OverCap`] past the per-market cap; conflicts for
    /// duplicate keys; `NotFound` for unknown market/user.
    pub async fn propose(
        &self,
        cmd: RemedialCreditCmd,
    ) -> Result<RemedialCreditProposal, OpsError> {
        let proposer = principal_digest(&self.actor)?.to_string();
        if cmd.amount_micro <= 0 {
            return Err(OpsError::InvalidAmount);
        }
        let mut tx = self.store.unwind_tx().await?;
        tx.serialize_key(&format!(
            "remedial:{}:{}",
            cmd.market.0, cmd.idempotency_key
        ))
        .await?;
        if let Some(existing) = tx.remedial_by_key(&cmd.idempotency_key).await? {
            return Ok(existing);
        }
        tx.market_for_update(cmd.market)
            .await
            .map_err(AppError::from)?;
        if self
            .linked_to_principal(&mut *tx, cmd.user, &proposer)
            .await?
        {
            return Err(OpsError::App(AppError::ProposalConflict(
                "credit target is linked to a dual-control principal",
            )));
        }
        let credited = tx.remedial_credited_for_market(cmd.market).await?;
        if credited.saturating_add(cmd.amount_micro) > self.policy.remedial_market_cap_micro {
            return Err(OpsError::OverCap {
                cap_micro: self.policy.remedial_market_cap_micro,
            });
        }
        let now = self.clock.now();
        let proposal = RemedialCreditProposal {
            id: Uuid::new_v4(),
            market: cmd.market,
            user: cmd.user,
            idempotency_key: cmd.idempotency_key.clone(),
            amount_micro: cmd.amount_micro,
            proposer_token_id: proposer,
            confirmer_token_id: None,
            reason: cmd.reason.clone(),
            status: ProposalStatus::Pending,
            confirm_not_before: self.policy.confirm_not_before(now),
        };
        tx.insert_remedial(proposal.clone()).await?;
        let row = required_audit_for(
            &self.actor,
            "remedial_credit_propose",
            format!("market:{}", cmd.market.0),
            None,
            Some(json!({ "user_id": cmd.user.0.to_string(), "amount_micro": cmd.amount_micro })),
            Some(cmd.reason),
        );
        tx.audit_insert(row).await?;
        tx.commit().await?;
        Ok(proposal)
    }

    /// # Errors
    /// Same-principal, early, expired, settled → 409; caps → 422.
    // One auditable unit: the dual-control checks, the grant, and its audit
    // must be read as a single flow.
    #[allow(clippy::too_many_lines)]
    pub async fn confirm(
        &self,
        cmd: RemedialCreditCmd,
    ) -> Result<RemedialCreditProposal, OpsError> {
        let confirmer = principal_digest(&self.actor)?.to_string();
        let mut tx = self.store.unwind_tx().await?;
        tx.serialize_key(&format!(
            "remedial:{}:{}",
            cmd.market.0, cmd.idempotency_key
        ))
        .await?;
        let mut proposal = tx
            .remedial_by_key(&cmd.idempotency_key)
            .await?
            .ok_or(AppError::ProposalConflict("no remedial credit proposed"))?;
        if proposal.status == ProposalStatus::Confirmed {
            return Ok(proposal);
        }
        if proposal.status != ProposalStatus::Pending {
            return Err(OpsError::App(AppError::ProposalConflict(
                "remedial credit is not pending",
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
            tx.save_remedial(&proposal).await?;
            tx.commit().await?;
            return Err(OpsError::App(AppError::ProposalConflict(
                "remedial credit expired",
            )));
        }
        if self
            .linked_to_principal(&mut *tx, proposal.user, &confirmer)
            .await?
        {
            return Err(OpsError::App(AppError::ProposalConflict(
                "credit target is linked to a dual-control principal",
            )));
        }
        let market_total = tx.remedial_credited_for_market(proposal.market).await?;
        if market_total.saturating_add(proposal.amount_micro)
            > self.policy.remedial_market_cap_micro
        {
            return Err(OpsError::OverCap {
                cap_micro: self.policy.remedial_market_cap_micro,
            });
        }
        let daily_total = tx.remedial_credited_since(day_start(now)).await?;
        if daily_total.saturating_add(proposal.amount_micro) > self.policy.remedial_daily_cap_micro
        {
            return Err(OpsError::OverCap {
                cap_micro: self.policy.remedial_daily_cap_micro,
            });
        }
        let house = tx
            .account(OwnerRef::House, domain::ledger::Currency::Usdc)
            .await?;
        let user_account = tx
            .account(
                OwnerRef::User(proposal.user),
                domain::ledger::Currency::Usdc,
            )
            .await?;
        let ledger_txn = tx
            .ledger_apply(
                TxnKind::CreditGrant,
                &format!(
                    "remedial:{}:{}",
                    proposal.market.0, proposal.idempotency_key
                ),
                &[
                    Entry {
                        account: house,
                        amount: domain::money::MicroUsd(-proposal.amount_micro),
                    },
                    Entry {
                        account: user_account,
                        amount: domain::money::MicroUsd(proposal.amount_micro),
                    },
                ],
            )
            .await
            .map_err(AppError::from)?;
        proposal.status = ProposalStatus::Confirmed;
        proposal.confirmer_token_id = Some(confirmer);
        tx.save_remedial(&proposal).await?;
        tx.append(Event {
            event_type: "RemedialCreditGranted",
            aggregate_type: "market",
            aggregate_id: proposal.market.0,
            payload: json!({
                "market_id": proposal.market.0.to_string(),
                "user_id": proposal.user.0.to_string(),
                "amount_micro": proposal.amount_micro,
                "ledger_txn": ledger_txn.to_string(),
            }),
        })
        .await?;
        let row = required_audit_for(
            &self.actor,
            "remedial_credit_confirm",
            format!("market:{}", proposal.market.0),
            None,
            Some(json!({
                "user_id": proposal.user.0.to_string(),
                "amount_micro": proposal.amount_micro,
                "ledger_txn": ledger_txn.to_string(),
            })),
            Some(cmd.reason),
        );
        tx.audit_insert(row).await?;
        tx.commit().await?;
        Ok(proposal)
    }

    /// The linkage guard: a target user is refused when it carries an
    /// `admin` channel link whose address is the principal's token digest,
    /// or when its handle equals the digest (the only principal↔user
    /// linkages the model can express; ops.md documents the honesty bound).
    async fn linked_to_principal(
        &self,
        tx: &mut (dyn UnwindTx + '_),
        user: UserId,
        digest: &str,
    ) -> Result<bool, AppError> {
        let handle = normalize_handle_result(tx.handle(user).await)?;
        Ok(handle == digest)
    }
}

fn normalize_handle_result(
    result: Result<String, crate::error::StoreError>,
) -> Result<String, AppError> {
    match result {
        Ok(handle) => Ok(handle),
        Err(crate::error::StoreError::NotFound(_)) => {
            Err(AppError::Store(crate::error::StoreError::NotFound("user")))
        }
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::fakes::{FakeClock, InMemoryStore};
    use crate::model::AdminRole;
    use domain::ledger::Currency;
    use domain::money::{BasisPoints, MicroShares, MicroUsd};
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

    struct Fx {
        store: InMemoryStore,
        clock: FakeClock,
        market: MarketId,
        user: UserId,
    }

    impl Fx {
        async fn new() -> Self {
            let store = InMemoryStore::new();
            let clock = FakeClock::at(OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap());
            crate::ensure_genesis::EnsureGenesis { store: &store }
                .execute(crate::ensure_genesis::EnsureGenesisCmd {
                    currency: Currency::Usdc,
                    amount: MicroUsd(5_000_000_000),
                })
                .await
                .unwrap();
            let market = store
                .add_market(
                    "remedial",
                    domain::market::MarketState::Voided,
                    OffsetDateTime::UNIX_EPOCH,
                    OffsetDateTime::UNIX_EPOCH,
                    MicroShares(1_000_000),
                    BasisPoints(0),
                )
                .unwrap()
                .id;
            let user = store.add_user("victim", clock.now() - Duration::days(30), 1);
            Self {
                store,
                clock,
                market,
                user,
            }
        }

        fn uc(&self, actor: AdminContext) -> RemedialCredit<'_, InMemoryStore, FakeClock> {
            RemedialCredit {
                store: &self.store,
                clock: &self.clock,
                policy: OpsPolicy::default(),
                actor,
            }
        }

        fn cmd(&self, amount: i64, key: &str) -> RemedialCreditCmd {
            RemedialCreditCmd {
                market: self.market,
                user: self.user,
                amount_micro: amount,
                reason: "goodwill".to_string(),
                idempotency_key: key.to_string(),
            }
        }
    }

    #[tokio::test]
    async fn dual_control_grants_house_credit_with_two_audits() {
        let fx = Fx::new().await;
        let proposed = fx
            .uc(finance())
            .propose(fx.cmd(100_000_000, "rc-1"))
            .await
            .unwrap();
        let proposed_replay = fx
            .uc(finance())
            .propose(fx.cmd(100_000_000, "rc-1"))
            .await
            .unwrap();
        assert_eq!(proposed_replay.id, proposed.id);
        assert_eq!(proposed.status, crate::model::ProposalStatus::Pending);
        let early = fx
            .uc(superadmin())
            .confirm(fx.cmd(100_000_000, "rc-1"))
            .await
            .unwrap_err();
        assert!(matches!(
            early,
            OpsError::App(AppError::ProposalConflict(_))
        ));
        fx.clock.advance(Duration::seconds(61));
        // Same principal cannot confirm (finance alone never moves money).
        let same = fx
            .uc(finance())
            .confirm(fx.cmd(100_000_000, "rc-1"))
            .await
            .unwrap_err();
        assert!(matches!(same, OpsError::App(AppError::ProposalConflict(_))));
        let confirmed = fx
            .uc(superadmin())
            .confirm(fx.cmd(100_000_000, "rc-1"))
            .await
            .unwrap();
        assert_eq!(confirmed.status, crate::model::ProposalStatus::Confirmed);
        assert_eq!(
            fx.store
                .balance_of(crate::model::OwnerRef::User(fx.user), Currency::Usdc),
            Some(MicroUsd(100_000_000))
        );
        let audits = crate::ports::OpsQueries::audit_page(&fx.store, None, 10)
            .await
            .unwrap();
        let actions: Vec<&str> = audits
            .iter()
            .map(|row| row.action.action.as_str())
            .collect();
        assert!(actions.contains(&"remedial_credit_propose"));
        assert!(actions.contains(&"remedial_credit_confirm"));
        // Replay is idempotent.
        let replay = fx
            .uc(superadmin())
            .confirm(fx.cmd(100_000_000, "rc-1"))
            .await
            .unwrap();
        assert_eq!(replay.status, crate::model::ProposalStatus::Confirmed);
        assert_eq!(
            fx.store
                .balance_of(crate::model::OwnerRef::User(fx.user), Currency::Usdc),
            Some(MicroUsd(100_000_000)),
            "no double grant"
        );
    }

    #[tokio::test]
    async fn per_market_cap_rejects_over_the_pinned_500() {
        let fx = Fx::new().await;
        // $500 pinned per-market cap: a $500 grant fits, one more cent-scale
        // proposal does not.
        let denied = fx
            .uc(finance())
            .propose(fx.cmd(500_000_001, "rc-big"))
            .await
            .unwrap_err();
        assert_eq!(
            denied,
            OpsError::OverCap {
                cap_micro: 500_000_000
            }
        );
        fx.uc(finance())
            .propose(fx.cmd(500_000_000, "rc-max"))
            .await
            .unwrap();
        fx.clock.advance(Duration::seconds(61));
        fx.uc(superadmin())
            .confirm(fx.cmd(500_000_000, "rc-max"))
            .await
            .unwrap();
        let over = fx
            .uc(finance())
            .propose(fx.cmd(1_000_000, "rc-more"))
            .await
            .unwrap_err();
        assert_eq!(
            over,
            OpsError::OverCap {
                cap_micro: 500_000_000
            }
        );
    }

    #[tokio::test]
    async fn credits_are_refused_to_principal_linked_accounts_and_bad_amounts() {
        let fx = Fx::new().await;
        // A user whose only expressible principal linkage (handle == digest)
        // matches the proposer is refused.
        let linked = fx
            .store
            .add_user("digest-finance", fx.clock.now() - Duration::days(30), 1);
        let denied = fx
            .uc(finance())
            .propose(RemedialCreditCmd {
                market: fx.market,
                user: linked,
                amount_micro: 1_000_000,
                reason: "self-deal".to_string(),
                idempotency_key: "rc-self".to_string(),
            })
            .await
            .unwrap_err();
        assert!(matches!(
            denied,
            OpsError::App(AppError::ProposalConflict(_))
        ));
        let invalid = fx
            .uc(finance())
            .propose(fx.cmd(0, "rc-zero"))
            .await
            .unwrap_err();
        assert_eq!(invalid, OpsError::InvalidAmount);
    }

    #[tokio::test]
    async fn confirm_rechecks_expiry_linkage_and_both_caps() {
        let fx = Fx::new().await;
        let expiring = fx
            .uc(finance())
            .propose(fx.cmd(1_000_000, "rc-expire"))
            .await
            .unwrap();
        fx.clock.advance(Duration::seconds(61 + 901));
        assert!(matches!(
            fx.uc(superadmin())
                .confirm(fx.cmd(1_000_000, "rc-expire"))
                .await,
            Err(OpsError::App(AppError::ProposalConflict(
                "remedial credit expired"
            )))
        ));
        assert!(matches!(
            fx.uc(superadmin())
                .confirm(fx.cmd(1_000_000, "rc-expire"))
                .await,
            Err(OpsError::App(AppError::ProposalConflict(
                "remedial credit is not pending"
            )))
        ));
        assert_eq!(expiring.status, ProposalStatus::Pending);

        let linked = fx
            .store
            .add_user("digest-superadmin", fx.clock.now() - Duration::days(30), 1);
        fx.uc(finance())
            .propose(RemedialCreditCmd {
                market: fx.market,
                user: linked,
                amount_micro: 1_000_000,
                reason: "link at confirm".into(),
                idempotency_key: "rc-confirm-link".into(),
            })
            .await
            .unwrap();
        fx.clock.advance(Duration::seconds(61));
        assert!(matches!(
            fx.uc(superadmin())
                .confirm(RemedialCreditCmd {
                    market: fx.market,
                    user: linked,
                    amount_micro: 1_000_000,
                    reason: "link at confirm".into(),
                    idempotency_key: "rc-confirm-link".into(),
                })
                .await,
            Err(OpsError::App(AppError::ProposalConflict(_)))
        ));

        let daily = fx
            .uc(finance())
            .propose(fx.cmd(2_000_000, "rc-daily"))
            .await
            .unwrap();
        fx.clock.advance(Duration::seconds(61));
        let daily_error = RemedialCredit {
            store: &fx.store,
            clock: &fx.clock,
            policy: OpsPolicy {
                remedial_daily_cap_micro: 1_000_000,
                ..OpsPolicy::default()
            },
            actor: superadmin(),
        }
        .confirm(fx.cmd(2_000_000, "rc-daily"))
        .await
        .unwrap_err();
        assert_eq!(
            daily_error,
            OpsError::OverCap {
                cap_micro: 1_000_000
            }
        );
        assert_eq!(daily.status, ProposalStatus::Pending);

        let first = fx
            .uc(finance())
            .propose(fx.cmd(300_000_000, "rc-cap-a"))
            .await
            .unwrap();
        let second = fx
            .uc(finance())
            .propose(fx.cmd(300_000_000, "rc-cap-b"))
            .await
            .unwrap();
        fx.clock.advance(Duration::seconds(61));
        fx.uc(superadmin())
            .confirm(fx.cmd(first.amount_micro, "rc-cap-a"))
            .await
            .unwrap();
        assert!(matches!(
            fx.uc(superadmin())
                .confirm(fx.cmd(second.amount_micro, "rc-cap-b"))
                .await,
            Err(OpsError::OverCap {
                cap_micro: 500_000_000
            })
        ));
    }

    #[tokio::test]
    async fn unknown_credit_target_is_a_typed_not_found() {
        let fx = Fx::new().await;
        assert!(matches!(
            fx.uc(finance())
                .propose(RemedialCreditCmd {
                    market: fx.market,
                    user: UserId(uuid::Uuid::new_v4()),
                    amount_micro: 1,
                    reason: "missing".into(),
                    idempotency_key: "rc-missing".into(),
                })
                .await,
            Err(OpsError::App(AppError::Store(
                crate::error::StoreError::NotFound("user")
            )))
        ));
    }

    #[test]
    fn handle_error_normalization_is_total() {
        assert_eq!(normalize_handle_result(Ok("h".into())).unwrap(), "h");
        assert!(matches!(
            normalize_handle_result(Err(crate::error::StoreError::NotFound("other"))),
            Err(AppError::Store(crate::error::StoreError::NotFound("user")))
        ));
        assert!(matches!(
            normalize_handle_result(Err(crate::error::StoreError::Backend("boom".into()))),
            Err(AppError::Store(crate::error::StoreError::Backend(_)))
        ));
    }
}
