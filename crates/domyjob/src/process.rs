#![expect(
    clippy::redundant_pub_crate,
    reason = "the binary composition root uses this private module"
)]

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
    #[cfg_attr(unix, expect(dead_code, reason = "Windows creates readiness tokens"))]
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

pub(crate) fn launch_worker(job: &JobId) -> Result<(), ProcessError> {
    os::launch_worker(&["worker", job.as_str()])
}

fn launch_chat_worker(agent: &str) -> Result<(), ProcessError> {
    os::launch_worker(&["chat-worker", "--agent", agent])
}

pub(crate) fn announce_ready(token: Option<&ReadyToken>) -> Result<(), ProcessError> {
    os::announce_ready(token)
}

#[cfg_attr(
    windows,
    expect(
        clippy::missing_const_for_fn,
        reason = "the shared Unix and Windows process interface is non-const"
    )
)]
pub(crate) fn reap(group: i32) {
    os::reap(group);
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
