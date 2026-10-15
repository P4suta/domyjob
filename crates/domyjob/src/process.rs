use std::ffi::OsStr;
use std::io;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::Mutex;

use domyjob_core::capture_policy::{CaptureAdmission, CapturePolicy, admit};
use domyjob_core::domain::JobId;
use thiserror::Error;

pub(crate) mod chat;
pub(crate) mod resources;

#[cfg(target_os = "macos")]
pub(crate) mod login;

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
use unix as os;
#[cfg(windows)]
use windows as os;

mod raw {
    #![expect(
        clippy::disallowed_methods,
        reason = "`process::command` is the only constructor of child processes"
    )]

    pub(super) fn command(program: &std::ffi::OsStr) -> std::process::Command {
        std::process::Command::new(program)
    }

    #[cfg(unix)]
    pub(super) fn stdout() -> std::io::Stdout {
        std::io::stdout()
    }

    #[cfg(windows)]
    pub(super) fn detached(program: &std::path::Path) -> windows_spawn::Command {
        windows_spawn::Command::new(program)
    }
}

pub(crate) fn command(program: impl AsRef<OsStr>) -> Command {
    let mut command = raw::command(program.as_ref());
    command.stdin(Stdio::null()).stdout(Stdio::null());
    command
}

#[derive(Debug, Error)]
pub(crate) enum ProcessError {
    #[error("starting {what}: {source}")]
    Spawn {
        what: &'static str,
        source: io::Error,
    },
    #[error("the worker exited before announcing readiness: {0}")]
    NotStarted(String),
    #[cfg(target_os = "macos")]
    #[error("macOS login-session hand-off refused: {0}")]
    Login(String),
    #[error("waiting for a process: {0}")]
    Wait(io::Error),
    #[error("signalling a process: {0}")]
    Signal(io::Error),
    #[error("announcing worker readiness: {0}")]
    Ready(io::Error),
    #[error("the process state lock was poisoned")]
    Poisoned,
    #[error("the operating system could not provide a readiness token: {0}")]
    #[cfg(windows)]
    Entropy(getrandom::Error),
    #[error("the readiness token is invalid")]
    InvalidReadyToken,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReadyToken(String);

impl ReadyToken {
    pub(crate) fn parse(value: String) -> Result<Self, ProcessError> {
        const READY_TOKEN_HEX_CHARS: usize = 32;
        if value.len() == READY_TOKEN_HEX_CHARS
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            Ok(Self(value))
        } else {
            Err(ProcessError::InvalidReadyToken)
        }
    }

    #[cfg(windows)]
    fn fresh() -> Result<Self, ProcessError> {
        let mut bytes = [0_u8; 16];
        getrandom::fill(&mut bytes).map_err(ProcessError::Entropy)?;
        Ok(Self(format!("{:032x}", u128::from_be_bytes(bytes))))
    }

    #[cfg(windows)]
    fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug)]
pub(crate) struct Tool<'a> {
    program: &'a str,
    arguments: &'a [String],
    environment: Vec<(&'static str, String)>,
    output: CapturePolicy,
}

impl<'a> Tool<'a> {
    #[must_use]
    pub(crate) const fn new(program: &'a str, arguments: &'a [String]) -> Self {
        Self {
            program,
            arguments,
            environment: Vec::new(),
            output: CapturePolicy::Prefix,
        }
    }

    #[must_use]
    pub(crate) fn env(mut self, name: &'static str, value: String) -> Self {
        self.environment.push((name, value));
        self
    }

