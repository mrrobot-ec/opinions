//! Pure decision and market-view values shared by personas, rings, and traces.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ActorRef {
    Agent { id: String },
    Ring { id: String },
}

impl ActorRef {
    #[must_use]
    pub fn agent(id: impl Into<String>) -> Self {
        Self::Agent { id: id.into() }
    }

    #[must_use]
    pub fn ring(id: impl Into<String>) -> Self {
        Self::Ring { id: id.into() }
    }

    #[must_use]
    pub fn id(&self) -> &str {
        match self {
            Self::Agent { id } | Self::Ring { id } => id,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Yes,
    No,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TradeDirection {
    Buy,
    Sell,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarketView {
    pub market_ref: String,
    pub state: String,
    pub price_yes_micro: i64,
    pub price_no_micro: i64,
    pub sim_tick: u64,
    pub closes_tick: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Action {
    Observe,
    Abstain {
        reason: String,
    },
    Vote {
        side: Side,
        crowd_guess_pct: u8,
    },
    Trade {
        side: Side,
        direction: TradeDirection,
        amount_micro: i64,
    },
    /// D35 money path: a withdraw request against the public rail. It is
    /// deliberately outside the gated `trade-confirm` series.
    Withdraw {
        amount_micro: i64,
    },
}

impl Action {
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Observe => "observe",
            Self::Abstain { .. } => "abstain",
            Self::Vote { .. } => "vote",
            Self::Trade { .. } => "trade",
            Self::Withdraw { .. } => "withdraw",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentAction {
    pub agent_id: String,
    pub action: Action,
}

impl AgentAction {
    #[must_use]
    pub fn new(agent_id: impl Into<String>, action: Action) -> Self {
        Self {
            agent_id: agent_id.into(),
            action,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Decision {
    pub actions: Vec<AgentAction>,
}

impl Decision {
    #[must_use]
    pub const fn new(actions: Vec<AgentAction>) -> Self {
        Self { actions }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actor_and_action_labels_are_stable() {
        assert_eq!(ActorRef::agent("a").id(), "a");
        assert_eq!(ActorRef::ring("r").id(), "r");
        assert_eq!(Action::Observe.kind(), "observe");
        assert_eq!(
            Action::Abstain {
                reason: "hold".into()
            }
            .kind(),
            "abstain"
        );
        assert_eq!(
            Action::Vote {
                side: Side::Yes,
                crowd_guess_pct: 70
            }
            .kind(),
            "vote"
        );
        assert_eq!(
            Action::Trade {
                side: Side::No,
                direction: TradeDirection::Buy,
                amount_micro: 1,
            }
            .kind(),
            "trade"
        );
        assert_eq!(Action::Withdraw { amount_micro: 5 }.kind(), "withdraw");
    }

    #[test]
    fn decision_preserves_ring_vector_order() {
        let decision = Decision::new(vec![
            AgentAction::new("b", Action::Observe),
            AgentAction::new("a", Action::Observe),
        ]);
        assert_eq!(decision.actions[0].agent_id, "b");
        assert_eq!(decision.actions[1].agent_id, "a");
    }
}
