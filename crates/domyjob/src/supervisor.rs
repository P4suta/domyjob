use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex, OnceLock};

use crate::authz::Submitter;
use crate::cas::{Applied, Cas};
use crate::clock::Timestamp;
use crate::control::Order;
use crate::domain::JobId;
use crate::local_socket::{Listener, Stream};
use crate::lock::OsLock;
use crate::node::NodeError;
use crate::paths::Dirs;
use crate::proc::{self, Group, Readiness};
use crate::protocol::{Location, Outcome, Phase, Spec, Workspace};
use crate::store::Store;
use crate::terminal::RemoteText;

#[derive(Debug)]
enum Event {
    Slot(OsLock),
    NoSlot(crate::lock::LockError),
    Kill,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stop {
    Asked,
    Ended,
}

#[derive(Debug, PartialEq, Eq)]
enum Ending {
    Concluded(Outcome),
    Returned,
}

impl Stop {
    fn before_start(self, when: &str) -> (Ending, String) {
        match self {
            Self::Asked => (
                Ending::Concluded(Outcome::Killed),
                format!("killed while {when}"),
            ),
            Self::Ended => (
                Ending::Returned,
                format!(
                    "the machine told domyjob to stop while the job was {when}; it starts again when the machine is next given a command, such as a new job"
                ),
            ),
        }
    }

    fn while_running(self) -> Outcome {
        match self {
            Self::Asked => Outcome::Killed,
            Self::Ended => Outcome::Errored {
                reason: RemoteText::new(
                    "the machine told domyjob to stop while the job ran, as it does when it shuts down; it was not run again because it may already have had effects"
                        .to_owned(),
                ),
            },
        }
    }
}

const LOG_HEAD: u64 = 256 << 20;
const LOG_TAIL: usize = 8 << 20;

#[derive(Debug)]
struct Log {
    file: File,
    len: u64,
    closed: bool,
    failure: Option<String>,
    head: u64,
    tail: std::collections::VecDeque<u8>,
    tail_limit: usize,
    left_out: u64,
}

impl Log {
    const fn new(file: File, len: u64) -> Self {
        Self {
            file,
            len,
            closed: false,
            failure: None,
            head: LOG_HEAD,
            tail: std::collections::VecDeque::new(),
            tail_limit: LOG_TAIL,
            left_out: 0,
        }
    }

    fn take(&mut self, chunk: &[u8]) {
        if self.failure.is_some() {
            return;
        }
        let room = match usize::try_from(self.head.saturating_sub(self.len)) {
            Ok(room) => room,
            Err(_beyond_memory) => chunk.len(),
        };
        let (now, later) = chunk.split_at(room.min(chunk.len()));
        if !now.is_empty() {
            match self.file.write_all(now) {
                Ok(()) => self.len = self.len.saturating_add(crate::domain::len_u64(now.len())),
                Err(error) => {
                    self.failure = Some(error.to_string());
                    return;
                }
            }
        }
        self.tail.extend(later);
        let over = self.tail.len().saturating_sub(self.tail_limit);
        if over > 0 {
            self.tail.drain(..over);
            self.left_out = self.left_out.saturating_add(crate::domain::len_u64(over));
        }
    }

    fn seal(&mut self) {
        if self.failure.is_none() && (self.left_out > 0 || !self.tail.is_empty()) {
            let mut rest = Vec::with_capacity(self.tail.len().saturating_add(160));
            if self.left_out > 0 {
                rest.extend_from_slice(
                    format!(
                        "\ndomyjob: {} bytes of output were left out here, to keep the log within bounds; the start and the end are kept\n",
                        self.left_out
                    )
                    .as_bytes(),
                );
            }
            rest.extend(self.tail.drain(..));
            match self.file.write_all(&rest) {
                Ok(()) => self.len = self.len.saturating_add(crate::domain::len_u64(rest.len())),
                Err(error) => self.failure = Some(error.to_string()),
            }
        }
        self.closed = true;
    }
}

#[derive(Debug)]
struct Shared {
    log: Mutex<Log>,
    grew: Condvar,
    finished: Mutex<bool>,
    ended: Condvar,
    killed: AtomicBool,
    stop: OnceLock<Stop>,
    group: OnceLock<Arc<Group>>,
    events: Sender<Event>,
    log_path: PathBuf,
    notes_path: PathBuf,
}

impl Shared {
    fn append(&self, chunk: &[u8]) {
        let Ok(mut log) = self.log.lock() else {
            return;
        };
        log.take(chunk);
        drop(log);
        self.grew.notify_all();
    }

