//! Six weighted load personas and their pure decision functions.

use serde::{Deserialize, Serialize};

use super::action::{Action, MarketView, Side, TradeDirection};
use super::rng::CountedRng;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[repr(u8)]
#[serde(rename_all = "snake_case")]
pub enum Persona {
    Whale,
    SmallDabbler,
    VoterOnly,
    CloseWindowSniper,
    PanicSeller,
    Noise,
    /// D35 money path. Deliberately NOT in [`Persona::ALL`]: the round-robin
    /// load mix is unchanged, and the release profile assigns this persona to
    /// its pinned 10% share explicitly.
    MoneyPath,
}

impl Persona {
    pub const ALL: [Self; 6] = [
        Self::Whale,
        Self::SmallDabbler,
        Self::VoterOnly,
        Self::CloseWindowSniper,
        Self::PanicSeller,
        Self::Noise,
    ];

    /// Share of a release roster that walks the money path (D35: 10%).
    pub const MONEY_PATH_EVERY_NTH: u32 = 10;

    pub fn decide(self, _agent: &AgentSpec, view: &MarketView, rng: &mut CountedRng) -> Action {
        let roll = rng.bounded(1_000_000);
        let side = if roll.is_multiple_of(2) {
            Side::Yes
        } else {
            Side::No
        };
        match self {
            Self::Whale => Action::Trade {
                side,
                direction: TradeDirection::Buy,
                amount_micro: 25_000_000,
            },
            Self::SmallDabbler => Action::Trade {
                side,
                direction: TradeDirection::Buy,
                amount_micro: 1_000_000,
            },
            Self::VoterOnly => Action::Vote {
                side,
                crowd_guess_pct: u8::try_from(rng.bounded(101)).unwrap_or(50),
            },
            Self::CloseWindowSniper => {
                if view.closes_tick.saturating_sub(view.sim_tick) <= 120 {
                    Action::Trade {
                        side,
                        direction: TradeDirection::Buy,
                        amount_micro: 5_000_000,
                    }
                } else {
                    Action::Observe
                }
            }
            Self::PanicSeller => {
                if view.price_yes_micro < 400_000 {
                    Action::Trade {
                        side: Side::Yes,
                        direction: TradeDirection::Sell,
                        amount_micro: 1_000_000,
                    }
                } else {
                    Action::Trade {
                        side: Side::Yes,
                        direction: TradeDirection::Buy,
                        amount_micro: 1_000_000,
                    }
                }
            }
            Self::Noise => match roll % 3 {
                0 => Action::Vote {
                    side,
                    crowd_guess_pct: 50,
                },
                1 => Action::Trade {
                    side,
                    direction: TradeDirection::Buy,
                    amount_micro: 500_000,
                },
                _ => Action::Abstain {
                    reason: "noise".into(),
                },
            },
            // Two thirds of its opportunities request a withdrawal; the rest
            // observe, so the money path is a share of the load and not a
            // per-tick flood against the hot-wallet cap.
            Self::MoneyPath => {
                if roll % 3 == 2 {
                    Action::Observe
                } else {
                    Action::Withdraw {
                        amount_micro: 5_000_000,
                    }
                }
            }
        }
    }

