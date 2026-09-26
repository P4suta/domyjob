use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::domain::{EnvName, Invalid, JobId, JobRef};
use crate::lock::{LockError, OsLock};
use crate::paths::Dirs;
use crate::protocol::{Job, Phase, Spec, Supervisor};

const SCHEMA: &str = "v3";

const OUTCOME_RESERVED: usize = 4096;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("{action} {path}: {source}")]
    Io {
        action: &'static str,
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{path} holds malformed JSON: {source}")]
    Json {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("no job matches {0}")]
    NoSuchJob(JobRef),
    #[error("{reference} is ambiguous: {count} jobs match")]
    Ambiguous { reference: JobRef, count: usize },
    #[error(transparent)]
    State(#[from] crate::state_file::StateError),
    #[error(transparent)]
    Lock(#[from] LockError),
    #[error("job {0} already exists")]
    Exists(JobId),
    #[error(transparent)]
    Invalid(#[from] Invalid),
}

fn io(action: &'static str, path: &Path) -> impl FnOnce(std::io::Error) -> StoreError + use<> {
    let path = path.to_path_buf();
    move |source| StoreError::Io {
        action,
        path,
        source,
    }
}

fn read_json<T: crate::ingress::Ingress>(path: &Path) -> Result<T, StoreError> {
    crate::state_file::read_json(path)?
        .ok_or_else(|| io("reading", path)(ErrorKind::NotFound.into()))
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchEnv {
    pub vars: BTreeMap<String, String>,
    pub not_unicode: Vec<String>,
}

impl LaunchEnv {
    #[must_use]
    pub fn with_agent(mut self, agent: Option<PathBuf>) -> Self {
        if let Some(agent) = agent {
            self.vars
                .insert("SSH_AUTH_SOCK".to_owned(), agent.display().to_string());
        }
        self
    }

    #[must_use]
    pub fn of_this_process() -> Self {
        let mut launch = Self::default();
        for (key, value) in std::env::vars_os() {
            match (key.into_string(), value.into_string()) {
                (Ok(key), Ok(value)) => {
                    launch.vars.insert(key, value);
                }
                (Ok(key), Err(_)) => launch.not_unicode.push(key),
                (Err(key), Ok(_) | Err(_)) => {
                    launch.not_unicode.push(key.to_string_lossy().into_owned());
                }
            }
        }
        launch
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Sequence {
    last: u64,
}

#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn open(dirs: &Dirs) -> Result<Self, StoreError> {
        let root = dirs.state.join(SCHEMA);
        for dir in [
            root.clone(),
            root.join("jobs"),
            root.join("staging"),
            root.join("live"),
        ] {
            crate::state_file::private_dir(&dir)?;
        }
        Ok(Self { root })
    }

    #[must_use]
    pub fn area(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    #[must_use]
    pub fn job_dir(&self, id: &JobId) -> PathBuf {
        self.root.join("jobs").join(id.as_str())
    }

    fn staged_dir(&self, id: &JobId) -> PathBuf {
        self.root.join("staging").join(id.as_str())
    }

    #[must_use]
    pub fn log_path(&self, id: &JobId) -> PathBuf {
        self.job_dir(id).join("log")
    }

    #[must_use]
    pub fn workspace_record(&self, id: &JobId) -> PathBuf {
        self.job_dir(id).join("workspace")
    }

    #[must_use]
    pub fn alive_path(&self, id: &JobId) -> PathBuf {
        self.root.join("live").join(format!("{id}.lock"))
    }

    #[must_use]
    pub fn control_path(&self, id: &JobId) -> PathBuf {
        self.root.join("live").join(format!("{id}.sock"))
    }

    pub fn next_sequence(&self) -> Result<u64, StoreError> {
        Ok(crate::state_file::update_json(
            &self.root.join("sequence.json"),
            Sequence::default,
            |sequence| {
                sequence.last = sequence.last.saturating_add(1);
                sequence.last
            },
        )?)
    }

    pub fn stage(
        &self,
        spec: &Spec,
        (env, launch): (&BTreeMap<EnvName, String>, &LaunchEnv),
    ) -> Result<(), StoreError> {
        let dir = self.staged_dir(&spec.id);
        for taken in [&dir, &self.job_dir(&spec.id)] {
            crate::faults::at("store::check", taken).map_err(io("checking", taken))?;
            match std::fs::symlink_metadata(taken) {
                Ok(_) => return Err(StoreError::Exists(spec.id.clone())),
                Err(e) if e.kind() == ErrorKind::NotFound => {}
                Err(e) => return Err(io("checking", taken)(e)),
            }
        }
        crate::state_file::private_dir(&dir)?;
        crate::state_file::write_json(&dir.join("spec.json"), spec)?;
        crate::state_file::write_json(&dir.join("env.json"), env)?;
        crate::state_file::write_json(&dir.join("launch-env.json"), launch)?;
        crate::state_file::write_json(&dir.join("phase.json"), &Phase::Queued)?;
        crate::state_file::create_empty(&dir.join("log"))?;
        crate::state_file::write_bytes(&dir.join("outcome"), &[b' '; OUTCOME_RESERVED])?;
        Ok(())
    }

    pub fn skip_the_queue(&self, id: &JobId) -> Result<(), StoreError> {
        Ok(crate::state_file::write_bytes(
            &self.staged_dir(id).join("now"),
            b"now",
        )?)
    }

    pub fn skips_the_queue(&self, id: &JobId) -> Result<bool, StoreError> {
        Ok(crate::state_file::read_bytes(&self.job_dir(id).join("now"))?.is_some())
    }

    pub fn publish(&self, id: &JobId) -> Result<(), StoreError> {
        Ok(crate::state_file::publish_dir(
            &self.staged_dir(id),
            &self.job_dir(id),
        )?)
    }

    pub fn is_published(&self, id: &JobId) -> Result<bool, StoreError> {
        Ok(crate::state_file::read_bytes(&self.job_dir(id).join("spec.json"))?.is_some())
    }

    fn failure_path(&self, id: &JobId) -> Result<PathBuf, StoreError> {
        Ok(if self.is_published(id)? {
            self.job_dir(id).join("failure")
        } else {
            self.staged_dir(id).join("failure")
        })
    }

    pub fn record_start_failure(&self, id: &JobId, why: &str) -> Result<(), StoreError> {
        Ok(crate::state_file::write_bytes(
            &self.failure_path(id)?,
            why.as_bytes(),
        )?)
    }

    pub fn start_failure(&self, id: &JobId) -> Result<Option<String>, StoreError> {
        Ok(crate::state_file::read_bytes(&self.failure_path(id)?)?
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned()))
    }

    #[must_use]
    pub fn staging_lock_path(&self, id: &JobId) -> PathBuf {
        self.root.join("staging").join(format!("{id}.lock"))
    }

    pub fn staged_ids(&self) -> Result<Vec<JobId>, StoreError> {
        let dir = self.root.join("staging");
        crate::faults::at("store::list", &dir).map_err(io("listing", &dir))?;
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(io("listing", &dir)(e)),
        };
        let mut ids = Vec::new();
        for entry in entries {
            let entry = entry.map_err(io("listing", &dir))?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            match name.parse::<JobId>() {
                Ok(id) => ids.push(id),
                Err(_lock_or_foreign) => {}
            }
        }
        Ok(ids)
    }

    #[must_use]
    pub fn nonce_path(&self, scope: &str, nonce: &crate::domain::Nonce) -> PathBuf {
        self.root.join("nonces").join(format!("{scope}-{nonce}"))
    }

    pub fn staged_spec(&self, id: &JobId) -> Result<Spec, StoreError> {
        read_json(&self.staged_dir(id).join("spec.json"))
    }

    #[must_use]
    pub fn collection_lock_path(&self) -> PathBuf {
        self.root.join("collecting.lock")
    }

    pub fn remove_job(&self, id: &JobId) -> Result<(), StoreError> {
        crate::state_file::remove_dir_all(&self.job_dir(id))?;
        crate::state_file::remove_file(&self.control_path(id))?;
        Ok(crate::state_file::remove_file(&self.alive_path(id))?)
    }

    pub fn forget_staging_lock(&self, id: &JobId) -> Result<(), StoreError> {
        Ok(crate::state_file::remove_file(&self.staging_lock_path(id))?)
    }

    pub fn discard_staged(&self, id: &JobId) -> Result<(), StoreError> {
        Ok(crate::state_file::remove_dir_all(&self.staged_dir(id))?)
    }

    pub fn spec(&self, id: &JobId) -> Result<Spec, StoreError> {
        read_json(&self.job_dir(id).join("spec.json"))
    }

    pub fn launch_env(&self, id: &JobId) -> Result<LaunchEnv, StoreError> {
        read_json(&self.job_dir(id).join("launch-env.json"))
    }

    pub fn forget_launch_env(&self, id: &JobId) -> Result<(), StoreError> {
        Ok(crate::state_file::remove_file(
            &self.job_dir(id).join("launch-env.json"),
        )?)
    }

    pub fn env(&self, id: &JobId) -> Result<BTreeMap<EnvName, String>, StoreError> {
        read_json(&self.job_dir(id).join("env.json"))
    }

    pub fn set_phase(&self, id: &JobId, phase: &Phase) -> Result<(), StoreError> {
        Ok(crate::state_file::write_json(
            &self.job_dir(id).join("phase.json"),
            phase,
        )?)
    }

    pub fn phase(&self, id: &JobId) -> Result<Phase, StoreError> {
        let phase: Phase = read_json(&self.job_dir(id).join("phase.json"))?;
        if matches!(phase, Phase::Finished { .. }) {
            return Ok(phase);
        }
        let reserved = crate::state_file::read_bytes(&self.job_dir(id).join("outcome"))?;
        let written = reserved.as_deref().map_or(&[][..], <[u8]>::trim_ascii);
        if written.is_empty() {
            return Ok(phase);
        }
        match crate::ingress::json::<Phase>(written) {
            Ok(finished @ Phase::Finished { .. }) => Ok(finished),
            Ok(_) | Err(_) => Ok(phase),
        }
    }

    pub fn record_outcome_in_place(&self, id: &JobId, phase: &Phase) -> Result<(), StoreError> {
        let mut bytes = serde_json::to_vec(phase).map_err(|e| {
            StoreError::from(crate::state_file::StateError::Io {
                action: "encoding the outcome of",
                path: self.job_dir(id),
                source: std::io::Error::other(e),
            })
        })?;
        if bytes.len() > OUTCOME_RESERVED {
            return Err(StoreError::from(crate::state_file::StateError::Io {
                action: "fitting the outcome into the space reserved for it in",
                path: self.job_dir(id),
                source: std::io::Error::other("the outcome is longer than its reserved space"),
            }));
        }
        bytes.resize(OUTCOME_RESERVED, b' ');
        Ok(crate::state_file::overwrite_in_place(
            &self.job_dir(id).join("outcome"),
            &bytes,
        )?)
    }

    fn supervisor(&self, id: &JobId) -> Result<Supervisor, StoreError> {
        match OsLock::probe(&self.alive_path(id))? {
            crate::lock::Probe::Held => Ok(Supervisor::Alive),
            crate::lock::Probe::Absent | crate::lock::Probe::Free => Ok(Supervisor::Gone),
        }
    }

    pub fn job(&self, id: &JobId) -> Result<Job, StoreError> {
        let supervisor = self.supervisor(id)?;
        let spec = self.spec(id)?;
        let phase = self.phase(id)?;
        let behind = match (&phase, supervisor) {
            (Phase::Queued, Supervisor::Alive) => self.slot_holders(&spec),
            _ => Vec::new(),
        };
        Ok(Job {
            spec,
            phase,
            supervisor,
            behind,
        })
    }

    #[must_use]
    pub fn slot_holder_path(slot_lock: &Path) -> PathBuf {
        slot_lock.with_extension("holder")
    }

    fn slot_holders(&self, waiting: &Spec) -> Vec<JobId> {
        let slots = self.area("slots");
        let Ok(entries) = std::fs::read_dir(&slots) else {
            return Vec::new();
        };
        let mut names: Vec<std::ffi::OsString> =
            entries.flatten().map(|entry| entry.file_name()).collect();
        names.sort();
        let mut holders = Vec::new();
        for name in names {
            let Some(index) = name.to_str().and_then(|name| name.strip_suffix(".holder")) else {
                continue;
            };
            let Ok(index) = index.parse::<usize>() else {
                continue;
            };
            if index >= waiting.concurrency.slots() {
                continue;
            }
            let Ok(Some(bytes)) = crate::state_file::read_bytes(&slots.join(&name)) else {
                continue;
            };
            let Ok(holder) = String::from_utf8_lossy(bytes.trim_ascii()).parse::<JobId>() else {
                continue;
            };
            if holder != waiting.id && self.holds_on(&holder) {
                holders.push(holder);
            }
        }
        holders.sort();
        holders.dedup();
        holders
    }

    fn holds_on(&self, holder: &JobId) -> bool {
        matches!(self.supervisor(holder), Ok(Supervisor::Alive))
            && !matches!(self.phase(holder), Ok(Phase::Finished { .. }) | Err(_))
    }

    pub fn ids(&self) -> Result<Vec<JobId>, StoreError> {
        let dir = self.root.join("jobs");
        crate::faults::at("store::list", &dir).map_err(io("listing", &dir))?;
        let mut ids = Vec::new();
        for entry in std::fs::read_dir(&dir).map_err(io("listing", &dir))? {
            let entry = entry.map_err(io("listing", &dir))?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            match name.parse::<JobId>() {
                Ok(id) => ids.push(id),
                Err(_not_a_job) => {}
            }
        }
        Ok(ids)
    }

    pub fn resolve(&self, reference: &JobRef) -> Result<JobId, StoreError> {
        let mut matching: Vec<JobId> = self
            .ids()?
            .into_iter()
            .filter(|id| id.matches(reference))
            .collect();
        match (matching.pop(), matching.len()) {
            (Some(only), 0) => Ok(only),
            (Some(_), others) => Err(StoreError::Ambiguous {
                reference: reference.clone(),
                count: others.saturating_add(1),
            }),
            (None, _) => Err(StoreError::NoSuchJob(reference.clone())),
        }
    }
}

impl crate::ingress::Ingress for Sequence {}
impl crate::ingress::Ingress for LaunchEnv {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Concurrency;
    use crate::protocol::{Command, Location};

    fn spec(id: &JobId, sequence: u64) -> Spec {
        Spec {
            id: id.clone(),
            name: None,
            command: Command::Script("true".into()),
            location: Location::Home,
            env_names: std::collections::BTreeSet::new(),
            shell: None,
            concurrency: Concurrency::DEFAULT,
            sequence,
            submitted_by: crate::authz::Submitter::Owner,
            submitted_at: crate::clock::Timestamp::observe(),
        }
    }

    fn dirs(root: &Path) -> Dirs {
        Dirs {
            home: root.into(),
            state: root.join("state"),
            config: root.join("c"),
            cache: root.join("k"),
            keys: crate::keystore::KeyStore::OwnerOnlyFile,
        }
    }

    #[test]
    fn an_outcome_written_into_its_reserved_space_is_the_jobs_phase() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&dirs(tmp.path())).unwrap();
        let id: JobId = "0CCCCCCCCCCCCCCC".parse().unwrap();
        store
            .stage(&spec(&id, 1), (&BTreeMap::new(), &LaunchEnv::default()))
            .unwrap();
        store.publish(&id).unwrap();
        assert!(matches!(store.phase(&id).unwrap(), Phase::Queued));
        let finished = Phase::Finished {
            started_at: None,
            finished_at: crate::clock::Timestamp::at_millis(5),
            outcome: crate::protocol::Outcome::Failed { exit_code: 3 },
        };
        store.record_outcome_in_place(&id, &finished).unwrap();
        assert_eq!(store.phase(&id).unwrap(), finished);
        let too_long = Phase::Finished {
            started_at: None,
            finished_at: crate::clock::Timestamp::at_millis(5),
            outcome: crate::protocol::Outcome::Errored {
                reason: crate::terminal::RemoteText::new("x".repeat(OUTCOME_RESERVED)),
            },
        };
        store.record_outcome_in_place(&id, &too_long).unwrap_err();
        assert_eq!(store.phase(&id).unwrap(), finished);
    }

    fn staged(store: &Store, id: &JobId, sequence: u64) {
        store
            .stage(
                &spec(id, sequence),
                (&BTreeMap::new(), &LaunchEnv::default()),
            )
            .unwrap();
    }

    fn at(path: &Path) -> String {
        path.display().to_string()
    }

    #[test]
    fn a_failing_disk_is_an_error_everywhere_in_the_store() {
        let tmp = tempfile::tempdir().unwrap();
        let tag = tmp
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        {
            let _faults = crate::faults::inject(&[("state_file::dir", &tag)]);
            Store::open(&dirs(tmp.path())).unwrap_err();
        }
        let store = Store::open(&dirs(tmp.path())).unwrap();
        let id: JobId = "0HHHHHHHHHHHHHHH".parse().unwrap();
        let staging = store.staged_dir(&id);
        for file in [
            "spec.json",
            "env.json",
            "launch-env.json",
            "phase.json",
            "outcome",
        ] {
            let target = at(&staging.join(file));
            let _faults = crate::faults::inject(&[("state_file::write", &target)]);
            store
                .stage(&spec(&id, 1), (&BTreeMap::new(), &LaunchEnv::default()))
                .unwrap_err();
            store.discard_staged(&id).unwrap();
        }
        for (site, target) in [
            ("state_file::create", at(&staging.join("log"))),
            ("state_file::dir", at(&staging)),
            ("store::check", tag.clone()),
        ] {
            let _faults = crate::faults::inject(&[(site, &target)]);
            store
                .stage(&spec(&id, 1), (&BTreeMap::new(), &LaunchEnv::default()))
                .unwrap_err();
            store.discard_staged(&id).unwrap();
        }
        staged(&store, &id, 1);
        {
            let _faults = crate::faults::inject(&[
                ("state_file::publish", &tag),
                ("store::list", &tag),
                ("state_file::remove", &tag),
            ]);
            store.publish(&id).unwrap_err();
            store.staged_ids().unwrap_err();
            store.discard_staged(&id).unwrap_err();
            store.forget_staging_lock(&id).unwrap_err();
        }
        store.publish(&id).unwrap();
        {
            let _faults = crate::faults::inject(&[
                ("state_file::read", &tag),
                ("store::list", &tag),
                ("state_file::lock", &tag),
                ("state_file::overwrite", &tag),
                ("state_file::remove", &tag),
            ]);
            store.is_published(&id).unwrap_err();
            store.record_start_failure(&id, "x").unwrap_err();
            store.start_failure(&id).unwrap_err();
            store.phase(&id).unwrap_err();
            store.job(&id).unwrap_err();
            store.ids().unwrap_err();
            store.resolve(&id.as_str().parse().unwrap()).unwrap_err();
            store.next_sequence().unwrap_err();
            store
                .record_outcome_in_place(&id, &Phase::Queued)
                .unwrap_err();
            store.remove_job(&id).unwrap_err();
        }
        {
            let outcome = at(&store.job_dir(&id).join("outcome"));
            let _faults = crate::faults::inject(&[("state_file::read", &outcome)]);
            store.phase(&id).unwrap_err();
        }
        store.remove_job(&id).unwrap();
        assert!(store.ids().unwrap().is_empty());
    }

    #[test]
    fn a_fault_in_one_file_of_a_job_is_an_error_about_that_file() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&dirs(tmp.path())).unwrap();
        let id: JobId = "0MMMMMMMMMMMMMMM".parse().unwrap();
        staged(&store, &id, 1);
        store.publish(&id).unwrap();
        let failure = at(&store.job_dir(&id).join("failure"));
        {
            let _faults = crate::faults::inject(&[
                ("state_file::write", &failure),
                ("state_file::read", &failure),
            ]);
            store.record_start_failure(&id, "x").unwrap_err();
            store.start_failure(&id).unwrap_err();
        }
        {
            let phase = at(&store.job_dir(&id).join("phase.json"));
            let _faults = crate::faults::inject(&[("state_file::read", &phase)]);
            store.job(&id).unwrap_err();
        }
        for doomed in [store.control_path(&id), store.alive_path(&id)] {
            let target = at(&doomed);
            let _faults = crate::faults::inject(&[("state_file::remove", &target)]);
            store.remove_job(&id).unwrap_err();
        }
    }

    #[test]
    fn a_start_failure_is_found_before_and_after_publishing_and_files_go_with_their_job() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&dirs(tmp.path())).unwrap();
        let id: JobId = "0JJJJJJJJJJJJJJJ".parse().unwrap();
        staged(&store, &id, 1);
        assert!(!store.is_published(&id).unwrap());
        store.record_start_failure(&id, "no shell").unwrap();
        assert_eq!(
            store.start_failure(&id).unwrap().as_deref(),
            Some("no shell")
        );
        store.publish(&id).unwrap();
        assert!(store.is_published(&id).unwrap());
        assert_eq!(
            store.start_failure(&id).unwrap().as_deref(),
            Some("no shell")
        );
        store.record_start_failure(&id, "no slot").unwrap();
        assert_eq!(
            store.start_failure(&id).unwrap().as_deref(),
            Some("no slot")
        );
        assert_eq!(
            store.control_path(&id),
            store.area("live").join(format!("{id}.sock"))
        );

        crate::state_file::write_bytes(&store.alive_path(&id), b"").unwrap();
        crate::state_file::write_bytes(&store.control_path(&id), b"").unwrap();
        store.remove_job(&id).unwrap();
        assert!(
            crate::state_file::read_bytes(&store.alive_path(&id))
                .unwrap()
                .is_none()
        );
        assert!(
            crate::state_file::read_bytes(&store.control_path(&id))
                .unwrap()
                .is_none()
        );

        crate::state_file::write_bytes(&store.staging_lock_path(&id), b"").unwrap();
        store.forget_staging_lock(&id).unwrap();
        assert!(
            crate::state_file::read_bytes(&store.staging_lock_path(&id))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn a_written_phase_outranks_the_reserved_outcome_and_an_exact_fit_is_kept() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&dirs(tmp.path())).unwrap();
        let id: JobId = "0KKKKKKKKKKKKKKK".parse().unwrap();
        staged(&store, &id, 1);
        store.publish(&id).unwrap();
        let written = Phase::Finished {
            started_at: None,
            finished_at: crate::clock::Timestamp::at_millis(1),
            outcome: crate::protocol::Outcome::Succeeded,
        };
        let reserved = Phase::Finished {
            started_at: None,
            finished_at: crate::clock::Timestamp::at_millis(2),
            outcome: crate::protocol::Outcome::Killed,
        };
        store.set_phase(&id, &written).unwrap();
        store.record_outcome_in_place(&id, &reserved).unwrap();
        assert_eq!(store.phase(&id).unwrap(), written);

        let errored = |length: usize| Phase::Finished {
            started_at: None,
            finished_at: crate::clock::Timestamp::at_millis(3),
            outcome: crate::protocol::Outcome::Errored {
                reason: crate::terminal::RemoteText::new("x".repeat(length)),
            },
        };
        let bare = serde_json::to_vec(&errored(0)).unwrap().len();
        let exact = errored(OUTCOME_RESERVED.saturating_sub(bare));
        assert_eq!(serde_json::to_vec(&exact).unwrap().len(), OUTCOME_RESERVED);
        store.set_phase(&id, &Phase::Queued).unwrap();
        store.record_outcome_in_place(&id, &exact).unwrap();
        assert_eq!(store.phase(&id).unwrap(), exact);
    }

    #[test]
    fn a_jobs_area_that_is_not_a_directory_is_an_error_to_list_and_on_unix_to_stage_into() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&dirs(tmp.path())).unwrap();
        let jobs = store.area("jobs");
        crate::state_file::remove_dir_all(&jobs).unwrap();
        crate::state_file::write_bytes(&jobs, b"").unwrap();
        store.ids().unwrap_err();
        let id: JobId = "0NNNNNNNNNNNNNNN".parse().unwrap();
        let staged = store.stage(&spec(&id, 1), (&BTreeMap::new(), &LaunchEnv::default()));
        if cfg!(unix) {
            assert!(
                matches!(
                    staged,
                    Err(StoreError::Io {
                        action: "checking",
                        ..
                    })
                ),
                "{staged:?}"
            );
        }
    }

    #[test]
    fn a_job_marked_to_skip_the_queue_keeps_the_mark_when_published() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&dirs(tmp.path())).unwrap();
        let [now, queued]: [JobId; 2] =
            ["0PPPPPPPPPPPPPPP", "0QQQQQQQQQQQQQQQ"].map(|id| id.parse().unwrap());
        for (sequence, id) in (1..).zip([&now, &queued]) {
            store
                .stage(
                    &spec(id, sequence),
                    (&BTreeMap::new(), &LaunchEnv::default()),
                )
                .unwrap();
        }
        store.skip_the_queue(&now).unwrap();
        for id in [&now, &queued] {
            store.publish(id).unwrap();
        }
        assert!(store.skips_the_queue(&now).unwrap());
        assert!(!store.skips_the_queue(&queued).unwrap());
    }

    #[test]
    fn a_queued_job_names_the_live_jobs_holding_its_slots() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&dirs(tmp.path())).unwrap();
        let [waiting, holding, done, outside]: [JobId; 4] = [
            "0HHHHHHHHHHHHHHH",
            "0JJJJJJJJJJJJJJJ",
            "0KKKKKKKKKKKKKKK",
            "0MMMMMMMMMMMMMMM",
        ]
        .map(|id| id.parse().unwrap());
        let mut alive = Vec::new();
        for (sequence, id) in (1..).zip([&waiting, &holding, &done, &outside]) {
            store
                .stage(
                    &spec(id, sequence),
                    (&BTreeMap::new(), &LaunchEnv::default()),
                )
                .unwrap();
            store.publish(id).unwrap();
            alive.push(OsLock::exclusive(&store.alive_path(id)).unwrap());
        }
        let running = Phase::Running {
            started_at: crate::clock::Timestamp::at_millis(1),
            pid: 1,
            workspace: String::new(),
        };
        store.set_phase(&holding, &running).unwrap();
        store.set_phase(&outside, &running).unwrap();
        store
            .set_phase(
                &done,
                &Phase::Finished {
                    started_at: None,
                    finished_at: crate::clock::Timestamp::at_millis(2),
                    outcome: crate::protocol::Outcome::Succeeded,
                },
            )
            .unwrap();
        assert!(store.job(&waiting).unwrap().behind.is_empty());

        let slots = store.area("slots");
        let holder = |name: &str, bytes: &[u8]| {
            crate::state_file::write_bytes(&slots.join(name), bytes).unwrap();
        };
        holder("0.holder", done.as_str().as_bytes());
        holder("0.lock", b"");
        crate::state_file::private_dir(&slots.join("01.holder")).unwrap();
        holder("0a.holder", holding.as_str().as_bytes());
        holder("1.holder", b"not a job");
        holder("10.holder", outside.as_str().as_bytes());
        holder("2.holder", waiting.as_str().as_bytes());
        crate::state_file::write_bytes(
            &Store::slot_holder_path(&slots.join("3.lock")),
            holding.as_str().as_bytes(),
        )
        .unwrap();
        holder("4.holder", outside.as_str().as_bytes());
        holder("x.holder", outside.as_str().as_bytes());
        assert_eq!(
            store.job(&waiting).unwrap().behind,
            std::slice::from_ref(&holding)
        );
        assert!(store.job(&holding).unwrap().behind.is_empty());

        for lock in alive {
            lock.release().unwrap();
        }
        assert!(store.job(&waiting).unwrap().behind.is_empty());
    }

    #[test]
    fn when_the_phase_cannot_be_written_the_reserved_outcome_still_is() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&dirs(tmp.path())).unwrap();
        let id: JobId = "0GGGGGGGGGGGGGGG".parse().unwrap();
        store
            .stage(&spec(&id, 1), (&BTreeMap::new(), &LaunchEnv::default()))
            .unwrap();
        store.publish(&id).unwrap();
        let finished = Phase::Finished {
            started_at: None,
            finished_at: crate::clock::Timestamp::at_millis(9),
            outcome: crate::protocol::Outcome::Succeeded,
        };
        let full = store.job_dir(&id).join("phase.json").display().to_string();
        let _faults = crate::faults::inject(&[("state_file::write", &full)]);
        store.set_phase(&id, &finished).unwrap_err();
        store.record_outcome_in_place(&id, &finished).unwrap();
        assert_eq!(store.phase(&id).unwrap(), finished);
    }

    #[test]
    fn jobs_appear_only_when_published_and_resolve_by_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&dirs(tmp.path())).unwrap();
        assert_eq!(store.next_sequence().unwrap(), 1);
        assert_eq!(store.next_sequence().unwrap(), 2);
        let a: JobId = "0AAAAAAAAAAAAAAA".parse().unwrap();
        let b: JobId = "0BBBBBBBBBBBBBBB".parse().unwrap();
        let env = BTreeMap::new();
        store
            .stage(&spec(&a, 1), (&env, &LaunchEnv::default()))
            .unwrap();
        assert!(store.ids().unwrap().is_empty());
        store.publish(&a).unwrap();
        store
            .stage(&spec(&b, 2), (&env, &LaunchEnv::default()))
            .unwrap();
        store.publish(&b).unwrap();
        assert!(matches!(
            store.stage(&spec(&a, 3), (&env, &LaunchEnv::default())),
            Err(StoreError::Exists(_))
        ));
        assert_eq!(store.resolve(&a.clone().into()).unwrap(), a);
        assert!(matches!(
            store.resolve(&JobRef::parse_loose("0").unwrap()),
            Err(StoreError::Ambiguous { count: 2, .. })
        ));
        assert!(matches!(
            store.resolve(&JobRef::parse_loose("Z").unwrap()),
            Err(StoreError::NoSuchJob(_))
        ));
        assert_eq!(store.job(&b).unwrap().supervisor, Supervisor::Gone);
    }
}
