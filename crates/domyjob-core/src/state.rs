use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::domain::RemoteText;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PhaseKind {
    Accepted,
    Running,
    Finished,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case", tag = "outcome")]
pub enum Outcome {
    Succeeded,
    Failed { code: i32 },
    LaunchFailed { reason: RemoteText },
    Lost,
    Killed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Phase {
    Accepted,
    Running { pid: u32 },
    Finished { outcome: Outcome },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawJobState", into = "RawJobState")]
pub struct JobState {
    phase: Phase,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawJobState {
    phase: RawPhase,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case", tag = "phase")]
enum RawPhase {
    Accepted,
    Running { pid: u32 },
    Finished { outcome: Outcome },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("a running job must have a nonzero process identifier")]
pub struct InvalidStoredState;

impl TryFrom<RawJobState> for JobState {
    type Error = InvalidStoredState;

    fn try_from(value: RawJobState) -> Result<Self, Self::Error> {
        let phase = match value.phase {
            RawPhase::Accepted => Phase::Accepted,
            RawPhase::Running { pid: 0 } => return Err(InvalidStoredState),
            RawPhase::Running { pid } => Phase::Running { pid },
            RawPhase::Finished { outcome } => Phase::Finished { outcome },
        };
        Ok(Self { phase })
    }
}

impl From<JobState> for RawJobState {
    fn from(value: JobState) -> Self {
        let phase = match value.phase {
            Phase::Accepted => RawPhase::Accepted,
            Phase::Running { pid } => RawPhase::Running { pid },
            Phase::Finished { outcome } => RawPhase::Finished { outcome },
        };
        Self { phase }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Spawned { pid: u32 },
    Exited { code: i32 },
    LaunchFailed { reason: RemoteText },
    SupervisorGone,
    Killed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("event {event:?} is invalid from phase {phase:?}")]
pub struct InvalidTransition {
    phase: PhaseKind,
    event: EventKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EventKind {
    Spawned,
    Exited,
    LaunchFailed,
    SupervisorGone,
    Killed,
}

impl Event {
    const fn kind(&self) -> EventKind {
        match self {
            Self::Spawned { .. } => EventKind::Spawned,
            Self::Exited { .. } => EventKind::Exited,
            Self::LaunchFailed { .. } => EventKind::LaunchFailed,
            Self::SupervisorGone => EventKind::SupervisorGone,
            Self::Killed => EventKind::Killed,
        }
    }
}

impl Default for JobState {
    fn default() -> Self {
        Self::accepted()
    }
}

impl JobState {
    #[must_use]
    pub const fn accepted() -> Self {
        Self {
            phase: Phase::Accepted,
        }
    }

    #[must_use]
    pub const fn kind(&self) -> PhaseKind {
        match self.phase {
            Phase::Accepted => PhaseKind::Accepted,
            Phase::Running { .. } => PhaseKind::Running,
            Phase::Finished { .. } => PhaseKind::Finished,
        }
    }

    #[must_use]
    pub const fn outcome(&self) -> Option<&Outcome> {
        match &self.phase {
            Phase::Finished { outcome } => Some(outcome),
            Phase::Accepted | Phase::Running { .. } => None,
        }
    }

    #[must_use]
    pub const fn pid(&self) -> Option<u32> {
        match self.phase {
            Phase::Running { pid } => Some(pid),
            Phase::Accepted | Phase::Finished { .. } => None,
        }
    }

    pub fn advance(&mut self, event: &Event) -> Result<(), InvalidTransition> {
        let next = match (&self.phase, event) {
            (Phase::Accepted, Event::Spawned { pid }) if *pid != 0 => Phase::Running { pid: *pid },
            (Phase::Accepted, Event::LaunchFailed { reason }) => Phase::Finished {
                outcome: Outcome::LaunchFailed {
                    reason: reason.clone(),
                },
            },
            (Phase::Accepted | Phase::Running { .. }, Event::SupervisorGone) => Phase::Finished {
                outcome: Outcome::Lost,
            },
            (Phase::Running { .. }, Event::Exited { code: 0 }) => Phase::Finished {
                outcome: Outcome::Succeeded,
            },
            (Phase::Running { .. }, Event::Exited { code }) => Phase::Finished {
                outcome: Outcome::Failed { code: *code },
            },
            (Phase::Running { .. }, Event::Killed) => Phase::Finished {
                outcome: Outcome::Killed,
            },
            (Phase::Accepted, Event::Spawned { .. } | Event::Exited { .. } | Event::Killed)
            | (Phase::Running { .. }, Event::Spawned { .. } | Event::LaunchFailed { .. })
            | (Phase::Finished { .. }, _) => {
                return Err(InvalidTransition {
                    phase: self.kind(),
                    event: event.kind(),
                });
            }
        };
        self.phase = next;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{Event, JobState, Outcome, PhaseKind};

    #[test]
    fn transition_matrix_has_one_terminal_outcome() {
        let mut state = JobState::accepted();
        assert_eq!(state.kind(), PhaseKind::Accepted);
        state.advance(&Event::Exited { code: 0 }).unwrap_err();
        state.advance(&Event::Spawned { pid: 0 }).unwrap_err();
        state.advance(&Event::Spawned { pid: 42 }).unwrap();
        assert_eq!(state.pid(), Some(42));
        state.advance(&Event::Spawned { pid: 43 }).unwrap_err();
        state.advance(&Event::Exited { code: 0 }).unwrap();
        assert_eq!(state.outcome(), Some(&Outcome::Succeeded));
        state.advance(&Event::Killed).unwrap_err();
    }

    #[test]
    fn a_dead_supervisor_has_an_explicit_result() {
        let mut state = JobState::accepted();
        state.advance(&Event::SupervisorGone).unwrap();
        assert_eq!(state.outcome(), Some(&Outcome::Lost));
    }

    #[test]
    fn deserialization_cannot_create_a_running_job_without_a_process() {
        let encoded = br#"{"phase":{"phase":"running","pid":0}}"#;
        crate::ingress::stored_job(encoded).unwrap_err();
    }
}