    fn say(&self, line: &str) {
        let noted = crate::state_file::open_append(&self.notes_path)
            .map(|mut notes| notes.write_all(format!("{line}\n").as_bytes()));
        match noted {
            Ok(Ok(()) | Err(_)) | Err(_) => {}
        }
    }

    fn close_log(&self) -> Option<String> {
        let failure = match self.log.lock() {
            Ok(mut log) => {
                log.seal();
                log.failure.clone()
            }
            Err(_poisoned) => Some("the log lock was poisoned".to_owned()),
        };
        self.grew.notify_all();
        failure
    }

    fn finish(&self) {
        if let Ok(mut finished) = self.finished.lock() {
            *finished = true;
        }
        self.ended.notify_all();
    }

    fn until_finished(&self) {
        let Ok(mut finished) = self.finished.lock() else {
            return;
        };
        while !*finished {
            finished = match self.ended.wait(finished) {
                Ok(guard) => guard,
                Err(_poisoned) => return,
            };
        }
    }

    fn kill(&self, stop: Stop) -> Result<(), NodeError> {
        match self.stop.set(stop) {
            Ok(()) | Err(_) => {}
        }
        self.killed.store(true, Ordering::SeqCst);
        match self.events.send(Event::Kill) {
            Ok(()) | Err(_) => {}
        }
        match self.group.get() {
            Some(group) => Ok(group.kill()?),
            None => Ok(()),
        }
    }

    fn killed(&self) -> bool {
        self.killed.load(Ordering::SeqCst)
    }

    fn stopped(&self) -> Stop {
        match self.stop.get() {
            Some(stop) => *stop,
            None => Stop::Asked,
        }
    }

    fn before_start(&self, when: &str) -> Ending {
        let (ending, note) = self.stopped().before_start(when);
        self.say(&note);
        ending
    }

    fn follow(&self, offset: u64, stream: &Stream) -> std::io::Result<()> {
        let mut file = File::open(&self.log_path)?;
        let mut position = file.seek(SeekFrom::Start(offset))?;
        let mut buffer = vec![0u8; 64 * 1024];
        let mut out = stream;
        loop {
            let read = file.read(&mut buffer)?;
            if let Some(chunk) = buffer.get(..read).filter(|c| !c.is_empty()) {
                out.write_all(chunk)?;
                position = position.saturating_add(crate::domain::len_u64(chunk.len()));
                continue;
            }
            let Ok(mut log) = self.log.lock() else {
                return Ok(());
            };
            while log.len <= position && !log.closed {
                log = match self.grew.wait(log) {
                    Ok(guard) => guard,
                    Err(_poisoned) => return Ok(()),
                };
            }
            let finished = log.len <= position && log.closed;
            drop(log);
            if finished {
                return Ok(());
            }
        }
    }
}

fn answer(shared: &Shared, stream: &Stream) {
    let order = match crate::control::read_order(stream) {
        Ok(order) => order,
        Err(_malformed) => return,
    };
    match order {
        Order::Kill => {
            if let Err(error) = shared.kill(Stop::Asked) {
                shared.say(&format!("stopping the job failed: {error}"));
            }
            shared.until_finished();
        }
        Order::Wait => shared.until_finished(),
        Order::Follow { offset } => match shared.follow(offset, stream) {
            Ok(()) | Err(_) => {}
        },
    }
}

#[derive(Debug, Default)]
struct Tally {
    open: usize,
    ended: u64,
}

#[derive(Debug, Default)]
struct Connections {
    tally: Mutex<Tally>,
    changed: Condvar,
}

struct Open(Arc<Connections>);

impl Drop for Open {
    fn drop(&mut self) {
        if let Ok(mut tally) = self.0.tally.lock() {
            tally.open = tally.open.saturating_sub(1);
            tally.ended = tally.ended.wrapping_add(1);
        }
        self.0.changed.notify_all();
    }
}

trait Counting {
    fn open(&self) -> Open;
    fn retrying<T, E>(&self, attempt: impl FnMut() -> Result<T, E>) -> Result<T, E>;
}

impl Counting for Arc<Connections> {
    fn open(&self) -> Open {
        if let Ok(mut tally) = self.tally.lock() {
            tally.open = tally.open.saturating_add(1);
        }
        Open(Self::clone(self))
    }

