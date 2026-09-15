//! Coordinated attack rings and roster-isolation contracts.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use super::action::{Action, AgentAction, Decision, MarketView, Side, TradeDirection};
use super::persona::AgentSpec;
use super::rng::CountedRng;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RingKind {
    AgedSybil,
    WashPair,
    VoidSuppress,
    ThresholdPad,
    ReferralChain,
    BonusWash,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RingSpec {
    pub id: String,
    pub kind: RingKind,
    pub members: Vec<String>,
    pub shared_device_id: String,
    pub forwarded_for: String,
    pub channel_prefix: String,
    pub target_market: String,
    pub honest_baseline: u32,
    pub prior_window_votes: u32,
    /// BonusWash: stamped lot size the wash fees are claimed against.
    #[serde(default)]
    pub bonus_lot_micro: i64,
    /// BonusWash: stated cash fee volume (must be ≥ lot so "no convert"
    /// proves the Paid-finalization rule, not thin volume).
    #[serde(default)]
    pub stated_fee_volume_micro: i64,
}

impl RingSpec {
    #[must_use]
    pub fn rng_words_per_decision(&self) -> u64 {
        match self.kind {
            RingKind::AgedSybil => u64::try_from(self.members.len()).unwrap_or(u64::MAX),
            RingKind::WashPair
            | RingKind::VoidSuppress
            | RingKind::ThresholdPad
            | RingKind::ReferralChain
            | RingKind::BonusWash => 1,
        }
    }

    #[must_use]
    pub fn opportunity_ticks(&self, close_tick: u64) -> Vec<u64> {
        match self.kind {
            RingKind::WashPair => vec![1, 2],
            RingKind::BonusWash => vec![4, 5],
            RingKind::ReferralChain => vec![3],
            RingKind::AgedSybil | RingKind::VoidSuppress | RingKind::ThresholdPad => {
                vec![close_tick.saturating_sub(1)]
            }
        }
    }

    pub fn decide(
        &self,
        local_seq: u64,
        _view: &MarketView,
        rng: &mut CountedRng,
    ) -> Result<Decision, RingError> {
        if self.members.is_empty() {
            return Err(RingError::EmptyRing);
        }
        let mut actions = Vec::new();
        match self.kind {
            RingKind::AgedSybil => {
                for member in &self.members {
                    actions.push(AgentAction::new(
                        member,
                        Action::Vote {
                            side: Side::Yes,
                            crowd_guess_pct: 70 + u8::try_from(rng.bounded(21)).unwrap_or(0),
                        },
                    ));
                }
            }
            RingKind::WashPair => {
                let jitter = i64::try_from(rng.bounded(10_000)).unwrap_or(0);
                for (index, member) in self.members.iter().enumerate() {
                    let direction = if local_seq > 0 && index == 0 {
                        TradeDirection::Sell
                    } else {
                        TradeDirection::Buy
                    };
                    actions.push(AgentAction::new(
                        member,
                        Action::Trade {
                            side: Side::Yes,
                            direction,
                            amount_micro: 1_000_000 + jitter,
                        },
                    ));
                }
            }
            RingKind::VoidSuppress => {
                let _ = rng.bounded(1);
                for member in &self.members {
                    actions.push(AgentAction::new(
                        member,
                        Action::Abstain {
                            reason: "suppress-resolution".into(),
                        },
                    ));
                }
            }
            RingKind::ThresholdPad => {
                let offset = usize::try_from(rng.bounded(2)).unwrap_or(0);
                for (index, member) in self.members.iter().enumerate() {
                    actions.push(AgentAction::new(
                        member,
                        Action::Vote {
                            side: if (index + offset) % 2 == 0 {
                                Side::Yes
                            } else {
                                Side::No
                            },
                            crowd_guess_pct: 50,
                        },
                    ));
                }
            }
            RingKind::ReferralChain => {
                let _ = rng.bounded(1);
                for (index, member) in self.members.iter().enumerate() {
                    actions.push(AgentAction::new(
                        member,
                        Action::Abstain {
                            reason: if index == 0 {
                                "referral-referrer".into()
                            } else {
                                "referral-referee".into()
                            },
                        },
                    ));
                }
            }
            RingKind::BonusWash => {
                let _ = rng.bounded(1);
                let notional = bonus_wash_notional(self.stated_fee_volume_micro);
                for (index, member) in self.members.iter().enumerate() {
                    let direction = if local_seq > 0 && index == 0 {
                        TradeDirection::Sell
                    } else {
                        TradeDirection::Buy
                    };
                    actions.push(AgentAction::new(
                        member,
                        Action::Trade {
                            side: Side::Yes,
                            direction,
                            amount_micro: notional,
                        },
                    ));
                }
            }
        }
        Ok(Decision::new(actions))
    }
}

#[must_use]
pub fn bonus_wash_notional(stated_fee_volume_micro: i64) -> i64 {
    // 100bp pool: notional such that ceil(notional * 100 / 10_000) ≥ fee volume.
    stated_fee_volume_micro.saturating_mul(100).max(1)
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RingError {
    #[error("the six pinned ring kinds are required exactly once")]
    PinnedSet,
    #[error("ring cannot be empty")]
    EmptyRing,
    #[error("ring member is absent from the manifest")]
    UnknownMember,
    #[error("aged sybil efficacy inequality is not met")]
    SybilInequality,
    #[error("wash ring must contain exactly two agents")]
    WashPairSize,
    #[error("referral chain must contain exactly two distinct agents")]
    ReferralChainSize,
    #[error("bonus-wash ring must contain exactly two agents")]
    BonusWashSize,
    #[error("bonus-wash stated fee volume must be at least the stamped lot")]
    BonusWashFeeBelowLot,
    #[error("void suppression and threshold padding need distinct markets")]
    BranchMarketsOverlap,
}

#[must_use]
pub fn pinned_rings(agents: &[AgentSpec]) -> Vec<RingSpec> {
    let ids = agents
        .iter()
        .map(|agent| agent.id.clone())
        .collect::<Vec<_>>();
    let sybil_members = agents
        .iter()
        .filter(|agent| agent.aged)
        .take(31)
        .map(|agent| agent.id.clone())
        .collect::<Vec<_>>();
    let available = ids
        .iter()
        .filter(|id| !sybil_members.contains(id))
        .cloned()
        .collect::<Vec<_>>();
    let aged_available = agents
        .iter()
        .filter(|agent| agent.aged && !sybil_members.contains(&agent.id))
        .map(|agent| agent.id.clone())
        .collect::<Vec<_>>();
    let members =
        |start: usize, count: usize| available.iter().skip(start).take(count).cloned().collect();
    vec![
        RingSpec {
            id: "ring-aged-sybil".into(),
            kind: RingKind::AgedSybil,
            members: sybil_members,
            shared_device_id: "ring-device-sybil".into(),
            forwarded_for: "10.250.1.1".into(),
            channel_prefix: "+1555901".into(),
            target_market: "fat-pot".into(),
            honest_baseline: 15,
            prior_window_votes: 0,
            bonus_lot_micro: 0,
            stated_fee_volume_micro: 0,
        },
        RingSpec {
            id: "ring-wash-pair".into(),
            kind: RingKind::WashPair,
            members: aged_available.into_iter().take(2).collect(),
            shared_device_id: "ring-device-wash".into(),
            forwarded_for: "10.250.2.1".into(),
            channel_prefix: "+1555902".into(),
            target_market: "wash".into(),
            honest_baseline: 0,
            prior_window_votes: 0,
            bonus_lot_micro: 0,
            stated_fee_volume_micro: 0,
        },
        RingSpec {
            id: "ring-void-suppress".into(),
            kind: RingKind::VoidSuppress,
            members: members(2, 1),
            shared_device_id: "ring-device-suppress".into(),
            forwarded_for: "10.250.3.1".into(),
            channel_prefix: "+1555903".into(),
            target_market: "void-suppress".into(),
            honest_baseline: 0,
            prior_window_votes: 0,
            bonus_lot_micro: 0,
            stated_fee_volume_micro: 0,
        },
        RingSpec {
            id: "ring-threshold-pad".into(),
            kind: RingKind::ThresholdPad,
            members: members(3, 1),
            shared_device_id: "ring-device-pad".into(),
            forwarded_for: "10.250.4.1".into(),
            channel_prefix: "+1555904".into(),
            target_market: "threshold-pad".into(),
            honest_baseline: 0,
            prior_window_votes: 0,
            bonus_lot_micro: 0,
            stated_fee_volume_micro: 0,
        },
        RingSpec {
            id: "ring-referral-chain".into(),
            kind: RingKind::ReferralChain,
            members: members(4, 2),
            shared_device_id: "ring-device-referral".into(),
            forwarded_for: "10.250.5.1".into(),
            channel_prefix: "+1555905".into(),
            target_market: "lifecycle".into(),
            honest_baseline: 0,
            prior_window_votes: 0,
            bonus_lot_micro: 0,
            stated_fee_volume_micro: 0,
        },
        RingSpec {
            id: "ring-bonus-wash".into(),
            kind: RingKind::BonusWash,
            members: members(6, 2),
            shared_device_id: "ring-device-bonus".into(),
            forwarded_for: "10.250.6.1".into(),
            channel_prefix: "+1555906".into(),
            target_market: "lifecycle".into(),
            honest_baseline: 0,
            prior_window_votes: 0,
            bonus_lot_micro: 5_000_000,
            stated_fee_volume_micro: 5_000_000,
        },
    ]
}

pub fn validate_ring_set(
    rings: &[RingSpec],
    known_agents: &BTreeSet<String>,
) -> Result<(), RingError> {
    let kinds = rings.iter().map(|ring| ring.kind).collect::<BTreeSet<_>>();
    if rings.len() != 6 || kinds.len() != 6 {
        return Err(RingError::PinnedSet);
    }
    for ring in rings {
        if ring.members.is_empty() {
            return Err(RingError::EmptyRing);
        }
        if ring
            .members
            .iter()
            .any(|member| !known_agents.contains(member))
        {
            return Err(RingError::UnknownMember);
        }
        match ring.kind {
            RingKind::AgedSybil => {
                let k = u64::try_from(ring.members.len()).unwrap_or(0);
                let total = k + u64::from(ring.honest_baseline);
                let share_ppm = k.saturating_mul(1_000_000).checked_div(total).unwrap_or(0);
                if k < 31
                    || ring.honest_baseline > 15
                    || share_ppm <= 600_000
                    || k <= 2 * u64::from(ring.prior_window_votes)
                {
                    return Err(RingError::SybilInequality);
                }
            }
            RingKind::WashPair if ring.members.len() != 2 => {
                return Err(RingError::WashPairSize);
            }
            RingKind::ReferralChain => {
                if ring.members.len() != 2 || ring.members[0] == ring.members[1] {
                    return Err(RingError::ReferralChainSize);
                }
            }
            RingKind::BonusWash => {
                if ring.members.len() != 2 {
                    return Err(RingError::BonusWashSize);
                }
                if ring.stated_fee_volume_micro < ring.bonus_lot_micro || ring.bonus_lot_micro <= 0
                {
                    return Err(RingError::BonusWashFeeBelowLot);
                }
            }
            RingKind::WashPair | RingKind::VoidSuppress | RingKind::ThresholdPad => {}
        }
    }
    let suppress = rings
        .iter()
        .find(|ring| ring.kind == RingKind::VoidSuppress)
        .map(|ring| ring.target_market.as_str());
    let pad = rings
        .iter()
        .find(|ring| ring.kind == RingKind::ThresholdPad)
        .map(|ring| ring.target_market.as_str());
    if suppress == pad {
        return Err(RingError::BranchMarketsOverlap);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::action::{MarketView, TradeDirection};
    use crate::domain::persona::{AgentSpec, Persona};
    use crate::domain::rng::CountedRng;

    fn view() -> MarketView {
        MarketView {
            market_ref: "target".into(),
            state: "live".into(),
            price_yes_micro: 500_000,
            price_no_micro: 500_000,
            sim_tick: 1,
            closes_tick: 2,
        }
    }

    #[test]
    fn all_six_ring_decisions_are_ordered_and_effective() {
        let agents = (0..50)
            .map(|index| AgentSpec::synthetic(index, Persona::VoterOnly, "target"))
            .collect::<Vec<_>>();
        let rings = pinned_rings(&agents);
        assert_eq!(rings.len(), 6);
        for ring in rings {
            let mut rng = CountedRng::new([7; 32]);
            let decision = ring.decide(1, &view(), &mut rng).unwrap();
            assert!(!decision.actions.is_empty());
            assert_eq!(rng.consumed(), ring.rng_words_per_decision());
            assert!(!ring.opportunity_ticks(180).is_empty());
        }
        let mut rng = CountedRng::new([7; 32]);
        let wash = pinned_rings(&agents)
            .remove(1)
            .decide(1, &view(), &mut rng)
            .unwrap();
        assert!(matches!(
            wash.actions[0].action,
            Action::Trade {
                direction: TradeDirection::Sell,
                ..
            }
        ));
        let mut rng = CountedRng::new([7; 32]);
        let bonus = pinned_rings(&agents)
            .into_iter()
            .find(|ring| ring.kind == RingKind::BonusWash)
            .unwrap();
        assert!(bonus.stated_fee_volume_micro >= bonus.bonus_lot_micro);
        let bonus_decision = bonus.decide(1, &view(), &mut rng).unwrap();
        assert!(matches!(
            bonus_decision.actions[0].action,
            Action::Trade {
                direction: TradeDirection::Sell,
                amount_micro: 500_000_000,
                ..
            }
        ));
        let referral = pinned_rings(&agents)
            .into_iter()
            .find(|ring| ring.kind == RingKind::ReferralChain)
            .unwrap();
        let mut rng = CountedRng::new([7; 32]);
        let referred = referral.decide(0, &view(), &mut rng).unwrap();
        assert_eq!(referred.actions.len(), 2);
        assert!(matches!(referred.actions[0].action, Action::Abstain { .. }));
    }

    #[test]
    fn ring_contracts_reject_bad_members_and_inequalities() {
        let agents = (0..50)
            .map(|index| AgentSpec::synthetic(index, Persona::VoterOnly, "target"))
            .collect::<Vec<_>>();
        let known = agents.iter().map(|agent| agent.id.clone()).collect();
        let mut rings = pinned_rings(&agents);
        assert!(validate_ring_set(&rings, &known).is_ok());
        rings[0].honest_baseline = 100;
        assert_eq!(
            validate_ring_set(&rings, &known),
            Err(RingError::SybilInequality)
        );
        rings = pinned_rings(&agents);
        rings[0].members[0] = "missing".into();
        assert_eq!(
            validate_ring_set(&rings, &known),
            Err(RingError::UnknownMember)
        );
        rings = pinned_rings(&agents);
        rings[3].target_market = rings[2].target_market.clone();
        assert_eq!(
            validate_ring_set(&rings, &known),
            Err(RingError::BranchMarketsOverlap)
        );

        rings = pinned_rings(&agents);
        rings[1].members.pop();
        assert_eq!(
            validate_ring_set(&rings, &known),
            Err(RingError::WashPairSize)
        );

        rings = pinned_rings(&agents);
        rings[2].members.clear();
        assert_eq!(validate_ring_set(&rings, &known), Err(RingError::EmptyRing));
        let mut rng = CountedRng::new([0; 32]);
        assert_eq!(
            rings[2].decide(0, &view(), &mut rng),
            Err(RingError::EmptyRing)
        );

        rings = pinned_rings(&agents);
        rings[4].members.pop();
        assert_eq!(
            validate_ring_set(&rings, &known),
            Err(RingError::ReferralChainSize)
        );
        rings = pinned_rings(&agents);
        rings[4].members[1] = rings[4].members[0].clone();
        assert_eq!(
            validate_ring_set(&rings, &known),
            Err(RingError::ReferralChainSize)
        );
        rings = pinned_rings(&agents);
        rings[5].members.pop();
        assert_eq!(
            validate_ring_set(&rings, &known),
            Err(RingError::BonusWashSize)
        );
        rings = pinned_rings(&agents);
        rings[5].stated_fee_volume_micro = rings[5].bonus_lot_micro - 1;
        assert_eq!(
            validate_ring_set(&rings, &known),
            Err(RingError::BonusWashFeeBelowLot)
        );
        rings = pinned_rings(&agents);
        rings.pop();
        assert_eq!(validate_ring_set(&rings, &known), Err(RingError::PinnedSet));
        assert_eq!(bonus_wash_notional(0), 1);
    }

    #[test]
    fn threshold_pad_exercises_both_side_assignments() {
        let agents = (0..50)
            .map(|index| AgentSpec::synthetic(index, Persona::VoterOnly, "target"))
            .collect::<Vec<_>>();
        let mut pad = pinned_rings(&agents).remove(3);
        pad.members.push(agents[36].id.clone());
        let mut rng = CountedRng::new([9; 32]);
        let decision = pad.decide(0, &view(), &mut rng).unwrap();
        assert_ne!(decision.actions[0].action, decision.actions[1].action);
    }
}
