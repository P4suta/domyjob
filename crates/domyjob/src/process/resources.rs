use std::io;
use std::path::PathBuf;

use domyjob_core::domain::JobId;
use domyjob_core::resource_policy::Policy;
use domyjob_core::state::Event;
use domyjob_core::wire::BuildId;

use crate::lock::OsLock;

mod raw {
    #![expect(
        clippy::disallowed_methods,
        clippy::disallowed_types,
        reason = "file-lock release has no waitable notification; retries only recheck the OS predicate and never declare readiness"
    )]

    const CAPACITY_RECHECK_MILLIS: u64 = 250;

    pub(super) fn receive<T>(
        receiver: &std::sync::mpsc::Receiver<T>,
    ) -> Result<T, std::sync::mpsc::RecvTimeoutError> {
        receiver.recv_timeout(std::time::Duration::from_millis(CAPACITY_RECHECK_MILLIS))
    }
}

pub(crate) fn wait_for_capacity<T>(
    receiver: &std::sync::mpsc::Receiver<T>,
) -> Result<T, std::sync::mpsc::RecvTimeoutError> {
    raw::receive(receiver)
}

#[cfg(target_os = "linux")]
mod linux;
mod ownership;
use ownership::SlotRecord;
pub(crate) use ownership::{SavedScope, ScopeNamespace};

#[derive(Debug)]
pub(crate) struct Limits {
    policy: Option<Policy>,
    directory: PathBuf,
    namespace: Option<ScopeNamespace>,
}

#[derive(Debug)]
pub(crate) struct AdmissionPermit {
    _slot: Option<OsLock>,
    record: Option<SlotRecord>,
    owned_scope: Option<Scope>,
    policy: Option<Policy>,
    namespace: Option<ScopeNamespace>,
}

impl AdmissionPermit {
    fn record_scope(&mut self, job: &JobId, build: BuildId, scope: &Scope) -> io::Result<()> {
        let namespace = self
            .namespace
            .ok_or_else(|| io::Error::other("scope has no store namespace"))?;
        let saved = SavedScope::new((namespace, job), build, scope)?;
        if let Some(record) = &self.record {
            record.write(&saved)?;
        }
        self.owned_scope = Some(scope.clone());
        Ok(())
    }
}

impl Drop for AdmissionPermit {
    fn drop(&mut self) {
        if let Some(scope) = &self.owned_scope {
            let stopped = scope.stop().and_then(|()| scope.cleanup());
            let cleared = stopped.and_then(|()| match &self.record {
                Some(record) => record.clear_owned(scope),
                None => Ok(()),
            });
            if let Err(error) = cleared {
                eprintln!(
                    "domyjob: admitted scope cleanup failed before releasing its slot: {error}"
                );
            }
        }
    }
}

#[derive(Debug)]
pub(crate) struct Launch {
    pub(super) command: std::process::Command,
    pub(super) scope: Option<Scope>,
    pub(super) permit: AdmissionPermit,
    pub(super) quiescence: Option<PathBuf>,
}

impl Launch {
    pub(crate) fn with_quiescence_marker(mut self, marker: PathBuf) -> Self {
        self.quiescence = Some(marker);
        self
    }

    pub(crate) const fn scope(&self) -> Option<&Scope> {
        self.scope.as_ref()
    }
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
                owned_scope: None,
                policy: None,
                namespace: None,
            },
            quiescence: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Scope {
    name: String,
}

impl Scope {
    pub(crate) fn for_job((namespace, job): (ScopeNamespace, &JobId), build: BuildId) -> Self {
        const SCOPE_DIGEST_HEX_CHARS: usize = 32;
        let mut identity = blake3::Hasher::new();
        identity.update(&namespace.bytes());
        identity.update(&build.fingerprint().to_le_bytes());
        identity.update(job.as_str().as_bytes());
        let name: String = identity
            .finalize()
            .to_hex()
            .chars()
            .take(SCOPE_DIGEST_HEX_CHARS)
            .collect();
        Self {
            name: format!("domyjob-job-{name}.scope"),
        }
    }
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

