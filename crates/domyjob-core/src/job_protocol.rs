use crate::state::PhaseKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(kani, derive(kani::Arbitrary))]
pub enum State {
    Absent,
    Accepted,
    Active,
    Finished,
}

impl From<PhaseKind> for State {
    fn from(phase: PhaseKind) -> Self {
        match phase {
            PhaseKind::Accepted => Self::Accepted,
            PhaseKind::Queued | PhaseKind::Starting | PhaseKind::Running => Self::Active,
            PhaseKind::Finished => Self::Finished,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Submit { same_request: bool },
    Attach,
    Disconnect,
    Reap,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    Missing,
    Conflict,
    NotActive,
}

pub const fn step(state: State, action: Action) -> Result<State, Refusal> {
    match (state, action) {
        (State::Absent, Action::Submit { .. }) => Ok(State::Accepted),
        (
            State::Accepted | State::Active | State::Finished,
            Action::Submit {
                same_request: false,
            },
        ) => Err(Refusal::Conflict),
        (
            State::Accepted | State::Active | State::Finished,
            Action::Submit { same_request: true },
        )
        | (_, Action::Disconnect)
        | (State::Active | State::Finished, Action::Attach) => Ok(state),
        (State::Accepted, Action::Attach) => Ok(State::Active),
        (State::Absent, Action::Attach) => Err(Refusal::Missing),
        (State::Active, Action::Reap) => Ok(State::Finished),
        (State::Absent | State::Accepted | State::Finished, Action::Reap) => {
            Err(Refusal::NotActive)
        }
    }
}

#[cfg(kani)]
mod proofs {
    use super::{Action, Refusal, State, step};
    use crate::state::PhaseKind;

    #[kani::proof]
    fn persistent_jobs_survive_client_actions() {
        assert_eq!(State::from(PhaseKind::Accepted), State::Accepted);
        assert_eq!(State::from(PhaseKind::Queued), State::Active);
        assert_eq!(State::from(PhaseKind::Starting), State::Active);
        assert_eq!(State::from(PhaseKind::Running), State::Active);
        assert_eq!(State::from(PhaseKind::Finished), State::Finished);
        let state: State = kani::any();
        assert_eq!(step(state, Action::Disconnect), Ok(state));
        assert_eq!(
            step(state, Action::Submit { same_request: true }),
            Ok(if state == State::Absent {
                State::Accepted
            } else {
                state
            })
        );
        assert_eq!(
            step(
                state,
                Action::Submit {
                    same_request: false
                }
            ),
            if state == State::Absent {
                Ok(State::Accepted)
            } else {
                Err(Refusal::Conflict)
            }
        );
        assert_eq!(
            step(state, Action::Attach),
            match state {
                State::Absent => Err(Refusal::Missing),
                State::Accepted => Ok(State::Active),
                State::Active | State::Finished => Ok(state),
            }
        );
        assert_eq!(
            step(state, Action::Reap),
            if state == State::Active {
                Ok(State::Finished)
            } else {
                Err(Refusal::NotActive)
            }
        );
        kani::cover!(state == State::Absent);
        kani::cover!(state == State::Accepted);
        kani::cover!(state == State::Active);
        kani::cover!(state == State::Finished);
    }

    #[kani::proof]
    fn rejects_disconnect_loss() {
        kani::assert(
            step(State::Active, Action::Disconnect) == Ok(State::Finished),
            "NEGATIVE_JOB_CONTROL",
        );
    }
}
