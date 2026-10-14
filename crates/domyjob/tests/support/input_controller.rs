use crate::input_controller::{self, Controller, Identity, Problem, Reader, Ready, Refusal};

const CASE_MILLIS: u64 = 180_000;
const PREPARED_MILLIS: u64 = 6_000;
const COMPLETE_MILLIS: u64 = 7_000;
const EXIT_CODE: i32 = 7;
const IDENTITY: Identity = Identity {
    case: [1; input_controller::CASE_BYTES],
    process: 17,
};

enum PortFailure {
    Begin,
    Progress,
}

struct PreparedReader {
    elapsed: u64,
    completed: u64,
    ready: Ready,
    released: usize,
    progress: usize,
    closed: usize,
    stopped: usize,
    reaped: usize,
    fault: Option<PortFailure>,
    cleanup_error: bool,
    ready_after_begin: Option<Ready>,
    regress_after_begin: bool,
}

impl PreparedReader {
    const fn new(elapsed: u64, completed: u64) -> Self {
        Self {
            elapsed,
            completed,
            ready: Ready::Published(IDENTITY),
            released: 0,
            progress: 0,
            closed: 0,
            stopped: 0,
            reaped: 0,
            fault: None,
            cleanup_error: false,
            ready_after_begin: None,
            regress_after_begin: false,
        }
    }
}

impl Reader for PreparedReader {
    type Exit = i32;
    type Error = &'static str;

    fn try_wait(&mut self) -> Result<Option<Self::Exit>, Self::Error> {
        Ok((self.elapsed >= self.completed).then_some(EXIT_CODE))
    }

    fn ready(&mut self) -> Result<Ready, Self::Error> {
        Ok(self.ready)
    }
    fn elapsed_millis(&mut self) -> u64 {
        self.elapsed
    }
    fn begin(&mut self) -> Result<(), Self::Error> {
        self.released = self.released.saturating_add(1);
        if matches!(self.fault, Some(PortFailure::Begin)) {
            Err("begin publication failed")
        } else {
            Ok(())
        }
    }
    fn progress(&mut self) -> Result<(), Self::Error> {
        self.progress = self.progress.saturating_add(1);
        if matches!(self.fault, Some(PortFailure::Progress)) {
            Err("input release failed")
        } else {
            Ok(())
        }
    }
    fn pause(&mut self) {
        if self.regress_after_begin && self.released != 0 {
            self.elapsed = self.elapsed.saturating_sub(1);
        } else {
            self.elapsed = self.elapsed.saturating_add(1_000);
        }
        if self.released != 0
            && let Some(ready) = self.ready_after_begin
        {
            self.ready = ready;
        }
    }
    fn close_input(&mut self) -> Result<(), Self::Error> {
        self.closed = self.closed.saturating_add(1);
        Ok(())
    }
    fn stop_tree(&mut self) -> Result<(), Self::Error> {
        self.stopped = self.stopped.saturating_add(1);
        if self.cleanup_error {
            Err("owned stop failed")
        } else {
            Ok(())
        }
    }
    fn reap(&mut self) -> Result<(), Self::Error> {
        self.reaped = self.reaped.saturating_add(1);
        if self.cleanup_error {
            Err("owned reap failed")
        } else {
            Ok(())
        }
    }
}

fn observe(reader: &mut PreparedReader) -> input_controller::Outcome<i32, &'static str> {
    input_controller::wait(reader, Controller::new(IDENTITY, CASE_MILLIS))
}

fn assert_cleanup(reader: &PreparedReader) {
    assert_eq!((reader.closed, reader.stopped, reader.reaped), (1, 1, 1));
}

#[test]
fn completed_preparation_does_not_spend_the_input_operation_budget() {
    let mut reader = PreparedReader::new(PREPARED_MILLIS, COMPLETE_MILLIS);
    let outcome = observe(&mut reader);
    assert_eq!(outcome.primary, Ok(EXIT_CODE));
    assert_eq!(outcome.cleanup, [Ok(()); 3]);
    assert_eq!(reader.released, 1);
    assert_cleanup(&reader);
}

#[test]
fn completion_at_the_operation_boundary_refuses_without_progress_resets() {
    let mut reader = PreparedReader::new(0, input_controller::OPERATION_MILLIS);
    let outcome = observe(&mut reader);
    assert_eq!(
        outcome.primary,
        Err(Problem::Refused(Refusal::OperationDeadline))
    );
    assert_eq!(reader.released, 1);
    assert_eq!(reader.progress, 4);
    assert_cleanup(&reader);
}

