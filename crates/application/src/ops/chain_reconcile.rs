//! D35 signed reconciliation at one pinned finalized chain cut.
//!
//! The signed formula is
//! `wallet@cut = −external@db − outbound_finalized_unsettled + inbound_observed_unbooked`.
//! Broadcast/unknown attempts are exposure, never drift.

use crate::ops::alerts::{IncidentKey, DETECTOR_RECON};

/// One pinned finalized cut. Observation slots used in each term are
/// persisted on the cut so a page is reconstructible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconciliationCut {
    pub cut_slot: i64,
    pub wallet_balance_micro: i64,
    pub external_balance_micro: i64,
    pub outbound_finalized_unsettled_micro: i64,
    pub inbound_observed_unbooked_micro: i64,
    pub broadcast_unknown_exposure_micro: i64,
    pub wallet_observation: &'static str,
    pub db_observation: &'static str,
}

/// Signed residual in micro-USD. Zero is green; ±1 pages.
#[must_use]
pub fn signed_residual(cut: &ReconciliationCut) -> i64 {
    // wallet = -external - outbound_unsettled + inbound_unbooked + residual
    cut.wallet_balance_micro + cut.external_balance_micro + cut.outbound_finalized_unsettled_micro
        - cut.inbound_observed_unbooked_micro
}

/// True when the residual must page (any nonzero micro).
#[must_use]
pub fn pages_at_one_micro(cut: &ReconciliationCut) -> bool {
    signed_residual(cut) != 0
}

/// Exposure report: in-flight attempts never enter the residual.
#[must_use]
pub fn exposure_micro(cut: &ReconciliationCut) -> i64 {
    cut.broadcast_unknown_exposure_micro
}

/// Incident key for a residual page at this cut.
#[must_use]
pub fn residual_incident(cut: &ReconciliationCut) -> IncidentKey {
    IncidentKey::new(
        DETECTOR_RECON,
        format!("slot:{}", cut.cut_slot),
        format!("residual:{}", signed_residual(cut)),
    )
}

/// Named numeric examples required by D35 / exit 10.
#[must_use]
pub fn example_cut(kind: ReconExample) -> ReconciliationCut {
    match kind {
        ReconExample::Balanced => ReconciliationCut {
            cut_slot: 10,
            wallet_balance_micro: 1_000,
            external_balance_micro: -1_000,
            outbound_finalized_unsettled_micro: 0,
            inbound_observed_unbooked_micro: 0,
            broadcast_unknown_exposure_micro: 0,
            wallet_observation: "finalized",
            db_observation: "repeatable_read",
        },
        ReconExample::DroppedObservation => ReconciliationCut {
            cut_slot: 11,
            wallet_balance_micro: 1_000,
            external_balance_micro: -500,
            outbound_finalized_unsettled_micro: 0,
            inbound_observed_unbooked_micro: 0,
            broadcast_unknown_exposure_micro: 0,
            wallet_observation: "finalized",
            db_observation: "repeatable_read",
        },
        ReconExample::ConfirmedOnly => ReconciliationCut {
            cut_slot: 12,
            wallet_balance_micro: 800,
            external_balance_micro: -800,
            outbound_finalized_unsettled_micro: 0,
            inbound_observed_unbooked_micro: 0,
            broadcast_unknown_exposure_micro: 200,
            wallet_observation: "finalized",
            db_observation: "repeatable_read",
        },
        ReconExample::FinalizedBeforeDb => ReconciliationCut {
            cut_slot: 13,
            wallet_balance_micro: 1_500,
            external_balance_micro: -1_000,
            outbound_finalized_unsettled_micro: 0,
            inbound_observed_unbooked_micro: 500,
            broadcast_unknown_exposure_micro: 0,
            wallet_observation: "finalized",
            db_observation: "repeatable_read",
        },
        ReconExample::Replacement => ReconciliationCut {
            cut_slot: 14,
            wallet_balance_micro: 900,
            external_balance_micro: -1_000,
            outbound_finalized_unsettled_micro: 100,
            inbound_observed_unbooked_micro: 0,
            broadcast_unknown_exposure_micro: 50,
            wallet_observation: "finalized",
            db_observation: "repeatable_read",
        },
        ReconExample::OneMicroResidual => ReconciliationCut {
            cut_slot: 15,
            wallet_balance_micro: 1_001,
            external_balance_micro: -1_000,
            outbound_finalized_unsettled_micro: 0,
            inbound_observed_unbooked_micro: 0,
            broadcast_unknown_exposure_micro: 0,
            wallet_observation: "finalized",
            db_observation: "repeatable_read",
        },
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconExample {
    Balanced,
    DroppedObservation,
    ConfirmedOnly,
    FinalizedBeforeDb,
    Replacement,
    OneMicroResidual,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signed_formula_matches_each_named_state() {
        let balanced = example_cut(ReconExample::Balanced);
        assert_eq!(signed_residual(&balanced), 0);
        assert!(!pages_at_one_micro(&balanced));
        assert_eq!(exposure_micro(&balanced), 0);

        let dropped = example_cut(ReconExample::DroppedObservation);
        assert_eq!(signed_residual(&dropped), 500);
        assert!(pages_at_one_micro(&dropped));

        let confirmed = example_cut(ReconExample::ConfirmedOnly);
        assert_eq!(signed_residual(&confirmed), 0);
        assert_eq!(exposure_micro(&confirmed), 200);
        assert!(!pages_at_one_micro(&confirmed));

        let inbound = example_cut(ReconExample::FinalizedBeforeDb);
        assert_eq!(signed_residual(&inbound), 0);

        let replacement = example_cut(ReconExample::Replacement);
        assert_eq!(signed_residual(&replacement), 0);
        assert_eq!(exposure_micro(&replacement), 50);

        let residual = example_cut(ReconExample::OneMicroResidual);
        assert_eq!(signed_residual(&residual), 1);
        assert!(pages_at_one_micro(&residual));
        let key = residual_incident(&residual);
        assert!(key.encoded().contains("reconciliation_residual"));
        assert!(key.encoded().contains("slot:15"));
    }
}
