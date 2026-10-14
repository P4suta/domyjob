#[path = "../crates/domyjob/tests/e2e/input_controller.rs"]
mod input_controller;

use input_controller::{Controller, Decision, Identity, Problem, Reader, Ready, Refusal};

const READY_VARIANTS: u8 = 3;
const COMPLETED_EXIT: i32 = 7;
const BEGIN_FAULT_INDEX: usize = 2;
const PROGRESS_FAULT_INDEX: usize = 3;
const CLOSE_FAULT_INDEX: usize = 4;
const STOP_FAULT_INDEX: usize = 5;
const REAP_FAULT_INDEX: usize = 6;
const REAP_COUNT_INDEX: usize = 2;

fn identity() -> Identity {
    Identity {
        case: kani::any(),
        process: kani::any(),
    }
}

fn readiness(actual: Identity) -> Ready {
    match kani::any::<u8>() % READY_VARIANTS {
        0 => Ready::Missing,
        1 => Ready::Invalid,
        _ => Ready::Published(actual),
    }
}

#[kani::proof]
#[kani::unwind(24)]
fn identity_tokens_preserve_every_case_and_process_bit() {
    let expected = identity();
    let token: [u8; input_controller::IDENTITY_BYTES] = kani::any();
    assert_eq!(Identity::from_bytes(expected.bytes()), expected);
    assert_eq!(Identity::from_bytes(token).bytes(), token);
    kani::cover!(expected.process == 0);
    kani::cover!(expected.process == u32::MAX);
    kani::cover!(expected.case == [u8::MAX; input_controller::CASE_BYTES]);
}

#[kani::proof]
#[kani::unwind(24)]
fn preparation_admits_only_live_matching_ready() {
    let expected = identity();
    let ready = readiness(identity());
    let elapsed: u64 = kani::any();
    let case_millis: u64 = kani::any();
    let exited: bool = kani::any();
    let mut controller = Controller::new(expected, case_millis);
    let decision = controller.observe(elapsed, exited, ready);
    assert_eq!(
        decision == Decision::Begin,
        elapsed < case_millis && !exited && ready == Ready::Published(expected)
    );
    assert_eq!(
        decision == Decision::Wait,
        elapsed < case_millis && !exited && ready == Ready::Missing
    );
    if elapsed >= case_millis {
        assert_eq!(decision, Decision::Refuse(Refusal::CaseDeadline));
    } else if exited {
        assert_eq!(decision, Decision::Refuse(Refusal::ExitedBeforeReady));
    } else if ready == Ready::Invalid {
        assert_eq!(decision, Decision::Refuse(Refusal::InvalidReady));
    } else if matches!(ready, Ready::Published(actual) if actual != expected) {
        assert_eq!(decision, Decision::Refuse(Refusal::ForeignReady));
    }
    kani::cover!(decision == Decision::Begin);
    kani::cover!(decision == Decision::Wait);
    kani::cover!(decision == Decision::Refuse(Refusal::ForeignReady));
    kani::cover!(decision == Decision::Refuse(Refusal::ExitedBeforeReady));
}

#[kani::proof]
#[kani::unwind(24)]
fn operation_progress_never_resets_either_budget() {
    let expected = identity();
    let case_millis: u64 = kani::any();
    let started: u64 = kani::any();
    let middle: u64 = kani::any();
    let finished: u64 = kani::any();
    let mut controller = Controller::new(expected, case_millis);
    if controller.observe(started, false, Ready::Published(expected)) == Decision::Begin {
        let progress = controller.observe(middle, false, Ready::Published(expected));
        if progress == Decision::Progress {
            let result = controller.observe(finished, true, Ready::Published(expected));
            let accepted = finished >= middle
                && finished < case_millis
                && finished - started < input_controller::OPERATION_MILLIS;
            assert_eq!(result == Decision::Complete, accepted);
            if finished < middle {
                assert_eq!(result, Decision::Refuse(Refusal::ClockRegressed));
            } else if finished >= case_millis {
                assert_eq!(result, Decision::Refuse(Refusal::CaseDeadline));
            } else if finished - started >= input_controller::OPERATION_MILLIS {
                assert_eq!(result, Decision::Refuse(Refusal::OperationDeadline));
            }
            kani::cover!(result == Decision::Complete);
            kani::cover!(result == Decision::Refuse(Refusal::OperationDeadline));
            kani::cover!(result == Decision::Refuse(Refusal::ClockRegressed));
            kani::cover!(result == Decision::Refuse(Refusal::CaseDeadline));
        }
    }
}

