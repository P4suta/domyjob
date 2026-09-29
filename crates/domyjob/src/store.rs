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
    fn at(parent: &Path, job: &JobId) -> Self {
        Self {
            dir: parent.join(job.as_str()),
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

    fn order(&self) -> PathBuf {
        self.dir.join("order")
    }
}

const KEEP_FINISHED: usize = 32;

fn in_use(root: &Path) -> Result<bool, StoreError> {
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        let kind = entry.file_type()?;
        let held = if kind.is_dir() && !kind.is_symlink() {
            entry.file_name() != "workspace" && in_use(&path)?
        } else {
            kind.is_file()
                && path
                    .extension()
                    .is_some_and(|extension| extension == "lock")
                && OsLock::probe(&path)? == Probe::Held
        };
        if held {
            return Ok(true);
        }
    }
    Ok(false)
}

const KEEP_OTHER_FORMATS: usize = 2;

pub(crate) fn prune_other_formats(
    state: &crate::layout::State,
) -> Result<Vec<PathBuf>, StoreError> {
    let current = Store {
        root: state.runner(),
    };
    state_file::private_dir(&current.root)?;
    let mut idle = Vec::new();
    for root in state.other_runners()? {
        let judged = in_use(&root).and_then(|busy| {
            if busy {
                Ok(None)
            } else {
                Ok(Some(Store { root: root.clone() }.opened()?))
            }
        });
        match judged {
            Ok(Some(place)) => idle.push((place, root)),
            Ok(None) => {}
            Err(error) => eprintln!(
                "domyjob node: keeping the job store {} for now: {error}",
                root.display()
            ),
        }
    }
    let mut removed = Vec::new();
    for (_, root) in crate::retention::beyond_newest(idle, KEEP_OTHER_FORMATS) {
        match state_file::set_aside(&root, &current.trash_dir()) {
            Ok(()) => removed.push(root),
            Err(error) => eprintln!(
                "domyjob node: keeping the job store {} for now: {error}",
                root.display()
            ),
        }
    }
    current.empty_trash();
    removed.sort();
    Ok(removed)
}

fn next_opening(state: &crate::layout::State) -> Result<u64, StoreError> {
    let _order = OsLock::exclusive(&state.runner_order_lock())?;
    let next = Store::read_number(&state.runner_order())?.saturating_add(1);
    state_file::write_bytes(&state.runner_order(), next.to_string().as_bytes())?;
    Ok(next)
}

impl Store {
    pub(crate) fn open() -> Result<Self, StoreError> {
        let state = crate::layout::State::here()?;
        let store = Self {
            root: state.runner(),
        };
        for dir in [
            store.root.clone(),
            store.jobs_dir(),
            store.staging_dir(),
            store.incoming_dir(),
            store.trash_dir(),
        ] {
            state_file::private_dir(&dir)?;
        }
        let place = next_opening(&state)?;
        state_file::write_bytes(&store.opened_path(), place.to_string().as_bytes())?;
        Ok(store)
    }

    fn opened_path(&self) -> PathBuf {
        self.root.join("opened")
    }

    fn opened(&self) -> Result<u64, StoreError> {
        Self::read_number(&self.opened_path())
    }

    fn jobs_dir(&self) -> PathBuf {
        self.root.join("jobs")
    }

    fn staging_dir(&self) -> PathBuf {
        self.root.join("staging")
    }

    fn incoming_dir(&self) -> PathBuf {
        self.root.join("incoming")
    }

    fn trash_dir(&self) -> PathBuf {
        self.root.join("trash")
    }

    fn empty_trash(&self) {
        match state_file::empty(&self.trash_dir()) {
            Ok(leftovers) => {
                for leftover in leftovers {
                    eprintln!(
                        "domyjob node: {} cannot be removed yet: {}",
                        leftover.path.display(),
                        leftover.error
                    );
                }
            }
            Err(error) => eprintln!("domyjob node: the trash cannot be emptied yet: {error}"),
        }
    }

    fn incoming_lock(&self) -> PathBuf {
        self.root.join("incoming.lock")
    }

    fn sequence(&self) -> PathBuf {
        self.root.join("sequence")
    }

    fn read_number(path: &Path) -> Result<u64, StoreError> {
        match state_file::read_bytes(path)? {
            None => Ok(0),
            Some(bytes) => std::str::from_utf8(&bytes)
                .map_err(|_invalid| StoreError::Corrupt)?
                .trim()
                .parse()
                .map_err(|_invalid| StoreError::Corrupt),
        }
    }

    fn next_sequence(&self) -> Result<u64, StoreError> {
        let next = Self::read_number(&self.sequence())?
            .checked_add(1)
            .ok_or(StoreError::Capacity)?;
        state_file::write_bytes(&self.sequence(), next.to_string().as_bytes())?;
        Ok(next)
    }