    fn retrying<T, E>(&self, mut attempt: impl FnMut() -> Result<T, E>) -> Result<T, E> {
        loop {
            let mark = match self.tally.lock() {
                Ok(tally) => tally.ended,
                Err(_poisoned) => return attempt(),
            };
            let error = match attempt() {
                Ok(done) => return Ok(done),
                Err(error) => error,
            };
            let Ok(tally) = self.tally.lock() else {
                return Err(error);
            };
            match self
                .changed
                .wait_while(tally, |tally| tally.ended == mark && tally.open > 0)
            {
                Ok(tally) if tally.ended != mark => {}
                Ok(_) | Err(_) => return Err(error),
            }
        }
    }
}

fn serve_control(listener: Listener, shared: Arc<Shared>) {
    let connections = Arc::new(Connections::default());
    std::thread::spawn(move || {
        loop {
            match connections.retrying(|| listener.accept()) {
                Ok(stream) => {
                    let shared = Arc::clone(&shared);
                    let open = connections.open();
                    std::thread::spawn(move || {
                        answer(&shared, &stream);
                        drop(open);
                    });
                }
                Err(error) => {
                    shared.say(&format!("the control socket stopped accepting: {error}"));
                    return;
                }
            }
        }
    });
}

#[must_use]
pub fn scope_name(submitter: &Submitter) -> String {
    match submitter {
        Submitter::Owner => "owner".to_owned(),
        Submitter::Peer { key, .. } => format!("peer-{}", key.fingerprint().replace('-', "")),
    }
}

fn workspace_root(store: &Store, spec: &Spec, slot: usize) -> Option<PathBuf> {
    match &spec.location {
        Location::Snapshot {
            source, workspace, ..
        } => {
            let base = store
                .area("work")
                .join(scope_name(&spec.submitted_by))
                .join(source.project.as_str());
            Some(match workspace {
                Workspace::Warm => base.join(slot.to_string()),
                Workspace::Fresh => base.join(format!("fresh-{}", spec.id)),
            })
        }
        Location::Home => None,
    }
}

#[must_use]
pub fn fresh_root(store: &Store, spec: &Spec) -> Option<PathBuf> {
    match &spec.location {
        Location::Snapshot {
            workspace: Workspace::Fresh,
            ..
        } => workspace_root(store, spec, 0),
        Location::Snapshot {
            workspace: Workspace::Warm,
            ..
        }
        | Location::Home => None,
    }
}

pub fn discard_workspace(root: &Path) -> Result<(), crate::state_file::StateError> {
    crate::state_file::remove_dir_all(root)?;
    crate::state_file::remove_file(&filled_by_path(root))?;
    crate::state_file::remove_file(&applied_path(root))
}

fn applied_path(root: &Path) -> PathBuf {
    beside(root, ".applied.json")
}

#[must_use]
pub fn filled_by_path(root: &Path) -> PathBuf {
    beside(root, ".filled-by")
}

fn beside(root: &Path, suffix: &str) -> PathBuf {
    let mut state = root.as_os_str().to_owned();
    state.push(suffix);
    PathBuf::from(state)
}

#[derive(Debug)]
struct Held {
    slot: Option<OsLock>,
    workspace: Option<OsLock>,
}

impl Held {
    fn release(self) -> Result<(), NodeError> {
        if let Some(workspace) = self.workspace {
            workspace.release()?;
        }
        if let Some(slot) = self.slot {
            slot.release()?;
        }
        Ok(())
    }
}

#[derive(Debug)]
struct Collecting {
    thread: std::thread::JoinHandle<Result<(), proc::ProcError>>,
    stopper: proc::Stopper,
}

impl Collecting {
    fn finish(self) -> Result<(), NodeError> {
        self.stopper.stop()?;
        match self.thread.join() {
            Ok(collected) => Ok(collected?),
            Err(_panicked) => Err(NodeError::QueueClosed),
        }
    }
}

struct Supervisor {
    dirs: Dirs,
    store: Store,
    cas: Cas,
    spec: Spec,
    shared: Arc<Shared>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stops {
    Heard,
    Unheard,
}

pub fn supervise(
    dirs: Dirs,
    id: &JobId,
    readiness: Readiness,
    stops: Stops,
) -> Result<(), NodeError> {
    let store = Store::open(&dirs)?;
    let (supervisor, alive, events) = match take_charge(dirs, store.clone(), id, (readiness, stops))
    {
        Ok(taken) => taken,
        Err(error) => {
            store.record_start_failure(id, &error.to_string())?;
            return Err(error);
        }
    };
    let finished = supervisor.conclude(&events);
    let released = alive.release();
    finished?;
    Ok(released?)
}

fn take_charge(
    dirs: Dirs,
    store: Store,
    id: &JobId,
    (readiness, stops): (Readiness, Stops),
) -> Result<(Supervisor, OsLock, Receiver<Event>), NodeError> {
    let Some(alive) = OsLock::try_exclusive(&store.alive_path(id))? else {
        return Err(NodeError::AlreadySupervised(id.clone()));
    };
    let control = store.control_path(id);
    crate::state_file::remove_file(&control)?;
    let listener = Listener::bind(&control).map_err(|source| {
        NodeError::Io(crate::failure::IoFailure {
            action: "listening on",
            path: control.clone(),
            source,
        })
    })?;
    let cas = Cas::open(dirs.state.join("objects"))?;
    if !store.is_published(id)? {
        store.publish(id)?;
    }
    let spec = store.spec(id)?;
    let log_path = store.log_path(id);
    let file = crate::state_file::open_append(&log_path)?;
    let len = file
        .metadata()
        .map_err(|source| {
            NodeError::Io(crate::failure::IoFailure {
                action: "measuring",
                path: log_path.clone(),
                source,
            })
        })?
        .len();
    let (events, received) = std::sync::mpsc::channel();
    let shared = Arc::new(Shared {
        log: Mutex::new(Log::new(file, len)),
        grew: Condvar::new(),
        finished: Mutex::new(false),
        ended: Condvar::new(),
        killed: AtomicBool::new(false),
        stop: OnceLock::new(),
        group: OnceLock::new(),
        events,
        log_path,
        notes_path: store.notes_path(id),
    });
    serve_control(listener, Arc::clone(&shared));
    let told = Arc::clone(&shared);
    if stops == Stops::Heard
        && let Err(error) = ctrlc::set_handler(move || {
            if let Err(error) = told.kill(Stop::Ended) {
                told.say(&format!("stopping the job failed: {error}"));
            }
        })
    {
        shared.say(&format!(
            "a shutdown will read as a vanished supervisor, because the machine's requests to stop cannot be heard: {error}"
        ));
    }
    if let Err(error) = readiness.announce() {
        shared.say(&format!(
            "the submitter left before the job started: {error}"
        ));
    }
    let supervisor = Supervisor {
        dirs,
        store,
        cas,
        spec,
        shared,
    };
    Ok((supervisor, alive, received))
}

impl Supervisor {
    fn conclude(&self, events: &Receiver<Event>) -> Result<(), NodeError> {
        let mut held = Held {
            slot: None,
            workspace: None,
        };
        let mut started = None;
        let result = self.run(events, &mut held, &mut started);
        let outcome = match result {
            Ok(Ending::Concluded(outcome)) => outcome,
            Ok(Ending::Returned) => return self.step_back(held),
            Err(error) => {
                self.shared.say(&error.to_string());
                Outcome::Errored {
                    reason: RemoteText::new(error.to_string()),
                }
            }
        };
        if let Err(error) = self.record_left() {
            self.shared.say(&format!(
                "what the job changed could not be kept, so pull needs its workspace: {error}"
            ));
        }
        if let Err(error) = self.cleanup() {
            self.shared.say(&format!("cleaning up: {error}"));
        }
        let released = held.release();
        let outcome = match (self.shared.close_log(), outcome) {
            (Some(failure), Outcome::Succeeded | Outcome::Failed { .. }) => Outcome::Errored {
                reason: RemoteText::new(format!("the log could not be written: {failure}")),
            },
            (Some(_) | None, outcome) => outcome,
        };
        let finished = Phase::Finished {
            started_at: started,
            finished_at: Timestamp::observe(),
            outcome,
        };
        if let Err(error) = self.store.set_phase(&self.spec.id, &finished) {
            self.store
                .record_outcome_in_place(&self.spec.id, &finished)
                .map_err(|_also| error)?;
        }
        self.shared.finish();
        released
    }

