use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, ExitCode, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc;

use domyjob_core::chat_wire::{ChatReply, ChatRequest};
use domyjob_core::domain::{
    Command as JobCommand, Invalid, JobId, JobReference, MachineName, SubmissionId,
};
use domyjob_core::ingress;
use domyjob_core::state::{JobState, Outcome, PhaseKind};
use domyjob_core::wire::{
    self, CleanTarget, ErrorCode, Input, Reply, Request, Unexpected, WireError,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::app::{self, AppError};
use crate::identity;
use crate::layout::State;
use crate::lock::{LockError, OsLock};
use crate::output::Output;
use crate::platform::{self, clock};
use crate::process;
use crate::source::{self, SourceError};
use crate::state_io::{self, StateError};
use crate::store::{Store, StoreError};

const EMBEDDED_SOURCE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/source.tar"));
const MISSING_NODE: i32 = 97;

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
    Source(#[from] SourceError),
    #[error(transparent)]
    State(#[from] StateError),
    #[error(transparent)]
    Lock(#[from] LockError),
    #[error("SSH or node I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("the operating system could not provide a submission identifier: {0}")]
    Entropy(getrandom::Error),
    #[error("remote node did not exit successfully: {0}")]
    Remote(ExitStatus),
    #[error("SSH could not reach the machine; check `ssh -o BatchMode=yes MACHINE true`")]
    Unreachable,
    #[error("remote node sent a reply of the wrong kind")]
    UnexpectedReply,
    #[error("remote node refused the request: {0:?}")]
    Refused(ErrorCode),
    #[error("automatic build failed: {0}")]
    Deployment(ExitStatus),
    #[error("the remote node still reports a different build after installation")]
    BuildMismatch,
    #[error("this build's node is not installed on the remote machine")]
    Missing,
    #[error("the remote host does not identify a supported shell")]
    RemoteShell,
    #[error("the remote call did not finish before its deadline")]
    Deadline,
    #[error("the shell cache is corrupt: {0}")]
    Cache(#[from] ingress::JsonError),
    #[error("encoding the shell cache failed: {0}")]
    CacheEncoding(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Shell {
    Unix,
    Windows,
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

fn ssh_arguments() -> Result<Vec<String>, TransportError> {
    let mut arguments: Vec<String> = [
        "-T",
        "-o",
        "BatchMode=yes",
        "-o",
        "ConnectTimeout=15",
        "-o",
        "ServerAliveInterval=10",
        "-o",
        "ServerAliveCountMax=3",
    ]
    .iter()
    .map(|word| (*word).to_owned())
    .collect();
    if let Some(control) = platform::ssh_control_path(&State::here()?.ssh_sockets())? {
        arguments.extend([
            "-o".to_owned(),
            "ControlMaster=auto".to_owned(),
            "-o".to_owned(),
            format!("ControlPath={}", control.display()),
            "-o".to_owned(),
            "ControlPersist=60".to_owned(),
        ]);
    }
    Ok(arguments)
}

impl SshChild {
    fn start(
        machine: &MachineName,
        remote_command: &str,
        stdout: Stdio,
    ) -> Result<Self, TransportError> {
        let child = process::command("ssh")
            .args(ssh_arguments()?)
            .args(["--", machine.as_str(), remote_command])
            .stdin(Stdio::piped())
            .stdout(stdout)
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

struct Watchdog(Option<mpsc::SyncSender<()>>);

impl Watchdog {
    fn start(child: &Child, deadline: Option<clock::Deadline>) -> Self {
        let Some(deadline) = deadline else {
            return Self(None);
        };
        let (sender, receiver) = mpsc::sync_channel(1);
        let pid = child.id();
        std::thread::spawn(move || {
            if matches!(clock::receive(&receiver, deadline), clock::Waited::Expired) {
                let _stopped = process::terminate(pid);
            }
        });
        Self(Some(sender))
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        if let Some(sender) = self.0.take() {
            let _stopped = sender.try_send(());
        }
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

fn node_command(shell: Shell, build: &str) -> String {
    match shell {
        Shell::Unix => format!(
            "sh -c 'p=\"$HOME/.cargo/domyjob/versions/{build}/bin/domyjob\"; [ -x \"$p\" ] || {{ echo \"domyjob: node {build} missing\" >&2; exit {MISSING_NODE}; }}; exec \"$p\" node'"
        ),
        Shell::Windows => format!(
            "$p = Join-Path $env:USERPROFILE '.cargo\\domyjob\\versions\\{build}\\bin\\domyjob.exe'; if (-not (Test-Path -LiteralPath $p)) {{ [Console]::Error.WriteLine('domyjob: node {build} missing'); exit {MISSING_NODE} }}; & $p node; exit $LASTEXITCODE"
        ),
    }
}

#[derive(Debug, Clone, Copy)]
struct Policy {
    deadline: Option<clock::Deadline>,
    hold_input: bool,
}

const PLAIN: Policy = Policy {
    deadline: None,
    hold_input: false,
};

fn failure(status: ExitStatus, policy: Policy, error: TransportError) -> TransportError {
    match status.code() {
        Some(MISSING_NODE) => TransportError::Missing,
        Some(255) => TransportError::Unreachable,
        None if policy.deadline.is_some_and(clock::Deadline::expired) => TransportError::Deadline,
        Some(_) | None => error,
    }
}

fn raw_call(
    (machine, shell): (&MachineName, Shell),
    request: &Request,
    payload: &[u8],
    policy: Policy,
) -> Result<Reply, TransportError> {
    let child = SshChild::start(
        machine,
        &node_command(shell, &identity::tag()),
        Stdio::piped(),
    )?;
    exchange(child, request, payload, policy)
}

fn exchange(
    mut child: SshChild,
    request: &Request,
    payload: &[u8],
    policy: Policy,
) -> Result<Reply, TransportError> {
    let watchdog = Watchdog::start(&child.child, policy.deadline);
    let mut stdin = child
        .child
        .stdin
        .take()
        .ok_or_else(|| std::io::Error::other("SSH standard input was not piped"))?;
    let sent = stdin
        .write_all(&wire::frame(request)?)
        .and_then(|()| stdin.write_all(payload))
        .and_then(|()| stdin.flush());
    if let Err(error) = sent {
        drop(stdin);
        drop(child.child.stdout.take());
        let status = child.wait()?;
        drop(watchdog);
        return Err(failure(status, policy, error.into()));
    }
    let held = if policy.hold_input {
        Some(stdin)
    } else {
        drop(stdin);
        None
    };
    let mut stdout = child
        .child
        .stdout
        .take()
        .ok_or_else(|| std::io::Error::other("SSH standard output was not piped"))?;
    let frame = read_frame(&mut stdout);
    drop(held);
    let frame = match frame {
        Ok(frame) => frame,
        Err(error) => {
            let status = child.wait()?;
            drop(watchdog);
            return Err(failure(status, policy, error));
        }
    };
    let reply = ingress::reply(&frame)?;
    require_end(&mut stdout)?;
    drop(stdout);
    let status = child.wait()?;
    drop(watchdog);
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

pub(crate) fn refresh_local() -> Result<Option<ExitCode>, TransportError> {
    if std::env::var_os("DOMYJOB_REFRESH").is_some_and(|value| value == "never") {
        return Ok(None);
    }
    let Some(checkout_build) = identity::checkout()? else {
        return Ok(None);
    };
    if checkout_build == identity::current() {
        return Ok(None);
    }
    if std::env::var_os("DOMYJOB_LOCAL_REFRESHED").is_some() {
        return Err(std::io::Error::other("the source changed during the local rebuild").into());
    }
    let checkout = source_checkout()?;
    let target = platform::local_refresh_target(&platform::cargo_target_dir(checkout))?;
    eprintln!("domyjob: rebuilding the local client from this checkout");
    let built = process::command("mise")
        .current_dir(checkout)
        .env("CARGO_TARGET_DIR", &target)
        .env("RUSTC_WRAPPER", "")
        .args(["x", "--", "cargo", "build", "--locked", "-p", "domyjob"])
        .stdout(Stdio::from(std::io::stderr()))
        .status()?;
    if !built.success() {
        return Err(TransportError::Deployment(built));
    }
    let executable = target
        .join("debug")
        .join(format!("domyjob{}", std::env::consts::EXE_SUFFIX));
    let status = process::command(executable)
        .env("DOMYJOB_LOCAL_REFRESHED", "1")
        .args(std::env::args_os().skip(1))
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .status()?;
    Ok(Some(match status.code().map(u8::try_from) {
        Some(Ok(code)) => ExitCode::from(code),
        Some(Err(_)) | None => ExitCode::FAILURE,
    }))
}

fn detect_shell(machine: &MachineName) -> Result<Shell, TransportError> {
    let mut child = SshChild::start(machine, "echo $env:OS", Stdio::piped())?;
    drop(child.child.stdin.take());
    let mut output = child
        .child
        .stdout
        .take()
        .ok_or_else(|| std::io::Error::other("SSH standard output was not piped"))?;
    let bytes = crate::bounded::prefix(&mut output, 65)?;
    drop(output);
    let status = child.wait()?;
    if status.code() == Some(255) {
        return Err(TransportError::Unreachable);
    }
    if !status.success() || bytes.len() > 64 {
        return Err(TransportError::RemoteShell);
    }
    match std::str::from_utf8(&bytes)
        .map_err(|_utf8| TransportError::RemoteShell)?
        .trim()
    {
        "Windows_NT" => Ok(Shell::Windows),
        ":OS" => Ok(Shell::Unix),
        _ => Err(TransportError::RemoteShell),
    }
}

fn shell(machine: &MachineName) -> Result<Shell, TransportError> {
    let path = State::here()?.shells();
    let mut hosts: BTreeMap<String, Shell> = match state_io::read_bytes(&path)? {
        Some(bytes) => ingress::json(&bytes, wire::MAX_CONTROL_BYTES)?,
        None => BTreeMap::new(),
    };
    if let Some(shell) = hosts.get(machine.as_str()) {
        return Ok(*shell);
    }
    let shell = detect_shell(machine)?;
    hosts.insert(machine.as_str().to_owned(), shell);
    state_io::write_bytes(&path, &serde_json::to_vec(&hosts)?)?;
    Ok(shell)
}

fn install_command(shell: Shell, build: &str) -> String {
    match shell {
        Shell::Unix => String::from(
            r#"bash -lc 'set -eu; umask 077; base="${XDG_CACHE_HOME:-$HOME/.cache}/domyjob/bootstrap"; install="$HOME/.cargo/domyjob/versions/@BUILD@"; mkdir -p "$base"; work="$(mktemp -d "$base/source.XXXXXXXX")"; tar -xmf - -C "$work"; cd "$work"; export CARGO_TARGET_DIR="$base/target" MISE_TRUSTED_CONFIG_PATHS="$work"; trap '"'"'cd "$work" && mise x -- cargo clean --quiet -p domyjob -p domyjob-core || true; cd "$HOME"; rm -rf -- "$work"'"'"' EXIT; mise x -- cargo install --debug --locked --path crates/domyjob --bin domyjob --root "$install" --force'"#,
        )
        .replace("@BUILD@", build),
        Shell::Windows => {
            let script = r#"$ErrorActionPreference="Stop"; $ProgressPreference="SilentlyContinue"; $base=Join-Path $env:LOCALAPPDATA "domyjob\bootstrap"; $install=Join-Path $env:USERPROFILE ".cargo\domyjob\versions\@BUILD@"; $null=New-Item -ItemType Directory -Force -Path $base; $work=Join-Path $base ([guid]::NewGuid().ToString("N")); $null=New-Item -ItemType Directory -Path $work; tar.exe -xmf - -C $work; if ($LASTEXITCODE -ne 0) { throw "source extraction failed" }; Set-Location $work; $env:CARGO_TARGET_DIR=Join-Path $base "target"; $env:MISE_TRUSTED_CONFIG_PATHS=$work; mise x -- cargo install --debug --locked --path crates/domyjob --bin domyjob --root $install --force; $result=$LASTEXITCODE; mise x -- cargo clean --quiet -p domyjob -p domyjob-core; Set-Location $env:USERPROFILE; if ($result -eq 0) { Remove-Item -LiteralPath $work -Recurse -Force }; exit $result"#.replace("@BUILD@", build);
            let utf16: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
            format!(
                "powershell.exe -NoProfile -EncodedCommand {}",
                data_encoding::BASE64.encode(&utf16)
            )
        }
    }
}

fn bootstrap(machine: &MachineName, shell: Shell) -> Result<(), TransportError> {
    if u64::try_from(EMBEDDED_SOURCE.len()).map_err(|_size| WireError::Snapshot)?
        > wire::MAX_SNAPSHOT_BYTES
    {
        return Err(WireError::Snapshot.into());
    }
    eprintln!("{}: installing this build's node", machine.as_str());
    let mut child = SshChild::start(
        machine,
        &install_command(shell, &identity::tag()),
        Stdio::from(std::io::stderr()),
    )?;
    let mut input = child
        .child
        .stdin
        .take()
        .ok_or_else(|| std::io::Error::other("SSH standard input was not piped"))?;
    input.write_all(EMBEDDED_SOURCE)?;
    drop(input);
    let status = child.wait()?;
    if status.success() {
        Ok(())
    } else {
        Err(TransportError::Deployment(status))
    }
}

fn call_with(
    machine: &MachineName,
    request: &Request,
    payload: &[u8],
    policy: Policy,
) -> Result<Reply, TransportError> {
    let shell = shell(machine)?;
    let call = || raw_call((machine, shell), request, payload, policy);
    match call() {
        Err(TransportError::Missing) => {}
        other => return other,
    }
    let _installing = OsLock::exclusive(&State::here()?.install_lock(machine))?;
    match call() {
        Err(TransportError::Missing) => {
            bootstrap(machine, shell)?;
            call()
        }
        other => other,
    }
}

fn pick<T>(reply: Reply, kind: fn(Reply) -> Result<T, Unexpected>) -> Result<T, TransportError> {
    match kind(reply) {
        Ok(value) => Ok(value),
        Err(Unexpected::Refused(code)) => Err(TransportError::Refused(code)),
        Err(Unexpected::Other) => Err(TransportError::UnexpectedReply),
    }
}

fn expect<T>(
    machine: &MachineName,
    request: &Request,
    payload: &[u8],
    kind: fn(Reply) -> Result<T, Unexpected>,
) -> Result<T, TransportError> {
    pick(call_with(machine, request, payload, PLAIN)?, kind)
}

pub(crate) fn doctor(machine: &MachineName, output: &Output) -> Result<(), TransportError> {
    let build = expect(machine, &Request::Hello, &[], Reply::into_hello)?;
    if build != identity::current() {
        return Err(TransportError::BuildMismatch);
    }
    output.line(format_args!(
        "{}: ready (build {})",
        machine.as_str(),
        identity::tag()
    ))?;
    Ok(())
}

pub(crate) fn chat(
    machine: &MachineName,
    request: &ChatRequest,
    deadline: clock::Deadline,
) -> Result<ChatReply, TransportError> {
    let policy = Policy {
        deadline: Some(deadline),
        hold_input: matches!(request, ChatRequest::Wait { .. }),
    };
    pick(
        call_with(machine, &Request::Chat(request.clone()), &[], policy)?,
        Reply::into_chat,
    )
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
    expect(machine, &request, payload, Reply::into_accepted)
}

#[derive(Debug)]
pub(crate) struct Submission {
    pub(crate) id: Option<SubmissionId>,
    pub(crate) command: JobCommand,
    pub(crate) wait: bool,
}

pub(crate) fn on(
    machine: &MachineName,
    submission: Submission,
    output: &Output,
) -> Result<ExitCode, TransportError> {
    let job = submit(machine, submission.id, submission.command, Source::Home)?;
    finish_submission(machine, job, submission.wait, output)
}

fn finish_submission(
    machine: &MachineName,
    job: JobId,
    wait_for_completion: bool,
    output: &Output,
) -> Result<ExitCode, TransportError> {
    output.line(format_args!("{}:{}", machine.as_str(), job.as_str()))?;
    if !wait_for_completion {
        return Ok(ExitCode::SUCCESS);
    }
    let reference = JobReference::new(machine.clone(), job);
    let result = wait(&reference, output)?;
    logs(&reference, output)?;
    Ok(result)
}

fn source_archive() -> Result<(Vec<u8>, wire::Snapshot), TransportError> {
    Ok(source::working_directory(&std::env::current_dir()?)?)
}

pub(crate) fn run(
    machine: &MachineName,
    submission: Submission,
    output: &Output,
) -> Result<ExitCode, TransportError> {
    let (archive, descriptor) = source_archive()?;
    let job = submit(
        machine,
        submission.id,
        submission.command,
        Source::Snapshot {
            archive: &archive,
            descriptor,
        },
    )?;
    finish_submission(machine, job, submission.wait, output)
}

pub(crate) fn ls(machine: &MachineName, output: &Output) -> Result<(), TransportError> {
    for job in expect(machine, &Request::List, &[], Reply::into_jobs)? {
        output.line(format_args!("{}:{}", machine.as_str(), job.as_str()))?;
    }
    Ok(())
}

pub(crate) fn clean(
    machine: &MachineName,
    target: CleanTarget,
    output: &Output,
) -> Result<(), TransportError> {
    let count = expect(
        machine,
        &Request::Clean { target },
        &[],
        Reply::into_cleaned,
    )?;
    output.line(format_args!(
        "{}: cleaned {count} finished jobs",
        machine.as_str()
    ))?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Observation {
    Current,
    Complete,
    Cancel,
}

fn observe(reference: &JobReference, observation: Observation) -> Result<JobState, TransportError> {
    let job = reference.job().clone();
    let request = match observation {
        Observation::Current => Request::Status { job },
        Observation::Complete => Request::Wait { job },
        Observation::Cancel => Request::Kill { job },
    };
    expect(reference.machine(), &request, &[], Reply::into_status)
}

fn print_state(reference: &JobReference, state: &JobState, output: &Output) -> std::io::Result<()> {
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
    output.line(format_args!(
        "{}:{} {phase}{result}",
        reference.machine().as_str(),
        reference.job().as_str()
    ))
}

pub(crate) fn status(reference: &JobReference, output: &Output) -> Result<(), TransportError> {
    let state = observe(reference, Observation::Current)?;
    print_state(reference, &state, output)?;
    Ok(())
}

pub(crate) fn wait(reference: &JobReference, output: &Output) -> Result<ExitCode, TransportError> {
    let state = observe(reference, Observation::Complete)?;
    print_state(reference, &state, output)?;
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

pub(crate) fn kill(reference: &JobReference, output: &Output) -> Result<ExitCode, TransportError> {
    let state = observe(reference, Observation::Cancel)?;
    print_state(reference, &state, output)?;
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

pub(crate) fn logs(reference: &JobReference, output: &Output) -> Result<(), TransportError> {
    let request = Request::Logs {
        job: reference.job().clone(),
    };
    let (text, omitted) = expect(reference.machine(), &request, &[], Reply::into_logs)?;
    if omitted != 0 {
        eprintln!("{omitted} earlier log bytes omitted");
    }
    output.write(text.for_terminal().as_bytes())?;
    Ok(())
}

fn watch_disconnect(abandoned: &Arc<AtomicBool>) {
    let abandoned = Arc::clone(abandoned);
    std::thread::spawn(move || {
        let mut sink = [0_u8; 64];
        let mut input = std::io::stdin();
        loop {
            match input.read(&mut sink) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
        abandoned.store(true, std::sync::atomic::Ordering::Release);
    });
}

const fn error_code(error: &AppError) -> ErrorCode {
    match error {
        AppError::Store(StoreError::Missing) => ErrorCode::MissingJob,
        AppError::Store(StoreError::Conflict) => ErrorCode::ConflictingSubmission,
        AppError::Store(StoreError::Capacity) => ErrorCode::ResourceLimit,
        AppError::Store(
            StoreError::Active | StoreError::ArchiveEntry | StoreError::ArchiveMismatch,
        ) => ErrorCode::InvalidRequest,
        AppError::Store(StoreError::Corrupt | StoreError::Transition(_) | StoreError::Wire(_)) => {
            ErrorCode::CorruptState
        }
        AppError::Chat(_)
        | AppError::Store(
            StoreError::State(_)
            | StoreError::Lock(_)
            | StoreError::Io(_)
            | StoreError::Workspace(_)
            | StoreError::Entropy(_),
        )
        | AppError::Proc(_)
        | AppError::Notify(_)
        | AppError::Io(_)
        | AppError::InvalidErrorText => ErrorCode::Internal,
    }
}

fn prune_builds() {
    let versions = match platform::home() {
        Ok(home) => home.join(".cargo").join("domyjob").join("versions"),
        Err(error) => {
            eprintln!("domyjob node: locating installed builds failed: {error}");
            return;
        }
    };
    if let Err(error) = crate::builds::prune(&versions, &identity::tag()) {
        eprintln!("domyjob node: removing unused builds failed: {error}");
    }
}

fn prune_runner_stores() {
    let removed = State::here()
        .map_err(StoreError::from)
        .and_then(|state| crate::store::prune_other_formats(&state));
    if let Err(error) = removed {
        eprintln!("domyjob node: removing job stores of other formats failed: {error}");
    }
}

pub(crate) fn node(output: &Output) -> Result<(), TransportError> {
    prune_builds();
    prune_runner_stores();
    let mut input = std::io::stdin().lock();
    let request = ingress::request(&read_frame(&mut input)?)?;
    let abandoned = Arc::new(AtomicBool::new(false));
    let archive = match &request {
        Request::Run {
            input: Input::Snapshot(snapshot),
            ..
        } => Some(Store::open()?.receive_archive(&mut input, snapshot)?),
        Request::Hello
        | Request::Chat(_)
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
    if matches!(&request, Request::Chat(ChatRequest::Wait { .. })) {
        drop(input);
        watch_disconnect(&abandoned);
    } else {
        require_end(&mut input)?;
        drop(input);
    }
    let reply = match app::handle(request, archive.as_ref(), &abandoned) {
        Ok(reply) => reply,
        Err(error) => {
            eprintln!("domyjob node: {error}");
            Reply::Error {
                code: error_code(&error),
            }
        }
    };
    output.write(&wire::frame(&reply)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::process::Stdio;

    use domyjob_core::wire::{BuildId, Request};

    use super::{
        EMBEDDED_SOURCE, PLAIN, Shell, SshChild, TransportError, exchange, identity,
        install_command, node_command,
    };
    use crate::{process, source_fingerprint};

    fn rejected_large_send(unix: &str, windows: &str) -> TransportError {
        let mut command = process::command(if cfg!(windows) { "cmd.exe" } else { "sh" });
        if cfg!(windows) {
            command.args(["/D", "/C", windows]);
        } else {
            command.args(["-c", unix]);
        }
        let child = SshChild {
            child: command
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap(),
            finished: false,
        };
        exchange(child, &Request::Hello, &vec![b'x'; 2_097_152], PLAIN).unwrap_err()
    }

    #[test]
    fn missing_node_status_survives_a_broken_pipe_during_a_large_send() {
        let error = rejected_large_send("exit 97", "exit /b 97");
        assert!(matches!(error, TransportError::Missing), "{error:?}");
    }

    #[test]
    fn unreachable_ssh_status_survives_a_broken_pipe_during_a_large_send() {
        let error = rejected_large_send("exit 255", "exit /b 255");
        assert!(matches!(error, TransportError::Unreachable), "{error:?}");
    }

    #[test]
    fn other_exit_status_preserves_the_original_send_io_error() {
        let error = rejected_large_send("exit 23", "exit /b 23");
        assert!(
            matches!(error, TransportError::Io(ref io) if io.kind() == std::io::ErrorKind::BrokenPipe),
            "{error:?}"
        );
    }

    #[test]
    fn embedded_deployment_source_matches_the_compiled_build() {
        let checkout = tempfile::tempdir().unwrap();
        tar::Archive::new(EMBEDDED_SOURCE)
            .unpack(checkout.path())
            .unwrap();
        let fingerprint =
            source_fingerprint::from_checkout(checkout.path(), |_kind, _path| {}).unwrap();
        assert_eq!(BuildId::from_fingerprint(fingerprint), identity::current());
    }

    #[test]
    fn remote_node_commands_report_a_missing_build_with_the_reserved_status() {
        let unix = node_command(Shell::Unix, "0123456789abcdef");
        assert!(unix.starts_with("sh -c '"));
        assert!(unix.contains("versions/0123456789abcdef/bin/domyjob"));
        assert!(unix.contains("exit 97"));
        assert!(unix.contains("exec \"$p\" node"));
        let windows = node_command(Shell::Windows, "0123456789abcdef");
        assert!(windows.contains("Test-Path -LiteralPath $p"));
        assert!(windows.contains("exit 97"));
        assert!(windows.ends_with("exit $LASTEXITCODE"));
    }

    #[test]
    fn remote_installs_leave_only_reusable_artifacts_behind() {
        let unix = install_command(Shell::Unix, "0123456789abcdef");
        assert!(unix.contains("tar -xmf"), "extraction stamps fresh times");
        assert!(unix.contains("trap '\"'\"'cd \"$work\" && mise x -- cargo clean --quiet -p domyjob -p domyjob-core || true"));
        let encoded = install_command(Shell::Windows, "0123456789abcdef");
        let base64 = encoded
            .strip_prefix("powershell.exe -NoProfile -EncodedCommand ")
            .unwrap();
        let utf16 = data_encoding::BASE64.decode(base64.as_bytes()).unwrap();
        let (units, rest) = utf16.as_chunks::<2>();
        assert!(rest.is_empty());
        let units: Vec<u16> = units.iter().map(|pair| u16::from_le_bytes(*pair)).collect();
        let script = String::from_utf16(&units).unwrap();
        assert!(
            script.contains("tar.exe -xmf"),
            "extraction stamps fresh times"
        );
        assert!(script.contains("mise x -- cargo clean --quiet -p domyjob -p domyjob-core"));
    }
}