    #[must_use]
    pub const fn rng_words_per_decision(self) -> u64 {
        if matches!(self, Self::VoterOnly) {
            2
        } else {
            1
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSpec {
    pub id: String,
    pub user_id: String,
    pub index: u32,
    pub persona: Persona,
    pub aged: bool,
    pub trade_capable: bool,
    pub device_id: String,
    pub forwarded_for: String,
    pub channel_address: String,
    pub rep_seed_micro: i64,
    pub target_market: String,
    /// D35 release-profile money path. Defaulted so a manifest written before
    /// the money path still deserializes.
    #[serde(default)]
    pub money_capable: bool,
}

impl AgentSpec {
    #[must_use]
    pub fn synthetic(index: u32, persona: Persona, target_market: impl Into<String>) -> Self {
        let second = (index / 256) % 256;
        let third = index % 256;
        Self {
            id: format!("agent-{index:04}"),
            user_id: format!("00000000-0000-4000-8000-{index:012}"),
            index,
            persona,
            aged: index % 5 != 4,
            trade_capable: !matches!(persona, Persona::VoterOnly | Persona::MoneyPath),
            device_id: format!("device-{index:04}"),
            forwarded_for: format!("10.{second}.{third}.1"),
            channel_address: format!("+1555{index:07}"),
            rep_seed_micro: 0,
            target_market: target_market.into(),
            money_capable: persona == Persona::MoneyPath,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::rng::CountedRng;

    fn agent(persona: Persona) -> AgentSpec {
        AgentSpec::synthetic(1, persona, "market")
    }

    fn view(tick: u64, yes: i64) -> MarketView {
        MarketView {
            market_ref: "market".into(),
            state: "live".into(),
            price_yes_micro: yes,
            price_no_micro: 1_000_000 - yes,
            sim_tick: tick,
            closes_tick: 100,
        }
    }

    #[test]
    fn all_six_personas_make_typed_decisions() {
        assert_eq!(Persona::ALL.len(), 6);
        for persona in Persona::ALL {
            let mut rng = CountedRng::new([persona as u8; 32]);
            let action = persona.decide(&agent(persona), &view(95, 300_000), &mut rng);
            assert!(!action.kind().is_empty());
            assert!(rng.consumed() > 0);
            assert_eq!(rng.consumed(), persona.rng_words_per_decision());
        }
    }

    #[test]
    fn time_and_price_sensitive_personas_cover_both_branches() {
        let mut rng = CountedRng::new([1; 32]);
        let mut far_from_close = view(1, 500_000);
        far_from_close.closes_tick = 200;
        assert_eq!(
            Persona::CloseWindowSniper.decide(
                &agent(Persona::CloseWindowSniper),
                &far_from_close,
                &mut rng
            ),
            Action::Observe
        );
        assert!(matches!(
            Persona::CloseWindowSniper.decide(
                &agent(Persona::CloseWindowSniper),
                &view(1, 500_000),
                &mut rng
            ),
            Action::Trade {
                direction: TradeDirection::Buy,
                ..
            }
        ));
        assert!(matches!(
            Persona::PanicSeller.decide(&agent(Persona::PanicSeller), &view(1, 700_000), &mut rng),
            Action::Trade {
                direction: TradeDirection::Buy,
                ..
            }
        ));
        assert!(matches!(
            Persona::PanicSeller.decide(&agent(Persona::PanicSeller), &view(1, 300_000), &mut rng),
            Action::Trade {
                direction: TradeDirection::Sell,
                ..
            }
        ));
    }

    #[test]
    fn money_path_persona_withdraws_and_observes_outside_the_load_mix() {
        assert!(!Persona::ALL.contains(&Persona::MoneyPath));
        assert_eq!(Persona::MONEY_PATH_EVERY_NTH, 10);
        let spec = agent(Persona::MoneyPath);
        assert!(spec.money_capable);
        assert!(!spec.trade_capable);
        assert!(!agent(Persona::Whale).money_capable);
        let mut kinds = std::collections::BTreeSet::new();
        for byte in 0..=u8::MAX {
            let mut rng = CountedRng::new([byte; 32]);
            let action = Persona::MoneyPath.decide(&spec, &view(1, 500_000), &mut rng);
            assert_eq!(rng.consumed(), Persona::MoneyPath.rng_words_per_decision());
            kinds.insert(action.kind());
        }
        assert_eq!(kinds, ["observe", "withdraw"].into_iter().collect());
    }

    #[test]
    fn noise_persona_covers_vote_trade_and_abstain() {
        let mut kinds = std::collections::BTreeSet::new();
        for byte in 0..=u8::MAX {
            let mut rng = CountedRng::new([byte; 32]);
            kinds.insert(
                Persona::Noise
                    .decide(&agent(Persona::Noise), &view(1, 500_000), &mut rng)
                    .kind(),
            );
        }
        assert_eq!(kinds, ["abstain", "trade", "vote"].into_iter().collect());
    }
}
