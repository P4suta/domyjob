//! Where each piece of this user's private state lives, defined once.
//!
//! Every path under the state root comes from here,
//! so no two parts of the program can disagree about a file or give one file two purposes.
//! No path carries a version: a store records the format it was written in and refuses another.

use std::io;
use std::path::{Path, PathBuf};

use domyjob_core::chat::id::AgentId;
use domyjob_core::domain::MachineName;

use crate::platform;

/// This user's private state directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct State(PathBuf);

impl State {
    /// `DOMYJOB_STATE`, or the system's directory for per-user state.
    pub(crate) fn here() -> io::Result<Self> {
        if let Some(explicit) = platform::variable("DOMYJOB_STATE") {
            return Ok(Self(PathBuf::from(explicit)));
        }
        if cfg!(windows)
            && let Some(local) = platform::variable("LOCALAPPDATA")
        {
            return Ok(Self(PathBuf::from(local).join("domyjob").join("state")));
        }
        if let Some(xdg) = platform::variable("XDG_STATE_HOME") {
            return Ok(Self(PathBuf::from(xdg).join("domyjob")));
        }
        Ok(Self(
            platform::home()?
                .join(".local")
                .join("state")
                .join("domyjob"),
        ))
    }

    /// State rooted at `root`, for tests.
    #[cfg(test)]
    pub(crate) fn at(root: &Path) -> Self {
        Self(root.to_path_buf())
    }

    /// The job runner's store, which holds its jobs, staged submissions, and received archives.
    ///
    /// Each format has its own directory, so builds of different formats never read each other's jobs,
    /// and a running job keeps its store when a build of another format arrives.
    pub(crate) fn runner(&self) -> PathBuf {
        self.0.join(format!("runner-{}", crate::formats::runner()))
    }

    pub(crate) fn chat(&self) -> Chat {
        Chat(self.0.join("chat"))
    }

    /// The job runner stores of other formats, which older or newer builds left.
    pub(crate) fn other_runners(&self) -> io::Result<Vec<PathBuf>> {
        let current = self.runner();
        let entries = match std::fs::read_dir(&self.0) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let mut found = Vec::new();
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            if entry.file_name().to_string_lossy().starts_with("runner-")
                && entry.file_type()?.is_dir()
                && path != current
            {
                found.push(path);
            }
        }
        found.sort();
        Ok(found)
    }

    /// Which shell each SSH alias runs, detected once.
    pub(crate) fn shells(&self) -> PathBuf {
        self.0.join("hosts.json")
    }

    /// The directory of OpenSSH connection-sharing sockets.
    pub(crate) fn ssh_sockets(&self) -> PathBuf {
        self.0.join("ssh")
    }

    /// Held while this user installs a node on `machine`.
    pub(crate) fn install_lock(&self, machine: &MachineName) -> PathBuf {
        self.0
            .join("install")
            .join(format!("{}.lock", machine.as_str()))
    }
}

/// A lock that one local agent's workers share.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgentLock {
    /// Serializes worker launches with a worker's final queue check.
    Launch,
    /// Held by the one worker that runs the agent's queue.
    Queue,
}

/// The chat store's directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Chat(PathBuf);

impl Chat {
    pub(crate) fn root(&self) -> &Path {
        &self.0
    }

    pub(crate) fn database(&self) -> PathBuf {
        self.0.join("chat.redb")
    }

    /// Serializes every transaction on the database.
    pub(crate) fn lock(&self) -> PathBuf {
        self.0.join("chat.lock")
    }

    /// The doorbell's own directory, so watching it never observes database writes.
    pub(crate) fn bell(&self) -> PathBuf {
        self.0.join("bell")
    }

    pub(crate) fn agent_lock(&self, agent: &AgentId, lock: AgentLock) -> PathBuf {
        let kind = match lock {
            AgentLock::Launch => "launch",
            AgentLock::Queue => "agent",
        };
        let digest = blake3::hash(agent.to_string().as_bytes());
        self.0.join(format!("{kind}-{}.lock", digest.to_hex()))
    }

    /// Where managed workers report what failed.
    pub(crate) fn worker_log(&self) -> PathBuf {
        self.0.join("worker.log")
    }

    /// Held by the one running chat service.
    pub(crate) fn service_lock(&self) -> PathBuf {
        self.0.join("serve.lock")
    }

    /// The running chat service's process ID.
    pub(crate) fn service_pid(&self) -> PathBuf {
        self.0.join("serve.pid")
    }

    /// Where the service manager sends the chat service's output.
    pub(crate) fn service_log(&self) -> PathBuf {
        self.0.join("serve.log")
    }

    /// Which program setup installed as the chat service.
    pub(crate) fn service_record(&self) -> PathBuf {
        self.0.join("service.json")
    }
}
