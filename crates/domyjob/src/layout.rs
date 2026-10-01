use std::io;
use std::path::{Path, PathBuf};

use domyjob_core::chat::id::AgentId;
use domyjob_core::domain::MachineName;

use crate::platform;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct State(PathBuf);

impl State {
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

    #[cfg(test)]
    pub(crate) fn at(root: &Path) -> Self {
        Self(root.to_path_buf())
    }

    pub(crate) fn runner(&self) -> PathBuf {
        self.0.join(format!("runner-{}", crate::formats::runner()))
    }

    pub(crate) fn chat(&self) -> Chat {
        Chat(self.0.join("chat"))
    }

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

    pub(crate) fn runner_order(&self) -> PathBuf {
        self.0.join("runner-order")
    }

    pub(crate) fn runner_order_lock(&self) -> PathBuf {
        self.0.join("runner-order.lock")
    }

    pub(crate) fn shells(&self) -> PathBuf {
        self.0.join("hosts.json")
    }

    pub(crate) fn ssh_sockets(&self) -> PathBuf {
        self.0.join("ssh")
    }

    pub(crate) fn install_lock(&self, machine: &MachineName) -> PathBuf {
        self.0
            .join("install")
            .join(format!("{}.lock", machine.as_str()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgentLock {
    Launch,
    Queue,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Chat(PathBuf);

impl Chat {
    pub(crate) fn root(&self) -> &Path {
        &self.0
    }

    pub(crate) fn database(&self) -> PathBuf {
        self.0.join("chat.redb")
    }

    pub(crate) fn lock(&self) -> PathBuf {
        self.0.join("chat.lock")
    }

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

    pub(crate) fn worker_log(&self) -> PathBuf {
        self.0.join("worker.log")
    }

    pub(crate) fn service_lock(&self) -> PathBuf {
        self.0.join("serve.lock")
    }

    pub(crate) fn service_pid(&self) -> PathBuf {
        self.0.join("serve.pid")
    }

    pub(crate) fn service_log(&self) -> PathBuf {
        self.0.join("serve.log")
    }

    pub(crate) fn service_record(&self) -> PathBuf {
        self.0.join("service.json")
    }
}
