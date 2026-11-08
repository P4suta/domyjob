use std::io::{self, Read, Write};
use std::os::unix::process::CommandExt as _;
use std::process::{Child, Command, Stdio};

use rustix::io::Errno;
use rustix::process::{Pid, Signal, WaitId, WaitIdOptions, kill_process_group};

use super::{Guard, ProcessError, ReadyToken, Stop};

const PROCESS_LIST: &str = "/bin/ps";
const QUIESCENCE_TIMEOUT_MILLIS: u64 = 5000;
const QUIESCENCE_POLL_MILLIS: u64 = 10;
const MAX_INVENTORY_ROW_CHARS: usize = 256;
const MAX_INVENTORY_STDERR_CHARS: usize = 512;

#[derive(Debug, thiserror::Error)]
#[error("{cause}; inventory {bytes} bytes; rejected row: {row:?}; stderr: {stderr:?}")]
struct InventoryDiagnostic {
    #[source]
    cause: domyjob_core::process_inventory::InvalidInventory,
    bytes: usize,
    row: String,
    stderr: String,
}

fn group_quiescent(group: i32) -> io::Result<bool> {
    group_quiescent_using(
        group,
        super::Tool::new(PROCESS_LIST, &["-axo".into(), "pid=,pgid=,stat=".into()]),
    )
}

fn group_quiescent_using(group: i32, tool: super::Tool<'_>) -> io::Result<bool> {
    let group = domyjob_core::process_group::ProcessGroup::new(group).map_err(io::Error::other)?;
    let output =
        super::run_tool(&tool.output(domyjob_core::capture_policy::CapturePolicy::Complete))
            .map_err(io::Error::other)?;
    if !output.success {
        return Err(io::Error::other(output.stderr));
    }
    domyjob_core::process_inventory::group_quiescent(&output.stdout, group)
        .map_err(|cause| inventory_error(cause, &output))
}

fn inventory_error(
    cause: domyjob_core::process_inventory::InvalidInventory,
    output: &super::ToolOutput,
) -> io::Error {
    let row = match cause {
        domyjob_core::process_inventory::InvalidInventory::Malformed { line } => {
            match output.stdout.lines().nth(line.saturating_sub(1)) {
                Some(text) => diagnostic_prefix(text, MAX_INVENTORY_ROW_CHARS),
                None => "unavailable".to_owned(),
            }
        }
        domyjob_core::process_inventory::InvalidInventory::Empty
        | domyjob_core::process_inventory::InvalidInventory::TooLarge { .. } => {
            "not applicable".to_owned()
        }
    };
    io::Error::new(
        io::ErrorKind::InvalidData,
        InventoryDiagnostic {
            cause,
            bytes: output.stdout.len(),
            row,
            stderr: diagnostic_prefix(&output.stderr, MAX_INVENTORY_STDERR_CHARS),
        },
    )
}

fn diagnostic_prefix(text: &str, limit: usize) -> String {
    let mut prefix: String = text.chars().take(limit).collect();
    if prefix.len() < text.len() {
        prefix.push_str(" [later diagnostic bytes omitted]");
    }
    prefix
}

fn await_group_quiescence(group: i32) -> io::Result<()> {
    let deadline = crate::platform::clock::Deadline::after_millis(QUIESCENCE_TIMEOUT_MILLIS);
    loop {
        if group_quiescent(group)? {
            return Ok(());
        }
        if deadline.expired() {
            return Err(io::Error::other("the process group still has live members"));
        }
        crate::platform::clock::pause_millis(QUIESCENCE_POLL_MILLIS);
    }
}

#[cfg(not(target_os = "macos"))]
pub(super) fn launch_worker(
    arguments: &[&str],
    errors: Option<std::fs::File>,
) -> Result<(), ProcessError> {
    let executable = std::env::current_exe().map_err(|source| ProcessError::Spawn {
        what: "the worker",
        source,
    })?;
    let mut command = super::command(executable);
    command.args(arguments);
    launch_worker_command(command, errors)
}

pub(super) fn launch_worker_command(
    mut command: Command,
    errors: Option<std::fs::File>,
) -> Result<(), ProcessError> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(errors.map_or_else(Stdio::null, Stdio::from))
        .process_group(0);
    let mut child = command.spawn().map_err(|source| ProcessError::Spawn {
        what: "the worker",
        source,
    })?;
    drop(command);
    let mut signal = [0_u8; 1];
    let read = match child.stdout.take() {
        Some(mut output) => output.read(&mut signal).map_err(ProcessError::Ready)?,
        None => 0,
    };
    std::thread::spawn(move || {
        let _reaped = child.wait();
    });
    if read == 1 && signal == *b"R" {
        Ok(())
    } else {
        Err(ProcessError::NotStarted("no readiness signal".to_owned()))
    }
}

pub(super) fn announce_ready(token: Option<&ReadyToken>) -> Result<(), ProcessError> {
    if token.is_some() {
        return Err(ProcessError::InvalidReadyToken);
    }
    let mut output = super::raw::stdout().lock();
    output
        .write_all(b"R")
        .and_then(|()| output.flush())
        .map_err(ProcessError::Ready)
}

pub(super) fn isolate(command: &mut Command) {
    command.process_group(0);
}

#[derive(Debug)]
pub(super) struct Tree {
    group: Pid,
}

impl Tree {
    pub(super) fn confirm_quiescence(&self) -> io::Result<()> {
        await_group_quiescence(self.group.as_raw_nonzero().get())
    }
    pub(super) fn adopt(child: &Child, _marker: Option<&std::path::Path>) -> io::Result<Self> {
        let raw = i32::try_from(child.id()).map_err(io::Error::other)?;
        let group = Pid::from_raw(raw).ok_or_else(|| io::Error::other("the job has no PID"))?;
        Ok(Self { group })
    }

