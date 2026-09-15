/// The server-authoritative lifecycle state of a market.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MarketState {
    Draft,
    Scheduled,
    Live,
    Closing,
    Closed,
    Resolving,
    Resolved,
    Paid,
    Voided,
}

/// An attempted lifecycle transition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MarketEvent {
    Approve,
    GoLive,
    EnterCloseWindow,
    Close,
    StartIntegritySweep,
    Resolve,
    Pay,
    VoidLowParticipation,
    VoidByAdmin,
}

/// A rejected lifecycle transition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransitionError {
    Illegal {
        from: MarketState,
        event: MarketEvent,
    },
}

/// Applies one market event, rejecting every edge not explicitly authorized.
///
/// # Errors
///
/// Returns [`TransitionError::Illegal`] when the state/event pair is not a
/// legal lifecycle edge.
pub fn transition(state: MarketState, event: MarketEvent) -> Result<MarketState, TransitionError> {
    use MarketEvent::{
        Approve, Close, EnterCloseWindow, GoLive, Pay, Resolve, StartIntegritySweep, VoidByAdmin,
        VoidLowParticipation,
    };
    use MarketState::{Closed, Closing, Draft, Live, Paid, Resolved, Resolving, Scheduled, Voided};

    match (state, event) {
        (Draft, Approve) => Ok(Scheduled),
        (Scheduled, GoLive) => Ok(Live),
        (Live, EnterCloseWindow) => Ok(Closing),
        (Closing, Close) => Ok(Closed),
        (Closed, StartIntegritySweep) => Ok(Resolving),
        (Closed | Resolving, Resolve) => Ok(Resolved),
        (Resolved, Pay) => Ok(Paid),
        (Closed | Resolving, VoidLowParticipation) => Ok(Voided),
        (Draft | Scheduled | Live | Closing | Closed | Resolving | Resolved, VoidByAdmin) => {
            Ok(Voided)
        }
        (
            from @ Draft,
            event @ (GoLive | EnterCloseWindow | Close | StartIntegritySweep | Resolve | Pay
            | VoidLowParticipation),
        )
        | (
            from @ Scheduled,
            event @ (Approve | EnterCloseWindow | Close | StartIntegritySweep | Resolve | Pay
            | VoidLowParticipation),
        )
        | (
            from @ Live,
            event @ (Approve | GoLive | Close | StartIntegritySweep | Resolve | Pay
            | VoidLowParticipation),
        )
        | (
            from @ Closing,
            event @ (Approve | GoLive | EnterCloseWindow | StartIntegritySweep | Resolve | Pay
            | VoidLowParticipation),
        )
        | (from @ Closed, event @ (Approve | GoLive | EnterCloseWindow | Close | Pay))
        | (
            from @ Resolving,
            event @ (Approve | GoLive | EnterCloseWindow | Close | StartIntegritySweep | Pay),
        )
        | (
            from @ Resolved,
            event @ (Approve | GoLive | EnterCloseWindow | Close | StartIntegritySweep | Resolve
            | VoidLowParticipation),
        )
        | (
            from @ (Paid | Voided),
            event @ (Approve | GoLive | EnterCloseWindow | Close | StartIntegritySweep | Resolve
            | Pay | VoidLowParticipation | VoidByAdmin),
        ) => Err(TransitionError::Illegal { from, event }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_STATES: [MarketState; 9] = [
        MarketState::Draft,
        MarketState::Scheduled,
        MarketState::Live,
        MarketState::Closing,
        MarketState::Closed,
        MarketState::Resolving,
        MarketState::Resolved,
        MarketState::Paid,
        MarketState::Voided,
    ];

    const ALL_EVENTS: [MarketEvent; 9] = [
        MarketEvent::Approve,
        MarketEvent::GoLive,
        MarketEvent::EnterCloseWindow,
        MarketEvent::Close,
        MarketEvent::StartIntegritySweep,
        MarketEvent::Resolve,
        MarketEvent::Pay,
        MarketEvent::VoidLowParticipation,
        MarketEvent::VoidByAdmin,
    ];

    const LEGAL_EDGES: [(MarketState, MarketEvent, MarketState); 17] = [
        (
            MarketState::Draft,
            MarketEvent::Approve,
            MarketState::Scheduled,
        ),
        (
            MarketState::Scheduled,
            MarketEvent::GoLive,
            MarketState::Live,
        ),
        (
            MarketState::Live,
            MarketEvent::EnterCloseWindow,
            MarketState::Closing,
        ),
        (
            MarketState::Closing,
            MarketEvent::Close,
            MarketState::Closed,
        ),
        (
            MarketState::Closed,
            MarketEvent::StartIntegritySweep,
            MarketState::Resolving,
        ),
        (
            MarketState::Closed,
            MarketEvent::Resolve,
            MarketState::Resolved,
        ),
        (
            MarketState::Resolving,
            MarketEvent::Resolve,
            MarketState::Resolved,
        ),
        (MarketState::Resolved, MarketEvent::Pay, MarketState::Paid),
        (
            MarketState::Closed,
            MarketEvent::VoidLowParticipation,
            MarketState::Voided,
        ),
        (
            MarketState::Resolving,
            MarketEvent::VoidLowParticipation,
            MarketState::Voided,
        ),
        (
            MarketState::Draft,
            MarketEvent::VoidByAdmin,
            MarketState::Voided,
        ),
        (
            MarketState::Scheduled,
            MarketEvent::VoidByAdmin,
            MarketState::Voided,
        ),
        (
            MarketState::Live,
            MarketEvent::VoidByAdmin,
            MarketState::Voided,
        ),
        (
            MarketState::Closing,
            MarketEvent::VoidByAdmin,
            MarketState::Voided,
        ),
        (
            MarketState::Closed,
            MarketEvent::VoidByAdmin,
            MarketState::Voided,
        ),
        (
            MarketState::Resolving,
            MarketEvent::VoidByAdmin,
            MarketState::Voided,
        ),
        (
            MarketState::Resolved,
            MarketEvent::VoidByAdmin,
            MarketState::Voided,
        ),
    ];

    #[test]
    fn happy_path_without_sweep() {
        use MarketEvent::{Approve, Close, EnterCloseWindow, GoLive, Pay, Resolve};
        use MarketState::{Closed, Closing, Draft, Live, Paid, Resolved, Scheduled};

        let path = [
            (Draft, Approve, Scheduled),
            (Scheduled, GoLive, Live),
            (Live, EnterCloseWindow, Closing),
            (Closing, Close, Closed),
            (Closed, Resolve, Resolved),
            (Resolved, Pay, Paid),
        ];

        for (from, event, to) in path {
            assert_eq!(
                transition(from, event),
                Ok(to),
                "{from:?} --{event:?}--> {to:?}"
            );
        }
    }

    #[test]
    fn happy_path_with_integrity_sweep() {
        assert_eq!(
            transition(MarketState::Closed, MarketEvent::StartIntegritySweep),
            Ok(MarketState::Resolving)
        );
        assert_eq!(
            transition(MarketState::Resolving, MarketEvent::Resolve),
            Ok(MarketState::Resolved)
        );
    }

    #[test]
    fn automatic_and_admin_void_paths_are_explicit() {
        assert_eq!(
            transition(MarketState::Closed, MarketEvent::VoidLowParticipation),
            Ok(MarketState::Voided)
        );
        assert_eq!(
            transition(MarketState::Live, MarketEvent::VoidByAdmin),
            Ok(MarketState::Voided)
        );
        assert_eq!(
            transition(MarketState::Resolved, MarketEvent::VoidByAdmin),
            Ok(MarketState::Voided)
        );
    }

    #[test]
    fn paid_and_voided_are_terminal() {
        for state in [MarketState::Paid, MarketState::Voided] {
            for event in ALL_EVENTS {
                assert_eq!(
                    transition(state, event),
                    Err(TransitionError::Illegal { from: state, event })
                );
            }
        }
    }

    #[test]
    fn every_state_event_pair_matches_the_legal_edge_table() {
        for state in ALL_STATES {
            for event in ALL_EVENTS {
                let expected = LEGAL_EDGES
                    .iter()
                    .find_map(|&(from, candidate, to)| {
                        (from == state && candidate == event).then_some(Ok(to))
                    })
                    .unwrap_or(Err(TransitionError::Illegal { from: state, event }));

                assert_eq!(transition(state, event), expected, "{state:?} --{event:?}");
            }
        }
    }
}
