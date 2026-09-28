#![expect(
    clippy::disallowed_methods,
    reason = "this module owns isolated worker and reaper process creation"
)]

use std::io::{self, Read, Write};
use std::os::unix::process::CommandExt as _;
use std::process::{Child, Command, Stdio};

use rustix::io::Errno;
use rustix::process::{Pid, Signal, WaitId, WaitIdOptions, kill_process_group};

use super::{ProcessError, ReadyToken};

pub(super) fn launch_worker(
    arguments: &[&str],
    errors: Option<std::fs::File>,
) -> Result<(), ProcessError> {
    let executable = std::env::current_exe().map_err(|source| ProcessError::Spawn {
        what: "the worker",
        source,
    })?;
    let mut command = Command::new(executable);
    command
        .args(arguments)
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
    let mut output = io::stdout().lock();
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
    pub(super) fn adopt(child: &Child) -> io::Result<Self> {
        let raw = i32::try_from(child.id()).map_err(io::Error::other)?;
        let group = Pid::from_raw(raw).ok_or_else(|| io::Error::other("the job has no PID"))?;
        Ok(Self { group })
    }

    pub(super) fn await_leader(&self) -> Result<(), ProcessError> {
        loop {
            match rustix::process::waitid(
                WaitId::Pid(self.group),
                WaitIdOptions::EXITED | WaitIdOptions::NOWAIT,
            ) {
                Ok(_) => return Ok(()),
                Err(Errno::INTR) => {}
                Err(error) => return Err(ProcessError::Wait(error.into())),
            }
        }
    }

    pub(super) fn kill_all(&self) -> Result<(), ProcessError> {
        match kill_process_group(self.group, Signal::KILL) {
            Ok(()) | Err(Errno::SRCH) => Ok(()),
            Err(error) => Err(ProcessError::Signal(error.into())),
        }
    }

    /// Stop what remains of the group after its leader exited but before the leader is reaped.
    ///
    /// macOS answers `EPERM` when the unreaped leader is the group's only member,
    /// so that answer means nothing is left to stop.
    pub(super) fn kill_remaining(&self) -> Result<(), ProcessError> {
        match kill_process_group(self.group, Signal::KILL) {
            Ok(()) | Err(Errno::SRCH | Errno::PERM) => Ok(()),
            Err(error) => Err(ProcessError::Signal(error.into())),
        }
    }
}

#[derive(Debug)]
pub(super) struct Reaper(std::process::ChildStdin);

impl Reaper {
    pub(super) fn stand_guard(tree: &Tree) -> Result<Self, ProcessError> {
        let executable = std::env::current_exe().map_err(|source| ProcessError::Spawn {
            what: "the reaper",
            source,
        })?;
        let mut command = Command::new(executable);
        command
            .arg("node")
            .arg("--reap")
            .arg(tree.group.as_raw_nonzero().get().to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);
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

    pub(super) fn stand_down(mut self) {
        let _sent = self.0.write_all(b"d");
    }
}

/// A job output pipe whose reader stops at the data already written once `OutputStop` fires.
///
/// A descendant that left the job's process group may keep the write end open forever,
/// so waiting for end of file alone could keep a finished job from completing.
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

impl OutputStop {
    /// Let the reader finish once it has read what is already in the pipe.
    pub(crate) fn stop(self) -> io::Result<()> {
        rustix::io::write(&self.0, b"s")?;
        Ok(())
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

pub(super) fn reap(group: i32) {
    let mut input = io::stdin().lock();
    let mut buffer = [0_u8; 256];
    loop {
        match input.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(count) if buffer.get(..count).is_some_and(|part| part.contains(&b'd')) => return,
            Ok(_) => {}
        }
    }
    if let Some(group) = Pid::from_raw(group) {
        let _stopped = kill_process_group(group, Signal::KILL);
    }
}
