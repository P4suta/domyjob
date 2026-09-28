use std::collections::BTreeSet;
use std::io::{ErrorKind, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use domyjob_core::domain::RelativePath;
use domyjob_core::domain::{JobId, RemoteText, SubmissionId};
use domyjob_core::ingress;
use domyjob_core::state::{Event, InvalidTransition, JobState, PhaseKind};
use domyjob_core::wire::{self, CleanTarget, Request, WireError};
use thiserror::Error;

use crate::lock::{LockError, OsLock, Probe};
use crate::platform;
use crate::state_io::{self as state_file, StateError};
use crate::workspace::{Rooted, WorkspaceError};

#[derive(Debug, Error)]
pub(crate) enum StoreError {
    #[error(transparent)]
    State(#[from] StateError),
    #[error(transparent)]
    Lock(#[from] LockError),
    #[error(transparent)]
    Wire(#[from] WireError),
    #[error(transparent)]
    Transition(#[from] InvalidTransition),
    #[error(transparent)]
    Workspace(#[from] WorkspaceError),
    #[error("job state I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("the operating system could not provide a job identifier: {0}")]
    Entropy(getrandom::Error),
    #[error("the job identifier is absent or its record is incomplete")]
    Missing,
    #[error("the same submission identifier was used for different work")]
    Conflict,
    #[error("job capacity is full")]
    Capacity,
    #[error("an active job cannot be cleaned")]
    Active,
    #[error("the job store contains an invalid identifier or record")]
    Corrupt,
    #[error("the source archive contains a non-portable or unsafe entry")]
    ArchiveEntry,
    #[error("the source archive is missing or does not match its declared size and digest")]
    ArchiveMismatch,
}

#[derive(Debug, Clone)]
pub(crate) struct Store {
    root: PathBuf,
}

#[derive(Debug)]
pub(crate) struct ReceivedArchive {
    path: PathBuf,
    descriptor: wire::Snapshot,
    _lock: OsLock,
}

impl Drop for ReceivedArchive {
    fn drop(&mut self) {
        let _removed = state_file::remove_file(&self.path);
    }
}

#[derive(Debug, Clone)]
struct JobPaths {
    dir: PathBuf,
}

#[derive(Debug, Clone, Copy)]
enum OrphanKind {
    Staging,
    Incoming,
}

impl JobPaths {
    fn at(root: &Path, job: &JobId) -> Self {
        Self {
            dir: root.join("jobs").join(job.as_str()),
        }
    }

    fn spec(&self) -> PathBuf {
        self.dir.join("request.json")
    }

    fn state(&self) -> PathBuf {
        self.dir.join("state.json")
    }

    fn state_lock(&self) -> PathBuf {
        self.dir.join("state.lock")
    }

    fn alive_lock(&self) -> PathBuf {
        self.dir.join("alive.lock")
    }

    fn launch_lock(&self) -> PathBuf {
        self.dir.join("launch.lock")
    }

    fn cancel(&self) -> PathBuf {
        self.dir.join("cancel")
    }

    pub(crate) fn log(&self) -> PathBuf {
        self.dir.join("output.log")
    }

    fn supervisor_log(&self) -> PathBuf {
        self.dir.join("supervisor.log")
    }

    fn workspace(&self) -> PathBuf {
        self.dir.join("workspace")
    }
}

impl Store {
    pub(crate) fn open() -> Result<Self, StoreError> {
        let root = platform::state()?.join("v1");
        for dir in [
            &root,
            &root.join("jobs"),
            &root.join("staging"),
            &root.join("incoming"),
        ] {
            state_file::private_dir(dir)?;
        }
        Ok(Self { root })
    }

    fn paths(&self, job: &JobId) -> JobPaths {
        JobPaths::at(&self.root, job)
    }

    fn verified_directory(path: PathBuf) -> Result<PathBuf, StoreError> {
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_dir() && !platform::reparse_point(&metadata) => {
                state_file::private_dir(&path)?;
                Ok(path)
            }
            Ok(_other) => Err(StoreError::Corrupt),
            Err(error) if error.kind() == ErrorKind::NotFound => Err(StoreError::Missing),
            Err(error) => Err(StoreError::Io(error)),
        }
    }

    fn verify_job(&self, job: &JobId) -> Result<JobPaths, StoreError> {
        let paths = self.paths(job);
        Self::verified_directory(paths.dir.clone())?;
        Ok(paths)
    }

    fn admission_lock(&self) -> PathBuf {
        self.root.join("admission.lock")
    }

    fn new_job_id() -> Result<JobId, StoreError> {
        let mut entropy = [0_u8; 16];
        getrandom::fill(&mut entropy).map_err(StoreError::Entropy)?;
        let text = format!("{:032x}", u128::from_be_bytes(entropy));
        JobId::try_from(text).map_err(|_invalid| StoreError::Corrupt)
    }

    fn read_request(&self, job: &JobId) -> Result<Request, StoreError> {
        let bytes =
            state_file::read_bytes(&self.verify_job(job)?.spec())?.ok_or(StoreError::Missing)?;
        Ok(ingress::stored_request(&bytes)?)
    }

    fn read_state(&self, job: &JobId) -> Result<JobState, StoreError> {
        let bytes =
            state_file::read_bytes(&self.verify_job(job)?.state())?.ok_or(StoreError::Missing)?;
        Ok(ingress::stored_job(&bytes)?)
    }

    fn write_state(&self, job: &JobId, state: &JobState) -> Result<(), StoreError> {
        let bytes = serde_json::to_vec(state).map_err(WireError::from)?;
        state_file::write_bytes(&self.paths(job).state(), &bytes)?;
        Ok(())
    }

    fn cleanup_orphans(&self, kind: OrphanKind) -> Result<(), StoreError> {
        let directory = match kind {
            OrphanKind::Staging => self.root.join("staging"),
            OrphanKind::Incoming => self.root.join("incoming"),
        };
        for (index, entry) in std::fs::read_dir(directory)?.enumerate() {
            if index >= 1024 {
                return Err(StoreError::Capacity);
            }
            let entry = entry?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_name| StoreError::Corrupt)?;
            JobId::try_from(name).map_err(|_invalid| StoreError::Corrupt)?;
            match kind {
                OrphanKind::Staging => state_file::remove_dir_all(&entry.path())?,
                OrphanKind::Incoming => {
                    let Some(file) = state_file::open_read(&entry.path())? else {
                        return Err(StoreError::Corrupt);
                    };
                    drop(file);
                    state_file::remove_file(&entry.path())?;
                }
            }
        }
        Ok(())
    }

    pub(crate) fn receive_archive(
        &self,
        input: &mut impl Read,
        snapshot: &wire::Snapshot,
    ) -> Result<ReceivedArchive, StoreError> {
        let lock = OsLock::exclusive(&self.root.join("incoming.lock"))?;
        self.cleanup_orphans(OrphanKind::Incoming)?;
        let id = Self::new_job_id()?;
        let path = self.root.join("incoming").join(id.as_str());
        state_file::create_empty(&path)?;
        let mut file = state_file::open_append(&path)?;
        let received = ReceivedArchive {
            path,
            descriptor: snapshot.clone(),
            _lock: lock,
        };
        let mut hasher = blake3::Hasher::new();
        let mut remaining = snapshot.bytes();
        let mut buffer = [0_u8; 16_384];
        while remaining != 0 {
            let wanted =
                usize::try_from(remaining.min(16_384)).map_err(|_length| StoreError::Capacity)?;
            let chunk = buffer.get_mut(..wanted).ok_or(StoreError::Capacity)?;
            let count = input.read(chunk)?;
            if count == 0 {
                return Err(StoreError::ArchiveMismatch);
            }
            file.write_all(chunk.get(..count).ok_or(StoreError::ArchiveMismatch)?)?;
            hasher.update(chunk.get(..count).ok_or(StoreError::ArchiveMismatch)?);
            remaining = remaining
                .saturating_sub(u64::try_from(count).map_err(|_count| StoreError::Capacity)?);
        }
        file.sync_all()?;
        drop(file);
        if hasher.finalize().to_hex().as_str() != snapshot.digest() {
            return Err(StoreError::ArchiveMismatch);
        }
        Ok(received)
    }

    fn extract_archive(staged: &Path, archive: &ReceivedArchive) -> Result<(), StoreError> {
        let workspace = staged.join("workspace");
        state_file::private_dir(&workspace)?;
        let rooted = Rooted::open(&workspace)?;
        let source = state_file::open_read(&archive.path)?.ok_or(StoreError::ArchiveMismatch)?;
        let mut tar = tar::Archive::new(source);
        let mut paths = BTreeSet::new();
        let mut path_bytes = 0_usize;
        let mut content_bytes = 0_u64;
        for entry in tar.entries()? {
            let mut entry = entry?;
            if paths.len() >= 100_000 || !entry.header().entry_type().is_file() {
                return Err(StoreError::ArchiveEntry);
            }
            let path = entry.path()?;
            let text = path.to_str().ok_or(StoreError::ArchiveEntry)?;
            let validated = RelativePath::try_from(text.to_owned())
                .map_err(|_invalid| StoreError::ArchiveEntry)?;
            path_bytes = path_bytes.saturating_add(validated.as_str().len());
            if path_bytes > 16_777_216 || !paths.insert(validated.as_str().to_lowercase()) {
                return Err(StoreError::ArchiveEntry);
            }
            let size = entry.header().size()?;
            content_bytes = content_bytes.saturating_add(size);
            if content_bytes > wire::MAX_SNAPSHOT_BYTES {
                return Err(StoreError::ArchiveEntry);
            }
            rooted.make_parents(&validated)?;
            let executable = entry.header().mode()? & 0o111 != 0;
            let mut output = rooted.create_file(&validated, executable)?;
            if std::io::copy(&mut entry, &mut output)? != size {
                return Err(StoreError::ArchiveMismatch);
            }
            output.sync_all()?;
        }
        Ok(())
    }

    fn stage(
        &self,
        job: &JobId,
        request: &Request,
        archive: Option<&ReceivedArchive>,
    ) -> Result<(), StoreError> {
        let staged = self.root.join("staging").join(job.as_str());
        state_file::private_dir(&staged)?;
        match (request, archive) {
            (
                Request::Run {
                    input: wire::Input::Snapshot(expected),
                    ..
                },
                Some(archive),
            ) if archive.descriptor == *expected => Self::extract_archive(&staged, archive)?,
            (
                Request::Run {
                    input: wire::Input::Home,
                    ..
                },
                None,
            ) => {}
            (
                Request::Run {
                    input: wire::Input::Snapshot(_),
                    ..
                },
                None | Some(_),
            )
            | (
                Request::Run {
                    input: wire::Input::Home,
                    ..
                },
                Some(_),
            )
            | (
                Request::Hello
                | Request::Chat(_)
                | Request::List
                | Request::Status { .. }
                | Request::Logs { .. }
                | Request::Wait { .. }
                | Request::Kill { .. }
                | Request::Clean { .. },
                _,
            ) => return Err(StoreError::ArchiveMismatch),
        }
        let encoded = wire::frame(request)?;
        state_file::write_bytes(&staged.join("request.json"), wire::payload(&encoded)?)?;
        let initial = serde_json::to_vec(&JobState::accepted()).map_err(WireError::from)?;
        state_file::write_bytes(&staged.join("state.json"), &initial)?;
        state_file::publish_dir(&staged, &self.paths(job).dir)?;
        Ok(())
    }

    pub(crate) fn reserve(
        &self,
        submission: &SubmissionId,
        request: &Request,
        archive: Option<&ReceivedArchive>,
    ) -> Result<JobId, StoreError> {
        let _admission = OsLock::exclusive(&self.admission_lock())?;
        self.cleanup_orphans(OrphanKind::Staging)?;
        let job = JobId::from(submission);
        match std::fs::symlink_metadata(self.paths(&job).dir) {
            Ok(_existing) => {
                self.verify_job(&job)?;
                if self.read_request(&job)? != *request {
                    return Err(StoreError::Conflict);
                }
                return Ok(job);
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(StoreError::Io(error)),
        }
        if self.list()?.len() >= 1024 {
            return Err(StoreError::Capacity);
        }
        self.stage(&job, request, archive)?;
        Ok(job)
    }

    pub(crate) fn list(&self) -> Result<Vec<JobId>, StoreError> {
        let mut jobs = Vec::new();
        for entry in std::fs::read_dir(self.root.join("jobs"))? {
            let entry = entry?;
            if jobs.len() >= 1024 {
                return Err(StoreError::Capacity);
            }
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_name| StoreError::Corrupt)?;
            let job = JobId::try_from(name).map_err(|_invalid| StoreError::Corrupt)?;
            state_file::private_dir(&entry.path())?;
            jobs.push(job);
        }
        jobs.sort();
        Ok(jobs)
    }

    pub(crate) fn transition(&self, job: &JobId, event: &Event) -> Result<JobState, StoreError> {
        let _lock = OsLock::exclusive(&self.verify_job(job)?.state_lock())?;
        let mut state = self.read_state(job)?;
        state.advance(event)?;
        self.write_state(job, &state)?;
        Ok(state)
    }

    pub(crate) fn finish_launch_failure(
        &self,
        job: &JobId,
        reason: RemoteText,
    ) -> Result<(), StoreError> {
        let _lock = OsLock::exclusive(&self.verify_job(job)?.state_lock())?;
        let mut state = self.read_state(job)?;
        match state.kind() {
            PhaseKind::Accepted | PhaseKind::Starting => {
                state.advance(&Event::LaunchFailed { reason })?;
                self.write_state(job, &state)?;
            }
            PhaseKind::Running | PhaseKind::Finished => {}
        }
        Ok(())
    }

    pub(crate) fn status(&self, job: &JobId) -> Result<JobState, StoreError> {
        let _lock = OsLock::exclusive(&self.verify_job(job)?.state_lock())?;
        let mut state = self.read_state(job)?;
        match state.kind() {
            PhaseKind::Accepted | PhaseKind::Finished => Ok(state),
            PhaseKind::Starting | PhaseKind::Running => {
                match OsLock::probe(&self.paths(job).alive_lock())? {
                    Probe::Held => Ok(state),
                    Probe::Absent | Probe::Free => {
                        state.advance(&Event::SupervisorGone)?;
                        self.write_state(job, &state)?;
                        Ok(state)
                    }
                }
            }
        }
    }

    pub(crate) fn worker_lock(&self, job: &JobId) -> Result<Option<OsLock>, StoreError> {
        Ok(OsLock::try_exclusive(&self.verify_job(job)?.alive_lock())?)
    }

    pub(crate) fn launch_lock(&self, job: &JobId) -> Result<OsLock, StoreError> {
        Ok(OsLock::exclusive(&self.verify_job(job)?.launch_lock())?)
    }

    pub(crate) fn request_cancel(&self, job: &JobId) -> Result<(), StoreError> {
        state_file::write_bytes(&self.verify_job(job)?.cancel(), &[])?;
        Ok(())
    }

    pub(crate) fn cancel_requested(&self, job: &JobId) -> Result<bool, StoreError> {
        Ok(state_file::open_read(&self.verify_job(job)?.cancel())?.is_some())
    }

    pub(crate) fn watch_dir(&self, job: &JobId) -> Result<PathBuf, StoreError> {
        Ok(self.verify_job(job)?.dir)
    }

    pub(crate) fn request(&self, job: &JobId) -> Result<Request, StoreError> {
        self.read_request(job)
    }

    pub(crate) fn log_path(&self, job: &JobId) -> PathBuf {
        self.paths(job).log()
    }

    pub(crate) fn supervisor_log_path(&self, job: &JobId) -> Result<PathBuf, StoreError> {
        Ok(self.verify_job(job)?.supervisor_log())
    }

    pub(crate) fn workspace(&self, job: &JobId) -> Result<PathBuf, StoreError> {
        Self::verified_directory(self.verify_job(job)?.workspace())
    }

    pub(crate) fn log_tail(&self, job: &JobId) -> Result<(RemoteText, u64), StoreError> {
        let path = self.verify_job(job)?.log();
        let Some(mut file) = state_file::open_read(&path)? else {
            return Ok((
                RemoteText::try_from(String::new()).map_err(|_invalid| StoreError::Corrupt)?,
                0,
            ));
        };
        let length = file.metadata()?.len();
        let take = length.min(16_384);
        let omitted = length.saturating_sub(take);
        file.seek(SeekFrom::Start(omitted))?;
        let size = usize::try_from(take).map_err(|_size| StoreError::Capacity)?;
        let mut bytes = vec![0_u8; size];
        file.read_exact(&mut bytes)?;
        let text = String::from_utf8_lossy(&bytes).replace("\r\n", "\n");
        Ok((
            RemoteText::try_from(text).map_err(|_invalid| StoreError::Corrupt)?,
            omitted,
        ))
    }

    pub(crate) fn wait(&self, job: &JobId) -> Result<JobState, StoreError> {
        let lock = OsLock::exclusive(&self.verify_job(job)?.alive_lock())?;
        drop(lock);
        self.status(job)
    }

    fn clean_one(&self, job: &JobId) -> Result<bool, StoreError> {
        let launch = self.launch_lock(job)?;
        if self.status(job)?.kind() != PhaseKind::Finished {
            return Ok(false);
        }
        let paths = self.verify_job(job)?;
        let alive = OsLock::exclusive(&paths.alive_lock())?;
        drop(alive);
        drop(launch);
        state_file::remove_dir_all(&paths.dir)?;
        Ok(true)
    }

    pub(crate) fn clean(&self, target: &CleanTarget) -> Result<u16, StoreError> {
        let _admission = OsLock::exclusive(&self.admission_lock())?;
        match target {
            CleanTarget::Job(job) => {
                if self.clean_one(job)? {
                    Ok(1)
                } else {
                    Err(StoreError::Active)
                }
            }
            CleanTarget::Finished => {
                let mut count = 0_u16;
                for job in self.list()? {
                    if self.clean_one(&job)? {
                        count = count.checked_add(1).ok_or(StoreError::Capacity)?;
                    }
                }
                Ok(count)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use domyjob_core::domain::{Command, RemoteText, SubmissionId};
    use domyjob_core::state::{Event, Outcome};
    use domyjob_core::wire::{Input, Request};

    use super::{Store, state_file};

    #[test]
    fn failed_supervisor_start_is_terminal_and_idempotent() {
        for starting in [false, true] {
            let temporary = tempfile::tempdir().expect("temporary state root");
            let root = temporary.path().join("v1");
            for directory in ["", "jobs", "staging", "incoming"] {
                state_file::private_dir(&root.join(directory)).expect("private store directory");
            }
            let store = Store { root };
            let submission = SubmissionId::try_from("1".repeat(32)).expect("submission ID");
            let request = Request::Run {
                submission: submission.clone(),
                command: Command::try_from(vec!["missing".to_owned()]).expect("command"),
                input: Input::Home,
            };
            let job = store
                .reserve(&submission, &request, None)
                .expect("reserved job");
            if starting {
                store.transition(&job, &Event::Starting).expect("start");
            }
            let reason = RemoteText::try_from("worker initialization failed".to_owned())
                .expect("failure reason");
            store
                .finish_launch_failure(&job, reason.clone())
                .expect("terminal launch failure");
            let outcome = Outcome::LaunchFailed { reason };
            assert_eq!(
                store.wait(&job).expect("completed job").outcome(),
                Some(&outcome)
            );
            store
                .finish_launch_failure(
                    &job,
                    RemoteText::try_from("later failure".to_owned()).expect("later reason"),
                )
                .expect("repeat failure report");
            assert_eq!(
                store.status(&job).expect("stable state").outcome(),
                Some(&outcome)
            );
        }
    }
}