    fn step_back(&self, held: Held) -> Result<(), NodeError> {
        if let Err(error) = self.cleanup() {
            self.shared.say(&format!("cleaning up: {error}"));
        }
        let released = held.release();
        match self.shared.close_log() {
            Some(_) | None => {}
        }
        self.shared.finish();
        released
    }

    fn queue(&self, events: &Receiver<Event>) -> Result<Option<OsLock>, NodeError> {
        let slots = self.store.area("slots");
        let count = self.spec.concurrency.slots();
        if count > 1 {
            for index in (0..count).rev() {
                if let Some(lock) = OsLock::try_exclusive(&slots.join(format!("{index}.lock")))? {
                    return Ok(Some(lock));
                }
            }
        }
        let won = Arc::new(AtomicBool::new(false));
        let failed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        for index in 0..count {
            let path = slots.join(format!("{index}.lock"));
            let won = Arc::clone(&won);
            let failed = Arc::clone(&failed);
            let sender = self.shared.events.clone();
            std::thread::spawn(move || {
                let lock = match OsLock::exclusive(&path) {
                    Ok(lock) => lock,
                    Err(error) => {
                        if failed.fetch_add(1, Ordering::SeqCst).saturating_add(1) == count {
                            match sender.send(Event::NoSlot(error)) {
                                Ok(()) | Err(_) => {}
                            }
                        }
                        return;
                    }
                };
                if won.swap(true, Ordering::SeqCst) {
                    match lock.release() {
                        Ok(()) | Err(_) => {}
                    }
                    return;
                }
                match sender.send(Event::Slot(lock)) {
                    Ok(()) | Err(_) => {}
                }
            });
        }
        match events.recv() {
            Ok(Event::Slot(lock)) => Ok(Some(lock)),
            Ok(Event::NoSlot(error)) => Err(NodeError::Lock(error)),
            Ok(Event::Kill) => Ok(None),
            Err(_disconnected) => Err(NodeError::QueueClosed),
        }
    }

