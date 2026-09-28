use std::io::{BufRead, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::audit::{AuditLog, Verdict};
use crate::authz::{
    self, Authorized, AuthorizedClean, AuthorizedConfigure, AuthorizedKill, AuthorizedRetry,
    AuthorizedSubmission, CommandAction, CommandAuthority, Commanded, Principal, Queried, Relation,
    Routed,
};
use crate::bounded::ScanFlow;
use crate::cas::{Cas, CasError};
use crate::clock::Timestamp;
use crate::control::{Order, Session};
use crate::domain::{BlobId, Invalid, JobId, JobRef};
use crate::lock::{LockError, SlotIndex};
use crate::paths::Dirs;
use crate::proc::{self, ProcError};
use crate::protocol::{
    Follow, Frame, Hello, Job, Location, Phase, PhaseKind, Refusal, RefusalCode, Reply, Request,
    Source, Spec, VERSION,
};
use crate::store::{Publication, Store, StoreError};
use crate::terminal::RemoteText;

impl JobId {
    fn generate() -> Result<Self, Invalid> {
        Self::try_from(crate::domain::random_crockford_id()?)
    }
}

pub trait Input: BufRead + Send + 'static {}

impl<T: BufRead + Send + 'static> Input for T {}

#[derive(Debug, thiserror::Error)]
pub enum NodeError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Cas(#[from] CasError),
    #[error(transparent)]
    Proc(#[from] ProcError),
    #[error(transparent)]
    Invalid(#[from] Invalid),
    #[error(transparent)]
    Denied(#[from] authz::Denied),
    #[error(transparent)]
    Lock(#[from] LockError),
    #[error(transparent)]
    Audit(#[from] crate::audit::AuditError),
    #[error(transparent)]
    State(#[from] crate::state_file::StateError),
    #[error("reading the request: {0}")]
    Input(std::io::Error),
    #[error("writing the reply: {0}")]
    Output(std::io::Error),
    #[error("the request is not valid: {0}")]
    Request(serde_json::Error),
    #[error("{0} is missing from the submitted snapshot")]
    Incomplete(String),
    #[error("the snapshot sent blob {got}, expected {expected}")]
    UnexpectedBlob { expected: BlobId, got: BlobId },
    #[error("a list request may return at most {0} jobs")]
    ListLimit(u32),
    #[error("more than {0} unreadable jobs prevent a complete list")]
    TooManyUnreadable(usize),
    #[error("CAS collection cannot partition its {0}-entry reachability budget any further")]
    CollectionBudget(usize),
    #[error("measuring {path}: a directory tree exceeds the {limit}-level depth budget")]
    MeasureDepth { path: PathBuf, limit: usize },
    #[error("job {0} has no workspace to fetch from")]
    NoWorkspace(JobId),
    #[error("all {0} warm workspace slots are locked")]
    WorkspaceSlotsFull(usize),
    #[error(
        "job {job}'s workspace has since been filled by job {by}, so only what {job} changed is kept; `domyjob pull` still brings that back"
    )]
    Reused { job: JobId, by: String },
    #[error("job {0} has not finished; its changes can be pulled once it has")]
    Unfinished(JobId),
    #[error("this machine is paused and takes no new jobs until it is resumed")]
    Paused,
    #[error(
        "this machine already has {0} unfinished jobs; wait for one to finish before submitting another"
    )]
    JobCapacity(usize),
    #[error(transparent)]
    Workspace(#[from] crate::workspace::WorkspaceError),
    #[error(transparent)]
    Snapshot(#[from] crate::snapshot::SnapshotError),
    #[error(transparent)]
    Io(#[from] crate::failure::IoFailure),
    #[error(transparent)]
    Scan(#[from] crate::logscan::ScanError),
    #[error("the {0} request streams its answer and cannot be answered in one reply")]
    Misrouted(&'static str),
    #[error("the supervisor could not start: {0}")]
    NotStarted(RemoteText),
    #[error("job {0} already has a supervisor")]
    AlreadySupervised(JobId),
    #[error(
        "job {0} may already have run, so it cannot be retried automatically; submit a new job only after checking its effects"
    )]
    UnsafeRetry(JobId),
    #[error("the queue lost every slot it was waiting for")]
    QueueClosed,
    #[error("the {0} thread panicked")]
    Panicked(&'static str),
    #[error("talking to the job's supervisor: {0}")]
    Control(std::io::Error),
}

const fn store_refusal(error: &StoreError) -> RefusalCode {
    match error {
        StoreError::NoSuchJob(_) => RefusalCode::NoSuchJob,
        StoreError::Ambiguous { .. } => RefusalCode::AmbiguousJob,
        StoreError::Io(_)
        | StoreError::Json { .. }
        | StoreError::State(_)
        | StoreError::Lock(_)
        | StoreError::Exists(_)
        | StoreError::InvalidHolder { .. }
        | StoreError::InvalidOutcome { .. }
        | StoreError::InvalidPhaseTransition { .. }
        | StoreError::Invalid(_) => RefusalCode::Storage,
    }
}

const fn cas_refusal(error: &CasError) -> RefusalCode {
    match error {
        CasError::Missing(_) => RefusalCode::MissingContent,
        CasError::Io(_)
        | CasError::Corrupt { .. }
        | CasError::TooLarge { .. }
        | CasError::InMemoryLimit { .. }
        | CasError::State(_)
        | CasError::Unportable(_)
        | CasError::Damaged(_)
        | CasError::Manifest { .. }
        | CasError::Encode(_) => RefusalCode::Storage,
    }
}

fn tree_refusal(error: &crate::tree::TreeError) -> RefusalCode {
    match error {
        crate::tree::TreeError::Io(failure) if failure.kind() == std::io::ErrorKind::NotFound => {
            RefusalCode::NoSuchPath
        }
        crate::tree::TreeError::Io(_)
        | crate::tree::TreeError::Blocked(_)
        | crate::tree::TreeError::Occupied(_)
        | crate::tree::TreeError::Unportable(_) => RefusalCode::Storage,
    }
}

fn workspace_refusal(error: &crate::workspace::WorkspaceError) -> RefusalCode {
    match error {
        crate::workspace::WorkspaceError::NotAFile(_) => RefusalCode::NotAFile,
        crate::workspace::WorkspaceError::Tree(error) => tree_refusal(error),
        crate::workspace::WorkspaceError::Cas(_)
        | crate::workspace::WorkspaceError::State(_)
        | crate::workspace::WorkspaceError::Stopped
        | crate::workspace::WorkspaceError::Gone(_)
        | crate::workspace::WorkspaceError::SizeMismatch { .. }
        | crate::workspace::WorkspaceError::Snapshot(_) => RefusalCode::Storage,
    }
}

const fn scan_refusal(error: &crate::logscan::ScanError) -> RefusalCode {
    match error {
        crate::logscan::ScanError::Pattern(..) | crate::logscan::ScanError::PatternTooLong => {
            RefusalCode::BadRequest
        }
        crate::logscan::ScanError::Io(_) => RefusalCode::Storage,
    }
}

impl NodeError {
    fn code(&self) -> RefusalCode {
        match out_of_space(self) {
            ErrorCause::DiskFull => return RefusalCode::DiskFull,
            ErrorCause::Other => {}
        }
        match self {
            Self::Store(error) => store_refusal(error),
            Self::Cas(error) => cas_refusal(error),
            Self::Workspace(error) => workspace_refusal(error),
            Self::Scan(error) => scan_refusal(error),
            Self::Incomplete(_) => RefusalCode::MissingContent,
            Self::Denied(_) => RefusalCode::Forbidden,
            Self::NoWorkspace(_) | Self::Reused { .. } => RefusalCode::NoWorkspace,
            Self::Paused => RefusalCode::Paused,
            Self::Unfinished(_)
            | Self::Request(_)
            | Self::Invalid(_)
            | Self::Input(_)
            | Self::UnexpectedBlob { .. }
            | Self::ListLimit(_)
            | Self::Misrouted(_)
            | Self::UnsafeRetry(_) => RefusalCode::BadRequest,
            Self::Proc(_)
            | Self::AlreadySupervised(_)
            | Self::NotStarted(_)
            | Self::JobCapacity(_) => RefusalCode::Spawn,
            Self::Snapshot(_)
            | Self::QueueClosed
            | Self::Panicked(_)
            | Self::Control(_)
            | Self::Output(_)
            | Self::Io(_)
            | Self::Audit(_)
            | Self::State(_)
            | Self::TooManyUnreadable(_)
            | Self::CollectionBudget(_)
            | Self::MeasureDepth { .. }
            | Self::WorkspaceSlotsFull(_)
            | Self::Lock(_) => RefusalCode::Storage,
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum WatchedPath {
    JobChange,
    Other,
}

#[derive(Debug, Clone, Copy)]
enum WatchWake {
    Changed,
    ClientGone,
}

fn telling(jobs: &Path, path: &Path) -> WatchedPath {
    if path.parent() == Some(jobs)
        || path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name == "phase.json" || name == "outcome")
    {
        WatchedPath::JobChange
    } else {
        WatchedPath::Other
    }
}

pub(crate) fn watching(jobs: &Path, error: &notify::Error) -> NodeError {
    NodeError::Io(crate::failure::IoFailure {
        action: "watching",
        path: jobs.to_path_buf(),
        source: std::io::Error::other(error.to_string()),
    })
}

fn io(action: &'static str, path: &Path) -> impl FnOnce(std::io::Error) -> NodeError + use<> {
    let path = path.to_path_buf();
    move |source| {
        NodeError::Io(crate::failure::IoFailure {
            action,
            path,
            source,
        })
    }
}

fn entries(path: &Path) -> Result<Option<std::fs::ReadDir>, NodeError> {
    crate::faults::at("node::list", path).map_err(io("listing", path))?;
    match std::fs::read_dir(path) {
        Ok(entries) => Ok(Some(entries)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(io("listing", path)(error)),
    }
}

fn visit_project_workspaces(
    project: &Path,
    visit: &mut impl FnMut(&Path, &Path) -> Result<ScanFlow, NodeError>,
) -> Result<ScanFlow, NodeError> {
    crate::faults::at("node::list", project).map_err(io("listing", project))?;
    let locks = project.join("locks");
    for index in SlotIndex::all() {
        let workspace = project.join(index.to_string());
        match std::fs::symlink_metadata(&workspace) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => continue,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(io("checking", &workspace)(error)),
        }
        if visit(&workspace, &index.lock_path(&locks))? == ScanFlow::Stop {
            return Ok(ScanFlow::Stop);
        }
    }
    Ok(ScanFlow::Continue)
}

fn size_of(path: &Path) -> Result<u64, NodeError> {
    let mut total = 0u64;
    let mut directories = Vec::new();
    let mut next = Some(path.to_path_buf());
    loop {
        if let Some(current) = next.take() {
            crate::faults::at("node::measure", &current).map_err(io("checking", &current))?;
            let meta = match std::fs::symlink_metadata(&current) {
                Ok(meta) => Some(meta),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(io("checking", &current)(error)),
            };
            if let Some(meta) = meta {
                if meta.is_dir() {
                    if directories.len() == MAX_MEASURE_DIRS {
                        return Err(NodeError::MeasureDepth {
                            path: current,
                            limit: MAX_MEASURE_DIRS,
                        });
                    }
                    if let Some(entries) = entries(&current)? {
                        directories.push((current, entries));
                    }
                } else {
                    total = total.saturating_add(meta.len());
                }
            }
        }
        let Some((parent, entries)) = directories.last_mut() else {
            return Ok(total);
        };
        match entries.next() {
            Some(Ok(entry)) => next = Some(entry.path()),
            Some(Err(error)) => return Err(io("listing", parent)(error)),
            None => {
                directories.pop();
            }
        }
    }
}

fn disk_pressure(area: &Path) -> Result<DiskPressure, NodeError> {
    crate::faults::at("node::disk", area).map_err(io("measuring disk space in", area))?;
    let stats = fs4::statvfs(area).map_err(io("measuring disk space in", area))?;
    Ok(pressure(stats.available_space(), stats.total_space()))
}

fn cores() -> u32 {
    let count = match std::thread::available_parallelism() {
        Ok(count) => count.get(),
        Err(_unknown) => return 0,
    };
    match u32::try_from(count) {
        Ok(fits) => fits,
        Err(_beyond_u32) => u32::MAX,
    }
}

fn hundredths(value: f64) -> u32 {
    let scaled = (value * 100.0).round();
    if scaled.is_finite() && scaled >= 0.0 && scaled <= f64::from(u32::MAX) {
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::as_conversions,
            reason = "the value was just checked to be a whole number within u32"
        )]
        let whole = scaled as u32;
        whole
    } else {
        0
    }
}

#[derive(Debug, Clone, Copy)]
enum DiskPressure {
    Enough,
    Short,
}

fn pressure(available: u64, total: u64) -> DiskPressure {
    if available < (total / ROOM_SHARE).min(ROOM_AT_LEAST) {
        DiskPressure::Short
    } else {
        DiskPressure::Enough
    }
}

#[derive(Debug, Clone, Copy)]
enum Reclamation {
    Busy,
    Done,
}

impl Reclamation {
    fn visit(
        self,
        pressure: &impl Fn() -> Result<DiskPressure, NodeError>,
    ) -> Result<ScanFlow, NodeError> {
        match self {
            Self::Busy => Ok(ScanFlow::Continue),
            Self::Done => match pressure()? {
                DiskPressure::Enough => Ok(ScanFlow::Stop),
                DiskPressure::Short => Ok(ScanFlow::Continue),
            },
        }
    }
}

#[derive(Debug)]
enum WorkspaceReclamation {
    Busy,
    Removed,
    Quarantined(crate::state_file::StateError),
}

impl WorkspaceReclamation {
    fn visit(
        self,
        pressure: &impl Fn() -> Result<DiskPressure, NodeError>,
    ) -> Result<ScanFlow, NodeError> {
        match self {
            Self::Busy | Self::Quarantined(_) => Ok(ScanFlow::Continue),
            Self::Removed => Reclamation::Done.visit(pressure),
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum WorkspaceFreshness {
    Current,
    Stale,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Keep {
    Every,
    Unfinished,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlobUse {
    Needed,
    Spent,
}

struct Marks {
    prefix: String,
    limit: usize,
    ids: std::collections::BTreeMap<BlobId, BlobUse>,
    children: std::collections::BTreeSet<char>,
    overflow: bool,
}

impl Marks {
    fn new(prefix: &str, limit: usize) -> Self {
        Self {
            prefix: prefix.to_owned(),
            limit,
            ids: std::collections::BTreeMap::new(),
            children: std::collections::BTreeSet::new(),
            overflow: false,
        }
    }

    fn record(&mut self, blob: &BlobId, use_kind: BlobUse) {
        if !blob.as_str().starts_with(&self.prefix) {
            return;
        }
        if let Some(child) = blob.as_str().chars().nth(self.prefix.len()) {
            self.children.insert(child);
        }
        if let Some(current) = self.ids.get_mut(blob) {
            if use_kind == BlobUse::Needed {
                *current = BlobUse::Needed;
            }
            return;
        }
        if self.ids.len() == self.limit {
            self.overflow = true;
            return;
        }
        self.ids.insert(blob.clone(), use_kind);
    }
}

enum MarkScan {
    Ready(Marks),
    Split(Vec<char>),
}

#[derive(Clone, Copy)]
enum MissingManifest {
    Refuse,
    Ignore,
}

#[derive(Clone, Copy)]
struct CollectionPlan<'a> {
    keep: Keep,
    incoming: Option<&'a Source>,
    limit: usize,
}

#[derive(Clone, Copy)]
struct MarkInput<'a> {
    source: &'a Source,
    job: Option<&'a JobId>,
    use_kind: BlobUse,
    missing: MissingManifest,
}

const CAS_MARK_LIMIT: usize = 65_536;

#[cfg(test)]
const WORKSPACE_SCAN_BATCH: usize = crate::bounded::DIRECTORY_BATCH.get();
const MAX_MEASURE_DIRS: usize = 64;

struct CleanItems {
    details: Vec<crate::protocol::Freeable>,
    other_count: u64,
    other_bytes: u64,
}

impl CleanItems {
    fn new() -> Self {
        Self {
            details: Vec::with_capacity(crate::protocol::CLEAN_DETAIL_LIMIT),
            other_count: 0,
            other_bytes: 0,
        }
    }

    fn add(&mut self, item: crate::protocol::Freeable) {
        if self.details.len() < crate::protocol::CLEAN_DETAIL_LIMIT {
            self.details.push(item);
            return;
        }
        let omitted = match self.details.iter_mut().min_by(|left, right| {
            (left.bytes, left.what.as_raw_str()).cmp(&(right.bytes, right.what.as_raw_str()))
        }) {
            Some(least)
                if (item.bytes, item.what.as_raw_str())
                    > (least.bytes, least.what.as_raw_str()) =>
            {
                std::mem::replace(least, item)
            }
            Some(_) => item,
            None => {
                self.details.push(item);
                return;
            }
        };
        self.other_count = self.other_count.saturating_add(1);
        self.other_bytes = self.other_bytes.saturating_add(omitted.bytes);
    }

    fn finish(self) -> crate::protocol::CleanReportItems {
        let summary = (self.other_count > 0).then(|| crate::protocol::Freeable {
            what: RemoteText::new(format!("the other {} cleanable items", self.other_count)),
            bytes: self.other_bytes,
        });
        let mut details = self.details.into_iter();
        crate::protocol::CleanReportItems::from_parts(
            std::array::from_fn(|_| details.next()),
            summary,
        )
    }
}

#[derive(Debug)]
pub(crate) struct JobAdmission<'a> {
    _collecting: &'a crate::lock::OsLock,
}

fn configured_agent(home: &Path) -> Option<PathBuf> {
    if !crate::platform::FAMILY.agent_socket() {
        return None;
    }
    let mut command = crate::spawn::Invocation::new(
        crate::template::Arg::literal("ssh"),
        vec![
            crate::template::Arg::literal("-G"),
            crate::template::Arg::literal("localhost"),
        ],
    )
    .command();
    let asked = crate::bounded::command_output(&mut command, crate::bounded::Capture::SshConfig);
    let printed = match asked {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).into_owned(),
        Ok(_) | Err(_) => return None,
    };
    let agent = identity_agent(&printed, home)?;
    match std::fs::symlink_metadata(&agent) {
        Ok(_) => Some(agent),
        Err(_absent) => None,
    }
}

fn identity_agent(printed: &str, home: &Path) -> Option<PathBuf> {
    let value = printed
        .lines()
        .find_map(|line| line.strip_prefix("identityagent "))?
        .trim();
    if value.eq_ignore_ascii_case("none") || value == "SSH_AUTH_SOCK" || value.starts_with('$') {
        return None;
    }
    let path = match value.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None => PathBuf::from(value),
    };
    path.is_absolute().then_some(path)
}

#[derive(Debug, Clone, Copy)]
enum ErrorCause {
    DiskFull,
    Other,
}

fn out_of_space(error: &(dyn std::error::Error + 'static)) -> ErrorCause {
    let mut current = Some(error);
    while let Some(link) = current {
        if let Some(io) = link.downcast_ref::<std::io::Error>()
            && matches!(
                io.kind(),
                std::io::ErrorKind::StorageFull | std::io::ErrorKind::QuotaExceeded
            )
        {
            return ErrorCause::DiskFull;
        }
        current = link.source();
    }
    ErrorCause::Other
}

#[derive(Debug, Clone, Copy)]
struct Cursor {
    offset: u64,
    follow: Follow,
}

#[derive(Debug, Clone)]
pub struct Node {
    dirs: Dirs,
    store: Store,
    cas: Cas,
    audit: AuditLog,
    pressure: fn(&Path) -> Result<DiskPressure, NodeError>,
}

#[must_use]
pub fn hello(dirs: &Dirs) -> Hello {
    Hello {
        wire: RemoteText::new(crate::protocol::wire().to_owned()),
        version: RemoteText::new(VERSION.to_owned()),
        build: RemoteText::new(crate::protocol::BUILD_STAMP.to_owned()),
        os: RemoteText::new(crate::platform::OS.to_owned()),
        arch: RemoteText::new(std::env::consts::ARCH.to_owned()),
        home: RemoteText::new(dirs.home().display().to_string()),
        state: RemoteText::new(dirs.state().display().to_string()),
        shell: RemoteText::new(crate::shell::default_shell()),
        binary: RemoteText::new(match crate::dist::running() {
            Ok(binary) => binary.get().sha256().to_owned(),
            Err(error) => format!("unknown: {error}"),
        }),
    }
}

const KEEP_FINISHED: usize = 500;

const MAX_UNFINISHED_JOBS: usize = 64;

#[cfg(test)]
const LOG_SCAN_BATCH: usize = crate::bounded::DIRECTORY_BATCH.get();

const ROOM_AT_LEAST: u64 = 10 << 30;

const ROOM_SHARE: u64 = 10;

const DISCARDED: &[u8] = b"domyjob: this log was discarded to free disk space on this machine\n";

const KEPT_BINARIES: usize = 3;

fn retire_old_binaries(dirs: &Dirs) {
    if let Ok(exe) = std::env::current_exe() {
        crate::user_files::sweep_retired(&exe);
    }
    retire_cached_binaries(dirs, crate::protocol::build_key());
}

fn cached_version_dirs(bin: &Path, mut visit: impl FnMut(String, PathBuf)) -> std::io::Result<()> {
    for entry in std::fs::read_dir(bin)? {
        let Ok(entry) = entry else { continue };
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if kind.is_dir() && !kind.is_symlink() {
            visit(name, entry.path());
        }
    }
    Ok(())
}

fn retire_cached_binaries(dirs: &Dirs, current: &str) {
    let bin = dirs.cache().join("bin");
    let mut newest = std::collections::BTreeSet::new();
    if cached_version_dirs(&bin, |name, path| {
        if name != current {
            newest.insert((name, path));
            if newest.len() > KEPT_BINARIES.saturating_sub(1) {
                newest.pop_first();
            }
        }
    })
    .is_err()
    {
        return;
    }
    match crate::bounded::SortedScan::<(String, PathBuf)>::new().walk(
        |offer| {
            cached_version_dirs(&bin, |name, path| {
                let candidate = (name, path);
                if candidate.0 != current && !newest.contains(&candidate) {
                    offer(candidate);
                }
            })
        },
        |(_, path)| {
            match crate::state_file::remove_dir_all(&path) {
                Ok(()) | Err(_) => {}
            }
            Ok(ScanFlow::Continue)
        },
    ) {
        Ok(_) | Err(_) => {}
    }
}

fn refusal(error: &NodeError) -> Refusal {
    Refusal {
        code: error.code(),
        detail: RemoteText::new(error.to_string()),
    }
}

fn streamed(
    output: &mut dyn Write,
    body: impl FnOnce(&mut crate::framed::Framed<'_>) -> Result<(), NodeError>,
) -> Result<(), NodeError> {
    send(output, &Reply::Stream)?;
    let mut framed = crate::framed::Framed::new(output);
    let outcome = body(&mut framed).map_err(|error| refusal(&error));
    framed.finish(outcome).map_err(NodeError::Output)
}

fn send(output: &mut dyn Write, reply: &Reply) -> Result<(), NodeError> {
    let mut line = serde_json::to_vec(reply).map_err(|e| NodeError::Output(e.into()))?;
    line.push(b'\n');
    output.write_all(&line).map_err(NodeError::Output)?;
    output.flush().map_err(NodeError::Output)
}

fn read_line<T: crate::ingress::Ingress>(input: &mut dyn BufRead) -> Result<T, NodeError> {
    let line =
        crate::bounded::line(input, crate::bounded::REQUEST_LINE).map_err(NodeError::Input)?;
    crate::ingress::json(&line).map_err(NodeError::Request)
}

struct SnapshotTransfer<'a> {
    cas: &'a Cas,
    collecting: crate::lock::OsLock,
}

impl<'a> SnapshotTransfer<'a> {
    fn begin(cas: &'a Cas, store: &Store) -> Result<Self, NodeError> {
        Ok(Self {
            cas,
            collecting: crate::lock::OsLock::exclusive(&store.collection_lock_path())?,
        })
    }

    fn receive(&self, input: &mut dyn BufRead, frame: &Frame) -> Result<(), NodeError> {
        Ok(self.cas.receive(input, &frame.blob, frame.size)?)
    }

    fn missing(&self, manifest: &BlobId) -> Result<Vec<BlobId>, NodeError> {
        Ok(self.cas.missing(&self.cas.manifest(manifest)?.blobs())?)
    }

    fn into_collection_lock(self) -> crate::lock::OsLock {
        self.collecting
    }
}

impl Node {
    pub fn open(dirs: Dirs) -> Result<Self, NodeError> {
        crate::state_file::private_dir(dirs.state())?;
        let store = Store::open(&dirs)?;
        let cas = Cas::open(dirs.state().join("objects"))?;
        let audit = AuditLog::at(&dirs);
        Ok(Self {
            dirs,
            store,
            cas,
            audit,
            pressure: disk_pressure,
        })
    }

    pub fn serve(
        &self,
        principal: &Principal,
        input: impl Input,
        output: &mut (dyn Write + Send),
    ) -> Result<(), NodeError> {
        crate::liveness::with_pulse(output, |pulsed| self.answer(principal, input, pulsed))
    }

    fn answer(
        &self,
        principal: &Principal,
        mut input: impl Input,
        output: &mut dyn Write,
    ) -> Result<(), NodeError> {
        let outcome = read_line::<crate::ingress::PeerRequest>(&mut input).and_then(|peer| {
            let (nature, about) = peer.audit();
            let verb = nature.name;
            let is_peer = match principal {
                Principal::Owner => false,
                Principal::Peer { .. } => true,
            };
            let must_audit = nature.audit == authz::Audit::Always || is_peer;
            let decision = authz::authorize(principal.clone(), peer);
            let verdict = match &decision {
                Ok(_) => Verdict::Allowed,
                Err(_) => Verdict::Denied,
            };
            if must_audit {
                let who = principal.describe();
                self.audit.record(crate::audit::Event {
                    principal: &who,
                    action: verb,
                    subject: about,
                    verdict,
                })?;
            }
            self.handle(decision?, input, output)
        });
        match outcome {
            Ok(()) => Ok(()),
            Err(error) => send(output, &Reply::Refused(refusal(&error))),
        }
    }

    fn handle(
        &self,
        authorized: Authorized,
        input: impl Input,
        output: &mut dyn Write,
    ) -> Result<(), NodeError> {
        match authorized.route() {
            Routed::Query(queried) => self.handle_query(queried, input, output),
            Routed::Command(commanded) => self.handle_command(commanded, input, output),
        }
    }

    fn handle_command(
        &self,
        commanded: Commanded,
        input: impl Input,
        output: &mut dyn Write,
    ) -> Result<(), NodeError> {
        match commanded.effect() {
            authz::Effect::Submit => {
                let submitted = commanded
                    .into_submission()
                    .map_err(|_not_submission| NodeError::Misrouted("submit"))?;
                self.upkeep_including(&submitted, submitted.submission().location.source())?;
                match &submitted.submission().location {
                    Location::Snapshot { .. } => {
                        return self.submit_snapshot(submitted, (input, output));
                    }
                    Location::Home => {}
                }
                let job = self.accept(submitted)?;
                return send(output, &Reply::Job(Box::new(job)));
            }
            authz::Effect::Kill => {}
            authz::Effect::MaintainedCommand => self.upkeep(&commanded)?,
            authz::Effect::Query => return Err(NodeError::Misrouted("query")),
        }
        let reply = self.reply_command(commanded, input)?;
        send(output, &reply)
    }

    fn handle_query(
        &self,
        queried: Queried,
        input: impl Input,
        output: &mut dyn Write,
    ) -> Result<(), NodeError> {
        match queried.request() {
            Request::Hold => self.hold(input, output),
            Request::Logs {
                job,
                offset,
                follow,
            } => {
                let cursor = Cursor {
                    offset: *offset,
                    follow: *follow,
                };
                self.logs(
                    &self.own(queried.principal(), job)?,
                    cursor,
                    (input, output),
                )
            }
            Request::Tail { job, lines } => {
                self.tail(&self.own(queried.principal(), job)?, *lines, output)
            }
            Request::Get { job, path } => {
                self.get(&self.own(queried.principal(), job)?, path, output)
            }
            Request::Changes { job } => self.changes(&self.own(queried.principal(), job)?, output),
            Request::Watch => self.watch(queried.principal(), input, output),
            Request::Hello
            | Request::Report
            | Request::AuditAt { .. }
            | Request::AuditHead
            | Request::Digest { .. }
            | Request::Search { .. }
            | Request::List { .. }
            | Request::Status { .. }
            | Request::Wait { .. } => {
                let reply = self.reply_query(queried, input)?;
                send(output, &reply)
            }
            Request::Submit { .. }
            | Request::Retry { .. }
            | Request::Kill { .. }
            | Request::Clean { .. }
            | Request::Configure { .. } => {
                Err(NodeError::Misrouted(authz::nature(queried.request()).name))
            }
        }
    }

    fn hold(&self, mut input: impl Input, output: &mut dyn Write) -> Result<(), NodeError> {
        send(output, &Reply::Hello(hello(&self.dirs)))?;
        std::io::copy(&mut input, &mut std::io::sink()).map_err(NodeError::Input)?;
        Ok(())
    }

    fn reply_query(&self, queried: Queried, input: impl Input) -> Result<Reply, NodeError> {
        let (principal, request) = queried.into_parts();
        Ok(match request {
            Request::Hello => Reply::Hello(hello(&self.dirs)),
            Request::Report => Reply::Report(Box::new(self.report()?)),
            Request::AuditAt { epoch, seq } => Reply::AuditAt {
                hash: self.audit.hash_at(epoch, seq)?,
            },
            Request::AuditHead => Reply::AuditHead(self.audit.head()?),
            Request::Digest { job, tail } => {
                Reply::Digest(Box::new(self.digest(&self.own(&principal, &job)?, tail)?))
            }
            Request::Search {
                job,
                pattern,
                context,
                limit,
            } => Reply::Found(self.search(
                &self.own(&principal, &job)?,
                &pattern,
                crate::logscan::Window { context, limit },
            )?),
            Request::List { limit } => {
                let (jobs, unreadable) = self.list(&principal, limit)?;
                Reply::Jobs { jobs, unreadable }
            }
            Request::Status { job } => {
                Reply::Job(Box::new(self.store.job(&self.own(&principal, &job)?)?))
            }
            Request::Wait { job } => Reply::Job(Box::new(self.settle(
                &self.own(&principal, &job)?,
                Order::Wait,
                input,
            )?)),
            Request::Hold
            | Request::Submit { .. }
            | Request::Logs { .. }
            | Request::Tail { .. }
            | Request::Get { .. }
            | Request::Watch
            | Request::Changes { .. }
            | Request::Retry { .. }
            | Request::Kill { .. }
            | Request::Clean { .. }
            | Request::Configure { .. } => {
                return Err(NodeError::Misrouted(authz::nature(&request).name));
            }
        })
    }

    fn reply_command(&self, commanded: Commanded, input: impl Input) -> Result<Reply, NodeError> {
        let action = commanded
            .into_action()
            .map_err(|_not_action| NodeError::Misrouted("command"))?;
        Ok(match action {
            CommandAction::Configure(authorized) => {
                self.configure(&authorized)?;
                Reply::Report(Box::new(self.report()?))
            }
            CommandAction::Clean(authorized) => Reply::Cleaned(Box::new(self.clean(&authorized)?)),
            CommandAction::Retry(authorized) => {
                let id = self.retry(&authorized)?;
                Reply::Job(Box::new(self.settle(&id, Order::Wait, input)?))
            }
            CommandAction::Kill(authorized) => {
                let id = self.kill(&authorized)?;
                Reply::Job(Box::new(self.settle(&id, Order::Kill, input)?))
            }
        })
    }

    fn own(&self, principal: &Principal, reference: &JobRef) -> Result<JobId, NodeError> {
        let id = self.store.resolve(reference)?;
        match principal.relation_to(&self.store.spec(&id)?.submitted_by) {
            Relation::Oversees | Relation::Submitted => Ok(id),
            Relation::Stranger => Err(NodeError::Store(StoreError::NoSuchJob(reference.clone()))),
        }
    }

    fn accept(&self, authorized: AuthorizedSubmission) -> Result<Job, NodeError> {
        let submission = authorized.submission();
        let settings = self.store.settings()?;
        if submission.queue == crate::protocol::Queue::Slot && settings.paused {
            return Err(NodeError::Paused);
        }
        self.make_room(&|| self.disk_pressure(), &authorized, &submission.location)?;
        let collecting = crate::lock::OsLock::exclusive(&self.store.collection_lock_path())?;
        self.accept_with_lock(authorized, collecting)
    }

    fn submit_snapshot(
        &self,
        authorized: AuthorizedSubmission,
        (mut input, output): (impl Input, &mut dyn Write),
    ) -> Result<(), NodeError> {
        let submission = authorized.submission();
        let source = submission
            .location
            .source()
            .ok_or(NodeError::Misrouted("submit snapshot"))?;
        let manifest_id = source.manifest.clone();
        let settings = self.store.settings()?;
        if submission.queue == crate::protocol::Queue::Slot && settings.paused {
            return Err(NodeError::Paused);
        }
        self.make_room(&|| self.disk_pressure(), &authorized, &Location::Home)?;
        let transfer = SnapshotTransfer::begin(&self.cas, &self.store)?;
        let manifest_frame: Frame = read_line(&mut input)?;
        if manifest_frame.blob != manifest_id {
            return Err(NodeError::UnexpectedBlob {
                expected: manifest_id,
                got: manifest_frame.blob,
            });
        }
        if manifest_frame.size > crate::bounded::IN_MEMORY_FILE {
            return Err(CasError::InMemoryLimit {
                blob: manifest_frame.blob,
                size: manifest_frame.size,
                limit: crate::bounded::IN_MEMORY_FILE,
            }
            .into());
        }
        transfer.receive(&mut input, &manifest_frame)?;
        let missing = transfer.missing(&manifest_frame.blob)?;
        send(
            output,
            &Reply::NeedBlobs {
                blobs: missing.clone(),
            },
        )?;
        for expected in missing {
            let content_frame: Frame = read_line(&mut input)?;
            if content_frame.blob != expected {
                return Err(NodeError::UnexpectedBlob {
                    expected,
                    got: content_frame.blob,
                });
            }
            transfer.receive(&mut input, &content_frame)?;
        }
        let job = self.accept_with_lock(authorized, transfer.into_collection_lock())?;
        send(output, &Reply::Job(Box::new(job)))
    }

    fn accept_with_lock(
        &self,
        authorized: AuthorizedSubmission,
        collecting: crate::lock::OsLock,
    ) -> Result<Job, NodeError> {
        let pending = authorized.submission();
        let settings = self.store.settings()?;
        if pending.queue == crate::protocol::Queue::Slot && settings.paused {
            return Err(NodeError::Paused);
        }
        let nonce_path = self.store.nonce_path(
            &crate::supervisor::scope_name(&authorized.principal().submitter()),
            &pending.nonce,
        );
        if let Some(job) = self.earlier_attempt(&nonce_path)? {
            collecting.release()?;
            return Ok(job);
        }
        let admission = self.admit_job(&collecting)?;
        if let Some(source) = pending.location.source() {
            let manifest = self.cas.manifest(&source.manifest)?;
            if let Some(blob) = self.cas.missing(&manifest.blobs())?.first() {
                return Err(NodeError::Incomplete(format!("blob {blob}")));
            }
        }
        let (principal, submission) = authorized.into_parts();
        let spec = Spec {
            id: JobId::generate()?,
            name: submission.name,
            command: submission.command,
            location: submission.location,
            env_names: submission.env.keys().cloned().collect(),
            shell: submission.shell,
            concurrency: settings.max_jobs,
            sequence: self.store.next_sequence()?,
            submitted_by: principal.submitter(),
            submitted_at: Timestamp::observe(),
        };
        let launch = crate::store::LaunchEnv::of_this_process()
            .with_agent(configured_agent(self.dirs.home()));
        let staging = crate::lock::OsLock::exclusive(&self.store.staging_lock_path(&spec.id))?;
        self.store
            .stage_admitted(&admission, &spec, (submission.env, &launch))?;
        if submission.queue == crate::protocol::Queue::Now {
            self.store.skip_the_queue(&spec.id)?;
        }
        crate::state_file::write_bytes(&nonce_path, spec.id.as_str().as_bytes())?;
        collecting.release()?;
        let launched = self.launch_supervisor(&spec.id);
        if let Err(error) = launched {
            let why = self.store.start_failure(&spec.id)?;
            self.store.discard_staged(&spec.id)?;
            crate::state_file::remove_file(&nonce_path)?;
            staging.release()?;
            self.store.forget_staging_lock(&spec.id)?;
            return Err(match why {
                Some(why) => NodeError::NotStarted(RemoteText::new(why)),
                None => error,
            });
        }
        staging.release()?;
        self.store.forget_staging_lock(&spec.id)?;
        Ok(self.store.job(&spec.id)?)
    }

    fn admit_job<'a>(
        &self,
        collecting: &'a crate::lock::OsLock,
    ) -> Result<JobAdmission<'a>, NodeError> {
        let mut unfinished = 0usize;
        for id in self.store.ids_iter()? {
            let id = id?;
            if self.store.phase(&id)?.kind() != PhaseKind::Finished {
                unfinished = unfinished.saturating_add(1);
                if unfinished >= MAX_UNFINISHED_JOBS {
                    return Err(NodeError::JobCapacity(MAX_UNFINISHED_JOBS));
                }
            }
        }
        for id in self.store.staged_ids_iter()? {
            id?;
            unfinished = unfinished.saturating_add(1);
            if unfinished >= MAX_UNFINISHED_JOBS {
                return Err(NodeError::JobCapacity(MAX_UNFINISHED_JOBS));
            }
        }
        Ok(JobAdmission {
            _collecting: collecting,
        })
    }

    fn earlier_attempt(&self, nonce_path: &Path) -> Result<Option<Job>, NodeError> {
        let Some(earlier) = crate::state_file::read_bytes(nonce_path)? else {
            return Ok(None);
        };
        let id: JobId = String::from_utf8_lossy(&earlier).trim().parse()?;
        let staging = crate::lock::OsLock::exclusive(&self.store.staging_lock_path(&id))?;
        let job = match self.store.publication(&id)? {
            Publication::Published => Some(self.store.job(&id)?),
            Publication::Unpublished => {
                self.store.discard_staged(&id)?;
                crate::state_file::remove_file(nonce_path)?;
                None
            }
        };
        staging.release()?;
        self.store.forget_staging_lock(&id)?;
        Ok(job)
    }

    fn launch_supervisor(&self, id: &JobId) -> Result<(), NodeError> {
        let exe = proc::own_executable().map_err(|source| {
            NodeError::Io(crate::failure::IoFailure {
                action: "locating",
                path: PathBuf::from("domyjob"),
                source,
            })
        })?;
        let invocation = crate::spawn::Invocation::new(
            crate::template::Arg::path(&exe),
            vec![
                crate::template::Arg::literal("node"),
                crate::template::Arg::literal("--supervise"),
                crate::template::Arg::word(id),
                crate::template::Arg::literal("--state-dir"),
                crate::template::Arg::path(&self.dirs.state_path()),
                crate::template::Arg::literal("--home-dir"),
                crate::template::Arg::path(&self.dirs.home_path()),
            ],
        )
        .in_dir(&self.dirs.home_path());
        Ok(proc::launch(&invocation)?)
    }

    pub fn running(&self) -> Result<Vec<JobId>, NodeError> {
        let mut running = Vec::new();
        for id in self.store.ids_iter()? {
            let id = id?;
            match self.store.phase(&id)? {
                Phase::Finished { .. } => continue,
                Phase::Queued
                | Phase::Preparing { .. }
                | Phase::Starting { .. }
                | Phase::Running { .. } => {}
            }
            match crate::lock::OsLock::try_exclusive(&self.store.alive_path(&id))? {
                Some(idle) => idle.release()?,
                None => running.push(id),
            }
        }
        Ok(running)
    }

    pub fn stop(&self, id: &JobId) -> Result<Job, NodeError> {
        self.recover(id)?;
        self.settle(id, Order::Kill, std::io::empty())
    }

    fn upkeep(&self, commanded: &impl CommandAuthority) -> Result<(), NodeError> {
        self.upkeep_including(commanded, None)
    }

    fn upkeep_including(
        &self,
        _commanded: &impl CommandAuthority,
        incoming: Option<&Source>,
    ) -> Result<(), NodeError> {
        for id in self.store.ids_iter()? {
            let id = id?;
            self.recover(&id)?;
        }
        for id in self.store.staged_ids_iter()? {
            let id = id?;
            self.abandon_staging(&id)?;
        }
        match self.retire_including(KEEP_FINISHED, incoming) {
            Ok(()) | Err(_) => {}
        }
        match self.empty_trash() {
            Ok(()) | Err(_) => {}
        }
        retire_old_binaries(&self.dirs);
        Ok(())
    }

    fn report(&self) -> Result<crate::protocol::Report, NodeError> {
        let settings = self.store.settings()?;
        let mut system = sysinfo::System::new();
        system.refresh_memory();
        let load = sysinfo::System::load_average();
        let load_hundredths = crate::platform::FAMILY
            .load_average()
            .then(|| [load.one, load.five, load.fifteen].map(hundredths));
        let jobs = self.store.area("jobs");
        let disk = match crate::faults::at("node::disk", &jobs).and_then(|()| fs4::statvfs(&jobs)) {
            Ok(stats) => crate::protocol::DiskSpace::Measured {
                total: stats.total_space(),
                available: stats.available_space(),
                short: match pressure(stats.available_space(), stats.total_space()) {
                    DiskPressure::Enough => false,
                    DiskPressure::Short => true,
                },
            },
            Err(error) => crate::protocol::DiskSpace::Unavailable {
                reason: RemoteText::new(error.to_string()),
            },
        };
        Ok(crate::protocol::Report {
            host: RemoteText::new(sysinfo::System::host_name().unwrap_or_default()),
            os: RemoteText::new(sysinfo::System::long_os_version().unwrap_or_default()),
            cores: cores(),
            load_hundredths,
            memory_total: system.total_memory(),
            memory_available: system.available_memory(),
            disk,
            uptime_seconds: sysinfo::System::uptime(),
            paused: settings.paused,
            max_jobs: settings.max_jobs,
        })
    }

    fn configure(&self, authorized: &AuthorizedConfigure) -> Result<(), NodeError> {
        Ok(self.store.configure(*authorized.payload())?)
    }

    fn disk_pressure(&self) -> Result<DiskPressure, NodeError> {
        (self.pressure)(&self.store.area("jobs"))
    }

    fn make_room(
        &self,
        pressure: &impl Fn() -> Result<DiskPressure, NodeError>,
        _commanded: &impl CommandAuthority,
        location: &Location,
    ) -> Result<(), NodeError> {
        match pressure()? {
            DiskPressure::Enough => return Ok(()),
            DiskPressure::Short => {}
        }
        if self
            .visit_idle_workspaces(|workspace, lock| self.evict(workspace, lock)?.visit(pressure))?
            == ScanFlow::Stop
        {
            return Ok(());
        }
        if self.visit_finished_logs(|id| self.discard_log(id)?.visit(pressure))? == ScanFlow::Stop {
            return Ok(());
        }
        self.collect_including(Keep::Unfinished, location.source())
    }

    fn freshness(&self, workspace: &Path) -> Result<WorkspaceFreshness, NodeError> {
        let filled_by = crate::supervisor::filled_by_path(workspace);
        let last = match crate::state_file::read_bytes(&filled_by)? {
            Some(bytes) => bytes,
            None => return Ok(WorkspaceFreshness::Stale),
        };
        match String::from_utf8_lossy(&last).trim().parse::<JobId>() {
            Ok(id) => match self.store.publication(&id)? {
                Publication::Published => Ok(WorkspaceFreshness::Current),
                Publication::Unpublished => Ok(WorkspaceFreshness::Stale),
            },
            Err(_foreign) => Ok(WorkspaceFreshness::Stale),
        }
    }

    fn evict(&self, workspace: &Path, lock: &Path) -> Result<WorkspaceReclamation, NodeError> {
        let Some(idle) = crate::lock::OsLock::try_exclusive(lock)? else {
            return Ok(WorkspaceReclamation::Busy);
        };
        let aside = self
            .store
            .area("trash")
            .join(format!("workspace-{}", JobId::generate()?));
        crate::state_file::move_aside(workspace, &aside)?;
        let removed = crate::state_file::remove_tree_forcibly(&aside);
        idle.release()?;
        Ok(match removed {
            Ok(()) => WorkspaceReclamation::Removed,
            Err(error) => WorkspaceReclamation::Quarantined(error),
        })
    }

    fn discard_log(&self, id: &JobId) -> Result<Reclamation, NodeError> {
        let Some(alive) = crate::lock::OsLock::try_exclusive(&self.store.alive_path(id))? else {
            return Ok(Reclamation::Busy);
        };
        let log = self.store.log_path(id);
        crate::state_file::cut_to(&log, 0)?;
        crate::state_file::overwrite_in_place(&log, DISCARDED)?;
        alive.release()?;
        Ok(Reclamation::Done)
    }

    fn clean(&self, authorized: &AuthorizedClean) -> Result<crate::protocol::Cleaned, NodeError> {
        let (apply, logs, idle) = *authorized.payload();
        let work = self.store.area("work");
        let mut items = CleanItems::new();
        self.visit_idle_workspaces(|workspace, lock| {
            let freshness = self.freshness(workspace)?;
            match freshness {
                WorkspaceFreshness::Current if !idle => return Ok(ScanFlow::Continue),
                WorkspaceFreshness::Current | WorkspaceFreshness::Stale => {}
            }
            let bytes = size_of(workspace)?;
            if apply {
                match self.evict(workspace, lock)? {
                    WorkspaceReclamation::Busy => return Ok(ScanFlow::Continue),
                    WorkspaceReclamation::Removed => {}
                    WorkspaceReclamation::Quarantined(error) => return Err(error.into()),
                }
            }
            let shown = match workspace.strip_prefix(&work) {
                Ok(inside) => inside,
                Err(_elsewhere) => workspace,
            };
            items.add(crate::protocol::Freeable {
                what: RemoteText::new(format!(
                    "{} workspace {}",
                    match freshness {
                        WorkspaceFreshness::Current => "idle",
                        WorkspaceFreshness::Stale => "stale",
                    },
                    shown.display()
                )),
                bytes,
            });
            Ok(ScanFlow::Continue)
        })?;
        if logs {
            let mut bytes = 0u64;
            let mut count = 0u64;
            self.visit_finished_logs(|id| {
                let size = size_of(&self.store.log_path(id))?;
                if apply {
                    match self.discard_log(id)? {
                        Reclamation::Busy => return Ok(ScanFlow::Continue),
                        Reclamation::Done => {}
                    }
                }
                bytes = bytes
                    .saturating_add(size.saturating_sub(crate::domain::len_u64(DISCARDED.len())));
                count = count.saturating_add(1);
                Ok(ScanFlow::Continue)
            })?;
            if count > 0 {
                items.add(crate::protocol::Freeable {
                    what: RemoteText::new(format!("the logs of {count} finished jobs")),
                    bytes,
                });
            }
        }
        let trash = size_of(&self.store.area("trash"))?;
        if trash > 0 {
            items.add(crate::protocol::Freeable {
                what: RemoteText::new("things set aside to remove".to_owned()),
                bytes: trash,
            });
        }
        if apply {
            self.empty_trash()?;
            self.collect(Keep::Every)?;
        }
        Ok(crate::protocol::Cleaned {
            applied: apply,
            items: items.finish(),
        })
    }

    fn visit_idle_workspaces(
        &self,
        mut visit: impl FnMut(&Path, &Path) -> Result<ScanFlow, NodeError>,
    ) -> Result<ScanFlow, NodeError> {
        let work = self.store.area("work");
        crate::bounded::SortedScan::<PathBuf>::new().walk(
            |offer| {
                let Some(scopes) = entries(&work)? else {
                    return Ok(());
                };
                for scope in scopes {
                    let scope = scope.map_err(io("listing", &work))?;
                    let scope_path = scope.path();
                    if !scope
                        .file_type()
                        .map_err(io("checking", &scope_path))?
                        .is_dir()
                    {
                        continue;
                    }
                    let Some(projects) = entries(&scope_path)? else {
                        continue;
                    };
                    for project in projects {
                        let project = project.map_err(io("listing", &scope_path))?;
                        let path = project.path();
                        if project.file_type().map_err(io("checking", &path))?.is_dir() {
                            offer(path);
                        }
                    }
                }
                Ok::<(), NodeError>(())
            },
            |project| visit_project_workspaces(&project, &mut visit),
        )
    }

    fn empty_trash(&self) -> Result<(), NodeError> {
        let trash = self.store.area("trash");
        let Some(entries) = entries(&trash)? else {
            return Ok(());
        };
        let mut first_error = None;
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    first_error.get_or_insert_with(|| io("listing", &trash)(error));
                    continue;
                }
            };
            if let Err(error) = crate::state_file::remove_tree_forcibly(&entry.path()) {
                first_error.get_or_insert_with(|| error.into());
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn visit_finished_logs(
        &self,
        mut visit: impl FnMut(&JobId) -> Result<ScanFlow, NodeError>,
    ) -> Result<ScanFlow, NodeError> {
        crate::bounded::SortedScan::<(std::cmp::Reverse<u64>, JobId)>::new().walk(
            |offer| {
                for id in self.store.ids_iter()? {
                    let id = id?;
                    if self.store.phase(&id)?.kind() != PhaseKind::Finished {
                        continue;
                    }
                    let path = self.store.log_path(&id);
                    let size = match std::fs::symlink_metadata(&path) {
                        Ok(meta) => meta.len(),
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                        Err(error) => return Err(io("checking", &path)(error)),
                    };
                    if size > crate::domain::len_u64(DISCARDED.len()) {
                        offer((std::cmp::Reverse(size), id));
                    }
                }
                Ok::<(), NodeError>(())
            },
            |(_size, id)| visit(&id),
        )
    }

    fn retire_including(&self, keep: usize, incoming: Option<&Source>) -> Result<(), NodeError> {
        let collecting = crate::lock::OsLock::exclusive(&self.store.collection_lock_path())?;
        let mut newest = std::collections::BTreeSet::new();
        let mut excess = false;
        for id in self.store.ids_iter()? {
            let id = id?;
            if self.store.phase(&id)?.kind() == PhaseKind::Finished {
                newest.insert((self.store.spec(&id)?.sequence, id));
                if newest.len() > keep {
                    newest.pop_first();
                    excess = true;
                }
            }
        }
        if !excess {
            return Ok(collecting.release()?);
        }
        for id in self.store.ids_iter()? {
            let id = id?;
            if self.store.phase(&id)?.kind() != PhaseKind::Finished
                || newest.contains(&(self.store.spec(&id)?.sequence, id.clone()))
            {
                continue;
            }
            let Some(alive) = crate::lock::OsLock::try_exclusive(&self.store.alive_path(&id))?
            else {
                continue;
            };
            self.store.remove_job(&id)?;
            alive.release()?;
        }
        self.forget_stale_nonces()?;
        collecting.release()?;
        self.collect_including(Keep::Every, incoming)
    }

    fn forget_stale_nonces(&self) -> Result<(), NodeError> {
        let dir = self.store.area("nonces");
        let Some(entries) = entries(&dir)? else {
            return Ok(());
        };
        for entry in entries {
            let entry = entry.map_err(io("listing", &dir))?;
            let path = entry.path();
            let Some(bytes) = crate::state_file::read_bytes(&path)? else {
                continue;
            };
            let gone = match String::from_utf8_lossy(&bytes).trim().parse::<JobId>() {
                Ok(id) => match self.store.publication(&id)? {
                    Publication::Published => false,
                    Publication::Unpublished => {
                        match crate::lock::OsLock::probe(&self.store.staging_lock_path(&id))? {
                            crate::lock::Probe::Held => false,
                            crate::lock::Probe::Absent | crate::lock::Probe::Free => true,
                        }
                    }
                },
                Err(_foreign) => true,
            };
            if gone {
                crate::state_file::remove_file(&path)?;
            }
        }
        Ok(())
    }

    fn collect(&self, keep: Keep) -> Result<(), NodeError> {
        self.collect_including(keep, None)
    }

    fn collect_including(&self, keep: Keep, incoming: Option<&Source>) -> Result<(), NodeError> {
        self.collect_with_mark_limit(keep, incoming, CAS_MARK_LIMIT)
    }

    fn collect_with_mark_limit(
        &self,
        keep: Keep,
        incoming: Option<&Source>,
        limit: usize,
    ) -> Result<(), NodeError> {
        if limit == 0 {
            return Err(NodeError::CollectionBudget(limit));
        }
        let collecting = crate::lock::OsLock::exclusive(&self.store.collection_lock_path())?;
        self.collect_partition(
            CollectionPlan {
                keep,
                incoming,
                limit,
            },
            "",
        )?;
        Ok(collecting.release()?)
    }

    fn collect_partition(&self, plan: CollectionPlan<'_>, prefix: &str) -> Result<(), NodeError> {
        match self.scan_marks(plan, prefix)? {
            MarkScan::Ready(marks) => match plan.keep {
                Keep::Every => self
                    .cas
                    .for_each_stored_matching::<NodeError>(prefix, |blob| {
                        if !marks.ids.contains_key(&blob) {
                            self.cas.remove(&blob)?;
                        }
                        Ok(())
                    }),
                Keep::Unfinished => {
                    for (blob, use_kind) in marks.ids {
                        if use_kind == BlobUse::Spent {
                            self.cas.remove(&blob)?;
                        }
                    }
                    Ok(())
                }
            },
            MarkScan::Split(children) => {
                if children.is_empty() {
                    return Err(NodeError::CollectionBudget(plan.limit));
                }
                for child in "0123456789abcdef".chars() {
                    let next = format!("{prefix}{child}");
                    if children.contains(&child) {
                        self.collect_partition(plan, &next)?;
                    } else if plan.keep == Keep::Every {
                        self.cas
                            .for_each_stored_matching::<NodeError>(&next, |blob| {
                                self.cas.remove(&blob)?;
                                Ok(())
                            })?;
                    }
                }
                Ok(())
            }
        }
    }

    fn scan_marks(&self, plan: CollectionPlan<'_>, prefix: &str) -> Result<MarkScan, NodeError> {
        let mut marks = Marks::new(prefix, plan.limit);
        if let Some(source) = plan.incoming {
            self.mark_source(
                MarkInput {
                    source,
                    job: None,
                    use_kind: BlobUse::Needed,
                    missing: MissingManifest::Refuse,
                },
                &mut marks,
            )?;
        }
        for id in self.store.ids_iter()? {
            let id = id?;
            let spec = self.store.spec(&id)?;
            let use_kind = if plan.keep == Keep::Unfinished
                && self.store.phase(&id)?.kind() == PhaseKind::Finished
            {
                BlobUse::Spent
            } else {
                BlobUse::Needed
            };
            self.mark_spec(&spec, use_kind, &mut marks)?;
        }
        for id in self.store.staged_ids_iter()? {
            let id = id?;
            self.mark_spec(&self.store.staged_spec(&id)?, BlobUse::Needed, &mut marks)?;
        }
        if marks.overflow {
            Ok(MarkScan::Split(marks.children.into_iter().collect()))
        } else {
            Ok(MarkScan::Ready(marks))
        }
    }

    fn mark_spec(
        &self,
        spec: &Spec,
        use_kind: BlobUse,
        marks: &mut Marks,
    ) -> Result<(), NodeError> {
        if let Some(source) = spec.source() {
            self.mark_source(
                MarkInput {
                    source,
                    job: Some(&spec.id),
                    use_kind,
                    missing: MissingManifest::Ignore,
                },
                marks,
            )?;
        }
        Ok(())
    }

    fn mark_source(&self, input: MarkInput<'_>, marks: &mut Marks) -> Result<(), NodeError> {
        marks.record(&input.source.manifest, BlobUse::Needed);
        match self.cas.manifest(&input.source.manifest) {
            Ok(manifest) => {
                for entry in manifest.entries.values() {
                    if let Some(file) = entry.file() {
                        marks.record(file.blob, input.use_kind);
                    }
                }
            }
            Err(error @ (CasError::Missing(_) | CasError::Damaged(_))) => match input.missing {
                MissingManifest::Ignore => {}
                MissingManifest::Refuse => return Err(error.into()),
            },
            Err(other) => return Err(other.into()),
        }
        if let Some(id) = input.job {
            for left in self.store.left(id)?.unwrap_or_default() {
                if let Some(file) = left.now.as_ref().and_then(crate::snapshot::Entry::file) {
                    marks.record(file.blob, input.use_kind);
                }
            }
        }
        Ok(())
    }

    fn recover(&self, id: &JobId) -> Result<(), NodeError> {
        let phase = self.store.phase(id)?;
        if phase.kind() == PhaseKind::Finished {
            return Ok(());
        }
        let Some(alive) = crate::lock::OsLock::try_exclusive(&self.store.alive_path(id))? else {
            return Ok(());
        };
        let failure = self.store.start_failure(id)?;
        let (started_at, reason) = match failure {
            Some(why) => (None, format!("the job could not start: {why}")),
            None => match &phase {
                Phase::Queued | Phase::Preparing { .. } => return Ok(alive.release()?),
                Phase::Starting { started_at, .. } | Phase::Running { started_at, .. } => {
                    let rebooted = match (
                        self.store.supervisor_boot(id)?,
                        crate::platform::boot_identity(),
                    ) {
                        (Some(before), Some(now)) if before != now => " when the machine restarted",
                        (Some(_), Some(_)) => " without the machine restarting",
                        (Some(_) | None, Some(_) | None) => "",
                    };
                    (
                        Some(*started_at),
                        format!(
                            "its supervisor vanished{rebooted} after the command may have started; it was not run again because it may already have had effects"
                        ),
                    )
                }
                Phase::Finished { .. } => return Ok(()),
            },
        };
        let finished = Phase::Finished {
            started_at,
            finished_at: Timestamp::observe(),
            outcome: crate::protocol::Outcome::Errored {
                reason: RemoteText::new(reason),
            },
        };
        if let Some(root) = crate::supervisor::fresh_root(&self.store, &self.store.spec(id)?) {
            crate::supervisor::discard_workspace(&root)?;
        }
        self.store.set_phase(id, &finished)?;
        Ok(alive.release()?)
    }

    fn retry(&self, authorized: &AuthorizedRetry) -> Result<JobId, NodeError> {
        let id = self.own(authorized.principal(), authorized.payload())?;
        let phase = self.store.phase(&id)?;
        if phase.kind() == PhaseKind::Finished {
            return Ok(id);
        }
        let Some(alive) = crate::lock::OsLock::try_exclusive(&self.store.alive_path(&id))? else {
            return Ok(id);
        };
        match phase {
            Phase::Queued | Phase::Preparing { .. } => {
                alive.release()?;
                self.launch_supervisor(&id)?;
                Ok(id)
            }
            Phase::Starting { .. } | Phase::Running { .. } => {
                alive.release()?;
                Err(NodeError::UnsafeRetry(id))
            }
            Phase::Finished { .. } => Ok(id),
        }
    }

    fn kill(&self, authorized: &AuthorizedKill) -> Result<JobId, NodeError> {
        let id = self.own(authorized.principal(), authorized.payload())?;
        self.recover(&id)?;
        Ok(id)
    }

    fn abandon_staging(&self, id: &JobId) -> Result<(), NodeError> {
        let Some(staging) = crate::lock::OsLock::try_exclusive(&self.store.staging_lock_path(id))?
        else {
            return Ok(());
        };
        let Some(alive) = crate::lock::OsLock::try_exclusive(&self.store.alive_path(id))? else {
            return Ok(staging.release()?);
        };
        self.store.discard_staged(id)?;
        alive.release()?;
        staging.release()?;
        Ok(self.store.forget_staging_lock(id)?)
    }

    fn list(
        &self,
        principal: &Principal,
        limit: u32,
    ) -> Result<(Vec<Job>, Vec<crate::protocol::Unreadable>), NodeError> {
        const MAX_LIST_JOBS: u32 = 1000;
        const MAX_UNREADABLE: usize = 1000;
        if limit > MAX_LIST_JOBS {
            return Err(NodeError::ListLimit(MAX_LIST_JOBS));
        }
        let keep = crate::domain::to_usize(limit);
        let mut jobs = std::collections::BTreeMap::new();
        let mut unreadable = Vec::new();
        for id in self.store.ids_iter()? {
            let id = id?;
            match self.store.job(&id) {
                Ok(job) => match principal.relation_to(&job.spec.submitted_by) {
                    Relation::Oversees | Relation::Submitted => {
                        if keep > 0 {
                            jobs.insert((job.spec.sequence, id), job);
                            if jobs.len() > keep {
                                jobs.pop_first();
                            }
                        }
                    }
                    Relation::Stranger => {}
                },
                Err(error) => match principal {
                    Principal::Owner => {
                        if unreadable.len() >= MAX_UNREADABLE {
                            return Err(NodeError::TooManyUnreadable(MAX_UNREADABLE));
                        }
                        unreadable.push(crate::protocol::Unreadable {
                            id,
                            why: RemoteText::new(error.to_string()),
                        });
                    }
                    Principal::Peer { .. } => {}
                },
            }
        }
        let jobs = jobs.into_iter().rev().map(|(_, job)| job).collect();
        Ok((jobs, unreadable))
    }

    fn settle(&self, id: &JobId, order: Order, input: impl Input) -> Result<Job, NodeError> {
        let session =
            Session::open(&self.store.control_path(id), order).map_err(NodeError::Control)?;
        if let Some(session) = session.into_option() {
            let session = abandon_when_the_client_leaves(input, session);
            session
                .relay(&mut std::io::sink())
                .map_err(NodeError::Control)?;
        }
        Ok(self.store.job(id)?)
    }

    fn open_log(&self, id: &JobId) -> Result<std::fs::File, NodeError> {
        let path = self.store.log_path(id);
        std::fs::File::open(&path).map_err(|source| {
            NodeError::Io(crate::failure::IoFailure {
                action: "opening",
                path,
                source,
            })
        })
    }

    fn logs(
        &self,
        id: &JobId,
        cursor: Cursor,
        (input, output): (impl Input, &mut dyn Write),
    ) -> Result<(), NodeError> {
        let mut file = self.open_log(id)?;
        streamed(output, |framed| {
            let mut offset = cursor.offset;
            if cursor.follow == Follow::UntilFinished {
                let order = Order::Follow { offset };
                let session = Session::open(&self.store.control_path(id), order)
                    .map_err(NodeError::Control)?;
                if let Some(session) = session.into_option() {
                    let session = abandon_when_the_client_leaves(input, session);
                    let relayed = session.relay(framed).map_err(NodeError::Output)?;
                    offset = offset.saturating_add(relayed);
                }
            }
            file.seek(SeekFrom::Start(offset))
                .map_err(NodeError::Input)?;
            std::io::copy(&mut file, framed).map_err(NodeError::Output)?;
            Ok(())
        })
    }

    fn digest(&self, id: &JobId, tail: u32) -> Result<crate::protocol::Digest, NodeError> {
        let job = self.store.job(id)?;
        let mut log = std::io::BufReader::new(self.open_log(id)?);
        let summary = crate::logscan::summarize(&mut log, tail)?;
        Ok(crate::protocol::Digest {
            job,
            lines: summary.lines,
            bytes: summary.bytes,
            tail: summary.tail.into_iter().map(RemoteText::new).collect(),
        })
    }

    fn search(
        &self,
        id: &JobId,
        pattern: &str,
        window: crate::logscan::Window,
    ) -> Result<crate::protocol::Found, NodeError> {
        let wanted = crate::logscan::pattern(pattern)?;
        let mut log = std::io::BufReader::new(self.open_log(id)?);
        let found = crate::logscan::search(&mut log, &wanted, window)?;
        Ok(crate::protocol::Found {
            hits: found
                .hits
                .into_iter()
                .map(|hit| crate::protocol::FoundLine {
                    line: hit.line,
                    text: RemoteText::new(hit.text),
                    matched: hit.matched,
                })
                .collect(),
            matched: found.matched,
            truncated: found.truncated,
        })
    }

    fn tail(&self, id: &JobId, lines: u32, output: &mut dyn Write) -> Result<(), NodeError> {
        let mut file = self.open_log(id)?;
        let size = file.seek(SeekFrom::End(0)).map_err(NodeError::Input)?;
        let start = size.saturating_sub(crate::bounded::TAIL_WINDOW);
        file.seek(SeekFrom::Start(start))
            .map_err(NodeError::Input)?;
        let window = crate::bounded::to_end(
            &mut file.take(crate::bounded::TAIL_WINDOW),
            crate::bounded::TAIL_WINDOW,
        )
        .map_err(NodeError::Input)?;
        let wanted = crate::domain::to_usize(lines);
        let starts: Vec<usize> = std::iter::once(0)
            .chain(
                window
                    .iter()
                    .enumerate()
                    .filter(|(_, b)| **b == b'\n')
                    .map(|(i, _)| i.saturating_add(1)),
            )
            .filter(|at| *at < window.len())
            .collect();
        let from = starts
            .len()
            .checked_sub(wanted)
            .and_then(|skip| starts.get(skip))
            .copied()
            .unwrap_or(0);
        streamed(output, |framed| {
            framed
                .write_all(window.get(from..).unwrap_or(&[]))
                .map_err(NodeError::Output)
        })
    }

    fn get(
        &self,
        id: &JobId,
        path: &crate::domain::RelPath,
        output: &mut dyn Write,
    ) -> Result<(), NodeError> {
        let job = self.store.job(id)?;
        let subdir = match &job.spec.location {
            Location::Snapshot { subdir, .. } => subdir,
            Location::Home => return Err(NodeError::NoWorkspace(job.spec.id)),
        };
        let (_, workspace) = self.workspace_of(id)?;
        let inside = match subdir {
            Some(sub) => format!("{sub}/{path}").parse::<crate::domain::RelPath>()?,
            None => path.clone(),
        };
        let mut file = workspace.open_file(&inside)?;
        streamed(output, |framed| {
            std::io::copy(&mut file, framed)
                .map(drop)
                .map_err(NodeError::Output)
        })
    }

    fn changes(&self, id: &JobId, output: &mut dyn Write) -> Result<(), NodeError> {
        let job = self.store.job(id)?;
        let source = job
            .spec
            .source()
            .ok_or_else(|| NodeError::NoWorkspace(job.spec.id.clone()))?;
        if !job.is_settled() {
            return Err(NodeError::Unfinished(job.spec.id));
        }
        let raw = self.cas.get(&source.manifest)?;
        let (left, workspace) = match self.store.left(id)? {
            Some(left) => (left, None),
            None => {
                let sent = self.cas.manifest(&source.manifest)?;
                let (_, workspace) = self.workspace_of(id)?;
                (workspace.left(&sent)?, Some(workspace))
            }
        };
        let header = crate::snapshot::Changed {
            sent: crate::domain::len_u64(raw.len()),
            left,
        };
        streamed(output, |framed| {
            let mut line = serde_json::to_vec(&header).map_err(|e| NodeError::Output(e.into()))?;
            line.push(b'\n');
            framed.write_all(&line).map_err(NodeError::Output)?;
            framed.write_all(&raw).map_err(NodeError::Output)?;
            for item in &header.left {
                if let Some(file) = item.now.as_ref().and_then(crate::snapshot::Entry::file) {
                    let copied = match &workspace {
                        Some(workspace) => {
                            let opened = workspace.open_file(&item.path)?;
                            std::io::copy(&mut opened.take(file.size), framed)
                                .map_err(NodeError::Output)?
                        }
                        None => self.cas.stream(file.blob, framed)?,
                    };
                    if copied != file.size {
                        return Err(NodeError::Output(std::io::Error::other(format!(
                            "{} changed while it was being sent",
                            item.path
                        ))));
                    }
                }
            }
            Ok(())
        })
    }

    fn watch(
        &self,
        principal: &Principal,
        mut input: impl Input,
        output: &mut dyn Write,
    ) -> Result<(), NodeError> {
        let jobs = self.store.area("jobs");
        let (wake, woken) = std::sync::mpsc::channel::<WatchWake>();
        let changed = wake.clone();
        let area = jobs.clone();
        let mut notifier =
            notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                if let Ok(event) = event
                    && event.paths.iter().any(|path| match telling(&area, path) {
                        WatchedPath::JobChange => true,
                        WatchedPath::Other => false,
                    })
                {
                    match changed.send(WatchWake::Changed) {
                        Ok(()) | Err(_) => {}
                    }
                }
            })
            .map_err(|error| watching(&jobs, &error))?;
        notify::Watcher::watch(&mut notifier, &jobs, notify::RecursiveMode::Recursive)
            .map_err(|error| watching(&jobs, &error))?;
        std::thread::spawn(move || {
            let mut buffer = [0u8; 256];
            while let Ok(read) = input.read(&mut buffer) {
                if read == 0 {
                    break;
                }
            }
            match wake.send(WatchWake::ClientGone) {
                Ok(()) | Err(_) => {}
            }
        });
        streamed(output, |framed| {
            loop {
                let (listed, _unreadable) = self.list(principal, 50)?;
                let survey = crate::protocol::Survey {
                    report: self.report()?,
                    jobs: listed,
                };
                let mut line =
                    serde_json::to_vec(&survey).map_err(|e| NodeError::Output(e.into()))?;
                line.push(b'\n');
                framed.write_all(&line).map_err(NodeError::Output)?;
                framed.flush().map_err(NodeError::Output)?;
                match woken.recv() {
                    Ok(WatchWake::Changed) => {
                        if woken.try_iter().any(|signal| match signal {
                            WatchWake::Changed => false,
                            WatchWake::ClientGone => true,
                        }) {
                            return Ok(());
                        }
                    }
                    Ok(WatchWake::ClientGone) | Err(_) => return Ok(()),
                }
            }
        })
    }

    fn workspace_of(
        &self,
        id: &JobId,
    ) -> Result<(PathBuf, crate::workspace::Workspace), NodeError> {
        let recorded = self.store.workspace_record(id);
        let root = crate::state_file::read_bytes(&recorded)?
            .ok_or_else(|| NodeError::NoWorkspace(id.clone()))?;
        let root = PathBuf::from(String::from_utf8_lossy(&root).trim());
        let filled_by = crate::state_file::read_bytes(&crate::supervisor::filled_by_path(&root))?;
        if let Some(other) = filled_by.filter(|by| by.as_slice() != id.as_str().as_bytes()) {
            return Err(NodeError::Reused {
                job: id.clone(),
                by: String::from_utf8_lossy(&other).trim().to_owned(),
            });
        }
        let workspace = crate::workspace::Workspace::open_existing(&root)?
            .ok_or_else(|| NodeError::NoWorkspace(id.clone()))?;
        Ok((root, workspace))
    }
}

fn abandon_when_the_client_leaves(mut input: impl Input, session: Session) -> Arc<Session> {
    let session = Arc::new(session);
    let watched = Arc::clone(&session);
    std::thread::spawn(move || {
        let mut buffer = [0u8; 256];
        loop {
            match input.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
        match watched.abandon() {
            Ok(()) | Err(_) => {}
        }
    });
    session
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authz::Commanded;
    use crate::domain::{Concurrency, Nonce as RetryNonce};
    use crate::protocol::{Change, Command, Location, Spec, Submission};
    use crate::store::LaunchEnv;

    fn dirs(root: &Path) -> Dirs {
        Dirs::for_test(root)
    }

    fn command(request: Request) -> Commanded {
        let authorized = authz::authorize(
            Principal::Owner,
            crate::ingress::PeerRequest::for_test(request),
        )
        .unwrap();
        match authorized.route() {
            Routed::Command(commanded) => commanded,
            Routed::Query(_) => panic!("expected a command"),
        }
    }

    fn commanded() -> Commanded {
        command(Request::Clean {
            apply: false,
            logs: false,
            idle: false,
        })
    }

    fn authorized_configure(change: Change) -> AuthorizedConfigure {
        let CommandAction::Configure(authorized) = command(Request::Configure { change })
            .into_action()
            .unwrap()
        else {
            panic!("configure is a command");
        };
        authorized
    }

    fn authorized_clean(options: (bool, bool, bool)) -> AuthorizedClean {
        let (apply, logs, idle) = options;
        let CommandAction::Clean(authorized) = command(Request::Clean { apply, logs, idle })
            .into_action()
            .unwrap()
        else {
            panic!("clean is a command");
        };
        authorized
    }

    fn submitted(submission: Submission) -> AuthorizedSubmission {
        command(Request::Submit {
            submission: Box::new(submission),
        })
        .into_submission()
        .unwrap()
    }

    fn query(candidate: Request) -> Option<Request> {
        let authorized = authz::authorize(
            Principal::Owner,
            crate::ingress::PeerRequest::for_test(candidate),
        )
        .unwrap();
        match authorized.route() {
            Routed::Query(queried) => Some(queried.into_parts().1),
            Routed::Command(_) => None,
        }
    }

    fn stage_with_sequence(store: &Store, id: &JobId, script: &str, sequence: u64) {
        let spec = Spec {
            id: id.clone(),
            name: None,
            command: Command::Script(script.into()),
            location: Location::Home,
            env_names: std::collections::BTreeSet::new(),
            shell: None,
            concurrency: Concurrency::DEFAULT,
            sequence,
            submitted_by: authz::Submitter::Owner,
            submitted_at: Timestamp::observe(),
        };
        store
            .stage(
                &spec,
                (&std::collections::BTreeMap::new(), &LaunchEnv::default()),
            )
            .unwrap();
    }

    fn staged(root: &Path, id: &JobId, script: &str) -> Store {
        let store = Store::open(&dirs(root)).unwrap();
        stage_with_sequence(&store, id, script, 1);
        store
    }

    fn published(root: &Path, log: &[u8]) -> (Node, JobRef) {
        let id: JobId = "0AAAAAAAAAAAAAAA".parse().unwrap();
        let store = staged(root, &id, "true");
        store.publish(&id).unwrap();
        store
            .force_phase(
                &id,
                &Phase::Finished {
                    started_at: None,
                    finished_at: Timestamp::at_millis(2),
                    outcome: crate::protocol::Outcome::Succeeded,
                },
            )
            .unwrap();
        crate::state_file::write_bytes(&store.log_path(&id), log).unwrap();
        (
            Node {
                pressure: |_| Ok(DiskPressure::Enough),
                ..Node::open(dirs(root)).unwrap()
            },
            id.as_str().parse().unwrap(),
        )
    }

    #[test]
    fn listing_keeps_only_requested_recent_jobs_and_rejects_an_excess_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&dirs(tmp.path())).unwrap();
        for (sequence, text) in [
            "0AAAAAAAAAAAAAAA",
            "0BBBBBBBBBBBBBBB",
            "0CCCCCCCCCCCCCCC",
            "0DDDDDDDDDDDDDDD",
        ]
        .into_iter()
        .enumerate()
        {
            let id: JobId = text.parse().unwrap();
            stage_with_sequence(
                &store,
                &id,
                "true",
                crate::domain::len_u64(sequence.saturating_add(1)),
            );
            store.publish(&id).unwrap();
        }
        let node = Node::open(dirs(tmp.path())).unwrap();
        let (jobs, unreadable) = node.list(&Principal::Owner, 2).unwrap();
        assert!(unreadable.is_empty());
        assert_eq!(
            jobs.iter().map(|job| job.spec.sequence).collect::<Vec<_>>(),
            [4, 3]
        );
        assert!(matches!(
            node.list(&Principal::Owner, 1001),
            Err(NodeError::ListLimit(1000))
        ));
    }

    #[test]
    fn admission_counts_staged_and_published_unfinished_jobs_but_not_finished_jobs() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&dirs(tmp.path())).unwrap();
        for index in 0..MAX_UNFINISHED_JOBS {
            let id: JobId = format!("{index:016X}").parse().unwrap();
            stage_with_sequence(&store, &id, "true", crate::domain::len_u64(index));
            if index == 0 {
                store.publish(&id).unwrap();
            }
        }
        let finished: JobId = "0AAAAAAAAAAAAAAA".parse().unwrap();
        stage_with_sequence(&store, &finished, "true", 100);
        store.publish(&finished).unwrap();
        store
            .force_phase(
                &finished,
                &Phase::Finished {
                    started_at: None,
                    finished_at: Timestamp::at_millis(1),
                    outcome: crate::protocol::Outcome::Succeeded,
                },
            )
            .unwrap();
        let node = Node {
            pressure: |_| Ok(DiskPressure::Enough),
            ..Node::open(dirs(tmp.path())).unwrap()
        };
        let collecting = crate::lock::OsLock::exclusive(&store.collection_lock_path()).unwrap();
        assert!(matches!(
            node.admit_job(&collecting),
            Err(NodeError::JobCapacity(MAX_UNFINISHED_JOBS))
        ));
        collecting.release().unwrap();
        let submit = |nonce| Submission {
            queue: crate::protocol::Queue::Slot,
            nonce,
            name: None,
            command: Command::Script("true".into()),
            location: Location::Home,
            env: std::collections::BTreeMap::new(),
            shell: None,
        };
        let fresh = crate::domain::Nonce::generate().unwrap();
        assert!(matches!(
            node.accept(submitted(submit(fresh))),
            Err(NodeError::JobCapacity(MAX_UNFINISHED_JOBS))
        ));
        let replay = crate::domain::Nonce::generate().unwrap();
        let nonce_path = store.nonce_path("owner", &replay);
        crate::state_file::write_bytes(&nonce_path, finished.as_str().as_bytes()).unwrap();
        let job = node.accept(submitted(submit(replay))).unwrap();
        assert_eq!(job.spec.id, finished);
        let first: JobId = "0000000000000000".parse().unwrap();
        store
            .force_phase(
                &first,
                &Phase::Finished {
                    started_at: None,
                    finished_at: Timestamp::at_millis(2),
                    outcome: crate::protocol::Outcome::Succeeded,
                },
            )
            .unwrap();
        let collecting_after_finish =
            crate::lock::OsLock::exclusive(&store.collection_lock_path()).unwrap();
        node.admit_job(&collecting_after_finish).unwrap();
        collecting_after_finish.release().unwrap();
    }

    fn ask(node: &Node, request: &Request) -> Vec<u8> {
        let mut line = serde_json::to_vec(request).unwrap();
        line.push(b'\n');
        let mut out = Vec::new();
        node.serve(&Principal::Owner, std::io::Cursor::new(line), &mut out)
            .unwrap();
        out
    }

    fn ask_as(node: &Node, principal: &Principal, request: &Request) -> Vec<u8> {
        let mut line = serde_json::to_vec(request).unwrap();
        line.push(b'\n');
        let mut out = Vec::new();
        node.serve(principal, std::io::Cursor::new(line), &mut out)
            .unwrap();
        out
    }

    #[test]
    fn what_changes_a_job_and_everything_a_peer_asks_is_audited_with_its_subject() {
        let tmp = tempfile::tempdir().unwrap();
        let (node, job) = published(tmp.path(), b"");
        let peer = Principal::Peer {
            key: crate::trust::PublicKey::from_slice(&[9; 32]).unwrap(),
            label: "mac".parse().unwrap(),
            capabilities: std::collections::BTreeSet::new(),
        };
        ask(&node, &Request::Status { job: job.clone() });
        ask(&node, &Request::List { limit: 5 });
        assert!(node.audit.tail(10).unwrap().is_empty());
        ask(&node, &Request::Kill { job: job.clone() });
        for request in [
            Request::Tail {
                job: job.clone(),
                lines: 3,
            },
            Request::Get {
                job: job.clone(),
                path: "out/a.txt".parse().unwrap(),
            },
            submission(crate::domain::Nonce::generate().unwrap(), Location::Home),
            Request::List { limit: 5 },
        ] {
            let reply = ask_as(&node, &peer, &request);
            let refused = crate::remote::receive("m", &mut reply.as_slice(), &mut Vec::new());
            assert!(
                matches!(&refused, Ok(Reply::Refused(refusal)) if refusal.code == RefusalCode::Forbidden),
                "{refused:?}"
            );
        }
        let seen: Vec<(String, String, Option<String>, Verdict)> = node
            .audit
            .tail(10)
            .unwrap()
            .into_iter()
            .map(|entry| (entry.principal, entry.action, entry.subject, entry.verdict))
            .collect();
        let peer_name = peer.describe();
        let denied = Verdict::Denied;
        assert_eq!(
            seen,
            [
                (
                    "owner".to_owned(),
                    "kill".to_owned(),
                    Some(job.to_string()),
                    Verdict::Allowed
                ),
                (
                    peer_name.clone(),
                    "tail".to_owned(),
                    Some(job.to_string()),
                    denied
                ),
                (
                    peer_name.clone(),
                    "get".to_owned(),
                    Some(format!("{job} out/a.txt")),
                    denied
                ),
                (
                    peer_name.clone(),
                    "submit".to_owned(),
                    Some("true".to_owned()),
                    denied
                ),
                (peer_name, "list".to_owned(), None, denied),
            ]
        );
    }

    fn refused(wire: &[u8]) -> bool {
        match crate::remote::receive("m", &mut &wire[..], &mut Vec::new()) {
            Ok(Reply::Refused(_)) | Err(crate::remote::RemoteError::Stream { .. }) => true,
            Ok(_) | Err(_) => false,
        }
    }

    fn submission(nonce: crate::domain::Nonce, location: Location) -> Request {
        Request::Submit {
            submission: Box::new(Submission {
                queue: crate::protocol::Queue::Slot,
                nonce,
                name: None,
                command: Command::Script("true".into()),
                location,
                env: std::collections::BTreeMap::new(),
                shell: None,
            }),
        }
    }

    #[test]
    fn every_request_that_meets_a_failing_disk_or_a_missing_job_is_refused_never_a_panic() {
        let tmp = tempfile::tempdir().unwrap();
        let (node, job) = published(tmp.path(), b"a log\n");
        let store = Store::open(&dirs(tmp.path())).unwrap();
        let id = store.resolve(&job).unwrap();
        let unknown: JobRef = "0ZZZZZZZZZZZZZZZ".parse().unwrap();
        let text = |path: &Path| path.display().to_string();
        let nonce = crate::domain::Nonce::generate().unwrap();
        let snapshot = Location::Snapshot {
            source: Source {
                project: "proj".parse().unwrap(),
                manifest: BlobId::of(b"never sent"),
                revision: crate::protocol::Revision::WorkingDirectory,
            },
            subdir: None,
            workspace: crate::protocol::Workspace::Warm,
        };
        let cases: Vec<(Request, Option<(&str, String)>)> = vec![
            (
                Request::Status { job: job.clone() },
                Some((
                    "state_file::read",
                    text(&store.job_dir(&id).join("spec.json")),
                )),
            ),
            (
                Request::Status { job },
                Some((
                    "state_file::read",
                    text(&store.job_dir(&id).join("phase.json")),
                )),
            ),
            (
                Request::Status {
                    job: unknown.clone(),
                },
                None,
            ),
            (
                Request::Wait {
                    job: unknown.clone(),
                },
                None,
            ),
            (
                Request::Kill {
                    job: unknown.clone(),
                },
                None,
            ),
            (logs_of(&unknown), None),
            (
                Request::List { limit: 5 },
                Some(("store::list", text(&store.area("jobs")))),
            ),
            (submission(nonce.clone(), snapshot), None),
            (
                submission(nonce.clone(), Location::Home),
                Some(("state_file::lock", text(&store.collection_lock_path()))),
            ),
            (
                submission(nonce.clone(), Location::Home),
                Some(("state_file::read", text(&store.nonce_path("owner", &nonce)))),
            ),
        ];
        for (request, fault) in cases {
            let _faults = fault
                .as_ref()
                .map(|(site, tag)| crate::faults::inject(&[(site, tag)]));
            assert!(refused(&ask(&node, &request)), "{request:?}");
        }
    }

    fn state_of(root: &Path) -> std::collections::BTreeMap<String, Option<BlobId>> {
        let mut found = std::collections::BTreeMap::new();
        let mut pending = vec![root.to_path_buf()];
        while let Some(dir) = pending.pop() {
            for item in std::fs::read_dir(&dir).unwrap() {
                let path = item.unwrap().path();
                let name = path.strip_prefix(root).unwrap().display().to_string();
                if std::fs::symlink_metadata(&path).unwrap().is_dir() {
                    found.insert(name, None);
                    pending.push(path);
                } else {
                    found.insert(name, Some(BlobId::of(&std::fs::read(&path).unwrap())));
                }
            }
        }
        found
    }

    fn one_of_every_request(job: &JobRef) -> Vec<Request> {
        vec![
            Request::Hello,
            Request::Hold,
            submission(crate::domain::Nonce::generate().unwrap(), Location::Home),
            Request::List { limit: 10 },
            Request::Status { job: job.clone() },
            Request::Wait { job: job.clone() },
            Request::Retry { job: job.clone() },
            Request::Kill { job: job.clone() },
            logs_of(job),
            Request::Tail {
                job: job.clone(),
                lines: 2,
            },
            Request::Get {
                job: job.clone(),
                path: "a.txt".parse().unwrap(),
            },
            Request::Changes { job: job.clone() },
            Request::Report,
            Request::Watch,
            Request::Clean {
                apply: false,
                logs: false,
                idle: false,
            },
            Request::Configure {
                change: Change::default(),
            },
            Request::AuditAt { epoch: 0, seq: 0 },
            Request::AuditHead,
            Request::Digest {
                job: job.clone(),
                tail: 2,
            },
            Request::Search {
                job: job.clone(),
                pattern: "line".to_owned(),
                context: 1,
                limit: 5,
            },
        ]
    }

    #[test]
    fn no_question_changes_anything_on_the_machine_even_when_its_disk_is_short() {
        let tmp = tempfile::tempdir().unwrap();
        let (node, finished) = published(tmp.path(), b"a log\nwith lines\n");
        let node = Node {
            pressure: |_| Ok(DiskPressure::Short),
            ..node
        };
        let store = Store::open(&dirs(tmp.path())).unwrap();
        let first = store.resolve(&finished).unwrap();
        let mut vanished = store.spec(&first).unwrap();
        vanished.id = "0BBBBBBBBBBBBBBB".parse().unwrap();
        vanished.sequence = 2;
        store
            .stage(
                &vanished,
                (&std::collections::BTreeMap::new(), &LaunchEnv::default()),
            )
            .unwrap();
        store.publish(&vanished.id).unwrap();
        let running = Phase::Running {
            started_at: Timestamp::at_millis(1),
            pid: 1,
            workspace: String::new(),
        };
        store.force_phase(&vanished.id, &running).unwrap();
        let mut abandoned = vanished.clone();
        abandoned.id = "0CCCCCCCCCCCCCCC".parse().unwrap();
        abandoned.sequence = 3;
        store
            .stage(
                &abandoned,
                (&std::collections::BTreeMap::new(), &LaunchEnv::default()),
            )
            .unwrap();
        crate::state_file::write_bytes(&store.area("trash").join("old"), b"junk").unwrap();

        let every = one_of_every_request(&finished);
        let schema = schemars::schema_for!(Request);
        let variants = schema.as_value()["oneOf"].as_array().unwrap().len();
        assert_eq!(every.len(), variants, "a request is missing from this law");
        let state = tmp.path().join("state");
        let audit = state.join("audit.jsonl");
        let not_audit = |mut all: std::collections::BTreeMap<String, Option<BlobId>>| {
            all.retain(|path, _| !path.starts_with("audit."));
            all
        };
        let before = not_audit(state_of(&state));
        for request in &every {
            if let Some(question) = query(request.clone()) {
                let audited = crate::state_file::read_bytes(&audit)
                    .unwrap()
                    .unwrap_or_default();
                ask(&node, &question);
                let after = not_audit(state_of(&state));
                let changed: Vec<&String> = before
                    .keys()
                    .chain(after.keys())
                    .filter(|path| before.get(*path) != after.get(*path))
                    .collect();
                assert!(changed.is_empty(), "{question:?} changed {changed:?}");
                let now = crate::state_file::read_bytes(&audit)
                    .unwrap()
                    .unwrap_or_default();
                assert!(
                    now.starts_with(&audited),
                    "{question:?} rewrote the audit log"
                );
            }
        }
        ask(
            &node,
            &Request::Clean {
                apply: false,
                logs: false,
                idle: false,
            },
        );
        assert_eq!(
            store.job(&vanished.id).unwrap().state(),
            crate::protocol::State::Errored
        );
    }

    fn holds_anywhere(root: &Path, needle: &[u8]) -> Vec<String> {
        state_of(root)
            .keys()
            .map(|name| root.join(name))
            .filter(|path| std::fs::symlink_metadata(path).unwrap().is_file())
            .filter(|path| {
                std::fs::read(path)
                    .unwrap()
                    .windows(needle.len())
                    .any(|window| window == needle)
            })
            .map(|path| path.display().to_string())
            .collect()
    }

    #[test]
    fn no_secret_outlives_the_queue_whichever_way_a_job_ends() {
        const CANARY: &str = "canary-3f9a1c";
        let tmp = tempfile::tempdir().unwrap();
        let (node, finished) = published(tmp.path(), b"");
        let store = Store::open(&dirs(tmp.path())).unwrap();
        let template = store.spec(&store.resolve(&finished).unwrap()).unwrap();
        let env = std::collections::BTreeMap::from([(
            "API_TOKEN".parse().unwrap(),
            format!("{CANARY}-env"),
        )]);
        let mut launch = LaunchEnv::default();
        launch
            .vars
            .insert("SESSION_TOKEN".to_owned(), format!("{CANARY}-launch"));
        let state = tmp.path().join("state");
        let staged = |id: &str, sequence: u64| {
            let mut spec = template.clone();
            spec.id = id.parse().unwrap();
            spec.sequence = sequence;
            store.stage(&spec, (&env, &launch)).unwrap();
            store.publish(&spec.id).unwrap();
            assert!(!holds_anywhere(&state, CANARY.as_bytes()).is_empty());
            spec.id
        };
        let finished_as = |outcome| Phase::Finished {
            started_at: None,
            finished_at: Timestamp::at_millis(3),
            outcome,
        };
        let running = Phase::Running {
            started_at: Timestamp::at_millis(1),
            pid: 1,
            workspace: String::new(),
        };

        let killed = staged("0BBBBBBBBBBBBBBB", 2);
        store
            .force_phase(&killed, &finished_as(crate::protocol::Outcome::Killed))
            .unwrap();
        assert_eq!(
            holds_anywhere(&state, CANARY.as_bytes()),
            Vec::<String>::new()
        );

        let started = staged("0CCCCCCCCCCCCCCC", 3);
        let taken = store.take_launch(&started).unwrap();
        assert!(!format!("{taken:?}").contains(CANARY));
        assert_eq!(
            holds_anywhere(&state, CANARY.as_bytes()),
            Vec::<String>::new()
        );

        let vanished = staged("0DDDDDDDDDDDDDDD", 4);
        store.force_phase(&vanished, &running).unwrap();
        let unstarted = staged("0EEEEEEEEEEEEEEE", 5);
        store.record_start_failure(&unstarted, "no shell").unwrap();
        node.upkeep(&commanded()).unwrap();
        assert_eq!(
            holds_anywhere(&state, CANARY.as_bytes()),
            Vec::<String>::new()
        );

        let full_disk = staged("0FFFFFFFFFFFFFFF", 6);
        store
            .record_outcome_in_place(
                &full_disk,
                &finished_as(crate::protocol::Outcome::Succeeded),
            )
            .unwrap();
        assert_eq!(
            holds_anywhere(&state, CANARY.as_bytes()),
            Vec::<String>::new()
        );
    }

    fn logs_of(job: &JobRef) -> Request {
        Request::Logs {
            job: job.clone(),
            offset: 0,
            follow: Follow::Snapshot,
        }
    }

    #[test]
    fn a_bad_record_an_oversized_request_or_a_missing_log_is_refused_never_a_panic() {
        let tmp = tempfile::tempdir().unwrap();
        let (node, job) = published(tmp.path(), b"a log\n");
        let store = Store::open(&dirs(tmp.path())).unwrap();
        let id = store.resolve(&job).unwrap();
        let nonce = crate::domain::Nonce::generate().unwrap();
        let nonce_path = store.nonce_path("owner", &nonce);
        for recorded in [b"not a job id".to_vec(), b"0ZZZZZZZZZZZZZZZ".to_vec()] {
            crate::state_file::write_bytes(&nonce_path, &recorded).unwrap();
            assert!(refused(&ask(
                &node,
                &submission(nonce.clone(), Location::Home)
            )));
        }
        let mut huge = vec![b'a'; 2 << 20];
        huge.push(b'\n');
        let mut out = Vec::new();
        node.serve(&Principal::Owner, std::io::Cursor::new(huge), &mut out)
            .unwrap();
        assert!(refused(&out));
        crate::state_file::remove_file(&store.log_path(&id)).unwrap();
        assert!(refused(&ask(&node, &logs_of(&job))));
        assert!(refused(&ask(&node, &Request::Tail { job, lines: 3 })));
    }

    #[test]
    fn the_agent_a_job_uses_is_the_one_the_machines_ssh_configuration_names() {
        if !crate::platform::FAMILY.agent_socket() {
            return;
        }
        let home = Path::new("/home/me");
        let printed = |agent: &str| format!("user me\nidentityagent {agent}\nport 22\n");
        assert_eq!(
            identity_agent(&printed("~/.agent/agent.sock"), home),
            Some(home.join(".agent/agent.sock"))
        );
        assert_eq!(
            identity_agent(&printed("/run/agent.sock"), home),
            Some(PathBuf::from("/run/agent.sock"))
        );
        for unusable in ["none", "SSH_AUTH_SOCK", "$AGENT", "relative.sock"] {
            assert_eq!(identity_agent(&printed(unusable), home), None, "{unusable}");
        }
        assert_eq!(identity_agent("user me\n", home), None);
        let launch = LaunchEnv::default().with_agent(Some(PathBuf::from("/run/agent.sock")));
        assert_eq!(
            launch.vars.get("SSH_AUTH_SOCK").map(String::as_str),
            Some("/run/agent.sock")
        );
        assert!(LaunchEnv::default().with_agent(None).vars.is_empty());
    }

    #[test]
    fn a_disk_is_short_below_a_tenth_of_its_size_or_ten_gigabytes() {
        let gib: u64 = 1 << 30;
        assert!(matches!(
            pressure(ROOM_AT_LEAST - 1, 500 * gib),
            DiskPressure::Short
        ));
        assert!(matches!(
            pressure(ROOM_AT_LEAST, 500 * gib),
            DiskPressure::Enough
        ));
        assert!(matches!(
            pressure(50 * gib / 10 - 1, 50 * gib),
            DiskPressure::Short
        ));
        assert!(matches!(
            pressure(50 * gib / 10, 50 * gib),
            DiskPressure::Enough
        ));
        assert!(matches!(pressure(1, 0), DiskPressure::Enough));
    }

    #[test]
    fn only_the_current_and_two_other_binary_builds_are_kept() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = dirs(tmp.path());
        let bin = dirs.cache().join("bin");
        for name in ["current", "100", "200", "300", "400"] {
            crate::state_file::private_dir(&bin.join(name)).unwrap();
        }
        let last = crate::bounded::DIRECTORY_BATCH.get();
        for index in 0..=last {
            crate::state_file::private_dir(&bin.join(format!("build-{index:03}"))).unwrap();
        }
        retire_cached_binaries(&dirs, "current");
        let mut kept: Vec<String> = std::fs::read_dir(bin)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        kept.sort();
        assert_eq!(
            kept,
            [
                format!("build-{:03}", last.saturating_sub(1)),
                format!("build-{last:03}"),
                "current".to_owned(),
            ]
        );
    }

    #[test]
    fn a_managed_binary_file_survives_legacy_directory_reclamation() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = dirs(tmp.path());
        let bin = dirs.cache().join("bin");
        for name in ["current", "100", "200", "300", "400"] {
            crate::state_file::private_dir(&bin.join(name)).unwrap();
        }
        let managed = bin.join("domyjob-new-build");
        crate::state_file::write_bytes(&managed, b"binary").unwrap();
        retire_cached_binaries(&dirs, "current");
        assert_eq!(std::fs::read(managed).unwrap(), b"binary");
    }

    #[test]
    fn a_roomy_disk_keeps_every_log_and_small_ones_are_never_replaced() {
        let tmp = tempfile::tempdir().unwrap();
        let (node, job) = published(tmp.path(), &[b'x'; 10_000]);
        let store = Store::open(&dirs(tmp.path())).unwrap();
        let id = store.resolve(&job).unwrap();
        node.make_room(&|| Ok(DiskPressure::Enough), &commanded(), &Location::Home)
            .unwrap();
        assert_eq!(std::fs::read(store.log_path(&id)).unwrap(), [b'x'; 10_000]);
        let same_size = vec![b'z'; DISCARDED.len()];
        for log in [b"tiny".to_vec(), same_size] {
            crate::state_file::write_bytes(&store.log_path(&id), &log).unwrap();
            node.make_room(&|| Ok(DiskPressure::Short), &commanded(), &Location::Home)
                .unwrap();
            assert_eq!(std::fs::read(store.log_path(&id)).unwrap(), log);
        }
    }

    #[test]
    fn retiring_nothing_leaves_fresh_uploads_and_failing_steps_are_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let (node, _) = published(tmp.path(), b"");
        let store = Store::open(&dirs(tmp.path())).unwrap();
        let first: JobId = "0AAAAAAAAAAAAAAA".parse().unwrap();
        finished_like(&store, &first, &["0FFFFFFFFFFFFFFF"]);
        let upload = BlobId::of(b"on its way");
        node.cas.put(&upload, b"on its way").unwrap();
        node.retire_including(2, None).unwrap();
        assert!(node.cas.stored().unwrap().contains(&upload));
        let nonce = crate::domain::Nonce::generate().unwrap();
        let nonce_path = store.nonce_path("owner", &nonce);
        crate::state_file::write_bytes(&nonce_path, first.as_str().as_bytes()).unwrap();
        let text = |path: &Path| path.display().to_string();
        for (site, tag) in [
            ("store::list", text(&store.area("jobs"))),
            ("state_file::lock", text(&store.alive_path(&first))),
            ("state_file::remove", text(&store.job_dir(&first))),
            ("state_file::read", text(&nonce_path)),
        ] {
            let _faults = crate::faults::inject(&[(site, &tag)]);
            node.retire_including(1, None).unwrap_err();
        }
    }

    #[test]
    fn the_sweep_leaves_finished_jobs_alone_and_survives_every_failing_step() {
        let tmp = tempfile::tempdir().unwrap();
        let (node, job) = published(tmp.path(), b"");
        let store = Store::open(&dirs(tmp.path())).unwrap();
        let id = store.resolve(&job).unwrap();
        crate::state_file::write_bytes(&store.job_dir(&id).join("failure"), b"once").unwrap();
        let finished = store.phase(&id).unwrap();
        node.upkeep(&commanded()).unwrap();
        assert_eq!(store.phase(&id).unwrap(), finished);
        let open: JobId = "0FFFFFFFFFFFFFFF".parse().unwrap();
        finished_like(&store, &id, &[open.as_str()]);
        let running = Phase::Running {
            started_at: Timestamp::at_millis(1),
            pid: 1,
            workspace: String::new(),
        };
        let staged: JobId = "0GGGGGGGGGGGGGGG".parse().unwrap();
        let mut spec = store.spec(&id).unwrap();
        spec.id = staged.clone();
        let text = |path: &Path| path.display().to_string();
        for (site, tag) in [
            ("state_file::lock", text(&store.alive_path(&open))),
            (
                "state_file::read",
                text(&store.job_dir(&open).join("failure")),
            ),
            ("state_file::lock", text(&store.staging_lock_path(&staged))),
            ("state_file::lock", text(&store.alive_path(&staged))),
            (
                "state_file::remove",
                text(&store.area("staging").join(staged.as_str())),
            ),
            (
                "state_file::remove",
                text(&store.staging_lock_path(&staged)),
            ),
        ] {
            store.force_phase(&open, &running).unwrap();
            if !store.staged_ids().unwrap().contains(&staged) {
                store
                    .stage(
                        &spec,
                        (&std::collections::BTreeMap::new(), &LaunchEnv::default()),
                    )
                    .unwrap();
            }
            let _faults = crate::faults::inject(&[(site, &tag)]);
            node.upkeep(&commanded()).unwrap_err();
        }
        Node::open(node.dirs.clone()).unwrap();
        for area in [
            node.dirs.state().to_path_buf(),
            store.area(""),
            node.dirs.state().join("objects"),
        ] {
            let _faults = crate::faults::inject(&[("state_file::dir", &text(&area))]);
            Node::open(node.dirs.clone()).unwrap_err();
        }
    }

    struct Unwritable {
        flush_only: bool,
    }

    impl Write for Unwritable {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if self.flush_only {
                Ok(bytes.len())
            } else {
                Err(std::io::ErrorKind::BrokenPipe.into())
            }
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::ErrorKind::BrokenPipe.into())
        }
    }

    #[test]
    fn an_answer_that_cannot_be_written_is_an_error_never_a_panic() {
        let tmp = tempfile::tempdir().unwrap();
        let (node, job) = published(tmp.path(), b"a log\n");
        for request in [Request::List { limit: 5 }, logs_of(&job)] {
            for flush_only in [false, true] {
                let mut line = serde_json::to_vec(&request).unwrap();
                line.push(b'\n');
                node.serve(
                    &Principal::Owner,
                    std::io::Cursor::new(line),
                    &mut Unwritable { flush_only },
                )
                .unwrap_err();
            }
        }
    }

    #[test]
    fn a_finished_job_sends_back_exactly_what_it_changed_and_an_unfinished_one_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let (node, job) = published(tmp.path(), b"");
        let store = Store::open(&dirs(tmp.path())).unwrap();
        let id = store.resolve(&job).unwrap();
        let workspace = tmp.path().join("home").join("ws");
        for (name, text) in [
            ("keep.txt", "same"),
            ("edit.txt", "old"),
            ("drop.txt", "bye"),
        ] {
            crate::state_file::write_bytes(&workspace.join(name), text.as_bytes()).unwrap();
        }
        crate::state_file::write_bytes(&workspace.join(".gitignore"), b"target/\n").unwrap();
        let sent = crate::snapshot::from_directory(&workspace).unwrap();
        let (manifest, bytes) = sent.manifest.encode().unwrap();
        node.cas.put(&manifest, &bytes).unwrap();
        sent_manifest(&store, &id, &manifest);
        crate::state_file::write_bytes(
            &store.workspace_record(&id),
            workspace.display().to_string().as_bytes(),
        )
        .unwrap();
        crate::state_file::write_bytes(&workspace.join("edit.txt"), b"new").unwrap();
        crate::state_file::remove_file(&workspace.join("drop.txt")).unwrap();
        crate::state_file::write_bytes(&workspace.join("born.txt"), b"hi").unwrap();
        crate::state_file::write_bytes(&workspace.join("target/out.bin"), b"built").unwrap();
        crate::state_file::write_bytes(&workspace.join(".git/config"), b"[core]").unwrap();
        crate::state_file::write_bytes(&tmp.path().join("home/.gitignore"), b"*\n").unwrap();

        let mut payload = Vec::new();
        let wire = ask(&node, &Request::Changes { job: job.clone() });
        let reply = crate::remote::receive("m", &mut wire.as_slice(), &mut payload).unwrap();
        assert!(matches!(reply, Reply::Stream), "{reply:?}");
        let end = payload.iter().position(|b| *b == b'\n').unwrap();
        let changed: crate::snapshot::Changed =
            crate::ingress::json(payload.get(..end).unwrap()).unwrap();
        let paths: Vec<String> = changed.left.iter().map(|c| c.path.to_string()).collect();
        assert_eq!(paths, ["born.txt", "drop.txt", "edit.txt"]);
        let rest = payload.get(end + 1..).unwrap();
        let (raw, files) = rest.split_at(usize::try_from(changed.sent).unwrap());
        assert_eq!(BlobId::of(raw), manifest);
        assert_eq!(files, b"hinew");

        let recorded = crate::workspace::Workspace::open_existing(&workspace)
            .unwrap()
            .unwrap()
            .left(&sent.manifest)
            .unwrap();
        for item in &recorded {
            if let Some(crate::snapshot::Entry::File { blob, .. }) = &item.now {
                let content = std::fs::read(workspace.join(item.path.as_str())).unwrap();
                node.cas.put(blob, &content).unwrap();
            }
        }
        crate::state_file::write_json(&store.left_path(&id), &recorded).unwrap();
        drop(sent);
        crate::state_file::remove_dir_all(&workspace).unwrap();
        let mut kept = Vec::new();
        let again = ask(&node, &Request::Changes { job: job.clone() });
        crate::remote::receive("m", &mut again.as_slice(), &mut kept).unwrap();
        assert_eq!(kept, payload, "a job's changes outlive its workspace");

        store.force_phase(&id, &Phase::Queued).unwrap();
        let _alive = crate::lock::OsLock::exclusive(&store.alive_path(&id)).unwrap();
        assert!(refused(&ask(&node, &Request::Changes { job })));
    }

    fn sent_manifest(store: &Store, id: &JobId, manifest: &BlobId) {
        let mut spec = store.spec(id).unwrap();
        spec.location = Location::Snapshot {
            source: Source {
                project: "proj".parse().unwrap(),
                manifest: manifest.clone(),
                revision: crate::protocol::Revision::WorkingDirectory,
            },
            subdir: None,
            workspace: crate::protocol::Workspace::Warm,
        };
        crate::state_file::write_json(&store.job_dir(id).join("spec.json"), &spec).unwrap();
    }

    #[test]
    fn a_report_describes_the_machine_and_a_paused_one_refuses_new_jobs() {
        let tmp = tempfile::tempdir().unwrap();
        let (node, _) = published(tmp.path(), b"");
        let report = node.report().unwrap();
        assert!(report.cores > 0 && report.memory_total > 0);
        assert!(matches!(
            report.disk,
            crate::protocol::DiskSpace::Measured { total, .. } if total > 0
        ));
        assert!(!report.paused);
        let pause = |paused| Request::Configure {
            change: Change {
                paused: Some(paused),
                max_jobs: None,
            },
        };
        let wire = ask(&node, &pause(true));
        let paused = crate::remote::receive("m", &mut wire.as_slice(), &mut Vec::new()).unwrap();
        assert!(paused.into_report().unwrap().paused);
        let nonce = crate::domain::Nonce::generate().unwrap();
        let refused_while_paused = ask(&node, &submission(nonce, Location::Home));
        let reply =
            crate::remote::receive("m", &mut refused_while_paused.as_slice(), &mut Vec::new())
                .unwrap();
        assert!(
            matches!(&reply, Reply::Refused(refusal) if refusal.code == RefusalCode::Paused),
            "{reply:?}"
        );
        let five = Concurrency::try_from(5).unwrap();
        node.configure(&authorized_configure(Change {
            paused: Some(false),
            max_jobs: Some(five),
        }))
        .unwrap();
        let resumed = node.report().unwrap();
        assert!(!resumed.paused && resumed.max_jobs == five);
    }

    #[test]
    fn cleaning_frees_stale_workspaces_and_on_request_every_idle_one_and_old_logs() {
        let tmp = tempfile::tempdir().unwrap();
        let (node, job) = published(tmp.path(), &[b'x'; 10_000]);
        let store = Store::open(&dirs(tmp.path())).unwrap();
        let id = store.resolve(&job).unwrap();
        let project = store.area("work").join("owner").join("proj");
        for slot in ["0", "1"] {
            crate::state_file::write_bytes(&project.join(slot).join("out"), &[b'b'; 5_000])
                .unwrap();
        }
        let busy = crate::lock::OsLock::exclusive(&project.join("locks").join("1.lock")).unwrap();
        let recent = project.join("3");
        crate::state_file::write_bytes(&recent.join("out"), b"warm").unwrap();
        crate::state_file::write_bytes(
            &crate::supervisor::filled_by_path(&recent),
            id.as_str().as_bytes(),
        )
        .unwrap();
        let listed = node.clean(&authorized_clean((false, true, false))).unwrap();
        assert!(!listed.applied);
        assert!(
            listed.items.iter().any(|item| item.bytes == 5_000),
            "{listed:?}"
        );
        assert!(project.join("0").join("out").try_exists().unwrap());
        assert_eq!(std::fs::read(store.log_path(&id)).unwrap().len(), 10_000);

        let freed = node.clean(&authorized_clean((true, false, false))).unwrap();
        assert!(freed.applied);
        assert_eq!(freed.items.len(), 1, "{freed:?}");
        assert!(recent.join("out").try_exists().unwrap());
        assert!(!project.join("0").try_exists().unwrap());
        assert!(project.join("1").join("out").try_exists().unwrap());
        assert_eq!(std::fs::read(store.log_path(&id)).unwrap().len(), 10_000);
        node.clean(&authorized_clean((true, true, true))).unwrap();
        assert!(!recent.join("out").try_exists().unwrap());
        assert_eq!(std::fs::read(store.log_path(&id)).unwrap(), DISCARDED);
        busy.release().unwrap();
    }

    #[test]
    fn cleaning_reports_many_canonical_workspaces_with_a_bounded_complete_report() {
        let tmp = tempfile::tempdir().unwrap();
        let (node, _) = published(tmp.path(), b"");
        let projects: Vec<_> = (0..5)
            .map(|group| {
                node.store
                    .area("work")
                    .join("owner")
                    .join(format!("project-{group}"))
            })
            .collect();
        let mut workspaces = 0;
        for project in &projects {
            for slot in SlotIndex::all() {
                workspaces += 1;
                crate::state_file::write_bytes(
                    &project.join(slot.to_string()).join("out"),
                    &vec![b'x'; workspaces],
                )
                .unwrap();
            }
        }
        let total = (1..=workspaces).map(crate::domain::len_u64).sum::<u64>();
        for apply in [false, true] {
            let cleaned = node.clean(&authorized_clean((apply, false, true))).unwrap();
            assert_eq!(cleaned.applied, apply);
            assert_eq!(cleaned.items.len(), crate::protocol::CLEAN_DETAIL_LIMIT + 1);
            assert_eq!(
                cleaned.items.iter().map(|item| item.bytes).sum::<u64>(),
                total
            );
            let summary = cleaned.items.iter().find(|item| {
                item.what.as_raw_str()
                    == format!(
                        "the other {} cleanable items",
                        workspaces - crate::protocol::CLEAN_DETAIL_LIMIT
                    )
            });
            let summary = summary.expect("omitted cleanable items have a summary");
            let mut details: Vec<_> = cleaned
                .items
                .iter()
                .filter(|item| item.what != summary.what)
                .map(|item| item.bytes)
                .collect();
            details.sort_unstable();
            assert_eq!(
                details,
                (workspaces - crate::protocol::CLEAN_DETAIL_LIMIT + 1..=workspaces)
                    .map(crate::domain::len_u64)
                    .collect::<Vec<_>>()
            );
        }
        for project in projects {
            assert!(
                std::fs::read_dir(&project)
                    .unwrap()
                    .all(|entry| entry.unwrap().file_name() == "locks")
            );
        }
    }

    #[test]
    fn cleaning_never_borrows_a_canonical_lock_for_an_alias_or_out_of_range_slot() {
        let tmp = tempfile::tempdir().unwrap();
        let (node, _) = published(tmp.path(), b"");
        let project = node.store.area("work").join("owner").join("project");
        for name in ["0", "01", "64"] {
            crate::state_file::write_bytes(&project.join(name).join("out"), b"stale").unwrap();
        }
        let cleaned = node.clean(&authorized_clean((true, false, true))).unwrap();
        assert_eq!(cleaned.items.len(), 1);
        assert!(!project.join("0").try_exists().unwrap());
        assert!(project.join("01").join("out").try_exists().unwrap());
        assert!(project.join("64").join("out").try_exists().unwrap());
        assert!(!project.join("locks").join("64.lock").try_exists().unwrap());
    }

    #[test]
    fn idle_workspace_order_is_stable_across_project_batches() {
        let tmp = tempfile::tempdir().unwrap();
        let (node, _) = published(tmp.path(), b"");
        let work = node.store.area("work").join("owner");
        let projects = crate::bounded::DIRECTORY_BATCH.get() + 2;
        for index in (0..projects).rev() {
            crate::state_file::write_bytes(
                &work
                    .join(format!("project-{index:03}"))
                    .join("0")
                    .join("out"),
                b"stale",
            )
            .unwrap();
        }
        crate::state_file::write_bytes(&work.join("unrelated-file"), b"ignored").unwrap();
        crate::state_file::write_bytes(
            &node.store.area("work").join("unrelated-scope-file"),
            b"ignored",
        )
        .unwrap();
        let mut visited = Vec::new();
        let result = node
            .visit_idle_workspaces(|workspace, _lock| {
                visited.push(workspace.to_path_buf());
                Ok(ScanFlow::Continue)
            })
            .unwrap();
        assert_eq!(result, ScanFlow::Continue);
        assert_eq!(visited.len(), projects);
        for (index, path) in visited.iter().enumerate() {
            assert_eq!(path, &work.join(format!("project-{index:03}")).join("0"));
        }
    }

    #[test]
    fn measuring_wide_trees_streams_entries_and_deep_trees_fail_at_the_budget() {
        let tmp = tempfile::tempdir().unwrap();
        let tree = tmp.path().join("tree");
        for index in 0..=WORKSPACE_SCAN_BATCH {
            crate::state_file::write_bytes(&tree.join(format!("{index:03}")), b"x").unwrap();
        }
        assert_eq!(
            size_of(&tree).unwrap(),
            crate::domain::len_u64(WORKSPACE_SCAN_BATCH + 1)
        );
        let mut deep = tree.clone();
        for _ in 1..MAX_MEASURE_DIRS {
            deep.push("d");
        }
        crate::state_file::write_bytes(&deep.join("leaf"), b"y").unwrap();
        assert_eq!(
            size_of(&tree).unwrap(),
            crate::domain::len_u64(WORKSPACE_SCAN_BATCH + 2)
        );
        deep.push("d");
        crate::state_file::write_bytes(&deep.join("too_deep"), b"z").unwrap();
        assert!(matches!(
            size_of(&tree),
            Err(NodeError::MeasureDepth {
                limit: MAX_MEASURE_DIRS,
                ..
            })
        ));
    }

    #[test]
    fn cleaning_reports_unreadable_workspaces_instead_of_a_partial_list() {
        let tmp = tempfile::tempdir().unwrap();
        let (node, _) = published(tmp.path(), b"");
        let work = node.store.area("work");
        let file = work.join("owner").join("project").join("0").join("file");
        crate::state_file::write_bytes(&file, b"kept").unwrap();
        for (site, path) in [
            ("node::list", work.as_path()),
            ("node::measure", file.as_path()),
        ] {
            let tag = path.display().to_string();
            let _faults = crate::faults::inject(&[(site, &tag)]);
            assert!(matches!(
                node.clean(&authorized_clean((false, false, true))),
                Err(NodeError::Io(_))
            ));
        }
        assert_eq!(std::fs::read(file).unwrap(), b"kept");
    }

    #[test]
    fn an_unmeasurable_disk_is_not_reported_as_roomy_or_zero_bytes_free() {
        let tmp = tempfile::tempdir().unwrap();
        let (node, _) = published(tmp.path(), b"");
        let tag = node.store.area("jobs").display().to_string();
        let _faults = crate::faults::inject(&[("node::disk", &tag)]);
        disk_pressure(&node.store.area("jobs")).unwrap_err();
        assert!(matches!(
            node.report().unwrap().disk,
            crate::protocol::DiskSpace::Unavailable { .. }
        ));
    }

    #[test]
    fn unreadable_settings_cannot_allow_a_job_or_report_the_machine_as_ready() {
        let tmp = tempfile::tempdir().unwrap();
        let (node, _) = published(tmp.path(), b"");
        let tag = node.store.settings_path().display().to_string();
        let _faults = crate::faults::inject(&[("state_file::read", &tag)]);
        for request in [
            Request::Report,
            submission(crate::domain::Nonce::generate().unwrap(), Location::Home),
        ] {
            let reply = ask(&node, &request);
            let received = crate::remote::receive("m", &mut reply.as_slice(), &mut Vec::new());
            assert!(matches!(
                received,
                Ok(Reply::Refused(Refusal {
                    code: RefusalCode::Storage,
                    ..
                }))
            ));
        }
    }

    #[test]
    fn failed_recovery_refuses_new_work_before_it_changes_state() {
        let tmp = tempfile::tempdir().unwrap();
        let (node, job) = published(tmp.path(), b"");
        let tag = node.store.area("jobs").display().to_string();
        {
            let _faults = crate::faults::inject(&[("store::list", &tag)]);
            for request in [
                submission(crate::domain::Nonce::generate().unwrap(), Location::Home),
                Request::Configure {
                    change: Change {
                        paused: Some(true),
                        max_jobs: None,
                    },
                },
            ] {
                assert!(matches!(
                    reply_to(&node, line_of(&request)),
                    Reply::Refused(Refusal {
                        code: RefusalCode::Storage,
                        ..
                    })
                ));
            }
            let killed = reply_to(&node, line_of(&Request::Kill { job }));
            assert!(matches!(killed, Reply::Job(_)), "{killed:?}");
        }
        assert_eq!(node.store.ids().unwrap().len(), 1);
        assert!(!node.store.settings().unwrap().paused);
    }

    #[test]
    fn kill_recovers_its_target_without_listing_other_jobs() {
        let tmp = tempfile::tempdir().unwrap();
        let id: JobId = "0BBBBBBBBBBBBBBB".parse().unwrap();
        let store = staged(tmp.path(), &id, "true");
        store.publish(&id).unwrap();
        store
            .force_phase(
                &id,
                &Phase::Running {
                    started_at: Timestamp::at_millis(1),
                    pid: 1,
                    workspace: String::new(),
                },
            )
            .unwrap();
        let node = Node::open(dirs(tmp.path())).unwrap();
        let tag = node.store.area("jobs").display().to_string();
        let _faults = crate::faults::inject(&[("store::list", &tag)]);
        let reply = reply_to(&node, line_of(&Request::Kill { job: id.into() }));
        let Reply::Job(job) = reply else {
            panic!("expected the target job, got {reply:?}");
        };
        assert_eq!(job.state(), crate::protocol::State::Errored);
    }

    #[test]
    fn uninstall_cannot_treat_unreadable_job_state_as_no_running_jobs() {
        let tmp = tempfile::tempdir().unwrap();
        let (node, _) = published(tmp.path(), b"");
        let tag = node.store.area("jobs").display().to_string();
        let _faults = crate::faults::inject(&[("store::list", &tag)]);
        assert!(matches!(node.running(), Err(NodeError::Store(_))));
    }

    #[test]
    fn a_watch_answers_at_once_again_on_every_job_change_and_ends_when_the_client_leaves() {
        let tmp = tempfile::tempdir().unwrap();
        let (node, job) = published(tmp.path(), b"");
        let store = Store::open(&dirs(tmp.path())).unwrap();
        let id = store.resolve(&job).unwrap();
        let (reader, writer) = std::io::pipe().unwrap();
        let mut line = serde_json::to_vec(&Request::Watch).unwrap();
        line.push(b'\n');
        let input = std::io::BufReader::new(Read::chain(std::io::Cursor::new(line), reader));
        let (told, heard) = std::sync::mpsc::channel();
        let serving = std::thread::spawn(move || {
            node.serve(&Principal::Owner, input, &mut crate::faults::Told(told))
                .unwrap();
        });
        let mut wire = Vec::new();
        let mut surveys = 0;
        while surveys < 2 {
            let chunk = heard.recv().unwrap();
            if chunk.windows(8).any(|w| w == b"\"report\"") {
                surveys += 1;
                if surveys == 1 {
                    store.force_phase(&id, &Phase::Queued).unwrap();
                }
            }
            wire.extend(chunk);
        }
        drop(writer);
        serving.join().unwrap();
        wire.extend(heard.try_iter().flatten());
        let mut streamed = Vec::new();
        let reply = crate::remote::receive("m", &mut wire.as_slice(), &mut streamed).unwrap();
        assert!(matches!(reply, Reply::Stream));
        let lines: Vec<&[u8]> = streamed
            .split(|b| *b == b'\n')
            .filter(|l| !l.is_empty())
            .collect();
        assert!(lines.len() >= 2, "{}", String::from_utf8_lossy(&streamed));
        let last: crate::protocol::Survey = crate::ingress::json(lines.last().unwrap()).unwrap();
        assert_eq!(last.jobs.first().unwrap().phase, Phase::Queued);
    }

    #[test]
    fn a_change_that_cannot_be_audited_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let (node, job) = published(tmp.path(), b"");
        let log = node.audit.path().display().to_string();
        let _faults = crate::faults::inject(&[("audit::append", &log)]);
        let reply = ask(&node, &Request::Kill { job });
        let refused = crate::remote::receive("m", &mut reply.as_slice(), &mut Vec::new());
        assert!(matches!(refused, Ok(Reply::Refused(_))), "{refused:?}");
    }

    #[test]
    fn a_streamed_log_arrives_whole_and_any_cut_is_an_error_not_a_shorter_log() {
        let tmp = tempfile::tempdir().unwrap();
        let (node, job) = published(tmp.path(), b"one\ntwo\nthree\n");
        let cases = [
            (
                Request::Logs {
                    job: job.clone(),
                    offset: 0,
                    follow: Follow::Snapshot,
                },
                b"one\ntwo\nthree\n".as_slice(),
            ),
            (Request::Tail { job, lines: 2 }, b"two\nthree\n".as_slice()),
        ];
        for (request, expected) in cases {
            let wire = ask(&node, &request);
            let mut got = Vec::new();
            let reply = crate::remote::receive("m", &mut wire.as_slice(), &mut got).unwrap();
            reply.into_stream().unwrap();
            assert_eq!(got, expected);
            let header = wire.iter().position(|b| *b == b'\n').unwrap();
            for cut in header.saturating_add(1)..wire.len() {
                let result =
                    crate::remote::receive("m", &mut wire.get(..cut).unwrap(), &mut Vec::new());
                assert!(
                    matches!(
                        result,
                        Err(crate::remote::RemoteError::Stream {
                            problem: crate::framed::Unframed::Truncated,
                            ..
                        })
                    ),
                    "a reply cut at {cut} of {} was {result:?}",
                    wire.len()
                );
            }
        }
    }

    #[test]
    fn one_unreadable_job_is_reported_beside_the_others_not_instead_of_them() {
        let tmp = tempfile::tempdir().unwrap();
        let (node, _) = published(tmp.path(), b"");
        let store = Store::open(&dirs(tmp.path())).unwrap();
        let broken: JobId = "0BBBBBBBBBBBBBBB".parse().unwrap();
        crate::state_file::write_bytes(
            &store.log_path(&broken).with_file_name("spec.json"),
            b"{\"id\": \"0BBBBBBBBBBBBBBB\", \"from_the_future\": 1}",
        )
        .unwrap();
        let wire = ask(&node, &Request::List { limit: 10 });
        let reply = crate::remote::receive("m", &mut wire.as_slice(), &mut Vec::new()).unwrap();
        let (jobs, unreadable) = reply.into_jobs().unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(unreadable.len(), 1);
        assert_eq!(unreadable.first().unwrap().id, broken);
        let peer = Principal::Peer {
            key: crate::trust::PublicKey::from_slice(&[9; 32]).unwrap(),
            label: "mac".parse().unwrap(),
            capabilities: std::collections::BTreeSet::from([authz::Capability::Observe]),
        };
        let seen_by_peer = ask_as(&node, &peer, &Request::List { limit: 10 });
        let (visible, hidden) =
            crate::remote::receive("m", &mut seen_by_peer.as_slice(), &mut Vec::new())
                .unwrap()
                .into_jobs()
                .unwrap();
        assert!(visible.is_empty() && hidden.is_empty());
    }

    fn reply_to(node: &Node, bytes: Vec<u8>) -> Reply {
        let mut out = Vec::new();
        node.serve(&Principal::Owner, std::io::Cursor::new(bytes), &mut out)
            .unwrap();
        crate::remote::receive("m", &mut out.as_slice(), &mut Vec::new()).unwrap()
    }

    fn line_of(message: &impl serde::Serialize) -> Vec<u8> {
        let mut line = serde_json::to_vec(message).unwrap();
        line.push(b'\n');
        line
    }

    fn snapshot_frames(nonce: RetryNonce, contents: &[u8]) -> (Vec<u8>, Vec<u8>, BlobId) {
        let content = BlobId::of(contents);
        let manifest = crate::snapshot::Manifest {
            entries: std::collections::BTreeMap::from([(
                "a.txt".parse().unwrap(),
                crate::snapshot::Entry::File {
                    blob: content.clone(),
                    size: crate::domain::len_u64(contents.len()),
                    mode: crate::snapshot::Mode::Regular,
                },
            )]),
        };
        let (manifest_id, encoded) = manifest.encode().unwrap();
        let location = Location::Snapshot {
            source: Source {
                project: "proj".parse().unwrap(),
                manifest: manifest_id.clone(),
                revision: crate::protocol::Revision::WorkingDirectory,
            },
            subdir: None,
            workspace: crate::protocol::Workspace::Warm,
        };
        let mut first = line_of(&submission(nonce, location));
        first.extend(line_of(&Frame {
            blob: manifest_id,
            size: crate::domain::len_u64(encoded.len()),
        }));
        first.extend(encoded);
        let mut second = line_of(&Frame {
            blob: content.clone(),
            size: crate::domain::len_u64(contents.len()),
        });
        second.extend_from_slice(contents);
        (first, second, content)
    }

    struct Feed {
        chunk: std::io::Cursor<Vec<u8>>,
        next: std::sync::mpsc::Receiver<Vec<u8>>,
    }

    impl Read for Feed {
        fn read(&mut self, target: &mut [u8]) -> std::io::Result<usize> {
            while self.chunk.position() == crate::domain::len_u64(self.chunk.get_ref().len()) {
                self.chunk = match self.next.recv() {
                    Ok(bytes) => std::io::Cursor::new(bytes),
                    Err(_) => return Ok(0),
                };
            }
            self.chunk.read(target)
        }
    }

    struct Replies(std::sync::mpsc::Sender<Vec<u8>>);

    impl Write for Replies {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.send(bytes.to_vec()).map_err(std::io::Error::other)?;
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_snapshot_holds_collection_lock_from_manifest_through_admission() {
        let tmp = tempfile::tempdir().unwrap();
        let (first, second, content) =
            snapshot_frames(RetryNonce::generate().unwrap(), b"source content");
        let (send_input, receive_input) = std::sync::mpsc::channel();
        let (send_reply, receive_reply) = std::sync::mpsc::channel();
        send_input.send(first).unwrap();
        let node = Node::open(dirs(tmp.path())).unwrap();
        let server = std::thread::spawn(move || {
            node.serve(
                &Principal::Owner,
                std::io::BufReader::new(Feed {
                    chunk: std::io::Cursor::new(Vec::new()),
                    next: receive_input,
                }),
                &mut Replies(send_reply),
            )
        });
        let first_reply = receive_reply.recv().unwrap();
        let need = crate::remote::receive("m", &mut first_reply.as_slice(), &mut Vec::new())
            .unwrap()
            .into_need_blobs()
            .unwrap();
        assert_eq!(need, vec![content.clone()]);
        let store = Store::open(&dirs(tmp.path())).unwrap();
        assert!(
            crate::lock::OsLock::try_exclusive(&store.collection_lock_path())
                .unwrap()
                .is_none()
        );
        let root = tmp.path().to_path_buf();
        let collector =
            std::thread::spawn(move || Node::open(dirs(&root)).unwrap().collect(Keep::Every));
        send_input.send(second).unwrap();
        drop(send_input);
        let last_reply = receive_reply.recv().unwrap();
        let final_reply = crate::remote::receive("m", &mut last_reply.as_slice(), &mut Vec::new());
        assert!(
            matches!(
                &final_reply,
                Ok(Reply::Job(_)
                    | Reply::Refused(Refusal {
                        code: RefusalCode::Spawn,
                        ..
                    }))
            ),
            "{final_reply:?}"
        );
        server.join().unwrap().unwrap();
        collector.join().unwrap().unwrap();
        let cas = Cas::open(tmp.path().join("state/objects")).unwrap();
        if matches!(&final_reply, Ok(Reply::Job(_))) {
            assert!(cas.has(&content).unwrap());
        }
        for stored in cas.stored().unwrap() {
            cas.get(&stored).unwrap();
        }
    }

    #[test]
    fn the_legacy_split_upload_requests_are_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let node = Node::open(dirs(tmp.path())).unwrap();
        for request in [
            br#"{"op":"missing","blobs":[]}"#.as_slice(),
            br#"{"op":"upload","count":0}"#.as_slice(),
        ] {
            let mut line = request.to_vec();
            line.push(b'\n');
            assert!(matches!(
                reply_to(&node, line),
                Reply::Refused(Refusal {
                    code: RefusalCode::BadRequest,
                    ..
                })
            ));
        }
    }

    fn sound_after_a_crash(root: &Path, step: usize, retried: &RetryNonce) {
        let at = |what: &str| format!("after a crash before step {step}: {what}");
        let node = Node::open(dirs(root)).unwrap_or_else(|e| panic!("{}", at(&e.to_string())));
        match node.upkeep(&commanded()) {
            Ok(()) | Err(_) => {}
        }
        node.audit
            .verify()
            .unwrap_or_else(|e| panic!("{}", at(&format!("the audit log: {e}"))));
        let listed = reply_to(&node, line_of(&Request::List { limit: 100 }));
        assert!(
            matches!(&listed, Reply::Jobs { unreadable, .. } if unreadable.is_empty()),
            "{}",
            at(&format!("listing answered {listed:?}"))
        );
        for blob in node.cas.stored().unwrap() {
            node.cas
                .get(&blob)
                .unwrap_or_else(|e| panic!("{}", at(&format!("blob {blob}: {e}"))));
        }
        for id in node.store.staged_ids().unwrap() {
            let alive = crate::lock::OsLock::exclusive(&node.store.alive_path(&id))
                .unwrap_or_else(|e| panic!("{}", at(&format!("waiting for {id}: {e}"))));
            alive
                .release()
                .unwrap_or_else(|e| panic!("{}", at(&format!("releasing {id}: {e}"))));
        }
        match node.upkeep(&commanded()) {
            Ok(()) | Err(_) => {}
        }
        assert!(
            node.store.staged_ids().unwrap().is_empty(),
            "{}",
            at("staging was left behind")
        );
        for id in node.store.ids().unwrap() {
            let phase = node.store.phase(&id);
            assert!(
                !matches!(phase, Ok(Phase::Running { .. })),
                "{}",
                at(&format!("{id} still reads as running: {phase:?}"))
            );
        }
        node.configure(&authorized_configure(Change::default()))
            .unwrap_or_else(|e| panic!("{}", at(&format!("configuring: {e}"))));
        for nonce in [retried.clone(), RetryNonce::generate().unwrap()] {
            let reply = reply_to(&node, line_of(&submission(nonce, Location::Home)));
            assert!(
                matches!(&reply, Reply::Job(_))
                    || matches!(&reply, Reply::Refused(refusal) if matches!(refusal.code, RefusalCode::Spawn | RefusalCode::Paused)),
                "{}",
                at(&format!("a submission answered {reply:?}"))
            );
        }
    }

    fn crash_everywhere(setup: fn(&Path), act: impl Fn(&Node, &RetryNonce)) {
        let retried = RetryNonce::generate().unwrap();
        let steps = {
            let tmp = tempfile::tempdir().unwrap();
            setup(tmp.path());
            let node = Node::open(dirs(tmp.path())).unwrap();
            let counting = crate::faults::crash_after(tmp.path(), None);
            act(&node, &retried);
            counting.steps()
        };
        assert!(steps > 0);
        for step in 0..steps {
            let tmp = tempfile::tempdir().unwrap();
            setup(tmp.path());
            {
                let node = Node::open(dirs(tmp.path())).unwrap();
                let _crashing = crate::faults::crash_after(tmp.path(), Some(step));
                act(&node, &retried);
            }
            sound_after_a_crash(tmp.path(), step, &retried);
        }
    }

    fn nothing_yet(_root: &Path) {}

    fn one_running_job(root: &Path) {
        let (_, job) = published(root, b"log");
        let store = Store::open(&dirs(root)).unwrap();
        let id = store.resolve(&job).unwrap();
        store
            .force_phase(
                &id,
                &Phase::Running {
                    started_at: Timestamp::at_millis(1),
                    pid: 1,
                    workspace: "w".into(),
                },
            )
            .unwrap();
    }

    #[test]
    fn a_crash_at_any_step_of_a_submission_leaves_a_machine_that_takes_it_again() {
        crash_everywhere(nothing_yet, |node, retried| {
            let reply = reply_to(node, line_of(&submission(retried.clone(), Location::Home)));
            assert!(
                matches!(reply, Reply::Job(_) | Reply::Refused(_)),
                "{reply:?}"
            );
        });
    }

    #[test]
    fn a_crash_at_any_step_of_settling_the_machine_leaves_it_answering() {
        crash_everywhere(nothing_yet, |node, _retried| {
            match node.configure(&authorized_configure(Change {
                paused: Some(true),
                max_jobs: Some(Concurrency::try_from(2).unwrap()),
            })) {
                Ok(()) | Err(_) => {}
            }
        });
    }

    #[test]
    fn a_crash_at_any_step_of_recovering_a_lost_job_is_recovered_from_next_time() {
        crash_everywhere(one_running_job, |node, _retried| {
            match node.upkeep(&commanded()) {
                Ok(()) | Err(_) => {}
            }
        });
    }

    #[test]
    fn a_crash_at_any_step_of_cleaning_leaves_a_machine_that_answers() {
        crash_everywhere(one_running_job, |node, _retried| {
            let reply = reply_to(
                node,
                line_of(&Request::Clean {
                    apply: true,
                    logs: true,
                    idle: true,
                }),
            );
            assert!(
                matches!(reply, Reply::Cleaned(_) | Reply::Refused(_)),
                "{reply:?}"
            );
        });
    }

    const SUPERVISED: &str = "0BBBBBBBBBBBBBBB";

    fn one_queued_job(root: &Path) {
        staged(root, &SUPERVISED.parse().unwrap(), "exit 0");
    }

    fn supervise_the_queued_job(node: &Node) {
        let supervised = crate::supervisor::supervise(
            node.dirs.clone(),
            &SUPERVISED.parse().unwrap(),
            proc::Readiness::unwatched(),
            crate::supervisor::Stops::Unheard,
        );
        match supervised {
            Ok(()) | Err(_) => {}
        }
    }

    #[test]
    fn a_crash_at_any_step_of_supervising_a_job_leaves_it_closed_or_waiting() {
        let tmp = tempfile::tempdir().unwrap();
        one_queued_job(tmp.path());
        let node = Node::open(dirs(tmp.path())).unwrap();
        supervise_the_queued_job(&node);
        assert_eq!(
            node.store
                .job(&SUPERVISED.parse().unwrap())
                .unwrap()
                .state(),
            crate::protocol::State::Succeeded
        );
        crash_everywhere(one_queued_job, |supervising, _retried| {
            supervise_the_queued_job(supervising);
        });
    }

    #[test]
    fn a_job_whose_supervisor_vanished_while_running_is_closed_not_rerun() {
        let tmp = tempfile::tempdir().unwrap();
        one_running_job(tmp.path());
        let node = Node::open(dirs(tmp.path())).unwrap();
        let store = Store::open(&dirs(tmp.path())).unwrap();
        let id: JobId = "0AAAAAAAAAAAAAAA".parse().unwrap();
        node.upkeep(&commanded()).unwrap();
        let Phase::Finished {
            outcome: crate::protocol::Outcome::Errored { reason },
            started_at,
            ..
        } = store.phase(&id).unwrap()
        else {
            panic!("a vanished running job was left open");
        };
        assert_eq!(started_at, Some(Timestamp::at_millis(1)));
        assert!(reason.as_raw_str().contains("vanished"));
    }

    #[test]
    fn a_sweep_that_cannot_write_leaves_the_job_open_and_the_next_one_closes_it() {
        let tmp = tempfile::tempdir().unwrap();
        let (node, job) = published(tmp.path(), b"");
        let store = Store::open(&dirs(tmp.path())).unwrap();
        let id = store.resolve(&job).unwrap();
        let running = Phase::Running {
            started_at: Timestamp::at_millis(1),
            pid: 1,
            workspace: "w".into(),
        };
        store.force_phase(&id, &running).unwrap();
        let phase_file = store.job_dir(&id).join("phase.json").display().to_string();
        {
            let _faults = crate::faults::inject(&[("state_file::write", &phase_file)]);
            node.upkeep(&commanded()).unwrap_err();
        }
        assert_eq!(store.phase(&id).unwrap(), running);
        node.upkeep(&commanded()).unwrap();
        assert!(matches!(store.phase(&id).unwrap(), Phase::Finished { .. }));
    }

    #[test]
    fn a_sweep_that_cannot_read_a_spec_leaves_the_job_open_for_the_next_one() {
        let tmp = tempfile::tempdir().unwrap();
        let (node, job) = published(tmp.path(), b"");
        let store = Store::open(&dirs(tmp.path())).unwrap();
        let id = store.resolve(&job).unwrap();
        let running = Phase::Running {
            started_at: Timestamp::at_millis(1),
            pid: 1,
            workspace: String::new(),
        };
        store.force_phase(&id, &running).unwrap();
        let spec_file = store.job_dir(&id).join("spec.json").display().to_string();
        {
            let _faults = crate::faults::inject(&[("state_file::read", &spec_file)]);
            assert!(matches!(node.recover(&id), Err(NodeError::Store(_))));
        }
        assert_eq!(store.phase(&id).unwrap(), running);
        node.recover(&id).unwrap();
        assert!(matches!(store.phase(&id).unwrap(), Phase::Finished { .. }));
    }

    #[test]
    fn a_job_that_failed_to_start_is_closed_with_its_reason() {
        let tmp = tempfile::tempdir().unwrap();
        let (node, job) = published(tmp.path(), b"");
        let store = Store::open(&dirs(tmp.path())).unwrap();
        let id = store.resolve(&job).unwrap();
        store.force_phase(&id, &Phase::Queued).unwrap();
        store.record_start_failure(&id, "no shell").unwrap();
        node.upkeep(&commanded()).unwrap();
        let Phase::Finished {
            outcome: crate::protocol::Outcome::Errored { reason },
            ..
        } = store.phase(&id).unwrap()
        else {
            panic!("a job that never started was left open");
        };
        assert!(reason.as_raw_str().contains("no shell"));
    }

    #[test]
    fn staging_left_by_a_dead_node_is_removed_but_not_while_someone_holds_it() {
        let tmp = tempfile::tempdir().unwrap();
        let (node, _) = published(tmp.path(), b"");
        let store = Store::open(&dirs(tmp.path())).unwrap();
        let orphan: JobId = "0DDDDDDDDDDDDDDD".parse().unwrap();
        let busy: JobId = "0EEEEEEEEEEEEEEE".parse().unwrap();
        let spec_of = |id: &JobId| {
            let mut spec = store.spec(&"0AAAAAAAAAAAAAAA".parse().unwrap()).unwrap();
            spec.id = id.clone();
            store
                .stage(
                    &spec,
                    (&std::collections::BTreeMap::new(), &LaunchEnv::default()),
                )
                .unwrap();
        };
        spec_of(&orphan);
        spec_of(&busy);
        let held = crate::lock::OsLock::exclusive(&store.staging_lock_path(&busy)).unwrap();
        node.upkeep(&commanded()).unwrap();
        assert_eq!(store.staged_ids().unwrap(), vec![busy]);
        held.release().unwrap();
    }

    #[test]
    fn old_finished_jobs_are_retired_by_count_and_unreachable_blobs_go_with_them() {
        let tmp = tempfile::tempdir().unwrap();
        let (node, _) = published(tmp.path(), b"");
        let store = Store::open(&dirs(tmp.path())).unwrap();
        let older: JobId = "0AAAAAAAAAAAAAAA".parse().unwrap();
        let newer: JobId = "0FFFFFFFFFFFFFFF".parse().unwrap();
        let mut spec = store.spec(&older).unwrap();
        spec.id = newer.clone();
        spec.sequence = 2;
        store
            .stage(
                &spec,
                (&std::collections::BTreeMap::new(), &LaunchEnv::default()),
            )
            .unwrap();
        store.publish(&newer).unwrap();
        store
            .force_phase(&newer, &store.phase(&older).unwrap())
            .unwrap();
        let stray = BlobId::of(b"nobody needs this");
        node.cas.put(&stray, b"nobody needs this").unwrap();
        node.retire_including(1, None).unwrap();
        assert_eq!(store.ids().unwrap(), vec![newer]);
        assert!(!node.cas.stored().unwrap().contains(&stray));
    }

    #[test]
    fn retiring_preserves_a_nonce_while_its_job_is_still_being_staged() {
        let tmp = tempfile::tempdir().unwrap();
        let (node, _) = published(tmp.path(), b"");
        let store = Store::open(&dirs(tmp.path())).unwrap();
        let first: JobId = "0AAAAAAAAAAAAAAA".parse().unwrap();
        finished_like(&store, &first, &["0BBBBBBBBBBBBBBB"]);
        let staged: JobId = "0CCCCCCCCCCCCCCC".parse().unwrap();
        stage_with_sequence(&store, &staged, "true", 3);
        let nonce = crate::domain::Nonce::generate().unwrap();
        let path = store.nonce_path("owner", &nonce);
        crate::state_file::write_bytes(&path, staged.as_str().as_bytes()).unwrap();
        let staging = crate::lock::OsLock::exclusive(&store.staging_lock_path(&staged)).unwrap();
        node.retire_including(1, None).unwrap();
        assert!(crate::state_file::read_bytes(&path).unwrap().is_some());
        staging.release().unwrap();
        node.retire_including(0, None).unwrap();
        assert!(crate::state_file::read_bytes(&path).unwrap().is_none());
    }

    #[test]
    fn finished_log_visits_cross_batches_without_revisiting_discarded_logs() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&dirs(tmp.path())).unwrap();
        for index in 0..=LOG_SCAN_BATCH {
            let id: JobId = format!("{index:016X}").parse().unwrap();
            stage_with_sequence(&store, &id, "true", crate::domain::len_u64(index));
            store.publish(&id).unwrap();
            store
                .force_phase(
                    &id,
                    &Phase::Finished {
                        started_at: None,
                        finished_at: Timestamp::at_millis(1),
                        outcome: crate::protocol::Outcome::Succeeded,
                    },
                )
                .unwrap();
            crate::state_file::write_bytes(
                &store.log_path(&id),
                &vec![b'x'; DISCARDED.len() + index + 1],
            )
            .unwrap();
        }
        let node = Node::open(dirs(tmp.path())).unwrap();
        let mut seen = Vec::new();
        node.visit_finished_logs(|id| {
            seen.push(usize::from_str_radix(id.as_str(), 16).unwrap());
            assert!(matches!(node.discard_log(id)?, Reclamation::Done));
            Ok(ScanFlow::Continue)
        })
        .unwrap();
        assert_eq!(seen, (0..=LOG_SCAN_BATCH).rev().collect::<Vec<_>>());
    }

    #[test]
    fn a_full_disk_is_named_as_such_however_deep_it_is_wrapped() {
        let failing = |kind: std::io::ErrorKind| {
            NodeError::Store(StoreError::State(crate::state_file::StateError::Io(
                crate::failure::IoFailure {
                    action: "writing",
                    path: "/state/x".into(),
                    source: kind.into(),
                },
            )))
        };
        assert_eq!(
            failing(std::io::ErrorKind::StorageFull).code(),
            RefusalCode::DiskFull
        );
        assert_eq!(
            failing(std::io::ErrorKind::QuotaExceeded).code(),
            RefusalCode::DiskFull
        );
        assert_eq!(
            failing(std::io::ErrorKind::PermissionDenied).code(),
            RefusalCode::Storage
        );
    }

    fn sent(node: &Node, store: &Store, id: &JobId, manifest: &[u8]) -> BlobId {
        let blob = BlobId::of(manifest);
        node.cas.put(&blob, manifest).unwrap();
        let mut spec = store.spec(id).unwrap();
        spec.location = Location::Snapshot {
            source: Source {
                project: "proj".parse().unwrap(),
                manifest: blob.clone(),
                revision: crate::protocol::Revision::WorkingDirectory,
            },
            subdir: None,
            workspace: crate::protocol::Workspace::Warm,
        };
        crate::state_file::write_json(&store.job_dir(id).join("spec.json"), &spec).unwrap();
        blob
    }

    fn reclamation_fixture(root: &Path) -> (Node, Store, PathBuf, PathBuf) {
        let (node, _) = published(root, b"");
        let store = Store::open(&dirs(root)).unwrap();
        let id: JobId = "0AAAAAAAAAAAAAAA".parse().unwrap();
        let log = store.log_path(&id);
        crate::state_file::write_bytes(&log, &[b'x'; 10_000]).unwrap();
        let workspace = store.area("work").join("owner").join("proj").join("0");
        crate::state_file::write_bytes(&workspace.join("file"), b"built").unwrap();
        (node, store, log, workspace)
    }

    #[test]
    fn every_step_of_making_room_that_fails_is_an_error_and_leaves_the_rest() {
        let tmp = tempfile::tempdir().unwrap();
        let (node, store, log, workspace) = reclamation_fixture(tmp.path());
        let id: JobId = "0AAAAAAAAAAAAAAA".parse().unwrap();
        let project = workspace.parent().unwrap();
        let text = |path: &Path| path.display().to_string();
        for (site, tag) in [
            (
                "state_file::lock",
                text(&project.join("locks").join("0.lock")),
            ),
            ("state_file::rename", text(&project.join("0"))),
            ("store::list", text(&store.area("jobs"))),
            ("state_file::lock", text(&store.alive_path(&id))),
            ("state_file::cut", text(&log)),
            ("state_file::overwrite", text(&log)),
        ] {
            let _faults = crate::faults::inject(&[(site, &tag)]);
            node.make_room(&|| Ok(DiskPressure::Short), &commanded(), &Location::Home)
                .unwrap_err();
        }
    }

    #[test]
    fn an_unremovable_workspace_does_not_block_other_disk_reclamation() {
        let tmp = tempfile::tempdir().unwrap();
        let (node, store, log, workspace) = reclamation_fixture(tmp.path());
        let trash = store.area("trash");
        let tag = trash.join("workspace-").display().to_string();
        let _faults = crate::faults::inject(&[("state_file::remove", &tag)]);
        let until_log_is_discarded = || {
            Ok(if std::fs::read(&log).unwrap() == DISCARDED {
                DiskPressure::Enough
            } else {
                DiskPressure::Short
            })
        };
        node.make_room(&until_log_is_discarded, &commanded(), &Location::Home)
            .unwrap();
        assert!(!workspace.try_exists().unwrap());
        assert!(size_of(&trash).unwrap() > 0);
        assert_eq!(std::fs::read(log).unwrap(), DISCARDED);
    }

    #[test]
    fn explicit_clean_reports_an_unremovable_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let (node, store, _log, workspace) = reclamation_fixture(tmp.path());
        let trash = store.area("trash");
        let tag = trash.join("workspace-").display().to_string();
        let _faults = crate::faults::inject(&[("state_file::remove", &tag)]);
        assert!(matches!(
            node.clean(&authorized_clean((true, false, true))),
            Err(NodeError::State(crate::state_file::StateError::Io(_)))
        ));
        assert!(!workspace.try_exists().unwrap());
        assert!(size_of(&trash).unwrap() > 0);
    }

    #[test]
    fn an_unremovable_trash_entry_does_not_block_other_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let (node, _) = published(tmp.path(), b"");
        let trash = node.store.area("trash");
        let blocked = trash.join("blocked");
        let removable = trash.join("removable");
        crate::state_file::write_bytes(&blocked.join("file"), b"one").unwrap();
        crate::state_file::write_bytes(&removable.join("file"), b"two").unwrap();
        {
            let tag = blocked.display().to_string();
            let _faults = crate::faults::inject(&[("state_file::remove", &tag)]);
            node.empty_trash().unwrap_err();
        }
        assert!(blocked.try_exists().unwrap());
        assert!(!removable.try_exists().unwrap());
        node.empty_trash().unwrap();
        assert!(!blocked.try_exists().unwrap());
    }

    #[test]
    fn collecting_keeps_what_is_needed_and_every_failing_step_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let (node, _) = published(tmp.path(), b"");
        let store = Store::open(&dirs(tmp.path())).unwrap();
        let first: JobId = "0AAAAAAAAAAAAAAA".parse().unwrap();
        let ids = finished_like(&store, &first, &["0FFFFFFFFFFFFFFF"]);
        let running = ids.last().unwrap();
        set_running(&store, running);
        let (finished_manifest, finished_content) =
            holding(&node, b"only a finished job used this");
        let finished = sent(&node, &store, &first, &finished_manifest);
        let unfinished = sent(&node, &store, running, br#"{ "entries":{}}"#);
        let stray = BlobId::of(b"stray");
        node.cas.put(&stray, b"stray").unwrap();
        let blob_tag = |blob: &BlobId| blob.split().1.to_owned();
        let text = |path: &Path| path.display().to_string();
        for (keep, site, tag) in [
            (
                Keep::Every,
                "state_file::lock",
                text(&store.collection_lock_path()),
            ),
            (Keep::Every, "store::list", text(&store.area("jobs"))),
            (Keep::Every, "store::list", text(&store.area("staging"))),
            (Keep::Every, "cas::read", blob_tag(&finished)),
            (
                Keep::Every,
                "cas::list",
                text(&node.dirs.state().join("objects")),
            ),
            (Keep::Every, "state_file::remove", blob_tag(&stray)),
            (Keep::Unfinished, "cas::read", blob_tag(&finished)),
        ] {
            let _faults = crate::faults::inject(&[(site, &tag)]);
            node.collect(keep).unwrap_err();
        }
        node.collect(Keep::Every).unwrap();
        let kept = node.cas.stored().unwrap();
        assert!(kept.contains(&finished) && kept.contains(&unfinished));
        assert!(!kept.contains(&stray));
        let incoming = Location::Snapshot {
            source: Source {
                project: "incoming".parse().unwrap(),
                manifest: finished.clone(),
                revision: crate::protocol::Revision::WorkingDirectory,
            },
            subdir: None,
            workspace: crate::protocol::Workspace::Warm,
        };
        node.make_room(&|| Ok(DiskPressure::Short), &commanded(), &incoming)
            .unwrap();
        assert!(node.cas.has(&finished_content).unwrap());
        node.collect(Keep::Unfinished).unwrap();
        let left = node.cas.stored().unwrap();
        assert!(left.contains(&finished) && left.contains(&unfinished));
        assert!(!left.contains(&finished_content));
    }

    struct PartitionedCollection {
        _tmp: tempfile::TempDir,
        node: Node,
        finished_manifest: BlobId,
        finished_blob: BlobId,
        running_manifest: BlobId,
        running_blob: BlobId,
        stray: BlobId,
    }

    fn partitioned_collection() -> PartitionedCollection {
        let tmp = tempfile::tempdir().unwrap();
        let (node, _) = published(tmp.path(), b"");
        let store = Store::open(&dirs(tmp.path())).unwrap();
        let finished: JobId = "0AAAAAAAAAAAAAAA".parse().unwrap();
        let running = finished_like(&store, &finished, &["0FFFFFFFFFFFFFFF"])
            .last()
            .cloned()
            .unwrap();
        set_running(&store, &running);
        let (finished_manifest, finished_blob) = holding(&node, b"finished content");
        let first_nibble = finished_blob.as_str().get(..1).unwrap();
        let running_content = (0..1024)
            .map(|index| format!("running content {index}"))
            .find(|bytes| {
                BlobId::of(bytes.as_bytes())
                    .as_str()
                    .starts_with(first_nibble)
            })
            .unwrap();
        let (running_manifest, running_blob) = holding(&node, running_content.as_bytes());
        let finished_manifest = sent(&node, &store, &finished, &finished_manifest);
        let running_manifest = sent(&node, &store, &running, &running_manifest);
        let occupied = [
            &finished_manifest,
            &finished_blob,
            &running_manifest,
            &running_blob,
        ]
        .map(|blob| blob.as_str().get(..1).unwrap());
        let stray_content = (0..1024)
            .map(|index| format!("stray content {index}"))
            .find(|bytes| {
                !occupied.contains(&BlobId::of(bytes.as_bytes()).as_str().get(..1).unwrap())
            })
            .unwrap();
        let stray = BlobId::of(stray_content.as_bytes());
        node.cas.put(&stray, stray_content.as_bytes()).unwrap();
        PartitionedCollection {
            _tmp: tmp,
            node,
            finished_manifest,
            finished_blob,
            running_manifest,
            running_blob,
            stray,
        }
    }

    #[test]
    fn collection_partitions_reachability_without_losing_shared_prefixes_or_incoming_blobs() {
        let fixture = partitioned_collection();
        let PartitionedCollection {
            node,
            finished_manifest,
            finished_blob,
            running_manifest,
            running_blob,
            stray,
            ..
        } = &fixture;
        assert!(matches!(
            node.collect_with_mark_limit(Keep::Every, None, 0),
            Err(NodeError::CollectionBudget(0))
        ));
        assert!(matches!(
            node.scan_marks(
                CollectionPlan {
                    keep: Keep::Every,
                    incoming: None,
                    limit: 1,
                },
                "",
            )
            .unwrap(),
            MarkScan::Split(_)
        ));
        node.collect_with_mark_limit(Keep::Every, None, 1).unwrap();
        for blob in [
            &finished_manifest,
            &finished_blob,
            &running_manifest,
            &running_blob,
        ] {
            assert!(node.cas.has(blob).unwrap());
        }
        assert!(!node.cas.has(stray).unwrap());

        let incoming = Source {
            project: "incoming".parse().unwrap(),
            manifest: finished_manifest.clone(),
            revision: crate::protocol::Revision::WorkingDirectory,
        };
        node.collect_with_mark_limit(Keep::Unfinished, Some(&incoming), 1)
            .unwrap();
        assert!(node.cas.has(finished_blob).unwrap());
        node.collect_with_mark_limit(Keep::Unfinished, None, 1)
            .unwrap();
        assert!(node.cas.has(finished_manifest).unwrap());
        assert!(!node.cas.has(finished_blob).unwrap());
        assert!(node.cas.has(running_manifest).unwrap());
        assert!(node.cas.has(running_blob).unwrap());
    }

    #[test]
    fn retiring_a_finished_job_preserves_blobs_for_the_incoming_submission() {
        let tmp = tempfile::tempdir().unwrap();
        let (node, _) = published(tmp.path(), b"");
        let store = Store::open(&dirs(tmp.path())).unwrap();
        let id: JobId = "0AAAAAAAAAAAAAAA".parse().unwrap();
        let (manifest, content) = holding(&node, b"shared with the incoming submission");
        let manifest = sent(&node, &store, &id, &manifest);
        let source = Source {
            project: "incoming".parse().unwrap(),
            manifest: manifest.clone(),
            revision: crate::protocol::Revision::WorkingDirectory,
        };

        node.retire_including(0, Some(&source)).unwrap();
        assert!(store.ids().unwrap().is_empty());
        assert!(node.cas.has(&manifest).unwrap());
        assert!(node.cas.has(&content).unwrap());

        node.collect(Keep::Every).unwrap();
        assert!(!node.cas.has(&manifest).unwrap());
        assert!(!node.cas.has(&content).unwrap());
    }

    fn holding(node: &Node, content: &[u8]) -> (Vec<u8>, BlobId) {
        let blob = BlobId::of(content);
        node.cas.put(&blob, content).unwrap();
        let manifest = crate::snapshot::Manifest {
            entries: std::collections::BTreeMap::from([(
                "a.txt".parse().unwrap(),
                crate::snapshot::Entry::File {
                    blob: blob.clone(),
                    size: crate::domain::len_u64(content.len()),
                    mode: crate::snapshot::Mode::Regular,
                },
            )]),
        };
        (serde_json::to_vec(&manifest).unwrap(), blob)
    }

    fn set_running(store: &Store, id: &JobId) {
        store
            .force_phase(
                id,
                &Phase::Running {
                    started_at: Timestamp::at_millis(1),
                    pid: 1,
                    workspace: String::new(),
                },
            )
            .unwrap();
    }

    fn finished_like(store: &Store, first: &JobId, more: &[&str]) -> Vec<JobId> {
        let mut ids = vec![first.clone()];
        for (sequence, id) in (2..).zip(more) {
            let id: JobId = id.parse().unwrap();
            let mut spec = store.spec(first).unwrap();
            spec.id = id.clone();
            spec.sequence = sequence;
            store
                .stage(
                    &spec,
                    (&std::collections::BTreeMap::new(), &LaunchEnv::default()),
                )
                .unwrap();
            store.publish(&id).unwrap();
            store
                .force_phase(&id, &store.phase(first).unwrap())
                .unwrap();
            ids.push(id);
        }
        ids
    }

    #[test]
    fn a_full_disk_gives_up_idle_workspaces_then_big_logs_then_sources_only_finished_jobs_used() {
        let tmp = tempfile::tempdir().unwrap();
        let (node, _) = published(tmp.path(), b"");
        let store = Store::open(&dirs(tmp.path())).unwrap();
        let oldest: JobId = "0AAAAAAAAAAAAAAA".parse().unwrap();
        let ids = finished_like(&store, &oldest, &["0FFFFFFFFFFFFFFF", "0GGGGGGGGGGGGGGG"]);
        let project = store.area("work").join("owner").join("proj");
        for slot in ["0", "1", "2"] {
            crate::state_file::write_bytes(&project.join(slot).join("file"), b"built").unwrap();
        }
        let there = |slot: &str| project.join(slot).try_exists().unwrap();
        let busy = crate::lock::OsLock::exclusive(&project.join("locks").join("1.lock")).unwrap();
        let running = crate::lock::OsLock::exclusive(&store.alive_path(&oldest)).unwrap();
        let home = Location::Home;
        let commanded = commanded();

        node.make_room(&|| Ok(DiskPressure::Enough), &commanded, &home)
            .unwrap();
        assert!(there("0") && there("2"));
        let until_one_workspace_is_gone = || {
            Ok(if there("0") && there("2") {
                DiskPressure::Short
            } else {
                DiskPressure::Enough
            })
        };
        node.make_room(&until_one_workspace_is_gone, &commanded, &home)
            .unwrap();
        assert_ne!(there("0"), there("2"));
        assert!(project.join("1").join("file").try_exists().unwrap());
        assert_eq!(store.ids().unwrap().len(), 3);

        let big = ids.get(1).unwrap();
        let small = ids.last().unwrap();
        crate::state_file::write_bytes(&store.log_path(big), &[b'x'; 10_000]).unwrap();
        crate::state_file::write_bytes(&store.log_path(small), &[b'y'; 9_000]).unwrap();
        let until_the_biggest_log_is_gone = || {
            Ok(
                if std::fs::read(store.log_path(big)).unwrap() == DISCARDED {
                    DiskPressure::Enough
                } else {
                    DiskPressure::Short
                },
            )
        };
        node.make_room(&until_the_biggest_log_is_gone, &commanded, &home)
            .unwrap();
        assert_eq!(std::fs::read(store.log_path(big)).unwrap(), DISCARDED);
        assert_eq!(std::fs::read(store.log_path(small)).unwrap(), [b'y'; 9_000]);
        assert_eq!(store.ids().unwrap().len(), 3);

        let snapshot = |id: &JobId, manifest: &[u8]| sent(&node, &store, id, manifest);
        let needed = snapshot(&oldest, br#"{"entries":{}}"#);
        store
            .force_phase(
                &oldest,
                &Phase::Running {
                    started_at: Timestamp::at_millis(1),
                    pid: 1,
                    workspace: String::new(),
                },
            )
            .unwrap();
        let (spent_manifest, spent_content) = holding(&node, b"sent for a job that finished");
        let spent = snapshot(small, &spent_manifest);
        let uploaded = BlobId::of(b"sent for a submission still on its way");
        node.cas
            .put(&uploaded, b"sent for a submission still on its way")
            .unwrap();
        node.make_room(&|| Ok(DiskPressure::Short), &commanded, &home)
            .unwrap();
        assert!(!there("0") && !there("2"));
        assert_eq!(store.ids().unwrap().len(), 3);
        assert_eq!(std::fs::read(store.log_path(small)).unwrap(), DISCARDED);
        assert!(project.join("1").join("file").try_exists().unwrap());
        let kept = node.cas.stored().unwrap();
        assert!(kept.contains(&needed) && kept.contains(&spent));
        assert!(!kept.contains(&spent_content));
        assert!(kept.contains(&uploaded));
        node.make_room(&|| Ok(DiskPressure::Enough), &commanded, &home)
            .unwrap();
        busy.release().unwrap();
        running.release().unwrap();
    }

    #[test]
    fn a_submission_seen_before_returns_its_job_instead_of_starting_another() {
        let tmp = tempfile::tempdir().unwrap();
        let (node, job) = published(tmp.path(), b"");
        let store = Store::open(&dirs(tmp.path())).unwrap();
        let id = store.resolve(&job).unwrap();
        let nonce = crate::domain::Nonce::generate().unwrap();
        crate::state_file::write_bytes(&store.nonce_path("owner", &nonce), id.as_str().as_bytes())
            .unwrap();
        let spec = store.spec(&id).unwrap();
        let submission = Submission {
            queue: crate::protocol::Queue::Slot,
            nonce,
            name: None,
            command: spec.command,
            location: Location::Home,
            env: std::collections::BTreeMap::new(),
            shell: None,
        };
        let wire = ask(
            &node,
            &Request::Submit {
                submission: Box::new(submission),
            },
        );
        let reply = crate::remote::receive("m", &mut wire.as_slice(), &mut Vec::new()).unwrap();
        assert_eq!(reply.into_job().unwrap().spec.id, id);
        assert_eq!(store.ids().unwrap().len(), 1);
    }

    #[test]
    fn a_refusal_before_the_stream_starts_stays_a_plain_refusal() {
        let tmp = tempfile::tempdir().unwrap();
        let (node, _) = published(tmp.path(), b"");
        let wire = ask(
            &node,
            &Request::Tail {
                job: "0ZZZZZZZZZZZZZZZ".parse().unwrap(),
                lines: 1,
            },
        );
        let reply = crate::remote::receive("m", &mut wire.as_slice(), &mut Vec::new()).unwrap();
        assert!(matches!(reply, Reply::Refused(_)));
    }
}