    #[must_use]
    pub(crate) const fn output(mut self, policy: CapturePolicy) -> Self {
        self.output = policy;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ToolOutput {
    pub(crate) success: bool,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
}

#[derive(Debug, Error)]
pub(crate) enum ToolError {
    #[error("{0} is not installed or not on PATH")]
    Missing(String),
    #[error("running {program}: {source}")]
    Run { program: String, source: io::Error },
}

const MAX_TOOL_OUTPUT: usize = 1024 * 1024;

fn capture(stream: Option<impl io::Read>, policy: CapturePolicy) -> io::Result<String> {
    let Some(mut stream) = stream else {
        return Ok(String::new());
    };
    let bytes = crate::bounded::prefix(&mut stream, MAX_TOOL_OUTPUT)?;
    let omitted = io::copy(&mut stream, &mut io::sink())?;
    if admit(policy, omitted) == CaptureAdmission::RefuseTruncated {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("tool output exceeds {MAX_TOOL_OUTPUT} bytes; a complete response is required"),
        ));
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

pub(crate) fn run_tool(tool: &Tool<'_>) -> Result<ToolOutput, ToolError> {
    let program = crate::platform::find_program(tool.program)
        .ok_or_else(|| ToolError::Missing(tool.program.to_owned()))?;
    let failed = |source| ToolError::Run {
        program: tool.program.to_owned(),
        source,
    };
    let mut command = command(program);
    command
        .args(tool.arguments)
        .envs(
            tool.environment
                .iter()
                .map(|(name, value)| (*name, value.as_str())),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().map_err(failed)?;
    let errors = child.stderr.take();
    let reader = std::thread::spawn(move || capture(errors, CapturePolicy::Prefix));
    let stdout = capture(child.stdout.take(), tool.output);
    let status = child.wait();
    let stderr = reader.join();
    let stdout = stdout.map_err(failed)?;
    let status = status.map_err(failed)?;
    let stderr = stderr
        .map_err(|_panic| failed(io::Error::other("the output reader panicked")))?
        .map_err(failed)?;
    Ok(ToolOutput {
        success: status.success(),
        stdout,
        stderr,
    })
}

pub(crate) use os::{OutputReader, OutputStop};

pub(crate) fn output_pipe() -> io::Result<(OutputReader, io::PipeWriter, OutputStop)> {
    os::output_pipe()
}

pub(crate) fn terminate(pid: u32) -> Result<(), ProcessError> {
    os::terminate(pid)
}

pub(crate) fn launch_worker(job: &JobId) -> Result<(), ProcessError> {
    #[cfg(target_os = "macos")]
    {
        login::launch_worker(job)
    }
    #[cfg(not(target_os = "macos"))]
    {
        os::launch_worker(&["worker", job.as_str()], None)
    }
}

pub(crate) fn launch_chat_worker(agent: &str, log: std::fs::File) -> Result<(), ProcessError> {
    os::launch_worker(&["chat-worker", "--agent", agent], Some(log))
}

pub(crate) const fn refuses_submission(error: &ProcessError) -> bool {
    #[cfg(target_os = "macos")]
    {
        matches!(error, ProcessError::Login(_))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = error;
        false
    }
}

pub(crate) fn start_login_agent() -> io::Result<()> {
    let start: fn() -> io::Result<()> = {
        #[cfg(target_os = "macos")]
        {
            login::start
        }
        #[cfg(not(target_os = "macos"))]
        {
            || Ok(())
        }
    };
    start()
}

pub(crate) fn announce_ready(token: Option<&ReadyToken>) -> Result<(), ProcessError> {
    os::announce_ready(token)
}

trait Guard: Sized {
    fn stand_guard(tree: &os::Tree, scope: Option<&resources::Scope>)
    -> Result<Self, ProcessError>;
    fn stand_down(self);
    fn reap(group: i32, scope: Option<&resources::Scope>);
}

pub(crate) trait Stop {
    fn stop(self) -> io::Result<()>;
}

pub(crate) fn reap(group: i32, scope: Option<&resources::Scope>) {
    <os::Reaper as Guard>::reap(group, scope);
}

#[derive(Debug)]
enum ChildState {
    Running(std::process::Child),
    Reaped,
}

#[derive(Debug)]
pub(crate) struct Group {
    child: Mutex<ChildState>,
    tree: os::Tree,
    id: u32,
    reaper: Mutex<Option<os::Reaper>>,
    scope: Option<resources::Scope>,
    _permit: Option<resources::AdmissionPermit>,
}

impl Group {
    #[cfg(test)]
    pub(crate) fn spawn_stdio(
        command: Command,
        output: Stdio,
        errors: Stdio,
    ) -> Result<Self, ProcessError> {
        Self::spawn_io(command, Stdio::null(), output, errors)
    }

    pub(crate) fn spawn_io(
        command: Command,
        input: Stdio,
        output: Stdio,
        errors: Stdio,
    ) -> Result<Self, ProcessError> {
        Self::spawn_owned(command, (input, output, errors), None)
    }

    pub(crate) fn spawn_job_stdio(
        launch: resources::Launch,
        output: Stdio,
        errors: Stdio,
    ) -> Result<Self, ProcessError> {
        Self::spawn_owned(
            launch.command,
            (Stdio::null(), output, errors),
            Some((launch.scope, launch.permit)),
        )
    }

    fn spawn_owned(
        mut command: Command,
        (input, output, errors): (Stdio, Stdio, Stdio),
        job: Option<(Option<resources::Scope>, resources::AdmissionPermit)>,
    ) -> Result<Self, ProcessError> {
        let (scope, permit) = match job {
            Some((scope, permit)) => (scope, Some(permit)),
            None => (None, None),
        };
        command.stdin(input).stdout(output).stderr(errors);
        os::isolate(&mut command);
        let mut child = command.spawn().map_err(|source| ProcessError::Spawn {
            what: "the job command",
            source,
        })?;
        drop(command);
        let id = child.id();
        let tree = match os::Tree::adopt(&child) {
            Ok(tree) => tree,
            Err(adopting) => {
                let stopped = child.kill().and_then(|()| child.wait());
                return Err(ProcessError::Spawn {
                    what: "the isolated job command",
                    source: match stopped {
                        Ok(_) => adopting,
                        Err(killing) => io::Error::other(format!(
                            "{adopting}; the half-started command could not be stopped: {killing}"
                        )),
                    },
                });
            }
        };
        let reaper = match os::Reaper::stand_guard(&tree, scope.as_ref()) {
            Ok(reaper) => reaper,
            Err(error) => {
                let _stopped = tree.kill_all();
                let _reaped = child.wait();
                return Err(error);
            }
        };
        Ok(Self {
            child: Mutex::new(ChildState::Running(child)),
            tree,
            id,
            reaper: Mutex::new(Some(reaper)),
            scope,
            _permit: permit,
        })
    }

    pub(crate) const fn id(&self) -> u32 {
        self.id
    }

    pub(crate) fn wait(&self) -> Result<ExitStatus, ProcessError> {
        self.tree.await_leader()?;
        let mut guard = self
            .child
            .lock()
            .map_err(|_poisoned| ProcessError::Poisoned)?;
        let state = std::mem::replace(&mut *guard, ChildState::Reaped);
        let mut child = match state {
            ChildState::Running(child) => child,
            ChildState::Reaped => {
                return Err(ProcessError::Wait(io::Error::other("already reaped")));
            }
        };
        self.tree.kill_remaining()?;
        let status = child.wait().map_err(ProcessError::Wait);
        drop(guard);
        if self.scope.is_none() {
            self.stand_down()?;
        }
        status
    }

    fn stand_down(&self) -> Result<(), ProcessError> {
        let reaper = self
            .reaper
            .lock()
            .map_err(|_poisoned| ProcessError::Poisoned)?
            .take();
        if let Some(reaper) = reaper {
            reaper.stand_down();
        }
        Ok(())
    }

    pub(crate) fn kill(&self) -> Result<(), ProcessError> {
        if let Some(scope) = &self.scope {
            scope.stop().map_err(ProcessError::Signal)?;
        }
        let guard = self
            .child
            .lock()
            .map_err(|_poisoned| ProcessError::Poisoned)?;
        match &*guard {
            ChildState::Running(_) => self.tree.kill_all(),
            ChildState::Reaped => Ok(()),
        }
    }

    pub(crate) fn completion(
        &self,
        event: domyjob_core::state::Event,
    ) -> Result<domyjob_core::state::Event, ProcessError> {
        let completion = match &self.scope {
            Some(scope) => scope.completion(&event).map_err(ProcessError::Wait),
            None => Ok(event),
        }?;
        self.stand_down()?;
        Ok(completion)
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        if let Some(scope) = &self.scope
            && let Err(error) = scope.stop().and_then(|()| scope.cleanup())
        {
            eprintln!("domyjob: stopping {} failed: {error}", scope.name());
        }
    }
}

#[cfg(all(test, unix))]
const FIXTURE_SHELL: &str = "/bin/sh";

#[cfg(all(test, unix))]
pub(crate) fn signal_termination_fixture() -> (Command, bool) {
    let mut command = command(FIXTURE_SHELL);
    command.args(["-c", "kill -TERM $$"]);
    (command, true)
}

#[cfg(all(test, windows))]
pub(crate) fn signal_termination_fixture() -> (Command, bool) {
    (stdout_then_stderr(), false)
}

#[cfg(all(test, unix))]
pub(crate) const STDOUT_THEN_STDERR: &[u8] = b"0123456789abcdefghij";
#[cfg(all(test, windows))]
pub(crate) const STDOUT_THEN_STDERR: &[u8] = b"0123456789\r\nabcdefghij\r\n";

#[cfg(all(test, unix))]
pub(crate) fn stdout_then_stderr() -> Command {
    let mut command = command(FIXTURE_SHELL);
    command.args(["-c", "printf 0123456789; printf abcdefghij >&2"]);
    command
}

#[cfg(all(test, windows))]
pub(crate) fn stdout_then_stderr() -> Command {
    use std::os::windows::process::CommandExt as _;

    let mut command = command("cmd.exe");
    command.raw_arg("/d /c echo 0123456789& echo abcdefghij>&2");
    command
}

#[cfg(test)]
mod tests {
    use super::{MAX_TOOL_OUTPUT, Tool, capture, run_tool};
    use domyjob_core::capture_policy::CapturePolicy;
    use std::io::{self, Read as _};

    struct FailingReader;

    impl io::Read for FailingReader {
        fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "injected tool stream failure",
            ))
        }
    }

