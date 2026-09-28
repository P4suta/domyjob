#![expect(
    clippy::disallowed_methods,
    reason = "this module owns the single OpenSSH process boundary"
)]
#![expect(
    clippy::redundant_pub_crate,
    reason = "the composition root needs these names but the binary has no public API"
)]

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, Command, ExitCode, ExitStatus, Stdio};

use domyjob_core::domain::{
    Command as JobCommand, Invalid, JobId, JobReference, MachineName, RelativePath, SubmissionId,
};
use domyjob_core::ingress;
use domyjob_core::state::{JobState, Outcome, PhaseKind};
use domyjob_core::wire::{self, CleanTarget, ErrorCode, Input, Reply, Request, WireError};
use thiserror::Error;

use crate::app::{self, AppError};
use crate::identity;
use crate::store::{Store, StoreError};

#[derive(Debug, Error)]
pub(crate) enum TransportError {
    #[error(transparent)]
    Invalid(#[from] Invalid),
    #[error(transparent)]
    Wire(#[from] WireError),
    #[error(transparent)]
    App(#[from] AppError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Snapshot(#[from] domyjob::snapshot::SnapshotError),
    #[error("SSH or node I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("the operating system could not provide a submission identifier: {0}")]
    Entropy(getrandom::Error),
    #[error("remote node did not exit successfully: {0}")]
    Remote(ExitStatus),
    #[error("remote node sent a reply of the wrong kind")]
    UnexpectedReply,
    #[error("remote node refused the request: {0:?}")]
    Refused(ErrorCode),
    #[error("automatic remote build failed: {0}")]
    Deployment(ExitStatus),
    #[error("remote node still has the wrong build after automatic installation")]
    BuildMismatch,
    #[error("the source contains a symlink, which the first release cannot transfer")]
    SourceLink,
}

struct SshChild {
    child: Child,
    finished: bool,
}

impl Drop for SshChild {
    fn drop(&mut self) {
        if !self.finished {
            let _killed = self.child.kill();
            let _reaped = self.child.wait();
        }
    }
}

impl SshChild {
    fn start(machine: &MachineName) -> Result<Self, TransportError> {
        let child = Command::new("ssh")
            .args([
                "-T",
                "-o",
                "BatchMode=yes",
                "--",
                machine.as_str(),
                "~/.cargo/bin/domyjob-next node",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()?;
        Ok(Self {
            child,
            finished: false,
        })
    }

    fn wait(&mut self) -> Result<ExitStatus, TransportError> {
        let status = self.child.wait()?;
        self.finished = true;
        Ok(status)
    }
}

fn read_frame(input: &mut impl Read) -> Result<Vec<u8>, TransportError> {
    let mut header = [0_u8; 4];
    input.read_exact(&mut header)?;
    let length = wire::ControlLength::try_from(header)?;
    let mut body = vec![0_u8; length.bytes()];
    input.read_exact(&mut body)?;
    let mut frame = Vec::with_capacity(length.bytes().saturating_add(4));
    frame.extend_from_slice(&header);
    frame.extend_from_slice(&body);
    Ok(frame)
}

fn require_end(input: &mut impl Read) -> Result<(), TransportError> {
    let mut extra = [0_u8; 1];
    if input.read(&mut extra)? == 0 {
        Ok(())
    } else {
        Err(WireError::Trailing.into())
    }
}

fn raw_call(
    machine: &MachineName,
    request: &Request,
    payload: &[u8],
) -> Result<Reply, TransportError> {
    let mut child = SshChild::start(machine)?;
    let mut stdin = child
        .child
        .stdin
        .take()
        .ok_or_else(|| std::io::Error::other("SSH standard input was not piped"))?;
    stdin.write_all(&wire::frame(request)?)?;
    stdin.write_all(payload)?;
    drop(stdin);
    let mut stdout = child
        .child
        .stdout
        .take()
        .ok_or_else(|| std::io::Error::other("SSH standard output was not piped"))?;
    let frame = match read_frame(&mut stdout) {
        Ok(frame) => frame,
        Err(error) => {
            let status = child.wait()?;
            return if status.code() == Some(255) {
                Err(TransportError::Remote(status))
            } else {
                Err(error)
            };
        }
    };
    let reply = ingress::reply(&frame)?;
    require_end(&mut stdout)?;
    drop(stdout);
    let status = child.wait()?;
    if !status.success() {
        return Err(TransportError::Remote(status));
    }
    Ok(reply)
}

fn source_checkout() -> Result<&'static Path, TransportError> {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| std::io::Error::other("the source checkout is unavailable").into())
}

fn bootstrap(machine: &MachineName) -> Result<(), TransportError> {
    let checkout = source_checkout()?;
    eprintln!(
        "{}: updating remote binary from this checkout",
        machine.as_str()
    );
    let built = Command::new("mise")
        .current_dir(checkout)
        .env("RUSTC_WRAPPER", "")
        .args(["x", "--", "cargo", "build", "--locked", "-p", "domyjob"])
        .status()?;
    if !built.success() {
        return Err(TransportError::Deployment(built));
    }
    let executable = checkout
        .join("target")
        .join("debug")
        .join(format!("domyjob{}", std::env::consts::EXE_SUFFIX));
    let status = Command::new(executable)
        .current_dir(checkout)
        .args([
            "run",
            machine.as_str(),
            "--wait",
            "--",
            "mise",
            "x",
            "--",
            "cargo",
            "install",
            "--debug",
            "--locked",
            "--path",
            "crates/domyjob-next",
            "--force",
        ])
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(TransportError::Deployment(status))
    }
}

fn stale(error: &TransportError) -> bool {
    match error {
        TransportError::Wire(WireError::Version | WireError::Json(_)) => true,
        TransportError::Io(error) => error.kind() == std::io::ErrorKind::UnexpectedEof,
        TransportError::Invalid(_)
        | TransportError::Wire(_)
        | TransportError::App(_)
        | TransportError::Store(_)
        | TransportError::Snapshot(_)
        | TransportError::Entropy(_)
        | TransportError::Remote(_)
        | TransportError::UnexpectedReply
        | TransportError::Refused(_)
        | TransportError::Deployment(_)
        | TransportError::BuildMismatch
        | TransportError::SourceLink => false,
    }
}

fn ensure_deployed(machine: &MachineName) -> Result<(), TransportError> {
    let first = raw_call(machine, &Request::Hello, &[]);
    match first {
        Ok(Reply::Hello { build }) if build == identity::current() => return Ok(()),
        Ok(Reply::Hello { .. }) => {}
        Err(ref error) if stale(error) => {}
        Ok(_) => return Err(TransportError::UnexpectedReply),
        Err(error) => return Err(error),
    }
    bootstrap(machine)?;
    match raw_call(machine, &Request::Hello, &[])? {
        Reply::Hello { build } if build == identity::current() => Ok(()),
        Reply::Hello { .. } => Err(TransportError::BuildMismatch),
        Reply::Accepted { .. }
        | Reply::Jobs { .. }
        | Reply::Status { .. }
        | Reply::Logs { .. }
        | Reply::Cleaned { .. }
        | Reply::Error { .. } => Err(TransportError::UnexpectedReply),
    }
}

fn call_with_payload(
    machine: &MachineName,
    request: &Request,
    payload: &[u8],
) -> Result<Reply, TransportError> {
    ensure_deployed(machine)?;
    raw_call(machine, request, payload)
}

fn call(machine: &MachineName, request: &Request) -> Result<Reply, TransportError> {
    call_with_payload(machine, request, &[])
}

pub(crate) fn doctor(machine: &MachineName) -> Result<(), TransportError> {
    ensure_deployed(machine)?;
    println!("{}: ready (wire {})", machine.as_str(), wire::VERSION);
    Ok(())
}

fn new_submission() -> Result<SubmissionId, TransportError> {
    let mut entropy = [0_u8; 16];
    getrandom::fill(&mut entropy).map_err(TransportError::Entropy)?;
    let text = format!("{:032x}", u128::from_be_bytes(entropy));
    Ok(SubmissionId::try_from(text)?)
}

enum Source<'a> {
    Home,
    Snapshot {
        archive: &'a [u8],
        descriptor: wire::Snapshot,
    },
}

impl<'a> Source<'a> {
    fn into_parts(self) -> (Input, &'a [u8]) {
        match self {
            Self::Home => (Input::Home, &[]),
            Self::Snapshot {
                archive,
                descriptor,
            } => (Input::Snapshot(descriptor), archive),
        }
    }
}

fn submit(
    machine: &MachineName,
    submission: Option<SubmissionId>,
    command: JobCommand,
    source: Source<'_>,
) -> Result<JobId, TransportError> {
    let submission = match submission {
        Some(submission) => submission,
        None => new_submission()?,
    };
    eprintln!("submission {}", submission.as_str());
    let (input, payload) = source.into_parts();
    let request = Request::Run {
        submission,
        command,
        input,
    };
    match call_with_payload(machine, &request, payload)? {
        Reply::Accepted { job } => Ok(job),
        Reply::Error { code } => Err(TransportError::Refused(code)),
        Reply::Hello { .. }
        | Reply::Jobs { .. }
        | Reply::Status { .. }
        | Reply::Logs { .. }
        | Reply::Cleaned { .. } => Err(TransportError::UnexpectedReply),
    }
}

pub(crate) fn on(
    machine: &MachineName,
    submission: Option<SubmissionId>,
    command: JobCommand,
    wait_for_completion: bool,
) -> Result<ExitCode, TransportError> {
    let job = submit(machine, submission, command, Source::Home)?;
    finish_submission(machine, job, wait_for_completion)
}

fn finish_submission(
    machine: &MachineName,
    job: JobId,
    wait_for_completion: bool,
) -> Result<ExitCode, TransportError> {
    println!("{}:{}", machine.as_str(), job.as_str());
    if !wait_for_completion {
        return Ok(ExitCode::SUCCESS);
    }
    let reference = JobReference::new(machine.clone(), job);
    let result = wait(&reference)?;
    logs(&reference)?;
    Ok(result)
}

fn source_archive() -> Result<(Vec<u8>, wire::Snapshot), TransportError> {
    let root = std::env::current_dir()?;
    let source = domyjob::snapshot::from_directory(&root)?;
    for (path, entry) in &source.manifest.entries {
        RelativePath::try_from(path.as_str().to_owned())?;
        match entry {
            domyjob::snapshot::Entry::File { .. } => {}
            domyjob::snapshot::Entry::Symlink { .. } => return Err(TransportError::SourceLink),
        }
    }
    let archive = domyjob::snapshot::archive(&source)?;
    let bytes = u64::try_from(archive.len()).map_err(|_length| WireError::Snapshot)?;
    let digest = blake3::hash(&archive).to_hex().to_string();
    let descriptor = wire::Snapshot::new(bytes, digest)?;
    Ok((archive, descriptor))
}

pub(crate) fn run(
    machine: &MachineName,
    submission: Option<SubmissionId>,
    command: JobCommand,
    wait_for_completion: bool,
) -> Result<ExitCode, TransportError> {
    let (archive, descriptor) = source_archive()?;
    let job = submit(
        machine,
        submission,
        command,
        Source::Snapshot {
            archive: &archive,
            descriptor,
        },
    )?;
    finish_submission(machine, job, wait_for_completion)
}

pub(crate) fn ls(machine: &MachineName) -> Result<(), TransportError> {
    match call(machine, &Request::List)? {
        Reply::Jobs { jobs } => {
            for job in jobs {
                println!("{}:{}", machine.as_str(), job.as_str());
            }
            Ok(())
        }
        Reply::Error { code } => Err(TransportError::Refused(code)),
        Reply::Hello { .. }
        | Reply::Accepted { .. }
        | Reply::Status { .. }
        | Reply::Logs { .. }
        | Reply::Cleaned { .. } => Err(TransportError::UnexpectedReply),
    }
}

pub(crate) fn clean(machine: &MachineName, target: CleanTarget) -> Result<(), TransportError> {
    match call(machine, &Request::Clean { target })? {
        Reply::Cleaned { count } => {
            println!("{}: cleaned {count} finished jobs", machine.as_str());
            Ok(())
        }
        Reply::Error { code } => Err(TransportError::Refused(code)),
        Reply::Hello { .. }
        | Reply::Accepted { .. }
        | Reply::Jobs { .. }
        | Reply::Status { .. }
        | Reply::Logs { .. } => Err(TransportError::UnexpectedReply),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Observation {
    Current,
    Complete,
    Cancel,
}

fn observe(reference: &JobReference, observation: Observation) -> Result<JobState, TransportError> {
    let request = match observation {
        Observation::Current => Request::Status {
            job: reference.job().clone(),
        },
        Observation::Complete => Request::Wait {
            job: reference.job().clone(),
        },
        Observation::Cancel => Request::Kill {
            job: reference.job().clone(),
        },
    };
    match call(reference.machine(), &request)? {
        Reply::Status { state } => Ok(state),
        Reply::Error { code } => Err(TransportError::Refused(code)),
        Reply::Hello { .. }
        | Reply::Accepted { .. }
        | Reply::Jobs { .. }
        | Reply::Logs { .. }
        | Reply::Cleaned { .. } => Err(TransportError::UnexpectedReply),
    }
}

fn print_state(reference: &JobReference, state: &JobState) {
    let phase = match state.kind() {
        PhaseKind::Accepted => "accepted",
        PhaseKind::Starting => "starting",
        PhaseKind::Running => "running",
        PhaseKind::Finished => "finished",
    };
    let result = match state.outcome() {
        None => String::new(),
        Some(Outcome::Succeeded) => " succeeded".to_owned(),
        Some(Outcome::Failed { code }) => format!(" failed ({code})"),
        Some(Outcome::LaunchFailed { reason }) => {
            format!(" launch failed: {}", reason.for_terminal())
        }
        Some(Outcome::Lost) => " lost".to_owned(),
        Some(Outcome::Killed) => " killed".to_owned(),
    };
    println!(
        "{}:{} {phase}{result}",
        reference.machine().as_str(),
        reference.job().as_str()
    );
}

pub(crate) fn status(reference: &JobReference) -> Result<(), TransportError> {
    let state = observe(reference, Observation::Current)?;
    print_state(reference, &state);
    Ok(())
}

pub(crate) fn wait(reference: &JobReference) -> Result<ExitCode, TransportError> {
    let state = observe(reference, Observation::Complete)?;
    print_state(reference, &state);
    match state.outcome() {
        Some(Outcome::Succeeded) => Ok(ExitCode::SUCCESS),
        Some(Outcome::Failed { code }) => match u8::try_from(code.get()) {
            Ok(0) | Err(_) => Ok(ExitCode::FAILURE),
            Ok(code) => Ok(ExitCode::from(code)),
        },
        Some(Outcome::LaunchFailed { .. } | Outcome::Lost | Outcome::Killed) => {
            Ok(ExitCode::FAILURE)
        }
        None => Err(TransportError::UnexpectedReply),
    }
}

pub(crate) fn kill(reference: &JobReference) -> Result<ExitCode, TransportError> {
    let state = observe(reference, Observation::Cancel)?;
    print_state(reference, &state);
    match state.outcome() {
        Some(Outcome::Killed) => Ok(ExitCode::SUCCESS),
        Some(
            Outcome::Succeeded
            | Outcome::Failed { .. }
            | Outcome::LaunchFailed { .. }
            | Outcome::Lost,
        ) => Ok(ExitCode::FAILURE),
        None => Err(TransportError::UnexpectedReply),
    }
}

pub(crate) fn logs(reference: &JobReference) -> Result<(), TransportError> {
    let request = Request::Logs {
        job: reference.job().clone(),
    };
    match call(reference.machine(), &request)? {
        Reply::Logs { text, omitted } => {
            if omitted != 0 {
                eprintln!("{omitted} earlier log bytes omitted");
            }
            std::io::stdout().write_all(text.for_terminal().as_bytes())?;
            Ok(())
        }
        Reply::Error { code } => Err(TransportError::Refused(code)),
        Reply::Hello { .. }
        | Reply::Accepted { .. }
        | Reply::Jobs { .. }
        | Reply::Status { .. }
        | Reply::Cleaned { .. } => Err(TransportError::UnexpectedReply),
    }
}

pub(crate) fn node() -> Result<(), TransportError> {
    let mut input = std::io::stdin().lock();
    let request = ingress::request(&read_frame(&mut input)?)?;
    let archive = match &request {
        Request::Run {
            input: Input::Snapshot(snapshot),
            ..
        } => Some(Store::open()?.receive_archive(&mut input, snapshot)?),
        Request::Hello
        | Request::Run {
            input: Input::Home, ..
        }
        | Request::List
        | Request::Status { .. }
        | Request::Logs { .. }
        | Request::Wait { .. }
        | Request::Kill { .. }
        | Request::Clean { .. } => None,
    };
    require_end(&mut input)?;
    drop(input);
    let reply = match app::handle(request, archive.as_ref()) {
        Ok(reply) => reply,
        Err(error) => {
            eprintln!("domyjob node: {error}");
            let code = match error {
                AppError::Store(StoreError::Missing) => ErrorCode::MissingJob,
                AppError::Store(StoreError::Conflict) => ErrorCode::ConflictingSubmission,
                AppError::Store(StoreError::Capacity) => ErrorCode::ResourceLimit,
                AppError::Store(StoreError::Active) => ErrorCode::InvalidRequest,
                AppError::Store(
                    StoreError::Corrupt | StoreError::Transition(_) | StoreError::Wire(_),
                ) => ErrorCode::CorruptState,
                AppError::Store(StoreError::ArchiveEntry | StoreError::ArchiveMismatch) => {
                    ErrorCode::InvalidRequest
                }
                AppError::Store(
                    StoreError::State(_)
                    | StoreError::Lock(_)
                    | StoreError::Io(_)
                    | StoreError::Tree(_)
                    | StoreError::Entropy(_),
                )
                | AppError::Proc(_)
                | AppError::OldDomain(_)
                | AppError::Notify(_)
                | AppError::Io(_)
                | AppError::InvalidErrorText => ErrorCode::Internal,
            };
            Reply::Error { code }
        }
    };
    let mut output = std::io::stdout().lock();
    output.write_all(&wire::frame(&reply)?)?;
    output.flush()?;
    Ok(())
}
