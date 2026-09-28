use crate::failure::io;
use std::collections::{BTreeMap, VecDeque};
use std::io::{BufReader, ErrorKind};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::domain::{EnvName, Invalid, JobId, JobRef};
use crate::lock::{LockError, OsLock, Probe, SlotIndex};
use crate::paths::Dirs;
use crate::protocol::{Change, Job, Phase, PhaseKind, Settings, Spec, Supervisor};

const SCHEMA: &str = "v3";

const OUTCOME_RESERVED: usize = 4096;
const NOTES_LINE: u64 = 64 << 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IdArea {
    Published,
    Staged,
}

pub(crate) struct JobIds {
    directory: PathBuf,
    entries: Option<std::fs::ReadDir>,
}

impl Iterator for JobIds {
    type Item = Result<JobId, StoreError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let entry = self.entries.as_mut()?.next()?;
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => return Some(Err(io("listing", &self.directory)(error).into())),
            };
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if let Ok(id) = name.parse::<JobId>() {
                return Some(Ok(id));
            }
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error(transparent)]
    Io(#[from] crate::failure::IoFailure),
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
    #[error("{path} does not name a job occupying the slot")]
    InvalidHolder { path: PathBuf },
    #[error("{path} does not hold a finished job outcome")]
    InvalidOutcome { path: PathBuf },
    #[error("job {id} cannot move from {from:?} to {to:?}")]
    InvalidPhaseTransition { id: JobId, from: Phase, to: Phase },
    #[error(transparent)]
    Invalid(#[from] Invalid),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Publication {
    Published,
    Unpublished,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueMode {
    Immediate,
    Ordinary,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum QueuePredecessor {
    Waiting(PathBuf),
    AliveFallback(PathBuf),
}

impl QueuePredecessor {
    #[must_use]
    pub(crate) fn path(&self) -> &Path {
        match self {
            Self::Waiting(path) | Self::AliveFallback(path) => path,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Blocker {
    Active,
    Cleared,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PhaseTransition {
    Advance,
    Refuse,
}

const fn phase_transition(from: &Phase, to: &Phase) -> PhaseTransition {
    use Phase::{Finished, Preparing, Queued, Running, Starting};
    use PhaseTransition::{Advance, Refuse};

    match (from, to) {
        (Queued, Preparing { .. } | Finished { .. })
        | (Preparing { .. }, Starting { .. } | Finished { .. })
        | (Starting { .. }, Running { .. } | Finished { .. })
        | (Running { .. }, Finished { .. }) => Advance,
        (Queued, Queued | Starting { .. } | Running { .. })
        | (Preparing { .. }, Queued | Preparing { .. } | Running { .. })
        | (Starting { .. }, Queued | Preparing { .. } | Starting { .. })
        | (Running { .. }, Queued | Preparing { .. } | Starting { .. } | Running { .. })
        | (
            Finished { .. },
            Queued | Preparing { .. } | Starting { .. } | Running { .. } | Finished { .. },
        ) => Refuse,
    }
}

fn read_required<T: crate::ingress::Ingress>(
    file: &crate::state_file::StateFile<T>,
) -> Result<T, StoreError> {
    let path = file.path();
    file.read()?
        .ok_or_else(|| io("reading", path)(ErrorKind::NotFound.into()).into())
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchEnv {
    pub vars: BTreeMap<String, String>,
    pub not_unicode: Vec<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(transparent)]
struct StoredEnv(BTreeMap<EnvName, String>);

#[derive(Serialize, Deserialize)]
#[serde(transparent)]
struct StoredLeft(Vec<crate::snapshot::Left>);

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
        Self::from_vars(std::env::vars_os())
    }

    fn from_vars(vars: impl IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>) -> Self {
        let mut launch = Self::default();
        for (key, value) in vars {
            let name = match key.into_string() {
                Ok(name) => classify_launch_name(name),
                Err(_) => LaunchName::Omitted,
            };
            match (name, value.into_string()) {
                (LaunchName::Allowed(name), Ok(value)) => {
                    launch.vars.insert(name, value);
                }
                (LaunchName::Allowed(name), Err(_)) => {
                    launch.not_unicode.push(name);
                }
                (LaunchName::Omitted, Ok(_) | Err(_)) => {}
            }
        }
        launch
    }
}

enum LaunchName {
    Allowed(String),
    Omitted,
}

fn classify_launch_name(name: String) -> LaunchName {
    let upper = name.to_ascii_uppercase();
    if upper.starts_with("LC_")
        || matches!(
            upper.as_str(),
            "PATH"
                | "HOME"
                | "USER"
                | "USERNAME"
                | "USERPROFILE"
                | "LOGNAME"
                | "SHELL"
                | "TMPDIR"
                | "TMP"
                | "TEMP"
                | "SYSTEMROOT"
                | "WINDIR"
                | "COMSPEC"
                | "PATHEXT"
                | "HOMEDRIVE"
                | "HOMEPATH"
                | "APPDATA"
                | "LOCALAPPDATA"
                | "PROGRAMDATA"
                | "PROGRAMFILES"
                | "PROGRAMFILES(X86)"
                | "PROGRAMW6432"
                | "COMMONPROGRAMFILES"
                | "COMMONPROGRAMFILES(X86)"
                | "COMMONPROGRAMW6432"
                | "LANG"
                | "LANGUAGE"
                | "TZ"
                | "TERM"
                | "COLORTERM"
                | "NO_COLOR"
        )
    {
        LaunchName::Allowed(name)
    } else {
        LaunchName::Omitted
    }
}

const ENV: &str = "env.json";
const NOTES_KEPT: usize = 20;
const LAUNCH_ENV: &str = "launch-env.json";
const SUPERVISOR_BOOT: &str = "supervisor-boot";

pub struct Launch {
    vars: BTreeMap<String, zeroize::Zeroizing<String>>,
    not_unicode: Vec<String>,
}

pub(crate) struct PreparedJobCommand {
    command: std::process::Command,
}

impl std::fmt::Debug for PreparedJobCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PreparedJobCommand(<redacted>)")
    }
}

impl PreparedJobCommand {
    pub(crate) fn into_command(self) -> std::process::Command {
        self.command
    }
}

impl std::fmt::Debug for Launch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Launch")
            .field("names", &self.vars.keys().collect::<Vec<_>>())
            .field("not_unicode", &self.not_unicode)
            .finish()
    }
}

impl Launch {
    pub(crate) fn prepare(
        self,
        mut command: std::process::Command,
        id: &JobId,
    ) -> (PreparedJobCommand, Vec<String>) {
        command.env_clear();
        for (key, value) in &self.vars {
            command.env(key, value.as_str());
        }
        command
            .env("DOMYJOB", "1")
            .env("DOMYJOB_JOB_ID", id.as_str());
        (PreparedJobCommand { command }, self.not_unicode)
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

pub(crate) struct AdmissionGuard {
    store: Store,
    lock: OsLock,
}

impl AdmissionGuard {
    pub(crate) fn earliest_waiter(
        &self,
        waiting: &Spec,
    ) -> Result<Option<QueuePredecessor>, StoreError> {
        self.store.earliest_waiter_unlocked(waiting)
    }

    pub(crate) fn release(self) -> Result<(), StoreError> {
        Ok(self.lock.release()?)
    }
}

impl Store {
    fn iter_ids(&self, area: IdArea) -> Result<JobIds, StoreError> {
        let name = match area {
            IdArea::Published => "jobs",
            IdArea::Staged => "staging",
        };
        let dir = self.root.join(name);
        crate::faults::at("store::list", &dir).map_err(io("listing", &dir))?;
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => Some(entries),
            Err(error) if error.kind() == ErrorKind::NotFound && area == IdArea::Staged => None,
            Err(error) => return Err(io("listing", &dir)(error).into()),
        };
        Ok(JobIds {
            directory: dir,
            entries,
        })
    }

    pub fn open(dirs: &Dirs) -> Result<Self, StoreError> {
        let root = dirs.state().join(SCHEMA);
        for dir in [
            root.clone(),
            root.join("jobs"),
            root.join("staging"),
            root.join("live"),
            root.join("slots"),
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
    pub fn settings_path(&self) -> PathBuf {
        self.root.join("settings.json")
    }

    fn settings_file(&self) -> crate::state_file::StateFile<Settings> {
        crate::state_file::StateFile::at(&self.settings_path())
    }

    pub fn settings(&self) -> Result<Settings, StoreError> {
        Ok(self.settings_file().read()?.unwrap_or_default())
    }

    pub fn configure(&self, change: Change) -> Result<(), StoreError> {
        let admission = self.admission()?;
        self.settings_file()
            .replace(&self.settings()?.with(change))?;
        self.signal_queue()?;
        admission.release()
    }

    #[must_use]
    fn admission_lock_path(&self) -> PathBuf {
        self.root.join("queue.lock")
    }

    pub(crate) fn admission(&self) -> Result<AdmissionGuard, StoreError> {
        Ok(AdmissionGuard {
            store: self.clone(),
            lock: OsLock::exclusive(&self.admission_lock_path())?,
        })
    }

    #[must_use]
    pub fn queue_changed_path(&self) -> PathBuf {
        self.root.join("queue.changed")
    }

    pub fn signal_queue(&self) -> Result<(), StoreError> {
        Ok(crate::state_file::write_bytes(
            &self.queue_changed_path(),
            b"changed",
        )?)
    }

    #[must_use]
    pub fn job_dir(&self, id: &JobId) -> PathBuf {
        self.root.join("jobs").join(id.as_str())
    }

    fn staged_dir(&self, id: &JobId) -> PathBuf {
        self.root.join("staging").join(id.as_str())
    }

    fn spec_file(&self, id: &JobId) -> crate::state_file::StateFile<Spec> {
        crate::state_file::StateFile::at(&self.job_dir(id).join("spec.json"))
    }

    fn staged_spec_file(&self, id: &JobId) -> crate::state_file::StateFile<Spec> {
        crate::state_file::StateFile::at(&self.staged_dir(id).join("spec.json"))
    }

    fn phase_file(&self, id: &JobId) -> crate::state_file::StateFile<Phase> {
        crate::state_file::StateFile::at(&self.job_dir(id).join("phase.json"))
    }

    fn staged_phase_file(&self, id: &JobId) -> crate::state_file::StateFile<Phase> {
        crate::state_file::StateFile::at(&self.staged_dir(id).join("phase.json"))
    }

    fn env_file(dir: &Path) -> crate::state_file::StateFile<StoredEnv> {
        crate::state_file::StateFile::at(&dir.join(ENV))
    }

    fn launch_env_file(dir: &Path) -> crate::state_file::StateFile<LaunchEnv> {
        crate::state_file::StateFile::at(&dir.join(LAUNCH_ENV))
    }

    fn sequence_file(&self) -> crate::state_file::StateFile<Sequence> {
        crate::state_file::StateFile::at(&self.root.join("sequence.json"))
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
    pub fn left_path(&self, id: &JobId) -> PathBuf {
        self.job_dir(id).join("left.json")
    }

    fn left_file(&self, id: &JobId) -> crate::state_file::StateFile<StoredLeft> {
        crate::state_file::StateFile::at(&self.left_path(id))
    }

    pub(crate) fn record_left(
        &self,
        id: &JobId,
        items: Vec<crate::snapshot::Left>,
    ) -> Result<(), StoreError> {
        Ok(self.left_file(id).replace(&StoredLeft(items))?)
    }

    pub fn left(&self, id: &JobId) -> Result<Option<Vec<crate::snapshot::Left>>, StoreError> {
        Ok(self.left_file(id).read()?.map(|stored| stored.0))
    }

    #[must_use]
    pub fn alive_path(&self, id: &JobId) -> PathBuf {
        self.root.join("live").join(format!("{id}.lock"))
    }

    #[must_use]
    pub(crate) fn queue_wait_path(&self, id: &JobId) -> PathBuf {
        self.root.join("live").join(format!("{id}.queue.lock"))
    }

    #[must_use]
    pub fn control_path(&self, id: &JobId) -> PathBuf {
        self.root.join("live").join(format!("{id}.sock"))
    }

    pub fn next_sequence(&self) -> Result<u64, StoreError> {
        Ok(self.sequence_file().update(Sequence::default, |sequence| {
            sequence.last = sequence.last.saturating_add(1);
            sequence.last
        })?)
    }

    pub(crate) fn stage_admitted(
        &self,
        _admission: &crate::node::JobAdmission<'_>,
        spec: &Spec,
        state: (BTreeMap<EnvName, String>, &LaunchEnv),
    ) -> Result<(), StoreError> {
        self.stage_inner(spec, state.0, state.1)
    }

    #[cfg(any(test, feature = "failpoints"))]
    pub fn stage(
        &self,
        spec: &Spec,
        state: (&BTreeMap<EnvName, String>, &LaunchEnv),
    ) -> Result<(), StoreError> {
        self.stage_inner(spec, state.0.clone(), state.1)
    }

    fn stage_inner(
        &self,
        spec: &Spec,
        env: BTreeMap<EnvName, String>,
        launch: &LaunchEnv,
    ) -> Result<(), StoreError> {
        let dir = self.staged_dir(&spec.id);
        for taken in [&dir, &self.job_dir(&spec.id)] {
            crate::faults::at("store::check", taken).map_err(io("checking", taken))?;
            match std::fs::symlink_metadata(taken) {
                Ok(_) => return Err(StoreError::Exists(spec.id.clone())),
                Err(e) if e.kind() == ErrorKind::NotFound => {}
                Err(e) => return Err(io("checking", taken)(e).into()),
            }
        }
        crate::state_file::private_dir(&dir)?;
        self.staged_spec_file(&spec.id).replace(spec)?;
        Self::env_file(&dir).replace(&StoredEnv(env))?;
        Self::launch_env_file(&dir).replace(launch)?;
        self.staged_phase_file(&spec.id).replace(&Phase::Queued)?;
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

    pub fn queue_mode(&self, id: &JobId) -> Result<QueueMode, StoreError> {
        Ok(
            match crate::state_file::read_bytes(&self.job_dir(id).join("now"))? {
                Some(_) => QueueMode::Immediate,
                None => QueueMode::Ordinary,
            },
        )
    }

    pub fn publish(&self, id: &JobId) -> Result<(), StoreError> {
        let collecting = OsLock::exclusive(&self.collection_lock_path())?;
        crate::state_file::publish_dir(&self.staged_dir(id), &self.job_dir(id))?;
        Ok(collecting.release()?)
    }

    pub fn publication(&self, id: &JobId) -> Result<Publication, StoreError> {
        Ok(
            match crate::state_file::read_bytes(&self.job_dir(id).join("spec.json"))? {
                Some(_) => Publication::Published,
                None => Publication::Unpublished,
            },
        )
    }

    fn failure_path(&self, id: &JobId) -> Result<PathBuf, StoreError> {
        Ok(match self.publication(id)? {
            Publication::Published => self.job_dir(id).join("failure"),
            Publication::Unpublished => self.staged_dir(id).join("failure"),
        })
    }

    pub fn record_start_failure(&self, id: &JobId, why: &str) -> Result<(), StoreError> {
        Ok(crate::state_file::write_bytes(
            &self.failure_path(id)?,
            why.as_bytes(),
        )?)
    }

    pub fn record_supervisor_boot(&self, id: &JobId) -> Result<(), StoreError> {
        match crate::platform::boot_identity() {
            Some(identity) => Ok(crate::state_file::write_bytes(
                &self.job_dir(id).join(SUPERVISOR_BOOT),
                identity.as_bytes(),
            )?),
            None => Ok(crate::state_file::remove_file(
                &self.job_dir(id).join(SUPERVISOR_BOOT),
            )?),
        }
    }

    pub fn supervisor_boot(&self, id: &JobId) -> Result<Option<String>, StoreError> {
        Ok(
            crate::state_file::read_bytes(&self.job_dir(id).join(SUPERVISOR_BOOT))?
                .map(|bytes| String::from_utf8_lossy(bytes.trim_ascii()).into_owned()),
        )
    }

    pub fn start_failure(&self, id: &JobId) -> Result<Option<String>, StoreError> {
        Ok(crate::state_file::read_bytes(&self.failure_path(id)?)?
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned()))
    }

    #[must_use]
    pub fn staging_lock_path(&self, id: &JobId) -> PathBuf {
        self.root.join("staging").join(format!("{id}.lock"))
    }

    pub(crate) fn staged_ids_iter(&self) -> Result<JobIds, StoreError> {
        self.iter_ids(IdArea::Staged)
    }

    #[cfg(test)]
    pub fn staged_ids(&self) -> Result<Vec<JobId>, StoreError> {
        self.staged_ids_iter()?.collect()
    }

    #[must_use]
    pub fn nonce_path(&self, scope: &str, nonce: &crate::domain::Nonce) -> PathBuf {
        self.root.join("nonces").join(format!("{scope}-{nonce}"))
    }

    pub fn staged_spec(&self, id: &JobId) -> Result<Spec, StoreError> {
        read_required(&self.staged_spec_file(id))
    }

    #[must_use]
    pub fn collection_lock_path(&self) -> PathBuf {
        self.root.join("collecting.lock")
    }

    pub fn remove_job(&self, id: &JobId) -> Result<(), StoreError> {
        crate::state_file::remove_dir_all(&self.job_dir(id))?;
        crate::state_file::remove_file(&self.control_path(id))?;
        crate::state_file::remove_file(&self.alive_path(id))?;
        Ok(crate::state_file::remove_file(&self.queue_wait_path(id))?)
    }

    pub fn forget_staging_lock(&self, id: &JobId) -> Result<(), StoreError> {
        Ok(crate::state_file::remove_file(&self.staging_lock_path(id))?)
    }

    pub fn discard_staged(&self, id: &JobId) -> Result<(), StoreError> {
        Ok(crate::state_file::remove_dir_all(&self.staged_dir(id))?)
    }

    pub fn spec(&self, id: &JobId) -> Result<Spec, StoreError> {
        read_required(&self.spec_file(id))
    }

    pub fn take_launch(&self, id: &JobId) -> Result<Launch, StoreError> {
        let dir = self.job_dir(id);
        let base = read_required(&Self::launch_env_file(&dir))?;
        let env = read_required(&Self::env_file(&dir))?;
        self.purge_secrets(id)?;
        let mut vars: BTreeMap<String, zeroize::Zeroizing<String>> = base
            .vars
            .into_iter()
            .map(|(key, value)| (key, zeroize::Zeroizing::new(value)))
            .collect();
        for (key, value) in env.0 {
            vars.insert(key.as_str().to_owned(), zeroize::Zeroizing::new(value));
        }
        Ok(Launch {
            vars,
            not_unicode: base.not_unicode,
        })
    }

    fn purge_secrets(&self, id: &JobId) -> Result<(), StoreError> {
        for name in [ENV, LAUNCH_ENV] {
            crate::state_file::remove_file(&self.job_dir(id).join(name))?;
        }
        Ok(())
    }

    pub fn set_phase(&self, id: &JobId, phase: &Phase) -> Result<(), StoreError> {
        let mut file = self.phase_file(id).lock()?;
        let from = self.phase(id)?;
        match phase_transition(&from, phase) {
            PhaseTransition::Advance => {}
            PhaseTransition::Refuse => {
                return Err(StoreError::InvalidPhaseTransition {
                    id: id.clone(),
                    from,
                    to: phase.clone(),
                });
            }
        }
        file.write(phase)?;
        file.release()?;
        match phase {
            Phase::Finished { .. } => self.purge_secrets(id),
            Phase::Queued
            | Phase::Preparing { .. }
            | Phase::Starting { .. }
            | Phase::Running { .. } => Ok(()),
        }
    }

    #[cfg(test)]
    pub fn force_phase(&self, id: &JobId, phase: &Phase) -> Result<(), StoreError> {
        self.phase_file(id).replace(phase)?;
        if phase.kind() == PhaseKind::Finished {
            self.purge_secrets(id)?;
        }
        Ok(())
    }

    pub fn phase(&self, id: &JobId) -> Result<Phase, StoreError> {
        let phase = read_required(&self.phase_file(id))?;
        if phase.kind() == PhaseKind::Finished {
            return Ok(phase);
        }
        let path = self.job_dir(id).join("outcome");
        let reserved = crate::state_file::read_bytes(&path)?;
        let written = reserved.as_deref().map_or(&[][..], <[u8]>::trim_ascii);
        if written.is_empty() {
            return Ok(phase);
        }
        match crate::ingress::json::<Phase>(written) {
            Ok(finished @ Phase::Finished { .. }) => Ok(finished),
            Ok(
                Phase::Queued
                | Phase::Preparing { .. }
                | Phase::Starting { .. }
                | Phase::Running { .. },
            ) => Err(StoreError::InvalidOutcome { path }),
            Err(source) => Err(StoreError::Json { path, source }),
        }
    }

    pub fn record_outcome_in_place(&self, id: &JobId, phase: &Phase) -> Result<(), StoreError> {
        if phase.kind() != PhaseKind::Finished {
            return Err(StoreError::InvalidOutcome {
                path: self.job_dir(id).join("outcome"),
            });
        }
        let mut bytes = serde_json::to_vec(phase).map_err(|e| {
            StoreError::from(crate::state_file::StateError::Io(
                crate::failure::IoFailure {
                    action: "encoding the outcome of",
                    path: self.job_dir(id),
                    source: std::io::Error::other(e),
                },
            ))
        })?;
        if bytes.len() > OUTCOME_RESERVED {
            return Err(StoreError::from(crate::state_file::StateError::Io(
                crate::failure::IoFailure {
                    action: "fitting the outcome into the space reserved for it in",
                    path: self.job_dir(id),
                    source: std::io::Error::other("the outcome is longer than its reserved space"),
                },
            )));
        }
        bytes.resize(OUTCOME_RESERVED, b' ');
        crate::state_file::overwrite_in_place(&self.job_dir(id).join("outcome"), &bytes)?;
        self.purge_secrets(id)
    }

    fn supervisor(&self, id: &JobId) -> Result<Supervisor, StoreError> {
        match OsLock::probe(&self.alive_path(id))? {
            Probe::Held => Ok(Supervisor::Alive),
            Probe::Absent | Probe::Free => Ok(Supervisor::Gone),
        }
    }

    fn earliest_waiter_unlocked(
        &self,
        waiting: &Spec,
    ) -> Result<Option<QueuePredecessor>, StoreError> {
        let mut earliest: Option<(u64, QueuePredecessor)> = None;
        for id in self.ids_iter()? {
            let id = id?;
            if id == waiting.id {
                continue;
            }
            match self.queue_mode(&id)? {
                QueueMode::Immediate => continue,
                QueueMode::Ordinary => {}
            }
            let spec = self.spec(&id)?;
            if spec.sequence >= waiting.sequence || self.phase(&id)?.kind() != PhaseKind::Queued {
                continue;
            }
            match self.supervisor(&id)? {
                Supervisor::Alive => {}
                Supervisor::Gone => continue,
            }
            let predecessor = match OsLock::probe(&self.queue_wait_path(&id))? {
                Probe::Held => QueuePredecessor::Waiting(self.queue_wait_path(&id)),
                Probe::Absent | Probe::Free => {
                    QueuePredecessor::AliveFallback(self.alive_path(&id))
                }
            };
            if earliest
                .as_ref()
                .is_none_or(|(sequence, _)| spec.sequence < *sequence)
            {
                earliest = Some((spec.sequence, predecessor));
            }
        }
        Ok(earliest.map(|(_, predecessor)| predecessor))
    }

    pub fn job(&self, id: &JobId) -> Result<Job, StoreError> {
        let supervisor = self.supervisor(id)?;
        let spec = self.spec(id)?;
        let phase = self.phase(id)?;
        let behind = match (&phase, supervisor) {
            (Phase::Queued, Supervisor::Alive) => self.slot_holders(&spec)?,
            (Phase::Queued, Supervisor::Gone)
            | (
                Phase::Preparing { .. }
                | Phase::Starting { .. }
                | Phase::Running { .. }
                | Phase::Finished { .. },
                Supervisor::Alive | Supervisor::Gone,
            ) => Vec::new(),
        };
        Ok(Job {
            spec,
            phase,
            supervisor,
            behind,
            notes: self.notes(id)?,
        })
    }

    #[must_use]
    pub fn notes_path(&self, id: &JobId) -> PathBuf {
        self.job_dir(id).join("notes")
    }

    fn notes(&self, id: &JobId) -> Result<Vec<crate::terminal::RemoteText>, StoreError> {
        let path = self.notes_path(id);
        let Some(file) = crate::state_file::open_read(&path)? else {
            return Ok(Vec::new());
        };
        let mut reader = BufReader::new(file);
        let mut latest = VecDeque::with_capacity(NOTES_KEPT);
        loop {
            let line =
                crate::bounded::line(&mut reader, NOTES_LINE).map_err(io("reading", &path))?;
            if line.is_empty() {
                break;
            }
            let text = String::from_utf8_lossy(&line);
            let text = text.strip_suffix('\n').unwrap_or(&text);
            let text = text.strip_suffix('\r').unwrap_or(text);
            if !text.is_empty() {
                if latest.len() == NOTES_KEPT {
                    let _oldest = latest.pop_front();
                }
                latest.push_back(crate::terminal::RemoteText::new(text.to_owned()));
            }
        }
        Ok(latest.into_iter().collect())
    }

    #[must_use]
    pub fn slot_holder_path(slot_lock: &Path) -> PathBuf {
        slot_lock.with_extension("holder")
    }

    pub(crate) fn held_slots(&self) -> Result<Vec<PathBuf>, StoreError> {
        let slots = self.area("slots");
        let mut held = Vec::new();
        for index in SlotIndex::all() {
            let lock = index.lock_path(&slots);
            let occupied = match OsLock::probe(&lock)? {
                Probe::Held => true,
                Probe::Absent | Probe::Free => false,
            };
            if occupied {
                held.push(lock);
            }
        }
        Ok(held)
    }

    fn slot_holders(&self, waiting: &Spec) -> Result<Vec<JobId>, StoreError> {
        let mut holders = Vec::new();
        for lock in self.held_slots()? {
            let path = Self::slot_holder_path(&lock);
            let Some(bytes) = crate::state_file::read_bytes(&path)? else {
                continue;
            };
            let holder = String::from_utf8_lossy(bytes.trim_ascii())
                .parse::<JobId>()
                .map_err(|_invalid| StoreError::InvalidHolder { path })?;
            if holder != waiting.id {
                match self.holds_on(&holder)? {
                    Blocker::Active => holders.push(holder),
                    Blocker::Cleared => {}
                }
            }
        }
        holders.sort();
        holders.dedup();
        Ok(holders)
    }

    fn holds_on(&self, holder: &JobId) -> Result<Blocker, StoreError> {
        match self.publication(holder)? {
            Publication::Published => {}
            Publication::Unpublished => return Ok(Blocker::Cleared),
        }
        Ok(match self.supervisor(holder)? {
            Supervisor::Gone => Blocker::Cleared,
            Supervisor::Alive => match self.phase(holder)? {
                Phase::Finished { .. } => Blocker::Cleared,
                Phase::Queued
                | Phase::Preparing { .. }
                | Phase::Starting { .. }
                | Phase::Running { .. } => Blocker::Active,
            },
        })
    }

    pub(crate) fn ids_iter(&self) -> Result<JobIds, StoreError> {
        self.iter_ids(IdArea::Published)
    }

    #[cfg(test)]
    pub fn ids(&self) -> Result<Vec<JobId>, StoreError> {
        self.ids_iter()?.collect()
    }

    pub fn resolve(&self, reference: &JobRef) -> Result<JobId, StoreError> {
        if let Ok(id) = reference.as_str().parse::<JobId>() {
            let spec = self.job_dir(&id).join("spec.json");
            return match crate::state_file::read_bytes(&spec)? {
                Some(_) => Ok(id),
                None => Err(StoreError::NoSuchJob(reference.clone())),
            };
        }
        let mut first = None;
        let mut count = 0usize;
        for id in self.ids_iter()? {
            let id = id?;
            if id.matches(reference) {
                count = count.saturating_add(1);
                if first.is_none() {
                    first = Some(id);
                }
            }
        }
        match (first, count) {
            (Some(only), 1) => Ok(only),
            (Some(_), others) => Err(StoreError::Ambiguous {
                reference: reference.clone(),
                count: others,
            }),
            (None, _) => Err(StoreError::NoSuchJob(reference.clone())),
        }
    }
}

impl crate::ingress::Ingress for Sequence {}
impl crate::ingress::Ingress for LaunchEnv {}
impl crate::ingress::Ingress for StoredEnv {}
impl crate::ingress::Ingress for StoredLeft {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_phases_advance_only_in_lifecycle_order() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&dirs(tmp.path())).unwrap();
        let id: JobId = "0PPPPPPPPPPPPPPP".parse().unwrap();
        store
            .stage(&spec(&id, 1), (&BTreeMap::new(), &LaunchEnv::default()))
            .unwrap();
        store.publish(&id).unwrap();
        let started_at = crate::clock::Timestamp::at_millis(1);
        let running = Phase::Running {
            started_at,
            pid: 42,
            workspace: "workspace".to_owned(),
        };
        assert!(matches!(
            store.set_phase(&id, &running),
            Err(StoreError::InvalidPhaseTransition { .. })
        ));
        assert_eq!(store.phase(&id).unwrap(), Phase::Queued);
        for phase in [
            Phase::Preparing { started_at },
            Phase::Starting {
                started_at,
                workspace: "workspace".to_owned(),
            },
            running,
            Phase::Finished {
                started_at: Some(started_at),
                finished_at: crate::clock::Timestamp::at_millis(2),
                outcome: crate::protocol::Outcome::Succeeded,
            },
        ] {
            store.set_phase(&id, &phase).unwrap();
            assert_eq!(store.phase(&id).unwrap(), phase);
        }
        assert!(matches!(
            store.set_phase(&id, &Phase::Queued),
            Err(StoreError::InvalidPhaseTransition { .. })
        ));
    }

    #[test]
    fn a_job_keeps_its_last_notes_apart_from_its_log() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&dirs(tmp.path())).unwrap();
        let id: JobId = "0NNNNNNNNNNNNNNN".parse().unwrap();
        store
            .stage(&spec(&id, 1), (&BTreeMap::new(), &LaunchEnv::default()))
            .unwrap();
        store.publish(&id).unwrap();
        let mut notes = crate::state_file::open_append(&store.notes_path(&id)).unwrap();
        for n in 0..30 {
            std::io::Write::write_all(&mut notes, format!("note {n}\n").as_bytes()).unwrap();
        }
        let job = store.job(&id).unwrap();
        assert_eq!(job.notes.len(), NOTES_KEPT);
        assert_eq!(job.notes.last().unwrap().to_string(), "note 29");
        assert!(
            crate::state_file::read_bytes(&store.log_path(&id))
                .unwrap()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn a_long_notes_file_does_not_hide_the_last_notes() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&dirs(tmp.path())).unwrap();
        let id: JobId = "0NNNNNNNNNNNNNNN".parse().unwrap();
        store
            .stage(&spec(&id, 1), (&BTreeMap::new(), &LaunchEnv::default()))
            .unwrap();
        store.publish(&id).unwrap();
        let mut notes = crate::state_file::open_append(&store.notes_path(&id)).unwrap();
        let large_line = format!("{}\n", "x".repeat(60 << 10));
        for _ in 0..18 {
            std::io::Write::write_all(&mut notes, large_line.as_bytes()).unwrap();
        }
        std::io::Write::write_all(&mut notes, b"last\n").unwrap();
        assert_eq!(
            store.job(&id).unwrap().notes.last().unwrap().to_string(),
            "last"
        );
    }
    use crate::domain::Concurrency;
    use crate::protocol::{Command, Location};
    use crate::state_file::StateError;

    #[test]
    fn a_job_inherits_only_the_explicit_base_environment_allowlist() {
        let launch = LaunchEnv::from_vars([
            ("PATH".into(), "/bin".into()),
            ("LC_ALL".into(), "C".into()),
            ("ProgramFiles(x86)".into(), "C:/Program Files (x86)".into()),
            ("USERPROFILE".into(), "C:/Users/me".into()),
            ("CI_CANARY".into(), "must-not-leak".into()),
            ("SSH_CONNECTION".into(), "secret-session-detail".into()),
        ]);
        assert_eq!(launch.vars.get("PATH").map(String::as_str), Some("/bin"));
        assert_eq!(launch.vars.get("LC_ALL").map(String::as_str), Some("C"));
        assert_eq!(
            launch.vars.get("ProgramFiles(x86)").map(String::as_str),
            Some("C:/Program Files (x86)")
        );
        assert_eq!(
            launch.vars.get("USERPROFILE").map(String::as_str),
            Some("C:/Users/me")
        );
        assert!(!launch.vars.contains_key("CI_CANARY"));
        assert!(!launch.vars.contains_key("SSH_CONNECTION"));
        if std::env::var_os("CI_CANARY").is_some() {
            assert!(!LaunchEnv::of_this_process().vars.contains_key("CI_CANARY"));
        }
    }

    #[expect(
        clippy::disallowed_methods,
        reason = "the test gives job preparation a raw command with an environment entry to remove"
    )]
    #[test]
    fn job_process_proof_clears_ambient_environment_and_sets_its_identity() {
        let id: JobId = "0AAAAAAAAAAAAAAA".parse().unwrap();
        let launch = Launch {
            vars: BTreeMap::from([(
                "PATH".to_owned(),
                zeroize::Zeroizing::new("/bin".to_owned()),
            )]),
            not_unicode: vec!["UNREADABLE".to_owned()],
        };
        let mut raw = std::process::Command::new("unused");
        raw.env("LEAK", "must-not-reach-the-job");
        let (prepared, omitted) = launch.prepare(raw, &id);
        assert_eq!(format!("{prepared:?}"), "PreparedJobCommand(<redacted>)");
        assert_eq!(omitted, ["UNREADABLE"]);
        let vars: BTreeMap<_, _> = prepared
            .into_command()
            .get_envs()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.map(|text| text.to_string_lossy().into_owned()),
                )
            })
            .collect();
        assert_eq!(
            vars,
            BTreeMap::from([
                ("DOMYJOB".to_owned(), Some("1".to_owned())),
                ("DOMYJOB_JOB_ID".to_owned(), Some(id.as_str().to_owned())),
                ("PATH".to_owned(), Some("/bin".to_owned())),
            ])
        );
    }

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
        Dirs::for_test(root)
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

    #[test]
    fn an_uncertain_outcome_cannot_be_read_as_a_queued_job() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&dirs(tmp.path())).unwrap();
        let id: JobId = "0CCCCCCCCCCCCCCC".parse().unwrap();
        store
            .stage(&spec(&id, 1), (&BTreeMap::new(), &LaunchEnv::default()))
            .unwrap();
        store.publish(&id).unwrap();
        let path = store.job_dir(&id).join("outcome");
        assert!(matches!(
            store.record_outcome_in_place(&id, &Phase::Queued),
            Err(StoreError::InvalidOutcome { .. })
        ));
        crate::state_file::write_bytes(&path, b"{incomplete").unwrap();
        assert!(matches!(store.phase(&id), Err(StoreError::Json { .. })));
        assert!(matches!(store.job(&id), Err(StoreError::Json { .. })));
        crate::state_file::write_json(&path, &Phase::Queued).unwrap();
        assert!(matches!(
            store.phase(&id),
            Err(StoreError::InvalidOutcome { .. })
        ));
    }

    fn staged(store: &Store, id: &JobId, sequence: u64) {
        store
            .stage(
                &spec(id, sequence),
                (&BTreeMap::new(), &LaunchEnv::default()),
            )
            .unwrap();
    }

    #[test]
    fn publication_requires_the_same_lock_as_collection() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&dirs(tmp.path())).unwrap();
        let id: JobId = "0BBBBBBBBBBBBBBB".parse().unwrap();
        staged(&store, &id, 1);
        let path = store.collection_lock_path().display().to_string();
        {
            let _faults = crate::faults::inject(&[("state_file::lock", &path)]);
            store.publish(&id).unwrap_err();
        }
        assert_eq!(store.publication(&id).unwrap(), Publication::Unpublished);
        store.publish(&id).unwrap();
        assert_eq!(store.publication(&id).unwrap(), Publication::Published);
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
            store.publication(&id).unwrap_err();
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
        assert_eq!(store.publication(&id).unwrap(), Publication::Unpublished);
        store.record_start_failure(&id, "no shell").unwrap();
        assert_eq!(
            store.start_failure(&id).unwrap().as_deref(),
            Some("no shell")
        );
        store.publish(&id).unwrap();
        assert_eq!(store.publication(&id).unwrap(), Publication::Published);
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
        crate::state_file::write_bytes(&store.queue_wait_path(&id), b"").unwrap();
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
        assert!(
            crate::state_file::read_bytes(&store.queue_wait_path(&id))
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
        store.force_phase(&id, &written).unwrap();
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
        store.force_phase(&id, &Phase::Queued).unwrap();
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
        if crate::platform::MODES {
            assert!(
                matches!(
                    staged,
                    Err(StoreError::Io(crate::failure::IoFailure {
                        action: "checking",
                        ..
                    }))
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
        assert_eq!(store.queue_mode(&now).unwrap(), QueueMode::Immediate);
        assert_eq!(store.queue_mode(&queued).unwrap(), QueueMode::Ordinary);
    }

    #[test]
    fn queue_order_keeps_only_the_earliest_live_predecessor() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&dirs(tmp.path())).unwrap();
        let [waiting, second, first, later]: [JobId; 4] = [
            "0AAAAAAAAAAAAAAA",
            "0BBBBBBBBBBBBBBB",
            "0CCCCCCCCCCCCCCC",
            "0DDDDDDDDDDDDDDD",
        ]
        .map(|id| id.parse().unwrap());
        let mut alive = Vec::new();
        for (id, sequence) in [(&waiting, 4), (&second, 2), (&first, 1), (&later, 5)] {
            staged(&store, id, sequence);
            store.publish(id).unwrap();
            alive.push(OsLock::exclusive(&store.alive_path(id)).unwrap());
        }
        let requested = spec(&waiting, 4);
        let predecessor = || {
            let admission = store.admission().unwrap();
            let earlier = admission.earliest_waiter(&requested).unwrap();
            admission.release().unwrap();
            earlier
        };
        assert_eq!(
            predecessor(),
            Some(QueuePredecessor::AliveFallback(store.alive_path(&first)))
        );
        let finished = |millis| Phase::Finished {
            started_at: None,
            finished_at: crate::clock::Timestamp::at_millis(millis),
            outcome: crate::protocol::Outcome::Succeeded,
        };
        store.force_phase(&first, &finished(1)).unwrap();
        let queue_lock = OsLock::exclusive(&store.queue_wait_path(&second)).unwrap();
        assert_eq!(
            predecessor(),
            Some(QueuePredecessor::Waiting(store.queue_wait_path(&second)))
        );
        queue_lock.release().unwrap();
        assert_eq!(
            predecessor(),
            Some(QueuePredecessor::AliveFallback(store.alive_path(&second)))
        );
        store.force_phase(&second, &finished(2)).unwrap();
        assert_eq!(predecessor(), None);
        for lock in alive {
            lock.release().unwrap();
        }
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
        store.force_phase(&holding, &running).unwrap();
        store.force_phase(&outside, &running).unwrap();
        store
            .force_phase(
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
        let damaged_slot = OsLock::exclusive(&slots.join("1.lock")).unwrap();
        assert!(matches!(
            store.job(&waiting),
            Err(StoreError::InvalidHolder { .. })
        ));
        crate::state_file::remove_file(&slots.join("1.holder")).unwrap();
        crate::state_file::private_dir(&slots.join("1.holder")).unwrap();
        assert!(matches!(
            store.job(&waiting),
            Err(StoreError::State(StateError::NotFile { .. }))
        ));
        crate::state_file::remove_tree_forcibly(&slots.join("1.holder")).unwrap();
        assert!(store.job(&waiting).unwrap().behind.is_empty());
        damaged_slot.release().unwrap();
        let slot = OsLock::exclusive(&slots.join("3.lock")).unwrap();
        assert_eq!(
            store.job(&waiting).unwrap().behind,
            std::slice::from_ref(&holding)
        );
        assert!(store.job(&holding).unwrap().behind.is_empty());
        slot.release().unwrap();

        for lock in alive {
            lock.release().unwrap();
        }
        assert!(store.job(&waiting).unwrap().behind.is_empty());
    }

    #[test]
    fn only_canonical_bounded_slot_locks_are_considered() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&dirs(tmp.path())).unwrap();
        let slots = store.area("slots");
        let alias = OsLock::exclusive(&slots.join("01.lock")).unwrap();
        let maximum = OsLock::exclusive(&slots.join("63.lock")).unwrap();
        let beyond = OsLock::exclusive(&slots.join("64.lock")).unwrap();
        assert_eq!(store.held_slots().unwrap(), vec![slots.join("63.lock")]);
        alias.release().unwrap();
        maximum.release().unwrap();
        beyond.release().unwrap();
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
        store.force_phase(&id, &finished).unwrap_err();
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
        {
            let tag = store.area("jobs").display().to_string();
            let _faults = crate::faults::inject(&[("store::list", &tag)]);
            assert_eq!(store.resolve(&a.clone().into()).unwrap(), a);
            assert!(matches!(
                store.resolve(&JobRef::parse_loose("0").unwrap()),
                Err(StoreError::Io(_))
            ));
        }
        assert!(matches!(
            store.resolve(&JobRef::parse_loose("0").unwrap()),
            Err(StoreError::Ambiguous { count: 2, .. })
        ));
        assert!(matches!(
            store.resolve(&JobRef::parse_loose("Z").unwrap()),
            Err(StoreError::NoSuchJob(_))
        ));
        assert!(matches!(
            store.resolve(&JobRef::parse_loose("0CCCCCCCCCCCCCCC").unwrap()),
            Err(StoreError::NoSuchJob(_))
        ));
        assert_eq!(store.job(&b).unwrap().supervisor, Supervisor::Gone);
    }
}
