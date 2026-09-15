//! D27 + D35 money identities: six ledger identities, receivables, plus
//! suspense Σ, `BonusReserve` coverage, and withdrawal attribution (a)–(e).
//! Policy lives here; the snapshot implementations serve raw rows only.

use domain::ledger::OwnerType;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::AppError;
use crate::model::{DepositMachineStatus, WithdrawalSendState, WithdrawalStatus};
use crate::ports::Store;

/// One identity's verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentityResult {
    pub identity: &'static str,
    pub pass: bool,
    pub detail: Option<String>,
}

/// The whole-suite verdict over one snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvariantReport {
    pub as_of: OffsetDateTime,
    pub pass: bool,
    pub identities: Vec<IdentityResult>,
}

fn result(identity: &'static str, pass: bool, detail: Option<String>) -> IdentityResult {
    IdentityResult {
        identity,
        pass,
        detail,
    }
}

fn failure_detail(pass: bool, detail: String) -> Option<String> {
    (!pass).then_some(detail)
}

/// Runs the full suite over one snapshot.
///
/// # Errors
/// Store failures opening or reading the snapshot.
#[allow(clippy::too_many_lines)]
pub async fn run<S: Store + ?Sized>(store: &S) -> Result<InvariantReport, AppError> {
    let mut tx = store.invariant_read_tx().await?;
    let as_of = tx.as_of().await?;
    let mut identities = Vec::with_capacity(14);

    // Identity 1: per-transaction sum-zero.
    let unbalanced = tx.unbalanced_txns().await?;
    identities.push(result(
        "per_txn_sum_zero",
        unbalanced.is_empty(),
        failure_detail(
            unbalanced.is_empty(),
            format!("{} unbalanced transactions", unbalanced.len()),
        ),
    ));

    // Identities 2 + 3 share the balance read.
    let balances = tx.account_balances().await?;
    let negative: Vec<&crate::model::AccountBalanceRow> = balances
        .iter()
        .filter(|row| row.owner_type != OwnerType::External && row.balance_micro < 0)
        .collect();
    identities.push(result(
        "non_external_non_negative",
        negative.is_empty(),
        failure_detail(
            negative.is_empty(),
            format!("{} negative internal accounts", negative.len()),
        ),
    ));
    let external: i128 = balances
        .iter()
        .filter(|row| row.owner_type == OwnerType::External)
        .map(|row| i128::from(row.balance_micro))
        .sum();
    let internal: i128 = balances
        .iter()
        .filter(|row| row.owner_type != OwnerType::External)
        .map(|row| i128::from(row.balance_micro))
        .sum();
    let pass = external.checked_neg() == Some(internal);
    identities.push(result(
        "external_mirrors_internal",
        pass,
        failure_detail(
            pass,
            format!("external {external} + internal {internal} is not zero"),
        ),
    ));

    // Identity 4: payment facts ↔ external legs 1:1.
    let unpaired = tx.unpaired_payment_facts().await?;
    identities.push(result(
        "payment_facts_paired",
        unpaired.is_empty(),
        failure_detail(
            unpaired.is_empty(),
            format!("{} unpaired payment facts", unpaired.len()),
        ),
    ));

    // Identity 5: settled markets (collateral fact recorded) drained their
    // escrow to zero; the fact itself is stamped in the resolution tx.
    let escrow = tx.escrow_history().await?;
    let residuals: Vec<String> = escrow
        .iter()
        .filter(|row| row.collateral_at_close_micro.is_some() && row.residual_micro != 0)
        .map(|row| format!("market {} residual {}", row.market.0, row.residual_micro))
        .collect();
    identities.push(result(
        "escrow_history_zero",
        residuals.is_empty(),
        failure_detail(residuals.is_empty(), residuals.join("; ")),
    ));

    // Identity 6: no duplicate terminal job effects.
    let duplicates = tx.duplicate_job_effects().await?;
    identities.push(result(
        "job_idempotency",
        duplicates.is_empty(),
        failure_detail(duplicates.is_empty(), duplicates.join("; ")),
    ));

    // Identity 7: per origin reversal tx — Σ opened equals that tx's house
    // shortfall legs, and collections/write-offs never exceed opened.
    let recon = tx.receivable_reconciliation().await?;
    let broken = format_broken_receivables(&recon);
    identities.push(result(
        "receivables_reconcile",
        broken.is_empty(),
        failure_detail(broken.is_empty(), broken.join("; ")),
    ));

    let facts = MoneyIdentityFacts::from_balances(&balances);
    identities.extend(evaluate_money_identities(&facts));

    Ok(InvariantReport {
        as_of,
        pass: identities.iter().all(|identity| identity.pass),
        identities,
    })
}

