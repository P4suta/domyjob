use std::io;
use std::path::{Path, PathBuf};

use domyjob_core::domain::MachineName;

use crate::platform;

pub(crate) fn admission() -> io::Result<PathBuf> {
    if cfg!(target_os = "linux") {
        let user = platform::user_id().ok_or_else(|| io::Error::other("user UID unavailable"))?;
        return Ok(Path::new("/run/user")
            .join(user.to_string())
            .join("domyjob/admission"));
    }
    Ok(platform::home()?.join(".local/state/domyjob/admission"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct State(PathBuf);

impl State {
    pub(crate) fn root(&self) -> &Path {
        &self.0
    }

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
        self.root()
            .join(format!("runner-{}", crate::formats::runner()))
    }

    pub(crate) fn service(&self) -> Service {
        Service(self.0.join("chat"))
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Service(PathBuf);

impl Service {
    pub(crate) fn root(&self) -> &Path {
        &self.0
    }

    pub(crate) fn service_lock(&self) -> PathBuf {
        self.0.join("serve.lock")
    }

    pub(crate) fn service_log(&self) -> PathBuf {
        self.0.join("serve.log")
    }

    pub(crate) fn service_record(&self) -> PathBuf {
        self.0.join("service.json")
    }
}
