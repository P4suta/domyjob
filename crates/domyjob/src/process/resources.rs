use std::io;
use std::path::PathBuf;

use domyjob_core::domain::JobId;
use domyjob_core::resource_policy::Policy;
use domyjob_core::state::Event;

use crate::lock::OsLock;

mod raw {
    #![expect(
        clippy::disallowed_methods,
        clippy::disallowed_types,
        reason = "file-lock release has no waitable notification; retries only recheck the OS predicate and never declare readiness"
    )]

    pub(super) fn receive<T>(
        receiver: &std::sync::mpsc::Receiver<T>,
    ) -> Result<T, std::sync::mpsc::RecvTimeoutError> {
        receiver.recv_timeout(std::time::Duration::from_millis(250))
    }
}

pub(crate) fn wait_for_capacity<T>(
    receiver: &std::sync::mpsc::Receiver<T>,
) -> Result<T, std::sync::mpsc::RecvTimeoutError> {
    raw::receive(receiver)
}

#[cfg(target_os = "linux")]
mod linux;

#[derive(Debug)]
pub(crate) struct Limits {
    policy: Option<Policy>,
    directory: PathBuf,
}

#[derive(Debug)]
pub(crate) struct AdmissionPermit {
    _slot: Option<OsLock>,
    record: Option<PathBuf>,
    policy: Option<Policy>,
}

impl AdmissionPermit {
    fn stop_previous(&self) -> io::Result<()> {
        if let Some(record) = &self.record
            && let Some(previous) = crate::state_io::read_bytes(record).map_err(io::Error::other)?
        {
            let scope = Scope::parse(String::from_utf8(previous).map_err(io::Error::other)?)?;
            scope.stop()?;
            scope.cleanup()?;
        }
        Ok(())
    }
}

impl Drop for AdmissionPermit {
    fn drop(&mut self) {
        if let Err(error) = self.stop_previous() {
            eprintln!("domyjob: admitted scope cleanup failed before releasing its slot: {error}");
        }
    }
}

#[derive(Debug)]
pub(crate) struct Launch {
    pub(super) command: std::process::Command,
    pub(super) scope: Option<Scope>,
    pub(super) permit: AdmissionPermit,
}

#[cfg(test)]
impl Launch {
    pub(crate) fn fixture(command: std::process::Command) -> Self {
        Self {
            command,
            scope: None,
            permit: AdmissionPermit {
                _slot: None,
                record: None,
                policy: None,
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Scope {
    name: String,
}

impl Scope {
    pub(crate) fn parse(name: String) -> io::Result<Self> {
        let id = name
            .strip_prefix("domyjob-job-")
            .and_then(|tail| tail.strip_suffix(".scope"))
            .ok_or_else(|| io::Error::other("invalid job scope name"))?;
        JobId::try_from(id.to_owned()).map_err(io::Error::other)?;
        Ok(Self { name })
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn stop(&self) -> io::Result<()> {
        #[cfg(target_os = "linux")]
        return linux::stop(self);
        #[cfg(not(target_os = "linux"))]
        Err(io::Error::other(format!(
            "job scope {} requires Linux",
            self.name
        )))
    }

    pub(crate) fn completion(&self, event: &Event) -> io::Result<Event> {
        #[cfg(target_os = "linux")]
        return linux::completion(self, event);
        #[cfg(not(target_os = "linux"))]
        {
            let message = format!(
                "job scope {} cannot complete {event:?} on this host",
                self.name
            );
            Err(io::Error::other(message))
        }
    }

    fn marker(&self) -> io::Result<PathBuf> {
        Ok(crate::layout::admission()?.join(format!("{}.started", self.name)))
    }

    pub(crate) fn cleanup(&self) -> io::Result<()> {
        #[cfg(target_os = "linux")]
        linux::cleanup(self)?;
        crate::state_io::remove_file(&self.marker()?).map_err(io::Error::other)
    }
}

impl Limits {
    #[cfg(test)]
    pub(crate) const fn fixture(policy: Option<Policy>, directory: PathBuf) -> Self {
        Self { policy, directory }
    }

    pub(crate) fn load() -> io::Result<Self> {
        #[cfg(target_os = "linux")]
        let policy = linux::load(std::path::Path::new("/etc/domyjob/resource-policy.json"))?;
        #[cfg(not(target_os = "linux"))]
        let policy = None;
        let directory = crate::layout::admission()?;
        Ok(Self { policy, directory })
    }

    pub(crate) const fn required(&self) -> bool {
        self.policy.is_some()
    }

    pub(crate) fn try_admit(&self) -> io::Result<Option<AdmissionPermit>> {
        let Some(policy) = self.policy else {
            return Ok(Some(AdmissionPermit {
                _slot: None,
                record: None,
                policy: None,
            }));
        };
        for slot in 0..policy.budget().concurrent {
            let path = self.directory.join(format!("slot-{slot}.lock"));
            if let Some(guard) = OsLock::try_exclusive(&path).map_err(io::Error::other)? {
                let record = self.directory.join(format!("slot-{slot}.scope"));
                let permit = AdmissionPermit {
                    _slot: Some(guard),
                    record: Some(record),
                    policy: self.policy,
                };
                permit.stop_previous()?;
                return Ok(Some(permit));
            }
        }
        Ok(None)
    }

    pub(crate) fn prepare(
        &self,
        permit: AdmissionPermit,
        job: &JobId,
        command: std::process::Command,
    ) -> io::Result<Launch> {
        if permit.policy != self.policy {
            return Err(io::Error::other(
                "admission permit belongs to another resource policy",
            ));
        }
        if self.policy.is_some() && permit.record.is_none() {
            return Err(io::Error::other(
                "admission permit has no shared scope record",
            ));
        }
        match self.policy {
            None => Ok(Launch {
                command,
                scope: None,
                permit,
            }),
            Some(policy) => {
                #[cfg(target_os = "linux")]
                return linux::prepare(permit, policy, job, &command);
                #[cfg(not(target_os = "linux"))]
                {
                    let _unused = (permit, policy, job, command);
                    Err(io::Error::other("resource policy requires Linux"))
                }
            }
        }
    }
}

pub(crate) fn exec(scope: &Scope, command: &domyjob_core::domain::Command) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    return linux::exec(scope, command);
    #[cfg(not(target_os = "linux"))]
    {
        Err(io::Error::other(format!(
            "scope {} cannot execute {} on this host",
            scope.name(),
            command.program()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::Limits;
    use domyjob_core::resource_policy::Policy;

    #[test]
    fn admission_is_shared_between_clients_and_releases_on_drop() {
        let root = tempfile::tempdir().unwrap();
        let policy: Policy = domyjob_core::ingress::foreign_json(
            r#"{"version":1,"max_concurrent_jobs":2,"slice":"domyjob.slice","memory_high_bytes":67108864,"memory_max_bytes":100663296,"memory_swap_max_bytes":0}"#,
        ).unwrap();
        let first = Limits {
            policy: Some(policy),
            directory: root.path().join("admission"),
        };
        let other = Limits {
            policy: Some(policy),
            directory: root.path().join("admission"),
        };
        let one = first.try_admit().unwrap().unwrap();
        let two = other.try_admit().unwrap().unwrap();
        assert!(first.try_admit().unwrap().is_none());
        drop(one);
        let replacement = other.try_admit().unwrap().unwrap();
        assert!(other.try_admit().unwrap().is_none());
        drop((two, replacement));
        assert!(first.try_admit().unwrap().is_some());
    }
}
