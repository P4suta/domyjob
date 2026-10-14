#[path = "../crates/domyjob-core/src/worker_handoff.rs"]
mod handoff;

use handoff::{HandoffFailure, HandoffPhase};

const PHASE_COUNT: usize = 4;
const READ_INDEX: usize = 0;
const DECODE_INDEX: usize = 1;
const VALIDATE_INDEX: usize = 2;
const LAUNCH_INDEX: usize = 3;
const NOT_CALLED: u8 = 0;
const CALLED_ONCE: u8 = 1;
const READ_ERROR: u8 = 35;
const DISCARDED_ERROR: u8 = 0;

fn outcome(
    failures: [bool; PHASE_COUNT],
    cause: u8,
) -> (Result<(), HandoffFailure<u8>>, [u8; PHASE_COUNT]) {
    let mut reads = NOT_CALLED;
    let mut decodes = NOT_CALLED;
    let mut validations = NOT_CALLED;
    let mut launches = NOT_CALLED;
    let result = handoff::start(
        || {
            reads += CALLED_ONCE;
            if failures[READ_INDEX] {
                Err(cause)
            } else {
                Ok(cause)
            }
        },
        |bytes| {
            decodes += CALLED_ONCE;
            assert_eq!(bytes, cause);
            if failures[DECODE_INDEX] {
                Err(cause)
            } else {
                Ok(bytes)
            }
        },
        |request| {
            validations += CALLED_ONCE;
            assert_eq!(*request, cause);
            if failures[VALIDATE_INDEX] {
                Err(cause)
            } else {
                Ok(())
            }
        },
        |request| {
            launches += CALLED_ONCE;
            assert_eq!(request, cause);
            if failures[LAUNCH_INDEX] {
                Err(cause)
            } else {
                Ok(())
            }
        },
    );
    (result, [reads, decodes, validations, launches])
}

#[kani::proof]
#[kani::unwind(8)]
fn handoff_stops_at_the_first_failure_and_retains_its_cause() {
    let failures: [bool; PHASE_COUNT] = kani::any();
    let cause: u8 = kani::any();
    let (result, calls) = outcome(failures, cause);
    assert_eq!(calls[READ_INDEX], CALLED_ONCE);
    assert_eq!(calls[DECODE_INDEX], u8::from(!failures[READ_INDEX]));
    assert_eq!(
        calls[VALIDATE_INDEX],
        u8::from(!failures[READ_INDEX] && !failures[DECODE_INDEX])
    );
    assert_eq!(
        calls[LAUNCH_INDEX],
        u8::from(!failures[READ_INDEX] && !failures[DECODE_INDEX] && !failures[VALIDATE_INDEX])
    );
    let expected = if failures[READ_INDEX] {
        Some(HandoffPhase::Read)
    } else if failures[DECODE_INDEX] {
        Some(HandoffPhase::Decode)
    } else if failures[VALIDATE_INDEX] {
        Some(HandoffPhase::Validate)
    } else if failures[LAUNCH_INDEX] {
        Some(HandoffPhase::Launch)
    } else {
        None
    };
    assert_eq!(result.as_ref().err().map(|error| error.phase), expected);
    if let Err(error) = &result {
        assert_eq!(error.cause, cause);
    }
    kani::cover!(result.is_ok());
    kani::cover!(expected == Some(HandoffPhase::Read));
    kani::cover!(expected == Some(HandoffPhase::Decode));
    kani::cover!(expected == Some(HandoffPhase::Validate));
    kani::cover!(expected == Some(HandoffPhase::Launch));
}

#[kani::proof]
#[kani::unwind(8)]
fn rejects_relabelled_request_read_failure() {
    let (result, _calls) = outcome([true; PHASE_COUNT], READ_ERROR);
    kani::assert(
        result.err().map(|error| error.phase) == Some(HandoffPhase::Launch),
        "NEGATIVE_HANDOFF_PHASE",
    );
}

#[kani::proof]
#[kani::unwind(8)]
fn rejects_discarded_handoff_error() {
    let (result, _calls) = outcome([true; PHASE_COUNT], READ_ERROR);
    kani::assert(
        result.err().map(|error| error.cause) == Some(DISCARDED_ERROR),
        "NEGATIVE_HANDOFF_CAUSE",
    );
}
