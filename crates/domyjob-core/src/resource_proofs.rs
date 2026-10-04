use crate::domain::RemoteText;
use crate::resource_budget::Budget;
use crate::state::{CommandCompletionError, Event, JobState, Outcome, PhaseKind};

#[kani::proof]
#[kani::unwind(16)]
fn execution_records_preserve_command_outcomes() {
    let kind: u8 = kani::any();
    let code: i32 = kani::any();
    kani::assume(kind < 8);
    let mut state = JobState::accepted();
    if kind == 1 {
        state.advance(&Event::Queued).unwrap();
    } else if kind >= 2 {
        state.advance(&Event::Starting).unwrap();
        state.advance(&Event::Spawned { pid: 1 }).unwrap();
        let ending = match kind {
            3 => Some(Event::Exited { code }),
            4 => Some(Event::LaunchFailed {
                reason: RemoteText::try_from("x".to_owned()).unwrap(),
            }),
            5 => Some(Event::SupervisorGone),
            6 => Some(Event::Killed),
            7 => Some(Event::MemoryLimitExceeded),
            _ => None,
        };
        if let Some(event) = ending {
            state.advance(&event).unwrap();
        }
    }
    let result = state.command_completion();
    match kind {
        0..=2 => assert_eq!(result, Err(CommandCompletionError::Unfinished)),
        3 => assert_eq!(result, Ok(Event::Exited { code })),
        4 => assert_eq!(
            result,
            Ok(Event::LaunchFailed {
                reason: RemoteText::try_from("x".to_owned()).unwrap(),
            })
        ),
        _ => assert_eq!(result, Err(CommandCompletionError::InvalidOutcome)),
    }
    kani::cover!(kind == 3 && code == 0);
    kani::cover!(kind == 3 && code < 0);
    kani::cover!(kind == 4);
    kani::cover!(kind == 7);
}

#[kani::proof]
#[kani::unwind(16)]
fn job_transitions_preserve_queue_and_terminal_invariants() {
    let phase: u8 = kani::any();
    let event: u8 = kani::any();
    let pid: u32 = kani::any();
    let code: i32 = kani::any();
    let terminal: u8 = kani::any();
    kani::assume(phase < 5 && event < 8 && terminal < 5);
    let mut state = JobState::accepted();
    if phase == 1 {
        state.advance(&Event::Queued).unwrap();
    } else if phase >= 2 {
        state.advance(&Event::Starting).unwrap();
        if phase >= 3 {
            state.advance(&Event::Spawned { pid: 1 }).unwrap();
        }
        if phase == 4 {
            let ending = match terminal {
                0 => Event::Exited { code },
                1 => Event::LaunchFailed {
                    reason: RemoteText::try_from("x".to_owned()).unwrap(),
                },
                2 => Event::SupervisorGone,
                3 => Event::Killed,
                _ => Event::MemoryLimitExceeded,
            };
            state.advance(&ending).unwrap();
        }
    }
    let input = match event {
        0 => Event::Queued,
        1 => Event::Starting,
        2 => Event::Spawned { pid },
        3 => Event::Exited { code },
        4 => Event::LaunchFailed {
            reason: RemoteText::try_from("x".to_owned()).unwrap(),
        },
        5 => Event::SupervisorGone,
        6 => Event::Killed,
        _ => Event::MemoryLimitExceeded,
    };
    let before = state.clone();
    let valid = match event {
        0 => phase == 0,
        1 => phase <= 1,
        2 => phase == 2 && pid != 0,
        3 | 7 => phase == 3,
        4..=6 => phase < 4,
        _ => false,
    };
    let result = state.advance(&input);
    assert_eq!(result.is_ok(), valid);
    if !valid {
        assert_eq!(state, before);
    } else {
        let expected = match event {
            0 => PhaseKind::Queued,
            1 => PhaseKind::Starting,
            2 => PhaseKind::Running,
            _ => PhaseKind::Finished,
        };
        assert_eq!(state.kind(), expected);
        if event == 2 {
            assert_eq!(state.pid(), Some(pid));
        }
        if event == 3 {
            if code == 0 {
                assert_eq!(state.outcome(), Some(&Outcome::Succeeded));
            } else {
                assert!(
                    matches!(state.outcome(), Some(Outcome::Failed { code: exit }) if exit.get() == code)
                );
            }
        }
    }
    assert!(state.pid().is_none_or(|id| id != 0));
    assert_eq!(
        state.outcome().is_some(),
        state.kind() == PhaseKind::Finished
    );
    if valid && event == 7 {
        assert_eq!(state.outcome(), Some(&Outcome::MemoryLimitExceeded));
    }
    if valid && event == 6 {
        assert_eq!(state.outcome(), Some(&Outcome::Killed));
    }
    kani::cover!(phase == 1 && event == 6 && valid);
    kani::cover!(phase == 3 && event == 7 && valid);
    kani::cover!(phase == 4 && !valid);
}

#[kani::proof]
fn resource_budget_is_bounded() {
    let budget = Budget {
        concurrent: kani::any(),
        high: kani::any(),
        max: kani::any(),
        swap: kani::any(),
    };
    let expected = (1..=2).contains(&budget.concurrent)
        && budget.high != 0
        && budget.high < budget.max
        && budget.max <= 11_811_160_064
        && budget.swap <= 2_147_483_648;
    assert_eq!(budget.valid(), expected);
    kani::cover!(budget.valid());
    kani::cover!(!budget.valid());
}
use alloc::borrow::ToOwned;