    #[test]
    fn complete_capture_admits_the_exact_budget_and_refuses_a_drained_suffix() {
        let exact = vec![b'x'; MAX_TOOL_OUTPUT];
        assert_eq!(
            capture(Some(exact.as_slice()), CapturePolicy::Complete)
                .unwrap()
                .len(),
            MAX_TOOL_OUTPUT
        );
        let larger = vec![b'x'; MAX_TOOL_OUTPUT.saturating_add(1)];
        let mut stream = io::Cursor::new(&larger);
        let error = capture(Some(&mut stream), CapturePolicy::Complete).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(stream.position(), u64::try_from(larger.len()).unwrap());
        let mut prefix = io::Cursor::new(&larger);
        assert_eq!(
            capture(Some(&mut prefix), CapturePolicy::Prefix)
                .unwrap()
                .len(),
            MAX_TOOL_OUTPUT
        );
        assert_eq!(prefix.position(), u64::try_from(larger.len()).unwrap());
    }

    #[test]
    fn tool_capture_preserves_read_errors_before_and_after_the_budget() {
        let prefix = vec![b'x'; MAX_TOOL_OUTPUT];
        for policy in [CapturePolicy::Prefix, CapturePolicy::Complete] {
            let before = capture(Some(FailingReader), policy).unwrap_err();
            assert_eq!(before.kind(), io::ErrorKind::PermissionDenied);
            let after = capture(Some(prefix.as_slice().chain(FailingReader)), policy).unwrap_err();
            assert_eq!(after.kind(), io::ErrorKind::PermissionDenied);
        }
    }