    fn run(
        &self,
        events: &Receiver<Event>,
        held: &mut Held,
        started: &mut Option<Timestamp>,
    ) -> Result<Ending, NodeError> {
        if !self.store.skips_the_queue(&self.spec.id)? {
            let Some(slot) = self.queue(events)? else {
                return Ok(self.shared.before_start("queued"));
            };
            let holder = Store::slot_holder_path(slot.path());
            match crate::state_file::write_bytes(&holder, self.spec.id.as_str().as_bytes()) {
                Ok(()) | Err(_) => {}
            }
            held.slot = Some(slot);
        }
        let started_at = Timestamp::observe();
        *started = Some(started_at);
        self.store
            .set_phase(&self.spec.id, &Phase::Preparing { started_at })?;
        let (root, workspace_lock) = match self.prepare() {
            Ok(prepared) => prepared,
            Err(NodeError::Workspace(crate::workspace::WorkspaceError::Stopped)) => {
                return Ok(self.shared.before_start("preparing"));
            }
            Err(other) => return Err(other),
        };
        held.workspace = workspace_lock;
        if self.shared.killed() {
            return Ok(self.shared.before_start("preparing"));
        }
        let (group, collecting) = self.start(&root)?;
        let group = Arc::new(group);
        if self.shared.group.set(Arc::clone(&group)).is_err() {
            match group.kill() {
                Ok(()) | Err(_) => {}
            }
            return Err(NodeError::AlreadySupervised(self.spec.id.clone()));
        }
        if self.shared.killed()
            && let Err(error) = group.kill()
        {
            self.shared
                .say(&format!("stopping the job failed: {error}"));
        }
        let running = Phase::Running {
            started_at,
            pid: group.id(),
            workspace: root.display().to_string(),
        };
        if let Err(error) = self.store.set_phase(&self.spec.id, &running) {
            self.shared.say(&format!(
                "recording that the job runs failed ({error}); it runs regardless"
            ));
        }
        Ok(Ending::Concluded(self.watch(&group, collecting)))
    }

