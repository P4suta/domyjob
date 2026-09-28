#![expect(
    clippy::disallowed_methods,
    reason = "this module owns isolated worker and reaper process creation"
)]

use std::io::{self, Read, Write};
use std::os::unix::process::CommandExt as _;
use std::process::{Child, Command, Stdio};

use domyjob_core::domain::JobId;
use rustix::io::Errno;
use rustix::process::{Pid, Signal, WaitId, WaitIdOptions, kill_process_group};

use super::{ProcessError, ReadyToken};

pub(super) fn launch_worker(job: &JobId) -> Result<(), ProcessError> {
    let executable = std::env::current_exe().map_err(|source| ProcessError::Spawn {
        what: "the worker",
        source,
    })?;
    let mut command = Command::new(executable);
    command
        .arg("worker")
        .arg(job.as_str())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
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