    #[cfg(unix)]
    #[test]
    fn complete_tool_refuses_stdout_overflow_after_draining_both_streams() {
        const DIAGNOSTIC_MULTIPLIER: usize = 3;
        let root = tempfile::tempdir().unwrap();
        let marker = root.path().join("finished");
        let arguments = [
            "-c".to_owned(),
            format!(
                "umask 077; head -c {} /dev/zero; head -c {} /dev/zero >&2; exec 1>&- 2>&-; printf done > \"$1\"",
                MAX_TOOL_OUTPUT.saturating_add(1),
                MAX_TOOL_OUTPUT * DIAGNOSTIC_MULTIPLIER
            ),
            "tool-fixture".to_owned(),
            marker.to_string_lossy().into_owned(),
        ];
        let error =
            run_tool(&Tool::new("sh", &arguments).output(CapturePolicy::Complete)).unwrap_err();
        assert!(
            matches!(error, super::ToolError::Run { source, .. } if source.kind() == io::ErrorKind::InvalidData)
        );
        assert_eq!(
            crate::state_io::read_bytes(&marker).unwrap(),
            Some(b"done".to_vec())
        );
    }

    #[cfg(unix)]
    #[test]
    fn complete_capture_preserves_a_nonzero_tool_outcome_and_diagnostics() {
        let arguments = [
            "-c".to_owned(),
            "printf structured; printf diagnostic >&2; exit 7".to_owned(),
        ];
        let output =
            run_tool(&Tool::new("sh", &arguments).output(CapturePolicy::Complete)).unwrap();
        assert!(!output.success);
        assert_eq!(output.stdout, "structured");
        assert_eq!(output.stderr, "diagnostic");
    }

    #[test]
    fn a_tool_that_prints_past_the_limit_keeps_its_own_outcome() {
        if cfg!(windows) {
            return;
        }
        let script = [
            "-c".to_owned(),
            format!(
                "echo started >&2; head -c {} /dev/zero",
                MAX_TOOL_OUTPUT * 3
            ),
        ];
        let output = run_tool(&Tool::new("sh", &script)).expect("the tool finishes");
        assert!(output.success, "the tool succeeded: {output:?}");
        assert_eq!(output.stdout.len(), MAX_TOOL_OUTPUT);
        assert_eq!(output.stderr.trim(), "started");
    }
}