    fn start(&self, root: &Path) -> Result<(Group, Collecting), NodeError> {
        let cwd = match (&self.spec.location, root) {
            (
                Location::Snapshot {
                    subdir: Some(sub), ..
                },
                root,
            ) => sub.parts().fold(root.to_path_buf(), |p, part| p.join(part)),
            (Location::Snapshot { subdir: None, .. } | Location::Home, root) => root.to_path_buf(),
        };
        let mut command =
            crate::shell::process(&self.spec.command, self.spec.shell.as_deref()).command();
        command.current_dir(&cwd);
        for name in self.store.take_launch(&self.spec.id)?.apply(&mut command) {
            self.shared.say(&format!(
                "the environment variable {} was not passed on because it is not Unicode",
                crate::terminal::neutralize(&name)
            ));
        }
        command
            .env("DOMYJOB", "1")
            .env("DOMYJOB_JOB_ID", self.spec.id.as_str());
        let (collector, stopper, writer) = proc::output_pipe()?;
        let group = Group::spawn(command, writer)?;
        let shared = Arc::clone(&self.shared);
        let thread = std::thread::spawn(move || {
            let mut sink = |chunk: &[u8]| shared.append(chunk);
            collector.run(&mut sink)
        });
        Ok((group, Collecting { thread, stopper }))
    }

    fn watch(&self, group: &Group, collecting: Collecting) -> Outcome {
        let status = group.wait();
        if let Err(error) = collecting.finish() {
            self.shared.say(&format!("collecting output: {error}"));
        }
        match (status, self.shared.killed()) {
            (Ok(_) | Err(_), true) => self.shared.stopped().while_running(),
            (Ok(status), false) => match proc::exit_code(status) {
                0 => Outcome::Succeeded,
                code => Outcome::Failed { exit_code: code },
            },
            (Err(error), false) => Outcome::Errored {
                reason: RemoteText::new(error.to_string()),
            },
        }
    }