    pub(super) fn await_leader(&self) -> Result<(), ProcessError> {
        await_leader_using(|| {
            rustix::process::waitid(
                WaitId::Pid(self.group),
                WaitIdOptions::EXITED | WaitIdOptions::NOWAIT,
            )
        })
    }

    pub(super) fn kill_all(&self) -> Result<(), ProcessError> {
        match kill_process_group(self.group, Signal::KILL) {
            Ok(()) | Err(Errno::SRCH) => Ok(()),
            Err(error) => Err(ProcessError::Signal(error.into())),
        }
    }

    pub(super) fn kill_remaining(&self) -> Result<(), ProcessError> {
        match kill_process_group(self.group, Signal::KILL) {
            Ok(()) | Err(Errno::SRCH | Errno::PERM) => Ok(()),
            Err(error) => Err(ProcessError::Signal(error.into())),
        }
    }
}

fn await_leader_using(
    mut wait: impl FnMut() -> rustix::io::Result<Option<rustix::process::WaitIdStatus>>,
) -> Result<(), ProcessError> {
    loop {
        match wait() {
            Ok(Some(status)) if status.exited() || status.killed() || status.dumped() => {
                return Ok(());
            }
            Ok(_) | Err(Errno::INTR) => {}
            Err(error) => return Err(ProcessError::Wait(error.into())),
        }
    }
}

#[derive(Debug)]
pub(super) struct Reaper(std::process::ChildStdin);

impl Guard for Reaper {
    fn stand_guard(
        tree: &Tree,
        scope: Option<&super::resources::Scope>,
        marker: Option<&std::path::Path>,
    ) -> Result<Self, ProcessError> {
        let executable = std::env::current_exe().map_err(|source| ProcessError::Spawn {
            what: "the reaper",
            source,
        })?;
        let mut command = super::command(executable);
        command
            .arg("node")
            .arg("--reap")
            .arg(tree.group.as_raw_nonzero().get().to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);
        if let Some(scope) = scope {
            command.arg("--reap-scope").arg(scope.name());
        }
        if let Some(marker) = marker {
            command.arg("--reap-marker").arg(marker);
        }
        let mut child = command.spawn().map_err(|source| ProcessError::Spawn {
            what: "the reaper",
            source,
        })?;
        let input = child.stdin.take().ok_or_else(|| ProcessError::Spawn {
            what: "the reaper",
            source: io::Error::other("the reaper has no input"),
        })?;
        std::thread::spawn(move || {
            let _reaped = child.wait();
        });
        Ok(Self(input))
    }

    fn stand_down(mut self) {
        let _sent = self.0.write_all(b"d");
    }

    fn reap(
        group: i32,
        scope: Option<&super::resources::Scope>,
        marker: Option<&std::path::Path>,
    ) -> io::Result<()> {
        let mut input = io::stdin().lock();
        let mut buffer = [0_u8; 256];
        loop {
            match input.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(count) if buffer.get(..count).is_some_and(|part| part.contains(&b'd')) => {
                    return Ok(());
                }
                Ok(_) => {}
            }
        }
        if let Some(group) = Pid::from_raw(group) {
            let _stopped = kill_process_group(group, Signal::KILL);
        }
        if let Some(scope) = scope {
            scope.stop().and_then(|()| scope.cleanup())?;
        } else {
            await_group_quiescence(group)?;
        }
        if let Some(marker) = marker {
            crate::state_io::mark_existing(marker).map_err(io::Error::other)?;
        }
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct OutputReader {
    pipe: io::PipeReader,
    stop: rustix::fd::OwnedFd,
    stopped: bool,
}

#[derive(Debug)]
pub(crate) struct OutputStop(rustix::fd::OwnedFd);

pub(super) fn output_pipe() -> io::Result<(OutputReader, io::PipeWriter, OutputStop)> {
    let (pipe, writer) = io::pipe()?;
    let (stop, signal) = rustix::pipe::pipe()?;
    Ok((
        OutputReader {
            pipe,
            stop,
            stopped: false,
        },
        writer,
        OutputStop(signal),
    ))
}

impl Stop for OutputStop {
    fn stop(self) -> io::Result<()> {
        match rustix::io::write(&self.0, b"s") {
            Ok(_) | Err(Errno::PIPE) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

impl Read for OutputReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        use rustix::event::{PollFd, PollFlags, poll};

        loop {
            if self.stopped {
                return match self.pipe.read(buffer) {
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(0),
                    other => other,
                };
            }
            let mut fds = [
                PollFd::new(&self.pipe, PollFlags::IN),
                PollFd::new(&self.stop, PollFlags::IN),
            ];
            match poll(&mut fds, None) {
                Ok(_) | Err(Errno::INTR) => {}
                Err(error) => return Err(error.into()),
            }
            let [output, stop] = &fds;
            if !output.revents().is_empty() {
                return self.pipe.read(buffer);
            }
            if !stop.revents().is_empty() {
                rustix::io::ioctl_fionbio(&self.pipe, true)?;
                self.stopped = true;
            }
        }
    }
}

pub(super) fn terminate(id: u32) -> Result<(), ProcessError> {
    let raw = i32::try_from(id).map_err(|error| ProcessError::Signal(io::Error::other(error)))?;
    let Some(pid) = Pid::from_raw(raw) else {
        return Ok(());
    };
    match rustix::process::kill_process(pid, Signal::TERM) {
        Ok(()) | Err(Errno::SRCH) => Ok(()),
        Err(error) => Err(ProcessError::Signal(error.into())),
    }
}

#[cfg(test)]
#[path = "../../tests/support/process_quiescence.rs"]
mod tests;
