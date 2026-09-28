use std::ffi::OsStr;
use std::io;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::Mutex;

use domyjob_core::domain::JobId;
use thiserror::Error;

pub(crate) mod chat;

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

    /// A worker announces its readiness to its launcher on standard output.
    #[cfg(unix)]
    pub(super) fn stdout() -> std::io::Stdout {
        std::io::stdout()
    }

    #[cfg(windows)]
    pub(super) fn detached(program: &std::path::Path) -> windows_spawn::Command {
        windows_spawn::Command::new(program)
    }
}

/// A command for `program` whose standard input and output start closed.
///
/// Every child process starts here, so a child writes into this process's own output,
/// such as an MCP server's JSON-RPC stream, only where a caller connects it on purpose.
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
        if value.len() == 32
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

/// A short command-line tool run by setup, doctor, and the service installer.
#[derive(Debug)]
pub(crate) struct Tool<'a> {
    program: &'a str,
    arguments: &'a [String],
    environment: Vec<(&'static str, String)>,
}

impl<'a> Tool<'a> {
    #[must_use]
    pub(crate) const fn new(program: &'a str, arguments: &'a [String]) -> Self {
        Self {
            program,
            arguments,
            environment: Vec::new(),
        }
    }

    #[must_use]
    pub(crate) fn env(mut self, name: &'static str, value: String) -> Self {
        self.environment.push((name, value));
        self
    }
}

/// What a tool printed and whether it succeeded.
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

/// The first 1 MiB of a stream; the rest is read and dropped,
/// because a tool blocked on a full pipe would never exit.
fn capture(stream: Option<impl io::Read>) -> io::Result<String> {
    let Some(mut stream) = stream else {
        return Ok(String::new());
    };
    let bytes = crate::bounded::prefix(&mut stream, MAX_TOOL_OUTPUT)?;
    io::copy(&mut stream, &mut io::sink())?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Run a tool to completion with no input, capturing at most 1 MiB of each output stream.
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
    let reader = std::thread::spawn(move || capture(errors));
    let stdout = capture(child.stdout.take()).map_err(failed)?;
    let status = child.wait().map_err(failed)?;
    let stderr = reader
        .join()
        .map_err(|_panic| failed(io::Error::other("the output reader panicked")))?
        .map_err(failed)?;
    Ok(ToolOutput {
        success: status.success(),
        stdout,
        stderr,
    })
}

pub(crate) use os::{OutputReader, OutputStop};

/// A pipe for a job's output whose reader can be told to stop at the data already written.
pub(crate) fn output_pipe() -> io::Result<(OutputReader, io::PipeWriter, OutputStop)> {
    os::output_pipe()
}

/// Stop a process tree by ID, as the service manager would.
pub(crate) fn terminate(pid: u32) -> Result<(), ProcessError> {
    os::terminate(pid)
}

pub(crate) fn launch_worker(job: &JobId) -> Result<(), ProcessError> {
    os::launch_worker(&["worker", job.as_str()], None)
}

/// Start a chat worker whose diagnostics append to `log`.
pub(crate) fn launch_chat_worker(agent: &str, log: std::fs::File) -> Result<(), ProcessError> {
    os::launch_worker(&["chat-worker", "--agent", agent], Some(log))
}

pub(crate) fn announce_ready(token: Option<&ReadyToken>) -> Result<(), ProcessError> {
    os::announce_ready(token)
}

/// Watches a job's process group from another process, so the group stops even when its supervisor dies.
trait Guard: Sized {
    fn stand_guard(tree: &os::Tree) -> Result<Self, ProcessError>;
    /// The supervisor stopped the group itself, so the guard leaves without stopping anything.
    fn stand_down(self);
    /// The guard process: wait for the supervisor to stand down, or stop the group once it is gone.
    fn reap(group: i32);
}

/// Tells a job's output reader to finish once it has read what is already in the pipe.
pub(crate) trait Stop {
    fn stop(self) -> io::Result<()>;
}

pub(crate) fn reap(group: i32) {
    <os::Reaper as Guard>::reap(group);
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
}

impl Group {
    pub(crate) fn spawn_stdio(
        command: Command,
        output: Stdio,
        errors: Stdio,
    ) -> Result<Self, ProcessError> {
        Self::spawn_io(command, Stdio::null(), output, errors)
    }

    pub(crate) fn spawn_io(
        mut command: Command,
        input: Stdio,
        output: Stdio,
        errors: Stdio,
    ) -> Result<Self, ProcessError> {
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
        let reaper = match os::Reaper::stand_guard(&tree) {
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
        let reaper = self
            .reaper
            .lock()
            .map_err(|_poisoned| ProcessError::Poisoned)?
            .take();
        if let Some(reaper) = reaper {
            reaper.stand_down();
        }
        status
    }

    pub(crate) fn kill(&self) -> Result<(), ProcessError> {
        let guard = self
            .child
            .lock()
            .map_err(|_poisoned| ProcessError::Poisoned)?;
        match &*guard {
            ChildState::Running(_) => self.tree.kill_all(),
            ChildState::Reaped => Ok(()),
        }
    }
}

/// A command that writes `0123456789` to standard output and then `abcdefghij` to standard error.
#[cfg(all(test, unix))]
pub(crate) fn stdout_then_stderr() -> Command {
    let mut command = command("/bin/sh");
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
    use super::{MAX_TOOL_OUTPUT, Tool, run_tool};

    #[test]
    fn a_tool_that_prints_past_the_limit_keeps_its_own_outcome() {
        if cfg!(windows) {
            // The shell below is Unix; the drain it proves is the same code on Windows.
            return;
        }
        // Closing the pipe after the limit would kill the writer with SIGPIPE and fail the tool.
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