    fn prepare(&self) -> Result<(PathBuf, Option<OsLock>), NodeError> {
        let (root, lock) = match &self.spec.location {
            Location::Snapshot {
                source, workspace, ..
            } => {
                let locks = self
                    .store
                    .area("work")
                    .join(scope_name(&self.spec.submitted_by))
                    .join(source.project.as_str())
                    .join("locks");
                let (slot, lock) = match workspace {
                    Workspace::Warm => match OsLock::first_free(&locks, usize::MAX)? {
                        Some((slot, lock)) => (slot, Some(lock)),
                        None => (0, None),
                    },
                    Workspace::Fresh => (0, None),
                };
                let root = workspace_root(&self.store, &self.spec, slot)
                    .unwrap_or_else(|| self.dirs.home.clone());
                self.fill(&root, &source.manifest)?;
                (root, lock)
            }
            Location::Home => (self.dirs.home.clone(), None),
        };
        crate::state_file::write_bytes(
            &self.store.workspace_record(&self.spec.id),
            root.display().to_string().as_bytes(),
        )?;
        Ok((root, lock))
    }

    fn fill(&self, root: &Path, manifest_id: &crate::domain::BlobId) -> Result<(), NodeError> {
        match self.fill_once(root, manifest_id) {
            Err(NodeError::Workspace(crate::workspace::WorkspaceError::Tree(
                crate::tree::TreeError::Io(crate::failure::IoFailure {
                    action,
                    path,
                    source,
                }),
            ))) => {
                self.shared.say(&format!(
                    "the workspace could not be updated ({action} {}: {source}); moving it aside and filling it afresh",
                    path.display()
                ));
                let aside = self.store.area("trash").join(self.spec.id.as_str());
                crate::state_file::move_aside(root, &aside)?;
                crate::state_file::remove_file(&applied_path(root))?;
                crate::state_file::remove_file(&filled_by_path(root))?;
                self.fill_once(root, manifest_id)
            }
            other => other,
        }
    }

    fn fill_once(&self, root: &Path, manifest_id: &crate::domain::BlobId) -> Result<(), NodeError> {
        let manifest = self.cas.manifest(manifest_id)?;
        let state = applied_path(root);
        let previous: Applied = crate::state_file::read_json(&state)?.unwrap_or_default();
        let mut intent = previous.clone();
        for rel in manifest.entries.keys() {
            intent.insert(rel.clone());
        }
        crate::state_file::remove_file(&filled_by_path(root))?;
        crate::state_file::write_json(&state, &intent)?;
        let workspace = crate::workspace::Workspace::open(root)?;
        let plan = crate::workspace::Plan {
            manifest: &manifest,
            previous: &previous,
        };
        let (applied, _) = workspace.materialize(&self.cas, plan, &self.shared.killed)?;
        crate::state_file::write_json(&state, &applied)?;
        crate::state_file::write_bytes(&filled_by_path(root), self.spec.id.as_str().as_bytes())?;
        Ok(())
    }

    fn record_left(&self) -> Result<(), NodeError> {
        let Location::Snapshot { source, .. } = &self.spec.location else {
            return Ok(());
        };
        let Some(root) =
            crate::state_file::read_bytes(&self.store.workspace_record(&self.spec.id))?
        else {
            return Ok(());
        };
        let root = PathBuf::from(String::from_utf8_lossy(&root).trim());
        let Some(workspace) = crate::workspace::Workspace::open_existing(&root)? else {
            return Ok(());
        };
        let sent = self.cas.manifest(&source.manifest)?;
        let left = workspace.left(&sent)?;
        for item in &left {
            if let Some(crate::snapshot::Entry::File { blob, size, .. }) = &item.now
                && !self.cas.has(blob)?
            {
                let mut file = workspace.open_file(&item.path)?;
                self.cas.receive(&mut file, blob, *size)?;
            }
        }
        Ok(crate::state_file::write_json(
            &self.store.left_path(&self.spec.id),
            &left,
        )?)
    }