    fn obsolete(&self) -> io::Result<bool> {
        #[cfg(target_os = "linux")]
        return linux::obsolete(self);
        #[cfg(not(target_os = "linux"))]
        Err(io::Error::other(format!(
            "legacy scope {} reconciliation requires Linux",
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
        Self {
            policy,
            directory,
            namespace: None,
        }
    }

    pub(crate) fn load() -> io::Result<Self> {
        #[cfg(target_os = "linux")]
        let policy = {
            const RESOURCE_POLICY: &str = "/etc/domyjob/resource-policy.json";
            linux::load(std::path::Path::new(RESOURCE_POLICY))?
        };
        #[cfg(not(target_os = "linux"))]
        let policy = None;
        let directory = crate::layout::admission()?;
        Ok(Self {
            policy,
            directory,
            namespace: None,
        })
    }

    pub(crate) const fn with_namespace(mut self, namespace: ScopeNamespace) -> Self {
        self.namespace = Some(namespace);
        self
    }

    pub(crate) const fn required(&self) -> bool {
        self.policy.is_some()
    }

    pub(crate) fn try_admit(&self) -> io::Result<Option<AdmissionPermit>> {
        let Some(policy) = self.policy else {
            return Ok(Some(AdmissionPermit {
                _slot: None,
                record: None,
                owned_scope: None,
                policy: None,
                namespace: self.namespace,
            }));
        };
        let namespace = self
            .namespace
            .ok_or_else(|| io::Error::other("resource admission has no store namespace"))?;
        for slot in 0..policy.budget().concurrent {
            let path = self.directory.join(format!("slot-{slot}.lock"));
            if let Some(guard) = OsLock::try_exclusive(&path).map_err(io::Error::other)? {
                let record = SlotRecord::new(
                    self.directory.join(format!("slot-{slot}.scope")),
                    slot,
                    namespace,
                );
                if !record.clear_previous()? {
                    drop(guard);
                    continue;
                }
                let permit = AdmissionPermit {
                    _slot: Some(guard),
                    record: Some(record),
                    owned_scope: None,
                    policy: self.policy,
                    namespace: self.namespace,
                };
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
        if permit.policy != self.policy || permit.namespace != self.namespace {
            return Err(io::Error::other(
                "admission permit belongs to another resource policy or store",
            ));
        }
        if self.policy.is_some() && permit.record.is_none() {
            return Err(io::Error::other(
                "admission permit has no shared scope record",
            ));
        }
        let mut launch = match self.policy {
            None => Launch {
                command,
                scope: None,
                permit,
                quiescence: None,
            },
            Some(policy) => {
                #[cfg(target_os = "linux")]
                {
                    linux::prepare(permit, policy, job, &command)?
                }
                #[cfg(not(target_os = "linux"))]
                {
                    let _unused = (permit, policy, job, command);
                    return Err(io::Error::other("resource policy requires Linux"));
                }
            }
        };
        if let Some(scope) = &launch.scope {
            launch
                .permit
                .record_scope(job, crate::identity::current(), scope)?;
        }
        Ok(launch)
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
    fn a_resource_permit_cannot_cross_the_store_namespace() {
        let root = tempfile::tempdir().unwrap();
        let stores = [root.path().join("first"), root.path().join("second")];
        for store in &stores {
            crate::state_io::private_dir(store).unwrap();
        }
        let first = Limits::fixture(None, root.path().join("admission"))
            .with_namespace(super::ScopeNamespace::for_store(&stores[0]).unwrap());
        let second = Limits::fixture(None, root.path().join("admission"))
            .with_namespace(super::ScopeNamespace::for_store(&stores[1]).unwrap());
        let foreign_permit = first.try_admit().unwrap().unwrap();
        let job = domyjob_core::domain::JobId::try_from("a".repeat(32)).unwrap();
        second
            .prepare(
                foreign_permit,
                &job,
                crate::process::command("never-launched"),
            )
            .unwrap_err();
        let own_permit = second.try_admit().unwrap().unwrap();
        second
            .prepare(own_permit, &job, crate::process::command("never-launched"))
            .unwrap();
    }

    #[test]
    fn admission_is_shared_between_clients_and_releases_on_drop() {
        let root = tempfile::tempdir().unwrap();
        let policy: Policy = domyjob_core::ingress::foreign_json(
            r#"{"version":1,"max_concurrent_jobs":2,"slice":"domyjob.slice","memory_high_bytes":67108864,"memory_max_bytes":100663296,"memory_swap_max_bytes":0}"#,
        ).unwrap();
        let store = root.path().join("store");
        crate::state_io::private_dir(&store).unwrap();
        let first = Limits {
            policy: Some(policy),
            directory: root.path().join("admission"),
            namespace: Some(super::ScopeNamespace::for_store(&store).unwrap()),
        };
        let other = Limits {
            policy: Some(policy),
            directory: root.path().join("admission"),
            namespace: first.namespace,
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
