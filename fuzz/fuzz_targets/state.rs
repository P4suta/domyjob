#![no_main]

use domyjob_core::domain::RemoteText;
use domyjob_core::ingress;
use domyjob_core::state::{Event, JobState, PhaseKind};

libfuzzer_sys::fuzz_target!(|bytes: &[u8]| {
    let mut state = JobState::accepted();
    let reason = RemoteText::try_from(String::from("launch failed")).unwrap();
    for &byte in bytes.iter().take(256) {
        let event = match byte % 8 {
            0 => Event::Starting,
            1 => Event::Spawned {
                pid: u32::from(byte),
            },
            2 => Event::Exited {
                code: i32::from(byte) - 128,
            },
            3 => Event::LaunchFailed {
                reason: reason.clone(),
            },
            4 => Event::SupervisorGone,
            5 => Event::Killed,
            6 => Event::Queued,
            _ => Event::MemoryLimitExceeded,
        };
        let before = state.clone();
        let result = state.advance(&event);
        if result.is_err() {
            assert_eq!(state, before);
        }
        if before.kind() == PhaseKind::Finished {
            assert!(result.is_err());
        }
        let encoded = serde_json::to_vec(&state).unwrap();
        assert_eq!(ingress::stored_job(&encoded).unwrap(), state);
    }
});