#[kani::proof]
#[kani::unwind(24)]
fn begun_operations_refuse_missing_or_foreign_identity() {
    let expected = identity();
    let ready = readiness(identity());
    let started: u64 = kani::any();
    let elapsed: u64 = kani::any();
    let case_millis: u64 = kani::any();
    let mut controller = Controller::new(expected, case_millis);
    if controller.observe(started, false, Ready::Published(expected)) == Decision::Begin {
        let result = controller.observe(elapsed, true, ready);
        let within = elapsed >= started
            && elapsed < case_millis
            && elapsed - started < input_controller::OPERATION_MILLIS;
        assert_eq!(
            result == Decision::Complete,
            within && ready == Ready::Published(expected)
        );
        if within {
            match ready {
                Ready::Missing => assert_eq!(result, Decision::Refuse(Refusal::ReadyDisappeared)),
                Ready::Invalid => assert_eq!(result, Decision::Refuse(Refusal::InvalidReady)),
                Ready::Published(actual) if actual != expected => {
                    assert_eq!(result, Decision::Refuse(Refusal::ForeignReady))
                }
                Ready::Published(_) => assert_eq!(result, Decision::Complete),
            }
        }
        assert_ne!(result, Decision::Begin);
        kani::cover!(result == Decision::Refuse(Refusal::ReadyDisappeared));
        kani::cover!(result == Decision::Refuse(Refusal::InvalidReady));
        kani::cover!(result == Decision::Refuse(Refusal::ForeignReady));
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fault {
    Observation,
    Ready,
    Begin,
    Progress,
    Close,
    Stop,
    Reap,
}

struct CountedReader {
    expected: Identity,
    clock: [u64; 3],
    index: usize,
    fail: [bool; 7],
    counts: [usize; 3],
}

impl Reader for CountedReader {
    type Exit = i32;
    type Error = Fault;
    fn try_wait(&mut self) -> Result<Option<i32>, Fault> {
        if self.fail[0] {
            Err(Fault::Observation)
        } else {
            Ok((self.index == self.clock.len() - 1).then_some(COMPLETED_EXIT))
        }
    }
    fn ready(&mut self) -> Result<Ready, Fault> {
        if self.fail[1] {
            Err(Fault::Ready)
        } else {
            Ok(Ready::Published(self.expected))
        }
    }
    fn elapsed_millis(&mut self) -> u64 {
        self.clock[self.index]
    }
    fn begin(&mut self) -> Result<(), Fault> {
        if self.fail[BEGIN_FAULT_INDEX] {
            Err(Fault::Begin)
        } else {
            Ok(())
        }
    }
    fn progress(&mut self) -> Result<(), Fault> {
        if self.fail[PROGRESS_FAULT_INDEX] {
            Err(Fault::Progress)
        } else {
            Ok(())
        }
    }
    fn pause(&mut self) {
        self.index += 1;
    }
    fn close_input(&mut self) -> Result<(), Fault> {
        self.counts[0] += 1;
        if self.fail[CLOSE_FAULT_INDEX] {
            Err(Fault::Close)
        } else {
            Ok(())
        }
    }
    fn stop_tree(&mut self) -> Result<(), Fault> {
        self.counts[1] += 1;
        if self.fail[STOP_FAULT_INDEX] {
            Err(Fault::Stop)
        } else {
            Ok(())
        }
    }
    fn reap(&mut self) -> Result<(), Fault> {
        self.counts[REAP_COUNT_INDEX] += 1;
        if self.fail[REAP_FAULT_INDEX] {
            Err(Fault::Reap)
        } else {
            Ok(())
        }
    }
}

#[kani::proof]
#[kani::unwind(24)]
fn every_outcome_retains_all_owned_cleanup_results() {
    let fail: [bool; 7] = kani::any();
    let mut reader = CountedReader {
        expected: identity(),
        clock: kani::any(),
        index: 0,
        fail,
        counts: [0; 3],
    };
    let controller = Controller::new(reader.expected, kani::any());
    let outcome = input_controller::wait(&mut reader, controller);
    assert_eq!(reader.counts[0], 1);
    assert_eq!(reader.counts[1], 1);
    assert_eq!(reader.counts[REAP_COUNT_INDEX], 1);
    assert_eq!(
        outcome.cleanup[0],
        if fail[CLOSE_FAULT_INDEX] {
            Err(Fault::Close)
        } else {
            Ok(())
        }
    );
    assert_eq!(
        outcome.cleanup[1],
        if fail[STOP_FAULT_INDEX] {
            Err(Fault::Stop)
        } else {
            Ok(())
        }
    );
    assert_eq!(
        outcome.cleanup[REAP_COUNT_INDEX],
        if fail[REAP_FAULT_INDEX] {
            Err(Fault::Reap)
        } else {
            Ok(())
        }
    );
    if fail[0] {
        assert_eq!(outcome.primary, Err(Problem::Port(Fault::Observation)));
    } else if fail[1] {
        assert_eq!(outcome.primary, Err(Problem::Port(Fault::Ready)));
    }
    kani::cover!(outcome.primary.is_ok());
    kani::cover!(
        outcome.primary == Err(Problem::Port(Fault::Begin))
            && fail[STOP_FAULT_INDEX]
            && fail[REAP_FAULT_INDEX]
    );
    kani::cover!(matches!(outcome.primary, Err(Problem::Refused(_))) && fail[CLOSE_FAULT_INDEX]);
}

#[kani::proof]
#[kani::unwind(24)]
fn rejects_completion_at_the_input_deadline() {
    let expected = Identity {
        case: [0; input_controller::CASE_BYTES],
        process: 1,
    };
    let mut controller = Controller::new(expected, u64::MAX);
    assert_eq!(
        controller.observe(0, false, Ready::Published(expected)),
        Decision::Begin
    );
    kani::assert(
        controller.observe(
            input_controller::OPERATION_MILLIS,
            true,
            Ready::Published(expected),
        ) == Decision::Complete,
        "NEGATIVE_INPUT_DEADLINE_CONTROL",
    );
}

#[kani::proof]
#[kani::unwind(24)]
fn rejects_foreign_ready_admission() {
    let expected = Identity {
        case: [0; input_controller::CASE_BYTES],
        process: 1,
    };
    let foreign = Identity {
        process: 2,
        ..expected
    };
    let mut controller = Controller::new(expected, u64::MAX);
    kani::assert(
        controller.observe(0, false, Ready::Published(foreign)) == Decision::Begin,
        "NEGATIVE_INPUT_IDENTITY_CONTROL",
    );
}

#[kani::proof]
#[kani::unwind(24)]
fn rejects_discarded_owned_cleanup_errors() {
    let expected = Identity {
        case: [0; input_controller::CASE_BYTES],
        process: 1,
    };
    let mut reader = CountedReader {
        expected,
        clock: [0, 1, 2],
        index: 0,
        fail: [false, false, true, false, false, true, true],
        counts: [0; 3],
    };
    let outcome = input_controller::wait(&mut reader, Controller::new(expected, u64::MAX));
    assert_eq!(outcome.primary, Err(Problem::Port(Fault::Begin)));
    kani::assert(
        outcome.cleanup.iter().all(Result::is_ok),
        "NEGATIVE_INPUT_CLEANUP_CONTROL",
    );
}
