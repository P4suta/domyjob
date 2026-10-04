use core::num::NonZeroI32;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::domain::RemoteText;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PhaseKind {
    Accepted,
    Queued,
    Starting,
    Running,
    Finished,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case", tag = "outcome")]
pub enum Outcome {
    Succeeded,
    Failed { code: NonZeroI32 },
    LaunchFailed { reason: RemoteText },
    Lost,
    Killed,
    MemoryLimitExceeded,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Phase {
    Accepted,
    Queued,
    Starting,
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
    Queued,
    Starting,
    Running { pid: u32 },
    Finished { outcome: Outcome },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("a running job must have a nonzero process identifier")]
pub struct InvalidStoredState;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum CommandCompletionError {
    #[error("the scoped command has not recorded completion")]
    Unfinished,
    #[error("the execution record has a supervisor-only outcome")]
    InvalidOutcome,
}

impl TryFrom<RawJobState> for JobState {
    type Error = InvalidStoredState;

    fn try_from(value: RawJobState) -> Result<Self, Self::Error> {
        let phase = match value.phase {
            RawPhase::Accepted => Phase::Accepted,
            RawPhase::Queued => Phase::Queued,
            RawPhase::Starting => Phase::Starting,
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
            Phase::Queued => RawPhase::Queued,
            Phase::Starting => RawPhase::Starting,
            Phase::Running { pid } => RawPhase::Running { pid },
            Phase::Finished { outcome } => RawPhase::Finished { outcome },
        };
        Self { phase }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Queued,
    Starting,
    Spawned { pid: u32 },
    Exited { code: i32 },
    LaunchFailed { reason: RemoteText },
    SupervisorGone,
    Killed,
    MemoryLimitExceeded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("event {event:?} is invalid from phase {phase:?}")]
pub struct InvalidTransition {
    phase: PhaseKind,
    event: EventKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EventKind {
    Queued,
    Starting,
    Spawned,
    Exited,
    LaunchFailed,
    SupervisorGone,
    Killed,
    MemoryLimitExceeded,
}

impl Event {
    const fn kind(&self) -> EventKind {
        match self {
            Self::Queued => EventKind::Queued,
            Self::Starting => EventKind::Starting,
            Self::Spawned { .. } => EventKind::Spawned,
            Self::Exited { .. } => EventKind::Exited,
            Self::LaunchFailed { .. } => EventKind::LaunchFailed,
            Self::SupervisorGone => EventKind::SupervisorGone,
            Self::Killed => EventKind::Killed,
            Self::MemoryLimitExceeded => EventKind::MemoryLimitExceeded,
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
            Phase::Queued => PhaseKind::Queued,
            Phase::Starting => PhaseKind::Starting,
            Phase::Running { .. } => PhaseKind::Running,
            Phase::Finished { .. } => PhaseKind::Finished,
        }
    }

    #[must_use]
    pub const fn outcome(&self) -> Option<&Outcome> {
        match &self.phase {
            Phase::Finished { outcome } => Some(outcome),
            Phase::Accepted | Phase::Queued | Phase::Starting | Phase::Running { .. } => None,
        }
    }

    #[must_use]
    pub const fn pid(&self) -> Option<u32> {
        match self.phase {
            Phase::Running { pid } => Some(pid),
            Phase::Accepted | Phase::Queued | Phase::Starting | Phase::Finished { .. } => None,
        }
    }

    pub fn command_completion(&self) -> Result<Event, CommandCompletionError> {
        match self.outcome() {
            Some(Outcome::Succeeded) => Ok(Event::Exited { code: 0 }),
            Some(Outcome::Failed { code }) => Ok(Event::Exited { code: code.get() }),
            Some(Outcome::LaunchFailed { reason }) => Ok(Event::LaunchFailed {
                reason: reason.clone(),
            }),
            Some(Outcome::Lost | Outcome::Killed | Outcome::MemoryLimitExceeded) => {
                Err(CommandCompletionError::InvalidOutcome)
            }
            None => Err(CommandCompletionError::Unfinished),
        }
    }

    pub fn advance(&mut self, event: &Event) -> Result<(), InvalidTransition> {
        let next = match (&self.phase, event) {
            (Phase::Accepted, Event::Queued) => Phase::Queued,
            (Phase::Accepted | Phase::Queued, Event::Starting) => Phase::Starting,
            (Phase::Starting, Event::Spawned { pid }) if *pid != 0 => Phase::Running { pid: *pid },
            (
                Phase::Accepted | Phase::Queued | Phase::Starting | Phase::Running { .. },
                Event::LaunchFailed { reason },
            ) => Phase::Finished {
                outcome: Outcome::LaunchFailed {
                    reason: reason.clone(),
                },
            },
            (
                Phase::Accepted | Phase::Queued | Phase::Starting | Phase::Running { .. },
                Event::SupervisorGone,
            ) => Phase::Finished {
                outcome: Outcome::Lost,
            },
            (Phase::Running { .. }, Event::Exited { code }) => Phase::Finished {
                outcome: match NonZeroI32::new(*code) {
                    Some(code) => Outcome::Failed { code },
                    None => Outcome::Succeeded,
                },
            },
            (
                Phase::Accepted | Phase::Queued | Phase::Starting | Phase::Running { .. },
                Event::Killed,
            ) => Phase::Finished {
                outcome: Outcome::Killed,
            },
            (Phase::Running { .. }, Event::MemoryLimitExceeded) => Phase::Finished {
                outcome: Outcome::MemoryLimitExceeded,
            },
            (
                Phase::Accepted | Phase::Queued,
                Event::Spawned { .. } | Event::Exited { .. } | Event::MemoryLimitExceeded,
            )
            | (Phase::Queued, Event::Queued)
            | (
                Phase::Starting,
                Event::Queued
                | Event::Starting
                | Event::Spawned { .. }
                | Event::Exited { .. }
                | Event::MemoryLimitExceeded,
            )
            | (Phase::Running { .. }, Event::Queued | Event::Starting | Event::Spawned { .. })
            | (
                Phase::Finished { .. },
                Event::Queued
                | Event::Starting
                | Event::Spawned { .. }
                | Event::Exited { .. }
                | Event::LaunchFailed { .. }
                | Event::SupervisorGone
                | Event::Killed
                | Event::MemoryLimitExceeded,
            ) => {
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
    fn queued_jobs_and_memory_exhaustion_are_distinct_stored_states() {
        let queued: JobState = crate::ingress::foreign_json(r#"{"phase":{"phase":"queued"}}"#)
            .expect("queued jobs survive serialization");
        assert!(queued.outcome().is_none());
        let exhausted: JobState = crate::ingress::foreign_json(
            r#"{"phase":{"phase":"finished","outcome":{"outcome":"memory_limit_exceeded"}}}"#,
        )
        .expect("memory exhaustion is not an ordinary process exit");
        assert!(exhausted.outcome().is_some());
    }

    #[test]
    fn a_new_job_starts_accepted() {
        assert_eq!(JobState::default(), JobState::accepted());
        assert_eq!(JobState::default().kind(), PhaseKind::Accepted);
    }

    #[test]
    fn transition_matrix_has_one_terminal_outcome() {
        let mut state = JobState::accepted();
        assert_eq!(state.kind(), PhaseKind::Accepted);
        assert_eq!(state.pid(), None);
        state.advance(&Event::Exited { code: 0 }).unwrap_err();
        state.advance(&Event::Spawned { pid: 0 }).unwrap_err();
        state.advance(&Event::Starting).unwrap();
        assert_eq!(state.kind(), PhaseKind::Starting);
        assert_eq!(state.pid(), None);
        state.advance(&Event::Starting).unwrap_err();
        state.advance(&Event::Spawned { pid: 0 }).unwrap_err();
        state.advance(&Event::Spawned { pid: 42 }).unwrap();
        assert_eq!(state.pid(), Some(42));
        state.advance(&Event::Spawned { pid: 43 }).unwrap_err();
        state.advance(&Event::Exited { code: 0 }).unwrap();
        assert_eq!(state.outcome(), Some(&Outcome::Succeeded));
        assert_eq!(state.pid(), None);
        state.advance(&Event::Killed).unwrap_err();
    }

    #[test]
    fn a_dead_supervisor_has_an_explicit_result() {
        let mut state = JobState::accepted();
        state.advance(&Event::SupervisorGone).unwrap();
        assert_eq!(state.outcome(), Some(&Outcome::Lost));
    }

    #[test]
    fn cancellation_is_terminal_before_or_after_process_start() {
        for starting in [false, true] {
            let mut state = JobState::accepted();
            if starting {
                state.advance(&Event::Starting).unwrap();
            }
            state.advance(&Event::Killed).unwrap();
            assert_eq!(state.outcome(), Some(&Outcome::Killed));
            state.advance(&Event::Starting).unwrap_err();
        }
    }

    #[test]
    fn deserialization_cannot_create_a_running_job_without_a_process() {
        let encoded = br#"{"phase":{"phase":"running","pid":0}}"#;
        crate::ingress::stored_job(encoded).unwrap_err();
    }

    #[test]
    fn a_zero_exit_code_cannot_be_stored_as_a_failure() {
        let encoded = br#"{"phase":{"phase":"finished","outcome":{"outcome":"failed","code":0}}}"#;
        crate::ingress::stored_job(encoded).unwrap_err();
    }
}