    fn reclaim(&self) -> Result<(), StoreError> {
        let mut finished = Vec::new();
        for job in self.list()? {
            if self.status(&job)?.kind() == PhaseKind::Finished {
                finished.push((Self::read_number(&self.paths(&job).order())?, job));
            }
        }
        finished.sort();
        let excess = finished.len().saturating_sub(KEEP_FINISHED);
        for (_, job) in finished.into_iter().take(excess) {
            if let Err(error) = self.clean_one(&job) {
                eprintln!(
                    "domyjob node: keeping finished job {} for now: {error}",
                    job.as_str()
                );
            }
        }
        self.empty_trash();
        Ok(())
    }

    fn staged(&self, job: &JobId) -> JobPaths {
        JobPaths::at(&self.staging_dir(), job)
    }

    fn paths(&self, job: &JobId) -> JobPaths {
        JobPaths::at(&self.jobs_dir(), job)
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
            OrphanKind::Staging => self.staging_dir(),
            OrphanKind::Incoming => self.incoming_dir(),
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
                OrphanKind::Staging => state_file::set_aside(&entry.path(), &self.trash_dir())?,
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
        let lock = OsLock::exclusive(&self.incoming_lock())?;
        self.cleanup_orphans(OrphanKind::Incoming)?;
        let id = Self::new_job_id()?;
        let path = self.incoming_dir().join(id.as_str());
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

    fn extract_archive(staged: &JobPaths, archive: &ReceivedArchive) -> Result<(), StoreError> {
        let workspace = staged.workspace();
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
        let staged = self.staged(job);
        state_file::private_dir(&staged.dir)?;
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
        state_file::write_bytes(&staged.spec(), wire::payload(&encoded)?)?;
        let initial = serde_json::to_vec(&JobState::accepted()).map_err(WireError::from)?;
        state_file::write_bytes(&staged.state(), &initial)?;
        state_file::write_bytes(
            &staged.order(),
            self.next_sequence()?.to_string().as_bytes(),
        )?;
        state_file::publish_dir(&staged.dir, &self.paths(job).dir)?;
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
        self.reclaim()?;
        if self.list()?.len() >= 1024 {
            return Err(StoreError::Capacity);
        }
        self.stage(&job, request, archive)?;
        Ok(job)
    }

    pub(crate) fn list(&self) -> Result<Vec<JobId>, StoreError> {
        let mut jobs = Vec::new();
        for entry in std::fs::read_dir(self.jobs_dir())? {
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
        state_file::set_aside(&paths.dir, &self.trash_dir())?;
        Ok(true)
    }

    pub(crate) fn clean(&self, target: &CleanTarget) -> Result<u16, StoreError> {
        let _admission = OsLock::exclusive(&self.admission_lock())?;
        match target {
            CleanTarget::Job(job) => {
                let cleaned = self.clean_one(job)?;
                self.empty_trash();
                if cleaned {
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
                self.empty_trash();
                Ok(count)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use domyjob_core::domain::{Command, JobId, RemoteText, SubmissionId};
    use domyjob_core::state::{Event, JobState, Outcome};
    use domyjob_core::wire::{self, Input, Request, Snapshot};

    use super::{Store, state_file};

    fn shown(path: &Path) -> String {
        path.components()
            .map(|part| part.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/")
    }

    fn specimen() -> String {
        let store = Store {
            root: PathBuf::from("runner"),
        };
        let job = JobId::try_from("0".repeat(32)).unwrap();
        let mut lines: Vec<String> = [
            store.jobs_dir(),
            store.staging_dir(),
            store.incoming_dir(),
            store.trash_dir(),
            store.incoming_lock(),
            store.admission_lock(),
            store.sequence(),
            store.opened_path(),
        ]
        .iter()
        .map(|path| format!("path {}", shown(path)))
        .collect();
        for paths in [store.paths(&job), store.staged(&job)] {
            for path in [
                paths.spec(),
                paths.state(),
                paths.state_lock(),
                paths.alive_lock(),
                paths.launch_lock(),
                paths.cancel(),
                paths.log(),
                paths.supervisor_log(),
                paths.workspace(),
                paths.order(),
            ] {
                lines.push(format!("path {}", shown(&path)));
            }
        }
        for input in [
            Input::Home,
            Input::Snapshot(Snapshot::new(1, "0".repeat(64)).unwrap()),
        ] {
            let request = Request::Run {
                submission: SubmissionId::try_from("1".repeat(32)).unwrap(),
                command: Command::try_from(vec!["cargo".to_owned(), "test".to_owned()]).unwrap(),
                input,
            };
            let framed = wire::frame(&request).unwrap();
            lines.push(format!(
                "request {}",
                String::from_utf8(wire::payload(&framed).unwrap().to_vec()).unwrap()
            ));
        }
        let reason = RemoteText::try_from("no such program".to_owned()).unwrap();
        let mut running = JobState::accepted();
        let mut states = vec![running.clone()];
        running.advance(&Event::Starting).unwrap();
        states.push(running.clone());
        let mut launch_failed = running.clone();
        launch_failed
            .advance(&Event::LaunchFailed { reason })
            .unwrap();
        states.push(launch_failed);
        running.advance(&Event::Spawned { pid: 42 }).unwrap();
        states.push(running.clone());
        for ending in [
            Event::Exited { code: 0 },
            Event::Exited { code: 3 },
            Event::SupervisorGone,
            Event::Killed,
        ] {
            let mut finished = running.clone();
            finished.advance(&ending).unwrap();
            states.push(finished);
        }
        for state in states {
            lines.push(format!("state {}", serde_json::to_string(&state).unwrap()));
        }
        lines.join("\n")
    }

    #[test]
    fn the_runner_format_is_its_specimen() {
        crate::formats::check("runner", &specimen());
    }

    fn store(temporary: &tempfile::TempDir) -> Store {
        let root = temporary.path().join("state");
        for directory in ["", "jobs", "staging", "incoming", "trash"] {
            state_file::private_dir(&root.join(directory)).expect("private store directory");
        }
        Store { root }
    }

    fn admit_finished(store: &Store, submission: String) -> JobId {
        let submission = SubmissionId::try_from(submission).unwrap();
        let request = Request::Run {
            submission: submission.clone(),
            command: Command::try_from(vec!["true".to_owned()]).unwrap(),
            input: Input::Home,
        };
        let job = store.reserve(&submission, &request, None).unwrap();
        store
            .finish_launch_failure(&job, RemoteText::try_from("done".to_owned()).unwrap())
            .unwrap();
        job
    }

    #[test]
    fn stores_of_other_formats_go_once_idle_and_older_than_the_last_opened() {
        let temporary = tempfile::tempdir().unwrap();
        let state = crate::layout::State::at(&temporary.path().join("state"));
        let other = |digit: u64| {
            temporary
                .path()
                .join("state")
                .join(format!("runner-{digit:016}"))
        };
        for (digit, opened) in [
            (1, Some(1)),
            (2, Some(2)),
            (3, Some(3)),
            (4, Some(4)),
            (5, None),
        ] {
            let root = other(digit);
            state_file::private_dir(&root.join("jobs").join("a")).unwrap();
            crate::testing::write(
                &root
                    .join("jobs")
                    .join("a")
                    .join("workspace")
                    .join("Cargo.lock"),
                "",
            );
            if let Some(place) = opened {
                state_file::write_bytes(&root.join("opened"), format!("{place}").as_bytes())
                    .unwrap();
            }
        }
        let running =
            crate::lock::OsLock::exclusive(&other(1).join("jobs").join("a").join("alive.lock"))
                .unwrap();
        assert_eq!(
            super::prune_other_formats(&state).unwrap(),
            [other(2), other(5)]
        );
        for kept in [1, 3, 4] {
            assert!(crate::testing::is_dir(&other(kept)));
        }
        drop(running);
        assert_eq!(super::prune_other_formats(&state).unwrap(), [other(1)]);
    }

    #[test]
    fn admissions_keep_only_the_newest_finished_jobs() {
        let temporary = tempfile::tempdir().unwrap();
        let store = store(&temporary);
        let digits: Vec<char> = "0123456789abcdef".chars().collect();
        let mut admitted = Vec::new();
        for first in &digits {
            for second in digits.iter().take(3) {
                admitted.push(admit_finished(
                    &store,
                    format!("{first}{second}").repeat(16),
                ));
            }
        }
        let last = admit_finished(&store, "f".repeat(32));
        admitted.push(last);
        let kept = store.list().unwrap();
        assert_eq!(kept.len(), super::KEEP_FINISHED.saturating_add(1));
        let newest: std::collections::BTreeSet<_> = admitted
            .iter()
            .rev()
            .take(super::KEEP_FINISHED.saturating_add(1))
            .cloned()
            .collect();
        assert_eq!(
            kept.into_iter().collect::<std::collections::BTreeSet<_>>(),
            newest
        );
    }

    #[test]
    fn a_finished_job_whose_files_cannot_be_removed_never_stops_admission() {
        let temporary = tempfile::tempdir().unwrap();
        let store = store(&temporary);
        let admit = |index: usize| admit_finished(&store, format!("{index:032x}"));
        let oldest = admit(0);
        let protected = store.paths(&oldest).workspace().join("pkg");
        crate::testing::write(&protected.join("package.json"), "{}");
        let permissions = crate::testing::protect(&protected);
        for index in 1..=super::KEEP_FINISHED {
            admit(index);
        }
        let newest = admit(super::KEEP_FINISHED.saturating_add(1));
        let kept = store.list().unwrap();
        assert!(!kept.contains(&oldest), "the oldest job left in one step");
        assert!(kept.contains(&newest));
        assert_eq!(kept.len(), super::KEEP_FINISHED.saturating_add(1));
        for leftover in state_file::empty(&store.trash_dir()).unwrap() {
            crate::testing::restore(
                &leftover.path.join("workspace").join("pkg"),
                permissions.clone(),
            );
        }
        assert!(state_file::empty(&store.trash_dir()).unwrap().is_empty());
    }

    #[test]
    fn failed_supervisor_start_is_terminal_and_idempotent() {
        for starting in [false, true] {
            let temporary = tempfile::tempdir().expect("temporary state root");
            let root = temporary.path().join("state");
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
