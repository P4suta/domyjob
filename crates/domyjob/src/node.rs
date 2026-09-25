use std::io::{BufRead, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::Arc;

use crate::audit::{AuditLog, Verdict};
use crate::authz::{self, Authorized, Principal, Relation};
use crate::cas::{Cas, CasError};
use crate::clock::Timestamp;
use crate::control::{Order, Session};
use crate::domain::{BlobId, Invalid, JobId, JobRef};
use crate::local_socket::Reach;
use crate::lock::LockError;
use crate::paths::Dirs;
use crate::proc::{self, ProcError};
use crate::protocol::{
    Follow, Frame, Hello, Job, Location, PROTOCOL, Phase, Refusal, RefusalCode, Reply, Request,
    Spec, Submission, VERSION,
};
use crate::store::{Store, StoreError};
use crate::terminal::RemoteText;

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
    #[error("{0} is not stored here yet; upload it first")]
    Incomplete(String),
    #[error("an upload of {0} items is more than one request may carry")]
    TooMany(u64),
    #[error("job {0} has no workspace to fetch from")]
    NoWorkspace(JobId),
    #[error(
        "job {job}'s workspace has since been filled by job {by}, so its files are gone; run it with --fresh to keep them apart"
    )]
    Reused { job: JobId, by: String },
    #[error(transparent)]
    Workspace(#[from] crate::workspace::WorkspaceError),
    #[error("{action} {path}: {source}")]
    Io {
        action: &'static str,
        path: PathBuf,
        source: std::io::Error,
    },
    #[error(transparent)]
    Scan(#[from] crate::logscan::ScanError),
    #[error("the {0} request streams its answer and cannot be answered in one reply")]
    Misrouted(&'static str),
    #[error("the supervisor could not start: {0}")]
    NotStarted(RemoteText),
    #[error("job {0} already has a supervisor")]
    AlreadySupervised(JobId),
    #[error("the queue lost every slot it was waiting for")]
    QueueClosed,
    #[error("the {0} thread panicked")]
    Panicked(&'static str),
    #[error("talking to the job's supervisor: {0}")]
    Control(std::io::Error),
}

impl NodeError {
    fn code(&self) -> RefusalCode {
        if out_of_space(self) {
            return RefusalCode::DiskFull;
        }
        match self {
            Self::Store(StoreError::NoSuchJob(_)) => RefusalCode::NoSuchJob,
            Self::Store(StoreError::Ambiguous { .. }) => RefusalCode::AmbiguousJob,
            Self::Cas(CasError::Missing(_)) | Self::Incomplete(_) => RefusalCode::MissingContent,
            Self::Denied(_) => RefusalCode::Forbidden,
            Self::Workspace(crate::workspace::WorkspaceError::NotAFile(_)) => RefusalCode::NotAFile,
            Self::Workspace(crate::workspace::WorkspaceError::Io { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                RefusalCode::NoSuchPath
            }
            Self::NoWorkspace(_) | Self::Reused { .. } => RefusalCode::NoWorkspace,
            Self::Request(_)
            | Self::Invalid(_)
            | Self::Input(_)
            | Self::TooMany(_)
            | Self::Scan(
                crate::logscan::ScanError::Pattern(..) | crate::logscan::ScanError::PatternTooLong,
            )
            | Self::Misrouted(_) => RefusalCode::BadRequest,
            Self::Proc(_) | Self::AlreadySupervised(_) | Self::NotStarted(_) => RefusalCode::Spawn,
            Self::Store(_)
            | Self::Workspace(_)
            | Self::QueueClosed
            | Self::Panicked(_)
            | Self::Control(_)
            | Self::Cas(_)
            | Self::Output(_)
            | Self::Io { .. }
            | Self::Audit(_)
            | Self::State(_)
            | Self::Scan(crate::logscan::ScanError::Io(_))
            | Self::Lock(_) => RefusalCode::Storage,
        }
    }
}

fn short(available: u64, total: u64) -> bool {
    available < (total / ROOM_SHARE).min(ROOM_AT_LEAST)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Keep {
    Every,
    Unfinished,
}

fn out_of_space(error: &(dyn std::error::Error + 'static)) -> bool {
    let mut current = Some(error);
    while let Some(link) = current {
        if let Some(io) = link.downcast_ref::<std::io::Error>()
            && matches!(
                io.kind(),
                std::io::ErrorKind::StorageFull | std::io::ErrorKind::QuotaExceeded
            )
        {
            return true;
        }
        current = link.source();
    }
    false
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
}

#[must_use]
pub fn hello(dirs: &Dirs) -> Hello {
    Hello {
        protocol: PROTOCOL,
        version: RemoteText::new(VERSION.to_owned()),
        os: RemoteText::new(std::env::consts::OS.to_owned()),
        arch: RemoteText::new(std::env::consts::ARCH.to_owned()),
        home: RemoteText::new(dirs.home.display().to_string()),
        state: RemoteText::new(dirs.state.display().to_string()),
        shell: RemoteText::new(crate::shell::default_shell()),
        binary: RemoteText::new(match crate::dist::running() {
            Ok(binary) => binary.get().sha256().to_owned(),
            Err(error) => format!("unknown: {error}"),
        }),
    }
}

const KEEP_FINISHED: usize = 500;

const ROOM_AT_LEAST: u64 = 10 << 30;

const ROOM_SHARE: u64 = 10;

const DISCARDED: &[u8] = b"domyjob: this log was discarded to free disk space on this machine\n";

fn retire_old_binaries() {
    if cfg!(test) {
        return;
    }
    if let Ok(exe) = std::env::current_exe() {
        crate::user_files::sweep_retired(&exe);
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

const fn action(request: &Request) -> &'static str {
    match request {
        Request::Hello => "hello",
        Request::Hold => "hold",
        Request::Missing { .. } => "missing",
        Request::Upload { .. } => "upload",
        Request::Submit { .. } => "submit",
        Request::List { .. } => "list",
        Request::Status { .. } => "status",
        Request::Wait { .. } => "wait",
        Request::Kill { .. } => "kill",
        Request::Logs { .. } => "logs",
        Request::Tail { .. } => "tail",
        Request::AuditAt { .. } => "audit-at",
        Request::AuditHead => "audit-head",
        Request::Digest { .. } => "digest",
        Request::Search { .. } => "search",
        Request::Get { .. } => "get",
    }
}

fn subject(request: &Request) -> Option<String> {
    match request {
        Request::Kill { job }
        | Request::Status { job }
        | Request::Wait { job, .. }
        | Request::Logs { job, .. }
        | Request::Tail { job, .. }
        | Request::Digest { job, .. }
        | Request::Search { job, .. } => Some(job.to_string()),
        Request::Get { job, path } => Some(format!("{job} {path}")),
        Request::Submit { submission } => Some(submission.command.display()),
        Request::Hello
        | Request::Hold
        | Request::AuditAt { .. }
        | Request::AuditHead
        | Request::Missing { .. }
        | Request::Upload { .. }
        | Request::List { .. } => None,
    }
}

const fn audited(request: &Request) -> bool {
    match request {
        Request::Submit { .. } | Request::Kill { .. } | Request::Get { .. } => true,
        Request::Hello
        | Request::Hold
        | Request::AuditAt { .. }
        | Request::AuditHead
        | Request::Missing { .. }
        | Request::Upload { .. }
        | Request::List { .. }
        | Request::Status { .. }
        | Request::Wait { .. }
        | Request::Logs { .. }
        | Request::Tail { .. }
        | Request::Digest { .. }
        | Request::Search { .. } => false,
    }
}

impl Node {
    pub fn open(dirs: Dirs) -> Result<Self, NodeError> {
        crate::state_file::private_dir(&dirs.state)?;
        let store = Store::open(&dirs)?;
        let cas = Cas::open(dirs.state.join("objects"))?;
        let audit = AuditLog::at(&dirs);
        Ok(Self {
            dirs,
            store,
            cas,
            audit,
        })
    }

    pub fn serve(
        &self,
        principal: &Principal,
        input: impl Input,
        output: &mut (dyn Write + Send),
    ) -> Result<(), NodeError> {
        self.sweep();
        crate::liveness::with_pulse(output, |pulsed| self.answer(principal, input, pulsed))
    }

    fn answer(
        &self,
        principal: &Principal,
        mut input: impl Input,
        output: &mut dyn Write,
    ) -> Result<(), NodeError> {
        let outcome = read_line::<Request>(&mut input).and_then(|request| {
            let verb = action(&request);
            let about = subject(&request);
            let must_audit = audited(&request) || matches!(principal, Principal::Peer { .. });
            let decision = authz::authorize(principal.clone(), request);
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
        let (principal, request) = authorized.into_parts();
        match request {
            Request::Hold => self.hold(input, output),
            Request::Logs {
                job,
                offset,
                follow,
            } => {
                let cursor = Cursor { offset, follow };
                self.logs(&self.own(&principal, &job)?, cursor, (input, output))
            }
            Request::Tail { job, lines } => self.tail(&self.own(&principal, &job)?, lines, output),
            Request::Get { job, path } => self.get(&self.own(&principal, &job)?, &path, output),
            single @ (Request::Hello
            | Request::AuditAt { .. }
            | Request::AuditHead
            | Request::Digest { .. }
            | Request::Search { .. }
            | Request::Missing { .. }
            | Request::Upload { .. }
            | Request::Submit { .. }
            | Request::List { .. }
            | Request::Status { .. }
            | Request::Wait { .. }
            | Request::Kill { .. }) => {
                let reply = self.reply(&principal, single, input)?;
                send(output, &reply)
            }
        }
    }

    fn hold(&self, mut input: impl Input, output: &mut dyn Write) -> Result<(), NodeError> {
        send(output, &Reply::Hello(hello(&self.dirs)))?;
        std::io::copy(&mut input, &mut std::io::sink()).map_err(NodeError::Input)?;
        Ok(())
    }

    fn reply(
        &self,
        principal: &Principal,
        request: Request,
        mut input: impl Input,
    ) -> Result<Reply, NodeError> {
        Ok(match request {
            Request::Hello => Reply::Hello(hello(&self.dirs)),
            Request::AuditAt { seq } => Reply::AuditAt {
                hash: self.audit.hash_at(seq)?,
            },
            Request::AuditHead => Reply::AuditHead(self.audit.head()?),
            Request::Digest { job, tail } => {
                Reply::Digest(Box::new(self.digest(&self.own(principal, &job)?, tail)?))
            }
            Request::Search {
                job,
                pattern,
                context,
                limit,
            } => Reply::Found(self.search(
                &self.own(principal, &job)?,
                &pattern,
                crate::logscan::Window { context, limit },
            )?),
            Request::Missing { blobs } => Reply::Missing {
                blobs: self.cas.missing(&blobs)?,
            },
            Request::Upload { count } => Reply::Stored {
                count: self.upload(count, &mut input)?,
            },
            Request::Submit { submission } => {
                Reply::Job(Box::new(self.accept(principal, *submission)?))
            }
            Request::List { limit } => {
                let (jobs, unreadable) = self.list(principal, limit)?;
                Reply::Jobs { jobs, unreadable }
            }
            Request::Status { job } => {
                Reply::Job(Box::new(self.store.job(&self.own(principal, &job)?)?))
            }
            Request::Wait { job } => Reply::Job(Box::new(self.settle(
                &self.own(principal, &job)?,
                Order::Wait,
                input,
            )?)),
            Request::Kill { job } => Reply::Job(Box::new(self.settle(
                &self.own(principal, &job)?,
                Order::Kill,
                input,
            )?)),
            Request::Hold | Request::Logs { .. } | Request::Tail { .. } | Request::Get { .. } => {
                return Err(NodeError::Misrouted(action(&request)));
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

    fn upload(&self, count: u64, input: &mut dyn BufRead) -> Result<u64, NodeError> {
        if count > crate::bounded::UPLOAD_COUNT {
            return Err(NodeError::TooMany(count));
        }
        for _ in 0..count {
            let frame: Frame = read_line(input)?;
            self.cas.receive(input, &frame.blob, frame.size)?;
        }
        Ok(count)
    }

    fn accept(&self, principal: &Principal, submission: Submission) -> Result<Job, NodeError> {
        let collecting = crate::lock::OsLock::exclusive(&self.store.collection_lock_path())?;
        let nonce_path = self.store.nonce_path(
            &crate::supervisor::scope_name(&principal.submitter()),
            &submission.nonce,
        );
        if let Some(earlier) = crate::state_file::read_bytes(&nonce_path)? {
            collecting.release()?;
            let id: JobId = String::from_utf8_lossy(&earlier).trim().parse()?;
            return Ok(self.store.job(&id)?);
        }
        if let Location::Snapshot { source, .. } = &submission.location {
            let manifest = self.cas.manifest(&source.manifest)?;
            if let Some(blob) = self.cas.missing(&manifest.blobs())?.first() {
                return Err(NodeError::Incomplete(format!("blob {blob}")));
            }
        }
        let spec = Spec {
            id: JobId::generate()?,
            name: submission.name,
            command: submission.command,
            location: submission.location,
            env_names: submission.env.keys().cloned().collect(),
            shell: submission.shell,
            concurrency: submission.concurrency,
            sequence: self.store.next_sequence()?,
            submitted_by: principal.submitter(),
            submitted_at: Timestamp::observe(),
        };
        let launch = crate::store::LaunchEnv::of_this_process();
        let staging = crate::lock::OsLock::exclusive(&self.store.staging_lock_path(&spec.id))?;
        self.store.stage(&spec, (&submission.env, &launch))?;
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

    fn launch_supervisor(&self, id: &JobId) -> Result<(), NodeError> {
        let exe = proc::own_executable().map_err(|source| NodeError::Io {
            action: "locating",
            path: PathBuf::from("domyjob"),
            source,
        })?;
        let invocation = crate::spawn::Invocation::new(
            crate::template::Arg::path(&exe),
            vec![
                crate::template::Arg::literal("node"),
                crate::template::Arg::literal("--supervise"),
                crate::template::Arg::word(id),
                crate::template::Arg::literal("--state-dir"),
                crate::template::Arg::path(&self.dirs.state),
                crate::template::Arg::literal("--home-dir"),
                crate::template::Arg::path(&self.dirs.home),
            ],
        )
        .in_dir(&self.dirs.home);
        Ok(proc::launch(&invocation)?)
    }

    #[must_use]
    pub fn running(&self) -> Vec<JobId> {
        let Ok(ids) = self.store.ids() else {
            return Vec::new();
        };
        ids.into_iter()
            .filter(|id| {
                !matches!(self.store.phase(id), Ok(Phase::Finished { .. }))
                    && matches!(
                        crate::lock::OsLock::try_exclusive(&self.store.alive_path(id)),
                        Ok(None)
                    )
            })
            .collect()
    }

    pub fn stop(&self, id: &JobId) -> Result<Job, NodeError> {
        self.settle(id, Order::Kill, std::io::empty())
    }

    pub fn sweep(&self) {
        if let Ok(ids) = self.store.ids() {
            for id in ids {
                match self.recover(&id) {
                    Ok(()) | Err(_) => {}
                }
            }
        }
        if let Ok(staged) = self.store.staged_ids() {
            for id in staged {
                match self.abandon_staging(&id) {
                    Ok(()) | Err(_) => {}
                }
            }
        }
        match self.retire(KEEP_FINISHED) {
            Ok(()) | Err(_) => {}
        }
        self.empty_trash();
        retire_old_binaries();
        let short = || self.short_of_room();
        match self.make_room(&short) {
            Ok(()) | Err(_) => {}
        }
    }

    fn short_of_room(&self) -> bool {
        match fs4::statvfs(self.store.area("jobs")) {
            Ok(stats) => short(stats.available_space(), stats.total_space()),
            Err(_unmeasurable) => false,
        }
    }

    fn make_room(&self, short: &impl Fn() -> bool) -> Result<(), NodeError> {
        if !short() {
            return Ok(());
        }
        for (workspace, lock) in self.idle_workspaces() {
            if let Some(idle) = crate::lock::OsLock::try_exclusive(&lock)? {
                let aside = self
                    .store
                    .area("trash")
                    .join(format!("workspace-{}", JobId::generate()?));
                crate::state_file::move_aside(&workspace, &aside)?;
                crate::state_file::remove_tree_forcibly(&aside)?;
                idle.release()?;
                if !short() {
                    return Ok(());
                }
            }
        }
        for id in self.finished_largest_log_first()? {
            let Some(alive) = crate::lock::OsLock::try_exclusive(&self.store.alive_path(&id))?
            else {
                continue;
            };
            let log = self.store.log_path(&id);
            crate::state_file::cut_to(&log, 0)?;
            crate::state_file::overwrite_in_place(&log, DISCARDED)?;
            alive.release()?;
            if !short() {
                return Ok(());
            }
        }
        self.collect(Keep::Unfinished)
    }

    fn idle_workspaces(&self) -> Vec<(PathBuf, PathBuf)> {
        let mut found = Vec::new();
        let Ok(scopes) = std::fs::read_dir(self.store.area("work")) else {
            return found;
        };
        for scope in scopes.flatten() {
            let Ok(projects) = std::fs::read_dir(scope.path()) else {
                continue;
            };
            for project in projects.flatten() {
                let Ok(slots) = std::fs::read_dir(project.path()) else {
                    continue;
                };
                for slot in slots.flatten() {
                    let name = slot.file_name();
                    let Some(index) = name.to_str().filter(|n| n.parse::<usize>().is_ok()) else {
                        continue;
                    };
                    let lock = project.path().join("locks").join(format!("{index}.lock"));
                    found.push((slot.path(), lock));
                }
            }
        }
        found
    }

    fn empty_trash(&self) {
        let trash = self.store.area("trash");
        let Ok(entries) = std::fs::read_dir(&trash) else {
            return;
        };
        for entry in entries.flatten() {
            match crate::state_file::remove_tree_forcibly(&entry.path()) {
                Ok(()) | Err(_) => {}
            }
        }
    }

    fn finished_largest_log_first(&self) -> Result<Vec<JobId>, NodeError> {
        let mut logs = Vec::new();
        for id in self.finished_oldest_first()? {
            let size = match std::fs::symlink_metadata(self.store.log_path(&id)) {
                Ok(meta) => meta.len(),
                Err(_absent) => continue,
            };
            if size > crate::domain::len_u64(DISCARDED.len()) {
                logs.push((std::cmp::Reverse(size), id));
            }
        }
        logs.sort();
        Ok(logs.into_iter().map(|(_, id)| id).collect())
    }

    fn finished_oldest_first(&self) -> Result<Vec<JobId>, NodeError> {
        let mut finished = Vec::new();
        for id in self.store.ids()? {
            if let (Ok(Phase::Finished { .. }), Ok(spec)) =
                (self.store.phase(&id), self.store.spec(&id))
            {
                finished.push((spec.sequence, id));
            }
        }
        finished.sort();
        Ok(finished.into_iter().map(|(_, id)| id).collect())
    }

    fn retire(&self, keep: usize) -> Result<(), NodeError> {
        let finished = self.finished_oldest_first()?;
        let Some(excess) = finished.len().checked_sub(keep).filter(|n| *n > 0) else {
            return Ok(());
        };
        for id in finished.into_iter().take(excess) {
            let Some(alive) = crate::lock::OsLock::try_exclusive(&self.store.alive_path(&id))?
            else {
                continue;
            };
            self.store.remove_job(&id)?;
            alive.release()?;
        }
        self.forget_stale_nonces()?;
        self.collect(Keep::Every)
    }

    fn forget_stale_nonces(&self) -> Result<(), NodeError> {
        let dir = self.store.area("nonces");
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Ok(());
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(bytes) = crate::state_file::read_bytes(&path)? else {
                continue;
            };
            let gone = match String::from_utf8_lossy(&bytes).trim().parse::<JobId>() {
                Ok(id) => !self.store.is_published(&id)?,
                Err(_foreign) => true,
            };
            if gone {
                crate::state_file::remove_file(&path)?;
            }
        }
        Ok(())
    }

    fn collect(&self, keep: Keep) -> Result<(), NodeError> {
        let collecting = crate::lock::OsLock::exclusive(&self.store.collection_lock_path())?;
        let mut needed = Vec::new();
        let mut spent = Vec::new();
        for id in self.store.ids()? {
            let Ok(spec) = self.store.spec(&id) else {
                return Ok(collecting.release()?);
            };
            if keep == Keep::Unfinished
                && matches!(self.store.phase(&id), Ok(Phase::Finished { .. }))
            {
                spent.push(spec);
            } else {
                needed.push(spec);
            }
        }
        for id in self.store.staged_ids()? {
            let Ok(spec) = self.store.staged_spec(&id) else {
                return Ok(collecting.release()?);
            };
            needed.push(spec);
        }
        let reachable = self.reachable(&needed)?;
        let doomed: Vec<BlobId> = match keep {
            Keep::Every => self.cas.stored()?,
            Keep::Unfinished => self.reachable(&spent)?.into_iter().collect(),
        };
        for blob in doomed {
            if !reachable.contains(&blob) {
                self.cas.remove(&blob)?;
            }
        }
        Ok(collecting.release()?)
    }

    fn reachable(&self, specs: &[Spec]) -> Result<std::collections::BTreeSet<BlobId>, NodeError> {
        let mut reachable = std::collections::BTreeSet::new();
        for spec in specs {
            if let Location::Snapshot { source, .. } = &spec.location {
                reachable.insert(source.manifest.clone());
                match self.cas.manifest(&source.manifest) {
                    Ok(manifest) => reachable.extend(manifest.blobs()),
                    Err(CasError::Missing(_) | CasError::Damaged(_)) => {}
                    Err(other) => return Err(other.into()),
                }
            }
        }
        Ok(reachable)
    }

    fn recover(&self, id: &JobId) -> Result<(), NodeError> {
        let phase = self.store.phase(id)?;
        if matches!(phase, Phase::Finished { .. }) {
            return Ok(());
        }
        let Some(alive) = crate::lock::OsLock::try_exclusive(&self.store.alive_path(id))? else {
            return Ok(());
        };
        let failure = self.store.start_failure(id)?;
        let (started_at, reason) = match (&phase, failure) {
            (_, Some(why)) => (None, format!("the job could not start: {why}")),
            (Phase::Queued | Phase::Preparing { .. }, None) => {
                alive.release()?;
                return self.launch_supervisor(id);
            }
            (Phase::Running { started_at, .. }, None) => (
                Some(*started_at),
                "its supervisor vanished while it ran (the machine may have restarted); it was not run again because it may already have had effects".to_owned(),
            ),
            (Phase::Finished { .. }, None) => return Ok(()),
        };
        let finished = Phase::Finished {
            started_at,
            finished_at: Timestamp::observe(),
            outcome: crate::protocol::Outcome::Errored {
                reason: RemoteText::new(reason),
            },
        };
        self.store.set_phase(id, &finished)?;
        if let Ok(spec) = self.store.spec(id)
            && let Some(root) = crate::supervisor::fresh_root(&self.store, &spec)
        {
            crate::supervisor::discard_workspace(&root)?;
        }
        Ok(alive.release()?)
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
        let mut jobs = Vec::new();
        let mut unreadable = Vec::new();
        for id in self.store.ids()? {
            match self.store.job(&id) {
                Ok(job) => match principal.relation_to(&job.spec.submitted_by) {
                    Relation::Oversees | Relation::Submitted => jobs.push(job),
                    Relation::Stranger => {}
                },
                Err(error) => {
                    if matches!(principal, Principal::Owner) {
                        unreadable.push(crate::protocol::Unreadable {
                            id,
                            why: RemoteText::new(error.to_string()),
                        });
                    }
                }
            }
        }
        jobs.sort_by_key(|job| std::cmp::Reverse(job.spec.sequence));
        jobs.truncate(crate::domain::to_usize(limit));
        Ok((jobs, unreadable))
    }

    fn settle(&self, id: &JobId, order: Order, input: impl Input) -> Result<Job, NodeError> {
        let session =
            Session::open(&self.store.control_path(id), order).map_err(NodeError::Control)?;
        if let Reach::Reached(session) = session {
            let session = abandon_when_the_client_leaves(input, session);
            session
                .relay(&mut std::io::sink())
                .map_err(NodeError::Control)?;
        }
        Ok(self.store.job(id)?)
    }

    fn open_log(&self, id: &JobId) -> Result<std::fs::File, NodeError> {
        let path = self.store.log_path(id);
        std::fs::File::open(&path).map_err(|source| NodeError::Io {
            action: "opening",
            path,
            source,
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
                if let Reach::Reached(session) = session {
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
        let mut window = Vec::new();
        file.take(crate::bounded::TAIL_WINDOW)
            .read_to_end(&mut window)
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
        let Location::Snapshot { subdir, .. } = &job.spec.location else {
            return Err(NodeError::NoWorkspace(job.spec.id));
        };
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
    use crate::domain::Concurrency;
    use crate::protocol::{Command, Location, Spec};
    use crate::store::LaunchEnv;

    fn dirs(root: &std::path::Path) -> Dirs {
        Dirs {
            home: root.into(),
            state: root.join("state"),
            config: root.join("c"),
            cache: root.join("k"),
            keys: crate::keystore::KeyStore::OwnerOnlyFile,
        }
    }

    fn published(root: &std::path::Path, log: &[u8]) -> (Node, JobRef) {
        let store = Store::open(&dirs(root)).unwrap();
        let id: JobId = "0AAAAAAAAAAAAAAA".parse().unwrap();
        let spec = Spec {
            id: id.clone(),
            name: None,
            command: Command::Script("true".into()),
            location: Location::Home,
            env_names: std::collections::BTreeSet::new(),
            shell: None,
            concurrency: Concurrency::DEFAULT,
            sequence: 1,
            submitted_by: authz::Submitter::Owner,
            submitted_at: Timestamp::observe(),
        };
        store
            .stage(
                &spec,
                (&std::collections::BTreeMap::new(), &LaunchEnv::default()),
            )
            .unwrap();
        store.publish(&id).unwrap();
        store
            .set_phase(
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
            Node::open(dirs(root)).unwrap(),
            id.as_str().parse().unwrap(),
        )
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
        let submission = Submission {
            nonce: crate::domain::Nonce::generate().unwrap(),
            name: None,
            command: Command::Script("make it".into()),
            location: Location::Home,
            env: std::collections::BTreeMap::new(),
            shell: None,
            concurrency: Concurrency::DEFAULT,
        };
        for request in [
            Request::Tail {
                job: job.clone(),
                lines: 3,
            },
            Request::Get {
                job: job.clone(),
                path: "out/a.txt".parse().unwrap(),
            },
            Request::Submit {
                submission: Box::new(submission),
            },
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
                    Some("make it".to_owned()),
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
                nonce,
                name: None,
                command: Command::Script("true".into()),
                location,
                env: std::collections::BTreeMap::new(),
                shell: None,
                concurrency: Concurrency::DEFAULT,
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
        let text = |path: &std::path::Path| path.display().to_string();
        let nonce = crate::domain::Nonce::generate().unwrap();
        let snapshot = Location::Snapshot {
            source: crate::protocol::Source {
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
    fn a_disk_is_short_below_a_tenth_of_its_size_or_ten_gigabytes() {
        let gib: u64 = 1 << 30;
        assert!(short(ROOM_AT_LEAST - 1, 500 * gib));
        assert!(!short(ROOM_AT_LEAST, 500 * gib));
        assert!(short(50 * gib / 10 - 1, 50 * gib));
        assert!(!short(50 * gib / 10, 50 * gib));
        assert!(!short(1, 0));
    }

    #[test]
    fn a_roomy_disk_keeps_every_log_and_small_ones_are_never_replaced() {
        let tmp = tempfile::tempdir().unwrap();
        let (node, job) = published(tmp.path(), &[b'x'; 10_000]);
        let store = Store::open(&dirs(tmp.path())).unwrap();
        let id = store.resolve(&job).unwrap();
        node.make_room(&|| false).unwrap();
        assert_eq!(std::fs::read(store.log_path(&id)).unwrap(), [b'x'; 10_000]);
        let same_size = vec![b'z'; DISCARDED.len()];
        for log in [b"tiny".to_vec(), same_size] {
            crate::state_file::write_bytes(&store.log_path(&id), &log).unwrap();
            node.make_room(&|| true).unwrap();
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
        node.retire(2).unwrap();
        assert!(node.cas.stored().unwrap().contains(&upload));
        let nonce = crate::domain::Nonce::generate().unwrap();
        let nonce_path = store.nonce_path("owner", &nonce);
        crate::state_file::write_bytes(&nonce_path, first.as_str().as_bytes()).unwrap();
        let text = |path: &std::path::Path| path.display().to_string();
        for (site, tag) in [
            ("store::list", text(&store.area("jobs"))),
            ("state_file::lock", text(&store.alive_path(&first))),
            ("state_file::remove", text(&store.job_dir(&first))),
            ("state_file::read", text(&nonce_path)),
        ] {
            let _faults = crate::faults::inject(&[(site, &tag)]);
            node.retire(1).unwrap_err();
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
        node.sweep();
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
        let text = |path: &std::path::Path| path.display().to_string();
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
            store.set_phase(&open, &running).unwrap();
            if !store.staged_ids().unwrap().contains(&staged) {
                store
                    .stage(
                        &spec,
                        (&std::collections::BTreeMap::new(), &LaunchEnv::default()),
                    )
                    .unwrap();
            }
            let _faults = crate::faults::inject(&[(site, &tag)]);
            node.sweep();
        }
        Node::open(node.dirs.clone()).unwrap();
        for area in [
            node.dirs.state.clone(),
            store.area(""),
            node.dirs.state.join("objects"),
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
            assert_eq!(reply, Reply::Stream);
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

    #[test]
    fn a_job_whose_supervisor_vanished_while_running_is_closed_not_rerun() {
        let tmp = tempfile::tempdir().unwrap();
        let (node, job) = published(tmp.path(), b"");
        let store = Store::open(&dirs(tmp.path())).unwrap();
        let id = store.resolve(&job).unwrap();
        store
            .set_phase(
                &id,
                &Phase::Running {
                    started_at: Timestamp::at_millis(1),
                    pid: 1,
                    workspace: "w".into(),
                },
            )
            .unwrap();
        node.sweep();
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
        store.set_phase(&id, &running).unwrap();
        let phase_file = store.job_dir(&id).join("phase.json").display().to_string();
        {
            let _faults = crate::faults::inject(&[("state_file::write", &phase_file)]);
            node.sweep();
        }
        assert_eq!(store.phase(&id).unwrap(), running);
        node.sweep();
        assert!(matches!(store.phase(&id).unwrap(), Phase::Finished { .. }));
    }

    #[test]
    fn a_job_that_failed_to_start_is_closed_with_its_reason() {
        let tmp = tempfile::tempdir().unwrap();
        let (node, job) = published(tmp.path(), b"");
        let store = Store::open(&dirs(tmp.path())).unwrap();
        let id = store.resolve(&job).unwrap();
        store.set_phase(&id, &Phase::Queued).unwrap();
        store.record_start_failure(&id, "no shell").unwrap();
        node.sweep();
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
        node.sweep();
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
            .set_phase(&newer, &store.phase(&older).unwrap())
            .unwrap();
        let stray = BlobId::of(b"nobody needs this");
        node.cas.put(&stray, b"nobody needs this").unwrap();
        node.retire(1).unwrap();
        assert_eq!(store.ids().unwrap(), vec![newer]);
        assert!(!node.cas.stored().unwrap().contains(&stray));
    }

    #[test]
    fn a_full_disk_is_named_as_such_however_deep_it_is_wrapped() {
        let failing = |kind: std::io::ErrorKind| {
            NodeError::Store(StoreError::State(crate::state_file::StateError::Io {
                action: "writing",
                path: "/state/x".into(),
                source: kind.into(),
            }))
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
            source: crate::protocol::Source {
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

    #[test]
    fn every_step_of_making_room_that_fails_is_an_error_and_leaves_the_rest() {
        let tmp = tempfile::tempdir().unwrap();
        let (node, _) = published(tmp.path(), b"");
        let store = Store::open(&dirs(tmp.path())).unwrap();
        let id: JobId = "0AAAAAAAAAAAAAAA".parse().unwrap();
        let log = store.log_path(&id);
        crate::state_file::write_bytes(&log, &[b'x'; 10_000]).unwrap();
        let project = store.area("work").join("owner").join("proj");
        crate::state_file::write_bytes(&project.join("0").join("file"), b"built").unwrap();
        let text = |path: &std::path::Path| path.display().to_string();
        for (site, tag) in [
            (
                "state_file::lock",
                text(&project.join("locks").join("0.lock")),
            ),
            ("state_file::rename", text(&project.join("0"))),
            (
                "state_file::remove",
                text(&store.area("trash").join("workspace-")),
            ),
            ("store::list", text(&store.area("jobs"))),
            ("state_file::lock", text(&store.alive_path(&id))),
            ("state_file::cut", text(&log)),
            ("state_file::overwrite", text(&log)),
        ] {
            let _faults = crate::faults::inject(&[(site, &tag)]);
            node.make_room(&|| true).unwrap_err();
        }
    }

    #[test]
    fn collecting_keeps_what_is_needed_and_every_failing_step_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let (node, _) = published(tmp.path(), b"");
        let store = Store::open(&dirs(tmp.path())).unwrap();
        let first: JobId = "0AAAAAAAAAAAAAAA".parse().unwrap();
        let ids = finished_like(&store, &first, &["0FFFFFFFFFFFFFFF"]);
        let running = ids.last().unwrap();
        store
            .set_phase(
                running,
                &Phase::Running {
                    started_at: Timestamp::at_millis(1),
                    pid: 1,
                    workspace: String::new(),
                },
            )
            .unwrap();
        let finished = sent(&node, &store, &first, br#"{"entries":{}}"#);
        let unfinished = sent(&node, &store, running, br#"{ "entries":{}}"#);
        let stray = BlobId::of(b"stray");
        node.cas.put(&stray, b"stray").unwrap();
        let blob_tag = |blob: &BlobId| blob.split().1.to_owned();
        let text = |path: &std::path::Path| path.display().to_string();
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
                text(&node.dirs.state.join("objects")),
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
        node.collect(Keep::Unfinished).unwrap();
        let left = node.cas.stored().unwrap();
        assert!(!left.contains(&finished) && left.contains(&unfinished));
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
            store.set_phase(&id, &store.phase(first).unwrap()).unwrap();
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

        node.make_room(&|| false).unwrap();
        assert!(there("0") && there("2"));
        let until_one_workspace_is_gone = || there("0") && there("2");
        node.make_room(&until_one_workspace_is_gone).unwrap();
        assert_ne!(there("0"), there("2"));
        assert!(project.join("1").join("file").try_exists().unwrap());
        assert_eq!(store.ids().unwrap().len(), 3);

        let big = ids.get(1).unwrap();
        let small = ids.last().unwrap();
        crate::state_file::write_bytes(&store.log_path(big), &[b'x'; 10_000]).unwrap();
        crate::state_file::write_bytes(&store.log_path(small), &[b'y'; 9_000]).unwrap();
        let until_the_biggest_log_is_gone =
            || std::fs::read(store.log_path(big)).unwrap() != DISCARDED;
        node.make_room(&until_the_biggest_log_is_gone).unwrap();
        assert_eq!(std::fs::read(store.log_path(big)).unwrap(), DISCARDED);
        assert_eq!(std::fs::read(store.log_path(small)).unwrap(), [b'y'; 9_000]);
        assert_eq!(store.ids().unwrap().len(), 3);

        let snapshot = |id: &JobId, manifest: &[u8]| sent(&node, &store, id, manifest);
        let needed = snapshot(&oldest, br#"{"entries":{}}"#);
        store
            .set_phase(
                &oldest,
                &Phase::Running {
                    started_at: Timestamp::at_millis(1),
                    pid: 1,
                    workspace: String::new(),
                },
            )
            .unwrap();
        let spent = snapshot(small, br#"{ "entries": {} }"#);
        let uploaded = BlobId::of(b"sent for a submission still on its way");
        node.cas
            .put(&uploaded, b"sent for a submission still on its way")
            .unwrap();
        node.make_room(&|| true).unwrap();
        assert!(!there("0") && !there("2"));
        assert_eq!(store.ids().unwrap().len(), 3);
        assert_eq!(std::fs::read(store.log_path(small)).unwrap(), DISCARDED);
        assert!(project.join("1").join("file").try_exists().unwrap());
        let kept = node.cas.stored().unwrap();
        assert!(kept.contains(&needed));
        assert!(!kept.contains(&spent));
        assert!(kept.contains(&uploaded));
        node.make_room(&|| false).unwrap();
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
            nonce,
            name: None,
            command: spec.command,
            location: Location::Home,
            env: std::collections::BTreeMap::new(),
            shell: None,
            concurrency: spec.concurrency,
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