    fn cleanup(&self) -> Result<(), NodeError> {
        let Location::Snapshot {
            workspace: Workspace::Fresh,
            ..
        } = &self.spec.location
        else {
            return Ok(());
        };
        let Some(root) = workspace_root(&self.store, &self.spec, 0) else {
            return Ok(());
        };
        crate::state_file::remove_dir_all(&root)?;
        crate::state_file::remove_file(&filled_by_path(&root))?;
        Ok(crate::state_file::remove_file(&applied_path(&root))?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_job_the_machine_stops_before_it_starts_is_returned_and_one_it_stops_while_running_is_closed()
     {
        assert_eq!(
            Stop::Asked.before_start("queued").0,
            Ending::Concluded(Outcome::Killed)
        );
        let (ending, note) = Stop::Ended.before_start("preparing");
        assert_eq!(ending, Ending::Returned);
        assert!(note.contains("while the job was preparing") && note.contains("starts again"));
        assert_eq!(Stop::Asked.while_running(), Outcome::Killed);
        let ended = Stop::Ended.while_running();
        assert!(
            matches!(&ended, Outcome::Errored { reason } if reason.as_raw_str().contains("told domyjob to stop")),
            "{ended:?}"
        );
    }

    #[test]
    fn an_endless_log_keeps_its_start_and_its_end_and_says_what_was_left_out() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("private");
        crate::state_file::private_dir(&dir).unwrap();
        let path = dir.join("log");
        let file = crate::state_file::open_append(&path).unwrap();
        let mut log = Log::new(file, 0);
        log.head = 10;
        log.tail_limit = 6;
        log.take(b"start-");
        log.take(b"0123456789");
        log.take(b"middle");
        log.take(b"-end\n");
        log.seal();
        let text =
            String::from_utf8(crate::state_file::read_bytes(&path).unwrap().unwrap()).unwrap();
        assert!(text.starts_with("start-0123"), "{text}");
        assert!(text.ends_with("\ne-end\n"), "{text}");
        assert!(text.contains("bytes of output were left out"), "{text}");
        assert_eq!(log.len, crate::domain::len_u64(text.len()));
        assert!(log.closed);

        let small = crate::state_file::open_append(&dir.join("small")).unwrap();
        let mut within = Log::new(small, 0);
        within.take(b"all of it\n");
        within.seal();
        assert_eq!(
            crate::state_file::read_bytes(&dir.join("small"))
                .unwrap()
                .unwrap(),
            b"all of it\n"
        );
    }

    #[test]
    fn a_failed_accept_is_retried_after_any_connection_ends_and_given_up_when_none_are_open() {
        let connections = Arc::new(Connections::default());
        let mut alone = 0;
        let nothing_open: Result<(), usize> = connections.retrying(|| {
            alone += 1;
            Err(alone)
        });
        assert_eq!(nothing_open, Err(1));

        let first = connections.open();
        let second = connections.open();
        let mut pending = Some(first);
        let mut during = 0;
        let ended_before_the_wait = connections.retrying(|| {
            during += 1;
            drop(pending.take());
            if during < 2 { Err(during) } else { Ok(during) }
        });
        assert_eq!(ended_before_the_wait, Ok(2));

        let (go, wait_for_go) = std::sync::mpsc::channel::<()>();
        let ending = std::thread::spawn(move || {
            wait_for_go.recv().unwrap();
            drop(second);
        });
        let mut after = 0;
        let ended_while_waiting: Result<usize, usize> = connections.retrying(|| {
            after += 1;
            if after < 2 {
                go.send(()).unwrap();
                Err(after)
            } else {
                Ok(after)
            }
        });
        ending.join().unwrap();
        assert_eq!(ended_while_waiting, Ok(2));
    }
}