/// Snapshot projection for Phase 7 money identities. `run` derives
/// balances from the D27 account read; tests (and later W3 snapshot
/// methods) supply the row-level attribution facts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MoneyIdentityFacts {
    pub withheld_balance_micro: i128,
    pub suspense_balance_micro: i128,
    pub bonus_reserve_balance_micro: i128,
    pub withdrawals: Vec<WithdrawalAttribution>,
    pub deposits: Vec<DepositLiability>,
    pub lots: Vec<BonusLotCoverage>,
    pub attempts: Vec<OutboundAttemptFact>,
}

impl MoneyIdentityFacts {
    #[must_use]
    pub fn from_balances(balances: &[crate::model::AccountBalanceRow]) -> Self {
        let sum = |owner: OwnerType| {
            balances
                .iter()
                .filter(|row| row.owner_type == owner)
                .map(|row| i128::from(row.balance_micro))
                .sum()
        };
        Self {
            withheld_balance_micro: sum(OwnerType::Withheld),
            suspense_balance_micro: sum(OwnerType::DepositSuspense),
            bonus_reserve_balance_micro: sum(OwnerType::BonusReserve),
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WithdrawalAttribution {
    pub id: Uuid,
    pub amount_micro: i64,
    pub status: WithdrawalStatus,
    pub send_state: WithdrawalSendState,
    pub hold_amount_micro: i64,
    pub release_amount_micro: Option<i64>,
    pub settle_amount_micro: Option<i64>,
    pub hold_tx_id: Uuid,
    pub release_tx_id: Option<Uuid>,
    pub settle_tx_id: Option<Uuid>,
}

impl WithdrawalAttribution {
    #[must_use]
    pub fn has_active_hold(&self) -> bool {
        matches!(
            self.send_state,
            WithdrawalSendState::Unsent
                | WithdrawalSendState::Sending
                | WithdrawalSendState::Broadcast
                | WithdrawalSendState::Unknown
                | WithdrawalSendState::Finalized
        ) && self.settle_tx_id.is_none()
            && self.release_tx_id.is_none()
            && !matches!(
                self.status,
                WithdrawalStatus::Denied | WithdrawalStatus::Failed | WithdrawalStatus::Settled
            )
    }

    #[must_use]
    pub fn is_accepted(&self) -> bool {
        !matches!(self.status, WithdrawalStatus::Denied)
    }

    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.status,
            WithdrawalStatus::Settled | WithdrawalStatus::Denied | WithdrawalStatus::Failed
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DepositLiability {
    pub amount_micro: i64,
    pub status: DepositMachineStatus,
}

impl DepositLiability {
    #[must_use]
    pub fn is_open_observation(&self) -> bool {
        matches!(
            self.status,
            DepositMachineStatus::ObservedFinalized
                | DepositMachineStatus::AdmissionPending
                | DepositMachineStatus::ComplianceHold
                | DepositMachineStatus::RefundApproved
                | DepositMachineStatus::RefundSending
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BonusLotCoverage {
    pub remaining_promise_micro: i64,
    pub grant_class: String,
    pub converted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboundAttemptFact {
    pub payment_id: Uuid,
    pub subject_id: Uuid,
    pub landing_state: &'static str,
}

/// Evaluates suspense Σ, `BonusReserve` coverage, and withdrawal (a)–(e).
#[must_use]
pub fn evaluate_money_identities(facts: &MoneyIdentityFacts) -> Vec<IdentityResult> {
    vec![
        deposit_suspense_sigma(facts),
        bonus_reserve_coverage(facts),
        withdrawal_hold_matches_active(facts),
        withdrawal_hold_exact(facts),
        withdrawal_terminal_xor(facts),
        withdrawal_attempt_lineage(facts),
        withdrawal_no_duplicate_terminal(facts),
    ]
}

fn receivable_row_broken(row: &crate::model::ReceivableReconRow) -> bool {
    row.opened_micro != row.house_shortfall_micro
        || i128::from(row.collected_micro) + i128::from(row.written_off_micro)
            > i128::from(row.opened_micro)
}

fn format_broken_receivables(recon: &[crate::model::ReceivableReconRow]) -> Vec<String> {
    recon
        .iter()
        .filter(|row| receivable_row_broken(row))
        .map(|row| {
            format!(
                "origin {}: opened {} shortfall {} collected {} written_off {}",
                row.origin_reversal_txn,
                row.opened_micro,
                row.house_shortfall_micro,
                row.collected_micro,
                row.written_off_micro
            )
        })
        .collect()
}

fn deposit_suspense_sigma(facts: &MoneyIdentityFacts) -> IdentityResult {
    let liabilities: i128 = facts
        .deposits
        .iter()
        .filter(|row| row.is_open_observation())
        .map(|row| i128::from(row.amount_micro))
        .sum();
    let pass = facts.suspense_balance_micro == liabilities;
    result(
        "deposit_suspense_sigma",
        pass,
        failure_detail(
            pass,
            format!(
                "balance {} != open observation Σ {}",
                facts.suspense_balance_micro, liabilities
            ),
        ),
    )
}

fn bonus_reserve_coverage(facts: &MoneyIdentityFacts) -> IdentityResult {
    let promised: i128 = facts
        .lots
        .iter()
        .filter(|lot| !lot.converted && lot.grant_class == "real_money")
        .map(|lot| i128::from(lot.remaining_promise_micro))
        .sum();
    let pass = facts.bonus_reserve_balance_micro >= promised;
    result(
        "bonus_reserve_coverage",
        pass,
        failure_detail(
            pass,
            format!(
                "reserve {} < remaining real_money promise {}",
                facts.bonus_reserve_balance_micro, promised
            ),
        ),
    )
}

fn withdrawal_hold_matches_active(facts: &MoneyIdentityFacts) -> IdentityResult {
    let active: i128 = facts
        .withdrawals
        .iter()
        .filter(|row| row.has_active_hold())
        .map(|row| i128::from(row.amount_micro))
        .sum();
    let pass = facts.withheld_balance_micro == active;
    result(
        "withdrawal_hold_matches_active",
        pass,
        failure_detail(
            pass,
            format!(
                "Withheld {} != active holds {}",
                facts.withheld_balance_micro, active
            ),
        ),
    )
}

fn withdrawal_hold_exact(facts: &MoneyIdentityFacts) -> IdentityResult {
    let broken: Vec<String> = facts
        .withdrawals
        .iter()
        .filter(|row| row.is_accepted() && row.hold_amount_micro != row.amount_micro)
        .map(|row| {
            format!(
                "{} hold {} != {}",
                row.id, row.hold_amount_micro, row.amount_micro
            )
        })
        .collect();
    result(
        "withdrawal_hold_exact",
        broken.is_empty(),
        failure_detail(broken.is_empty(), broken.join("; ")),
    )
}

fn withdrawal_terminal_xor(facts: &MoneyIdentityFacts) -> IdentityResult {
    let broken: Vec<String> = facts
        .withdrawals
        .iter()
        .filter(|row| row.is_terminal())
        .filter(|row| {
            let release = row.release_amount_micro.is_some();
            let settle = row.settle_amount_micro.is_some();
            release == settle
                || row.release_amount_micro == Some(0)
                || row.settle_amount_micro == Some(0)
                || (release && row.release_amount_micro != Some(row.amount_micro))
                || (settle && row.settle_amount_micro != Some(row.amount_micro))
        })
        .map(|row| {
            format!(
                "{} release={:?} settle={:?}",
                row.id, row.release_tx_id, row.settle_tx_id
            )
        })
        .collect();
    result(
        "withdrawal_terminal_xor",
        broken.is_empty(),
        failure_detail(broken.is_empty(), broken.join("; ")),
    )
}

fn withdrawal_attempt_lineage(facts: &MoneyIdentityFacts) -> IdentityResult {
    let mut by_payment: std::collections::BTreeMap<Uuid, Vec<&OutboundAttemptFact>> =
        std::collections::BTreeMap::new();
    for attempt in &facts.attempts {
        by_payment
            .entry(attempt.payment_id)
            .or_default()
            .push(attempt);
    }
    let mut broken = Vec::new();
    for (payment, attempts) in &by_payment {
        let finalized = attempts
            .iter()
            .filter(|attempt| attempt.landing_state == "finalized")
            .count();
        if finalized > 1 {
            broken.push(format!(
                "payment {payment} has {finalized} finalized attempts"
            ));
        }
    }
    let settled: Vec<&WithdrawalAttribution> = facts
        .withdrawals
        .iter()
        .filter(|row| row.status == WithdrawalStatus::Settled)
        .collect();
    for row in settled {
        let payment_attempts: Vec<_> = facts
            .attempts
            .iter()
            .filter(|attempt| attempt.subject_id == row.id)
            .collect();
        let finalized = payment_attempts
            .iter()
            .filter(|attempt| attempt.landing_state == "finalized")
            .count();
        if finalized != 1 || row.settle_tx_id.is_none() {
            broken.push(format!(
                "settled {} finalized_attempts={finalized} settle={:?}",
                row.id, row.settle_tx_id
            ));
        }
    }
    result(
        "withdrawal_attempt_lineage",
        broken.is_empty(),
        failure_detail(broken.is_empty(), broken.join("; ")),
    )
}

fn withdrawal_no_duplicate_terminal(facts: &MoneyIdentityFacts) -> IdentityResult {
    let mut seen = std::collections::BTreeSet::new();
    let mut broken = Vec::new();
    for row in &facts.withdrawals {
        if !row.is_terminal() {
            continue;
        }
        if let Some(tx) = row.settle_tx_id {
            if !seen.insert(("settle", tx)) {
                broken.push(format!("duplicate settle {tx}"));
            }
        }
        if let Some(tx) = row.release_tx_id {
            if !seen.insert(("release", tx)) {
                broken.push(format!("duplicate release {tx}"));
            }
        }
    }
    result(
        "withdrawal_no_duplicate_terminal",
        broken.is_empty(),
        failure_detail(broken.is_empty(), broken.join("; ")),
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::ensure_genesis::{EnsureGenesis, EnsureGenesisCmd};
    use crate::fakes::InMemoryStore;
    use domain::ledger::Currency;
    use domain::money::MicroUsd;

    #[tokio::test]
    async fn a_fresh_capitalized_store_passes_every_identity() {
        let store = InMemoryStore::new();
        EnsureGenesis { store: &store }
            .execute(EnsureGenesisCmd {
                currency: Currency::Usdc,
                amount: MicroUsd(1_000_000_000),
            })
            .await
            .unwrap();
        let report = run(&store).await.unwrap();
        assert!(report.pass, "{:?}", report.identities);
        assert_eq!(report.identities.len(), 14);
        let names: Vec<&str> = report.identities.iter().map(|i| i.identity).collect();
        assert_eq!(
            names,
            [
                "per_txn_sum_zero",
                "non_external_non_negative",
                "external_mirrors_internal",
                "payment_facts_paired",
                "escrow_history_zero",
                "job_idempotency",
                "receivables_reconcile",
                "deposit_suspense_sigma",
                "bonus_reserve_coverage",
                "withdrawal_hold_matches_active",
                "withdrawal_hold_exact",
                "withdrawal_terminal_xor",
                "withdrawal_attempt_lineage",
                "withdrawal_no_duplicate_terminal",
            ]
        );
    }

    #[tokio::test]
    async fn deposits_and_markets_keep_the_suite_green() {
        let store = InMemoryStore::new();
        let user = crate::model::UserId(uuid::Uuid::new_v4());
        store.fund_user(user, MicroUsd(50_000_000)).unwrap();
        store
            .add_market(
                "sweep-market",
                domain::market::MarketState::Live,
                OffsetDateTime::UNIX_EPOCH,
                OffsetDateTime::UNIX_EPOCH,
                domain::money::MicroShares(1_000_000),
                domain::money::BasisPoints(100),
            )
            .unwrap();
        let report = run(&store).await.unwrap();
        assert!(report.pass, "{:?}", report.identities);
        assert_eq!(failure_detail(true, "unused".into()), None);
        assert_eq!(
            failure_detail(false, "detail".into()).as_deref(),
            Some("detail")
        );
    }

    fn accepted_hold(id: Uuid, amount: i64) -> WithdrawalAttribution {
        WithdrawalAttribution {
            id,
            amount_micro: amount,
            status: WithdrawalStatus::Queued,
            send_state: WithdrawalSendState::Unsent,
            hold_amount_micro: amount,
            release_amount_micro: None,
            settle_amount_micro: None,
            hold_tx_id: id,
            release_tx_id: None,
            settle_tx_id: None,
        }
    }

    #[test]
    fn money_identities_pass_on_a_coherent_snapshot() {
        let hold = Uuid::from_u128(1);
        let settle = Uuid::from_u128(2);
        let facts = MoneyIdentityFacts {
            withheld_balance_micro: 5,
            suspense_balance_micro: 7,
            bonus_reserve_balance_micro: 10,
            withdrawals: vec![
                accepted_hold(hold, 5),
                WithdrawalAttribution {
                    id: settle,
                    amount_micro: 3,
                    status: WithdrawalStatus::Settled,
                    send_state: WithdrawalSendState::Finalized,
                    hold_amount_micro: 3,
                    release_amount_micro: None,
                    settle_amount_micro: Some(3),
                    hold_tx_id: Uuid::from_u128(3),
                    release_tx_id: None,
                    settle_tx_id: Some(settle),
                },
            ],
            deposits: vec![DepositLiability {
                amount_micro: 7,
                status: DepositMachineStatus::AdmissionPending,
            }],
            lots: vec![BonusLotCoverage {
                remaining_promise_micro: 10,
                grant_class: "real_money".into(),
                converted: false,
            }],
            attempts: vec![OutboundAttemptFact {
                payment_id: Uuid::from_u128(9),
                subject_id: settle,
                landing_state: "finalized",
            }],
        };
        let results = evaluate_money_identities(&facts);
        assert!(results.iter().all(|row| row.pass), "{results:?}");
        assert!(facts.withdrawals[0].has_active_hold());
        assert!(facts.withdrawals[0].is_accepted());
        assert!(!facts.withdrawals[0].is_terminal());
        assert!(facts.deposits[0].is_open_observation());
        let empty = MoneyIdentityFacts::from_balances(&[]);
        assert_eq!(empty.withheld_balance_micro, 0);
        assert!(evaluate_money_identities(&empty).iter().all(|row| row.pass));
    }

    #[test]
    fn money_identities_flag_each_broken_predicate() {
        let id = Uuid::from_u128(4);
        let mut facts = MoneyIdentityFacts {
            withheld_balance_micro: 9,
            suspense_balance_micro: 1,
            bonus_reserve_balance_micro: 0,
            withdrawals: vec![WithdrawalAttribution {
                id,
                amount_micro: 4,
                status: WithdrawalStatus::Settled,
                send_state: WithdrawalSendState::Finalized,
                hold_amount_micro: 1,
                release_amount_micro: Some(4),
                settle_amount_micro: Some(4),
                hold_tx_id: id,
                release_tx_id: Some(id),
                settle_tx_id: Some(id),
            }],
            deposits: vec![DepositLiability {
                amount_micro: 8,
                status: DepositMachineStatus::ObservedFinalized,
            }],
            lots: vec![BonusLotCoverage {
                remaining_promise_micro: 5,
                grant_class: "real_money".into(),
                converted: false,
            }],
            attempts: vec![
                OutboundAttemptFact {
                    payment_id: id,
                    subject_id: id,
                    landing_state: "finalized",
                },
                OutboundAttemptFact {
                    payment_id: id,
                    subject_id: id,
                    landing_state: "finalized",
                },
            ],
        };
        let results = evaluate_money_identities(&facts);
        assert!(results.iter().any(|row| !row.pass));
        assert!(!deposit_suspense_sigma(&facts).pass);
        assert!(!bonus_reserve_coverage(&facts).pass);
        assert!(!withdrawal_hold_matches_active(&facts).pass);
        assert!(!withdrawal_hold_exact(&facts).pass);
        assert!(!withdrawal_terminal_xor(&facts).pass);
        assert!(!withdrawal_attempt_lineage(&facts).pass);

        facts.lots[0].converted = true;
        facts.lots[0].grant_class = "sweeps".into();
        assert!(bonus_reserve_coverage(&facts).pass);

        let denied = WithdrawalAttribution {
            id: Uuid::from_u128(8),
            amount_micro: 2,
            status: WithdrawalStatus::Denied,
            send_state: WithdrawalSendState::Unsent,
            hold_amount_micro: 2,
            release_amount_micro: Some(2),
            settle_amount_micro: None,
            hold_tx_id: Uuid::from_u128(8),
            release_tx_id: Some(Uuid::from_u128(11)),
            settle_tx_id: None,
        };
        assert!(!denied.has_active_hold());
        assert!(!denied.is_accepted());
        assert!(denied.is_terminal());
        let admitted = DepositLiability {
            amount_micro: 1,
            status: DepositMachineStatus::Admitted,
        };
        assert!(!admitted.is_open_observation());
        let zero_release = WithdrawalAttribution {
            release_amount_micro: Some(0),
            settle_amount_micro: None,
            status: WithdrawalStatus::Failed,
            send_state: WithdrawalSendState::DefinitiveFailed,
            ..denied
        };
        let xor_facts = MoneyIdentityFacts {
            withdrawals: vec![zero_release],
            ..MoneyIdentityFacts::default()
        };
        assert!(!withdrawal_terminal_xor(&xor_facts).pass);
    }

    #[test]
    fn money_identities_flag_missing_lineage_and_duplicate_terminal_effects() {
        let settled_no_attempt = MoneyIdentityFacts {
            withdrawals: vec![WithdrawalAttribution {
                id: Uuid::from_u128(12),
                amount_micro: 1,
                status: WithdrawalStatus::Settled,
                send_state: WithdrawalSendState::Finalized,
                hold_amount_micro: 1,
                release_amount_micro: None,
                settle_amount_micro: Some(1),
                hold_tx_id: Uuid::from_u128(12),
                release_tx_id: None,
                settle_tx_id: Some(Uuid::from_u128(12)),
            }],
            ..MoneyIdentityFacts::default()
        };
        assert!(!withdrawal_attempt_lineage(&settled_no_attempt).pass);

        let dup = MoneyIdentityFacts {
            withdrawals: vec![
                WithdrawalAttribution {
                    id: Uuid::from_u128(20),
                    amount_micro: 1,
                    status: WithdrawalStatus::Failed,
                    send_state: WithdrawalSendState::DefinitiveFailed,
                    hold_amount_micro: 1,
                    release_amount_micro: Some(1),
                    settle_amount_micro: None,
                    hold_tx_id: Uuid::from_u128(20),
                    release_tx_id: Some(Uuid::from_u128(99)),
                    settle_tx_id: None,
                },
                WithdrawalAttribution {
                    id: Uuid::from_u128(21),
                    amount_micro: 1,
                    status: WithdrawalStatus::Failed,
                    send_state: WithdrawalSendState::DefinitiveFailed,
                    hold_amount_micro: 1,
                    release_amount_micro: Some(1),
                    settle_amount_micro: None,
                    hold_tx_id: Uuid::from_u128(21),
                    release_tx_id: Some(Uuid::from_u128(99)),
                    settle_tx_id: None,
                },
            ],
            ..MoneyIdentityFacts::default()
        };
        assert!(!withdrawal_no_duplicate_terminal(&dup).pass);

        let settle_dup = MoneyIdentityFacts {
            withdrawals: vec![
                WithdrawalAttribution {
                    id: Uuid::from_u128(30),
                    amount_micro: 1,
                    status: WithdrawalStatus::Settled,
                    send_state: WithdrawalSendState::Finalized,
                    hold_amount_micro: 1,
                    release_amount_micro: None,
                    settle_amount_micro: Some(1),
                    hold_tx_id: Uuid::from_u128(30),
                    release_tx_id: None,
                    settle_tx_id: Some(Uuid::from_u128(77)),
                },
                WithdrawalAttribution {
                    id: Uuid::from_u128(31),
                    amount_micro: 1,
                    status: WithdrawalStatus::Settled,
                    send_state: WithdrawalSendState::Finalized,
                    hold_amount_micro: 1,
                    release_amount_micro: None,
                    settle_amount_micro: Some(1),
                    hold_tx_id: Uuid::from_u128(31),
                    release_tx_id: None,
                    settle_tx_id: Some(Uuid::from_u128(77)),
                },
            ],
            ..MoneyIdentityFacts::default()
        };
        assert!(!withdrawal_no_duplicate_terminal(&settle_dup).pass);

        for status in [
            DepositMachineStatus::ComplianceHold,
            DepositMachineStatus::RefundApproved,
            DepositMachineStatus::RefundSending,
        ] {
            assert!(DepositLiability {
                amount_micro: 1,
                status,
            }
            .is_open_observation());
        }
        for send in [
            WithdrawalSendState::Sending,
            WithdrawalSendState::Broadcast,
            WithdrawalSendState::Unknown,
            WithdrawalSendState::Finalized,
        ] {
            let row = WithdrawalAttribution {
                send_state: send,
                ..accepted_hold(Uuid::from_u128(40), 1)
            };
            assert!(row.has_active_hold());
        }
    }

    #[test]
    fn money_identity_facts_project_balances_and_flag_receivable_rows() {
        let from_balances = MoneyIdentityFacts::from_balances(&[
            crate::model::AccountBalanceRow {
                owner_type: OwnerType::Withheld,
                balance_micro: 3,
            },
            crate::model::AccountBalanceRow {
                owner_type: OwnerType::DepositSuspense,
                balance_micro: 4,
            },
            crate::model::AccountBalanceRow {
                owner_type: OwnerType::BonusReserve,
                balance_micro: 5,
            },
        ]);
        assert_eq!(from_balances.withheld_balance_micro, 3);
        assert_eq!(from_balances.suspense_balance_micro, 4);
        assert_eq!(from_balances.bonus_reserve_balance_micro, 5);

        let failed_send = WithdrawalAttribution {
            send_state: WithdrawalSendState::DefinitiveFailed,
            status: WithdrawalStatus::Failed,
            release_tx_id: Some(Uuid::from_u128(1)),
            ..accepted_hold(Uuid::from_u128(50), 1)
        };
        assert!(!failed_send.has_active_hold());

        let mismatch = crate::model::ReceivableReconRow {
            origin_reversal_txn: Uuid::from_u128(1),
            opened_micro: 5,
            house_shortfall_micro: 4,
            collected_micro: 0,
            written_off_micro: 0,
        };
        assert!(receivable_row_broken(&mismatch));
        let over = crate::model::ReceivableReconRow {
            origin_reversal_txn: Uuid::from_u128(2),
            opened_micro: 5,
            house_shortfall_micro: 5,
            collected_micro: 3,
            written_off_micro: 3,
        };
        assert!(receivable_row_broken(&over));
        let ok_row = crate::model::ReceivableReconRow {
            origin_reversal_txn: Uuid::from_u128(3),
            opened_micro: 5,
            house_shortfall_micro: 5,
            collected_micro: 2,
            written_off_micro: 1,
        };
        assert!(!receivable_row_broken(&ok_row));
        assert_eq!(
            format_broken_receivables(&[mismatch, over, ok_row]).len(),
            2
        );
    }

    #[test]
    fn aggregate_money_identities_handle_totals_larger_than_i64() {
        let mut facts = MoneyIdentityFacts::from_balances(&[
            crate::model::AccountBalanceRow {
                owner_type: OwnerType::Withheld,
                balance_micro: i64::MAX,
            },
            crate::model::AccountBalanceRow {
                owner_type: OwnerType::Withheld,
                balance_micro: 1,
            },
            crate::model::AccountBalanceRow {
                owner_type: OwnerType::DepositSuspense,
                balance_micro: i64::MAX,
            },
            crate::model::AccountBalanceRow {
                owner_type: OwnerType::DepositSuspense,
                balance_micro: 1,
            },
            crate::model::AccountBalanceRow {
                owner_type: OwnerType::BonusReserve,
                balance_micro: i64::MAX,
            },
            crate::model::AccountBalanceRow {
                owner_type: OwnerType::BonusReserve,
                balance_micro: 1,
            },
        ]);
        facts.withdrawals = vec![
            accepted_hold(Uuid::from_u128(60), i64::MAX),
            accepted_hold(Uuid::from_u128(61), 1),
        ];
        facts.deposits = vec![
            DepositLiability {
                amount_micro: i64::MAX,
                status: DepositMachineStatus::AdmissionPending,
            },
            DepositLiability {
                amount_micro: 1,
                status: DepositMachineStatus::AdmissionPending,
            },
        ];
        facts.lots = vec![
            BonusLotCoverage {
                remaining_promise_micro: i64::MAX,
                grant_class: "real_money".into(),
                converted: false,
            },
            BonusLotCoverage {
                remaining_promise_micro: 1,
                grant_class: "real_money".into(),
                converted: false,
            },
        ];

        assert_eq!(facts.withheld_balance_micro.to_string(), "9223372036854775808");
        assert_eq!(facts.suspense_balance_micro.to_string(), "9223372036854775808");
        assert_eq!(
            facts.bonus_reserve_balance_micro.to_string(),
            "9223372036854775808"
        );
        let results = evaluate_money_identities(&facts);
        assert!(results.iter().all(|row| row.pass), "{results:?}");
    }
}
