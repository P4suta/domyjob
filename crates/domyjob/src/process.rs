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

fn capture(stream: Option<impl io::Read>) -> io::Result<String> {
    let Some(mut stream) = stream else {
        return Ok(String::new());
    };
    let bytes = crate::bounded::prefix(&mut stream, MAX_TOOL_OUTPUT)?;
    io::copy(&mut stream, &mut io::sink())?;
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

pub(crate) fn output_pipe() -> io::Result<(OutputReader, io::PipeWriter, OutputStop)> {
    os::output_pipe()
}

pub(crate) fn terminate(pid: u32) -> Result<(), ProcessError> {
    os::terminate(pid)
}

pub(crate) fn launch_worker(job: &JobId) -> Result<(), ProcessError> {
    os::launch_worker(&["worker", job.as_str()], None)
}

pub(crate) fn launch_chat_worker(agent: &str, log: std::fs::File) -> Result<(), ProcessError> {
    os::launch_worker(&["chat-worker", "--agent", agent], Some(log))
}

pub(crate) fn announce_ready(token: Option<&ReadyToken>) -> Result<(), ProcessError> {
    os::announce_ready(token)
}

trait Guard: Sized {
    fn stand_guard(tree: &os::Tree) -> Result<Self, ProcessError>;
    fn stand_down(self);
    fn reap(group: i32);
}

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

#[cfg(all(test, unix))]
pub(crate) fn signal_termination_fixture() -> (Command, bool) {
    let mut command = command("/bin/sh");
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