#[test]
fn missing_ready_refuses_at_the_original_command_boundary() {
    let mut reader = PreparedReader::new(CASE_MILLIS, u64::MAX);
    reader.ready = Ready::Missing;
    assert_eq!(
        observe(&mut reader).primary,
        Err(Problem::Refused(Refusal::CaseDeadline))
    );
    assert_eq!(reader.released, 0);
    assert_cleanup(&reader);
}

#[test]
fn ready_never_admits_an_already_exited_child() {
    let mut reader = PreparedReader::new(COMPLETE_MILLIS, COMPLETE_MILLIS);
    assert_eq!(
        observe(&mut reader).primary,
        Err(Problem::Refused(Refusal::ExitedBeforeReady))
    );
    assert_eq!(reader.released, 0);
    assert_cleanup(&reader);
}

#[test]
fn malformed_wrong_process_and_stale_case_never_release_begin() {
    let foreign_process = Identity {
        process: IDENTITY.process + 1,
        ..IDENTITY
    };
    let stale_case = Identity {
        case: [2; input_controller::CASE_BYTES],
        ..IDENTITY
    };
    for ready in [
        Ready::Invalid,
        Ready::Published(foreign_process),
        Ready::Published(stale_case),
    ] {
        let mut reader = PreparedReader::new(0, COMPLETE_MILLIS);
        reader.ready = ready;
        assert!(matches!(
            observe(&mut reader).primary,
            Err(Problem::Refused(
                Refusal::InvalidReady | Refusal::ForeignReady
            ))
        ));
        assert_eq!(reader.released, 0);
        assert_cleanup(&reader);
    }
}

#[test]
fn begin_failure_keeps_primary_and_all_cleanup_failures() {
    let mut reader = PreparedReader::new(0, COMPLETE_MILLIS);
    reader.fault = Some(PortFailure::Begin);
    reader.cleanup_error = true;
    let outcome = observe(&mut reader);
    assert_eq!(
        outcome.primary,
        Err(Problem::Port("begin publication failed"))
    );
    assert_eq!(
        outcome.cleanup,
        [Ok(()), Err("owned stop failed"), Err("owned reap failed")]
    );
    assert_eq!(reader.released, 1);
    assert_eq!(reader.progress, 0);
    assert_cleanup(&reader);
}

#[test]
fn native_ready_identity_bytes_preserve_every_case_and_process_field() {
    for process in [0, IDENTITY.process, u32::MAX] {
        let identity = Identity {
            process,
            ..IDENTITY
        };
        assert_eq!(Identity::from_bytes(identity.bytes()), identity);
    }
    let token = [u8::MAX; input_controller::IDENTITY_BYTES];
    assert_eq!(Identity::from_bytes(token).bytes(), token);
}

#[test]
fn vanished_ready_and_regressed_operation_clock_do_not_read_input() {
    let mut vanished = PreparedReader::new(0, COMPLETE_MILLIS);
    vanished.ready_after_begin = Some(Ready::Missing);
    assert_eq!(
        observe(&mut vanished).primary,
        Err(Problem::Refused(Refusal::ReadyDisappeared))
    );
    assert_eq!(vanished.progress, 0);
    assert_cleanup(&vanished);
    let mut regressed = PreparedReader::new(PREPARED_MILLIS, COMPLETE_MILLIS);
    regressed.regress_after_begin = true;
    assert_eq!(
        observe(&mut regressed).primary,
        Err(Problem::Refused(Refusal::ClockRegressed))
    );
    assert_eq!(regressed.progress, 0);
    assert_cleanup(&regressed);
}

#[test]
fn actual_progress_failure_stops_and_reaps_the_same_owned_reader() {
    let mut reader = PreparedReader::new(0, COMPLETE_MILLIS);
    reader.fault = Some(PortFailure::Progress);
    let outcome = observe(&mut reader);
    assert_eq!(outcome.primary, Err(Problem::Port("input release failed")));
    assert_eq!(reader.progress, 1);
    assert_cleanup(&reader);
}

#[test]
fn preparation_near_the_case_limit_cannot_extend_total_lifetime() {
    let mut reader = PreparedReader::new(CASE_MILLIS - 1_000, u64::MAX);
    let outcome = observe(&mut reader);
    assert_eq!(
        outcome.primary,
        Err(Problem::Refused(Refusal::CaseDeadline))
    );
    assert_eq!(reader.released, 1);
    assert_eq!(reader.progress, 0);
    assert_cleanup(&reader);
}
