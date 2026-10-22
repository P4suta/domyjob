pub(crate) const CASE_BYTES: usize = 16;
pub(crate) const IDENTITY_BYTES: usize = CASE_BYTES + size_of::<u32>();

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Identity {
    pub(crate) case: [u8; CASE_BYTES],
    pub(crate) process: u32,
}

impl Identity {
    pub(crate) const fn bytes(self) -> [u8; IDENTITY_BYTES] {
        let mut token = [0; IDENTITY_BYTES];
        let (case, process) = token.split_at_mut(CASE_BYTES);
        case.copy_from_slice(&self.case);
        process.copy_from_slice(&self.process.to_le_bytes());
        token
    }

    pub(crate) const fn from_bytes(token: [u8; IDENTITY_BYTES]) -> Self {
        let mut case = [0; CASE_BYTES];
        let mut process = [0; size_of::<u32>()];
        let (case_bytes, process_bytes) = token.split_at(CASE_BYTES);
        case.copy_from_slice(case_bytes);
        process.copy_from_slice(process_bytes);
        let process = u32::from_le_bytes(process);
        Self { case, process }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Ready {
    Missing,
    Invalid,
    Published(Identity),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Refusal {
    ExitedBeforeReady,
    InvalidReady,
    ForeignReady,
    ReadyDisappeared,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Decision {
    Wait,
    Begin,
    Progress,
    Complete,
    Refuse(Refusal),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Preparing,
    Operating,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Controller {
    expected: Identity,
    phase: Phase,
}

impl Controller {
    pub(crate) const fn new(expected: Identity) -> Self {
        Self {
            expected,
            phase: Phase::Preparing,
        }
    }

    pub(crate) fn observe(&mut self, exited: bool, ready: Ready) -> Decision {
        match self.phase {
            Phase::Preparing => self.prepare(exited, ready),
            Phase::Operating => {
                let decision = self.operate(ready);
                if exited && decision == Decision::Progress {
                    Decision::Complete
                } else {
                    decision
                }
            }
        }
    }

    fn prepare(&mut self, exited: bool, ready: Ready) -> Decision {
        if exited {
            return Decision::Refuse(Refusal::ExitedBeforeReady);
        }
        match ready {
            Ready::Missing => Decision::Wait,
            Ready::Invalid => Decision::Refuse(Refusal::InvalidReady),
            Ready::Published(actual) if actual != self.expected => {
                Decision::Refuse(Refusal::ForeignReady)
            }
            Ready::Published(_) => {
                self.phase = Phase::Operating;
                Decision::Begin
            }
        }
    }

    fn operate(&self, ready: Ready) -> Decision {
        match ready {
            Ready::Missing => Decision::Refuse(Refusal::ReadyDisappeared),
            Ready::Invalid => Decision::Refuse(Refusal::InvalidReady),
            Ready::Published(actual) if actual != self.expected => {
                Decision::Refuse(Refusal::ForeignReady)
            }
            Ready::Published(_) => Decision::Progress,
        }
    }
}

pub(crate) trait Reader {
    type Exit;
    type Error;

    fn try_wait(&mut self) -> Result<Option<Self::Exit>, Self::Error>;
    fn ready(&mut self) -> Result<Ready, Self::Error>;
    fn begin(&mut self) -> Result<(), Self::Error>;
    fn progress(&mut self) -> Result<(), Self::Error>;
    fn pause(&mut self);
    fn close_input(&mut self) -> Result<(), Self::Error>;
    fn stop_tree(&mut self) -> Result<(), Self::Error>;
    fn reap(&mut self) -> Result<(), Self::Error>;
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Problem<E> {
    Refused(Refusal),
    Port(E),
}

#[derive(Debug)]
pub(crate) struct Outcome<T, E> {
    pub(crate) primary: Result<T, Problem<E>>,
    pub(crate) cleanup: [Result<(), E>; 3],
}

pub(crate) fn wait<R: Reader>(
    reader: &mut R,
    mut controller: Controller,
) -> Outcome<R::Exit, R::Error> {
    let primary = run(reader, &mut controller);
    let cleanup = [reader.close_input(), reader.stop_tree(), reader.reap()];
    Outcome { primary, cleanup }
}

fn run<R: Reader>(
    reader: &mut R,
    controller: &mut Controller,
) -> Result<R::Exit, Problem<R::Error>> {
    loop {
        let status = reader.try_wait().map_err(Problem::Port)?;
        let ready = reader.ready().map_err(Problem::Port)?;
        match controller.observe(status.is_some(), ready) {
            Decision::Wait => {}
            Decision::Begin => reader.begin().map_err(Problem::Port)?,
            Decision::Progress => reader.progress().map_err(Problem::Port)?,
            Decision::Complete => {
                return status.ok_or(Problem::Refused(Refusal::ExitedBeforeReady));
            }
            Decision::Refuse(reason) => return Err(Problem::Refused(reason)),
        }
        reader.pause();
    }
}
