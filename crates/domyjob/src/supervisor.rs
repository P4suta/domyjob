use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::{Arc, Condvar, Mutex, OnceLock};

use crate::authz::Submitter;
use crate::cas::{Applied, Cas};
use crate::clock::Timestamp;
use crate::control::Order;
use crate::domain::JobId;
use crate::local_socket::{Listener, Stream};
use crate::lock::{OsLock, SlotIndex};
use crate::node::NodeError;
use crate::paths::Dirs;
use crate::proc::{self, Group, Readiness};
use crate::protocol::{Location, Outcome, Phase, Settings, Spec, Workspace};
use crate::store::{Publication, QueueMode, QueuePredecessor, Store};
use crate::terminal::RemoteText;

pub(crate) struct WorkingDirectory(PathBuf);

impl WorkingDirectory {
    fn for_job(location: &Location, root: &Path) -> Self {
        let path = match (location, root) {
            (
                Location::Snapshot {
                    subdir: Some(sub), ..
                },
                root,
            ) => sub.parts().fold(root.to_path_buf(), |p, part| p.join(part)),
            (Location::Snapshot { subdir: None, .. } | Location::Home, root) => root.to_path_buf(),
        };
        Self(path)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}

#[derive(Debug)]
enum Event {
    Changed,
    Released(PathBuf),
    LockFailed(crate::lock::LockError),
    Kill,
}

const EVENT_CAPACITY: usize = 130;

fn watch_lock_release(path: PathBuf, sender: SyncSender<Event>) {
    std::thread::spawn(move || match OsLock::exclusive(&path) {
        Ok(lock) => {
            match lock.release() {
                Ok(()) | Err(_) => {}
            }
            match sender.send(Event::Released(path)) {
                Ok(()) | Err(_) => {}
            }
        }
        Err(error) => match sender.send(Event::LockFailed(error)) {
            Ok(()) | Err(_) => {}
        },
    });
}

struct QueueWatchers {
    slots: std::collections::BTreeSet<PathBuf>,
    earlier: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WatchRegistration {
    Start,
    Ignore,
}

impl QueueWatchers {
    const fn new() -> Self {
        Self {
            slots: std::collections::BTreeSet::new(),
            earlier: None,
        }
    }

    fn register_slot(&mut self, path: &Path) -> WatchRegistration {
        let limit = crate::domain::to_usize(crate::domain::Concurrency::MOST);
        if self.slots.contains(path) || self.slots.len() >= limit {
            return WatchRegistration::Ignore;
        }
        self.slots.insert(path.to_path_buf());
        WatchRegistration::Start
    }

    fn register_earlier(&mut self, path: &Path) -> WatchRegistration {
        if self.earlier.as_deref() == Some(path) {
            return WatchRegistration::Ignore;
        }
        self.earlier = Some(path.to_path_buf());
        WatchRegistration::Start
    }

    fn observe(&mut self, decision: &Admission, held: &[PathBuf], mut watch: impl FnMut(PathBuf)) {
        match decision {
            Admission::Paused => {}
            Admission::EarlierWaiter(predecessor) => {
                if self.register_earlier(predecessor.path()) == WatchRegistration::Start {
                    watch(predecessor.path().to_path_buf());
                }
            }
            Admission::Full | Admission::Open => {
                for path in held {
                    if self.register_slot(path) == WatchRegistration::Start {
                        watch(path.clone());
                    }
                }
            }
        }
    }

    fn released(&mut self, path: &Path) {
        self.slots.remove(path);
        if self.earlier.as_deref() == Some(path) {
            self.earlier = None;
        }
    }
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
                    "the machine told domyjob to stop while the job was {when}; it waits for an explicit `domyjob retry`"
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

#[derive(Debug, Clone, PartialEq, Eq)]
enum Admission {
    Open,
    Paused,
    EarlierWaiter(QueuePredecessor),
    Full,
}

fn decide_admission(
    settings: Settings,
    held: usize,
    earlier: Option<QueuePredecessor>,
) -> Admission {
    if settings.paused {
        Admission::Paused
    } else if let Some(path) = earlier {
        Admission::EarlierWaiter(path)
    } else if held >= settings.max_jobs.slots() {
        Admission::Full
    } else {
        Admission::Open
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KillState {
    Open,
    Asked,
}

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
    events: SyncSender<Event>,
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
        let first = self.stop.set(stop);
        self.killed.store(true, Ordering::SeqCst);
        if first.is_ok() {
            match self.events.send(Event::Kill) {
                Ok(()) | Err(_) => {}
            }
        }
        match self.group.get() {
            Some(group) => Ok(group.kill()?),
            None => Ok(()),
        }
    }

    fn kill_state(&self) -> KillState {
        if self.killed.load(Ordering::SeqCst) {
            KillState::Asked
        } else {
            KillState::Open
        }
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

const MAX_CONTROL_CONNECTIONS: usize = 32;

struct ControlPermit(Arc<Connections>);

impl ControlPermit {
    fn respond(self, shared: Arc<Shared>, stream: Stream) {
        std::thread::spawn(move || {
            answer(&shared, &stream);
            drop(self);
        });
    }
}

impl Drop for ControlPermit {
    fn drop(&mut self) {
        if let Ok(mut tally) = self.0.tally.lock() {
            tally.open = tally.open.saturating_sub(1);
            tally.ended = tally.ended.wrapping_add(1);
        }
        self.0.changed.notify_all();
    }
}

impl Connections {
    fn admit(self: &Arc<Self>) -> Option<ControlPermit> {
        let Ok(mut tally) = self.tally.lock() else {
            return None;
        };
        if tally.open >= MAX_CONTROL_CONNECTIONS {
            return None;
        }
        tally.open = tally.open.saturating_add(1);
        drop(tally);
        Some(ControlPermit(Arc::clone(self)))
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
                    let Some(permit) = connections.admit() else {
                        continue;
                    };
                    permit.respond(Arc::clone(&shared), stream);
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

fn acquire_warm_slot(locks: &Path) -> Result<(SlotIndex, OsLock), NodeError> {
    let limit = crate::domain::to_usize(crate::domain::Concurrency::MOST);
    OsLock::first_free(locks)?.ok_or(NodeError::WorkspaceSlotsFull(limit))
}

fn workspace_root(store: &Store, spec: &Spec, slot: SlotIndex) -> Option<PathBuf> {
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
        } => workspace_root(store, spec, SlotIndex::FIRST),
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

fn applied_file(root: &Path) -> crate::state_file::StateFile<Applied> {
    crate::state_file::StateFile::at(&applied_path(root))
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
    waiting: Option<OsLock>,
    slot: Option<OsLock>,
    workspace: Option<OsLock>,
    store: Store,
}

struct SupervisorLocks {
    alive: OsLock,
    waiting: OsLock,
}

impl Held {
    fn left_queue(&mut self) -> Result<(), NodeError> {
        if let Some(waiting) = self.waiting.take() {
            waiting.release()?;
        }
        Ok(())
    }

    fn begin_preparing(&mut self, id: &JobId, started_at: Timestamp) -> Result<(), NodeError> {
        let admission = self.store.admission()?;
        self.store.set_phase(id, &Phase::Preparing { started_at })?;
        self.left_queue()?;
        admission.release()?;
        Ok(self.store.signal_queue()?)
    }

    fn release(mut self) -> Result<(), NodeError> {
        self.left_queue()?;
        if let Some(workspace) = self.workspace {
            workspace.release()?;
        }
        if let Some(slot) = self.slot {
            slot.release()?;
            self.store.signal_queue()?;
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
    let (supervisor, SupervisorLocks { alive, waiting }, events) =
        match take_charge(dirs, store.clone(), id, (readiness, stops)) {
            Ok(taken) => taken,
            Err(error) => {
                store.record_start_failure(id, &error.to_string())?;
                return Err(error);
            }
        };
    let finished = supervisor.conclude(&events, waiting);
    let released = alive.release();
    let signaled = supervisor.store.signal_queue();
    finished?;
    released?;
    Ok(signaled?)
}

fn take_charge(
    dirs: Dirs,
    store: Store,
    id: &JobId,
    (readiness, stops): (Readiness, Stops),
) -> Result<(Supervisor, SupervisorLocks, Receiver<Event>), NodeError> {
    let locks = claim_supervisor_locks(&store, id, || {})?;
    let control = store.control_path(id);
    crate::state_file::remove_file(&control)?;
    let listener = Listener::bind(&control).map_err(|source| {
        NodeError::Io(crate::failure::IoFailure {
            action: "listening on",
            path: control.clone(),
            source,
        })
    })?;
    let cas = Cas::open(dirs.state().join("objects"))?;
    match store.publication(id)? {
        Publication::Published => {}
        Publication::Unpublished => store.publish(id)?,
    }
    store.record_supervisor_boot(id)?;
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
    let (events, received) = std::sync::mpsc::sync_channel(EVENT_CAPACITY);
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
    Ok((supervisor, locks, received))
}

fn claim_supervisor_locks(
    store: &Store,
    id: &JobId,
    alive_claimed: impl FnOnce(),
) -> Result<SupervisorLocks, NodeError> {
    let admission = store.admission()?;
    let Some(alive) = OsLock::try_exclusive(&store.alive_path(id))? else {
        return Err(NodeError::AlreadySupervised(id.clone()));
    };
    alive_claimed();
    let waiting = OsLock::exclusive(&store.queue_wait_path(id))?;
    admission.release()?;
    Ok(SupervisorLocks { alive, waiting })
}

impl Supervisor {
    fn conclude(&self, events: &Receiver<Event>, waiting: OsLock) -> Result<(), NodeError> {
        let mut held = Held {
            waiting: Some(waiting),
            slot: None,
            workspace: None,
            store: self.store.clone(),
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
        let outcome = match self.shared.close_log() {
            Some(failure) => match outcome {
                Outcome::Succeeded | Outcome::Failed { .. } => Outcome::Errored {
                    reason: RemoteText::new(format!("the log could not be written: {failure}")),
                },
                Outcome::Killed | Outcome::Errored { .. } => outcome,
            },
            None => outcome,
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
        let watched = self.store.area("");
        let changed = self.shared.events.clone();
        let queue_changed = self.store.queue_changed_path();
        let settings_path = self.store.settings_path();
        let mut notifier =
            notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                if let Ok(event) = event
                    && event
                        .paths
                        .iter()
                        .any(|path| path == &queue_changed || path == &settings_path)
                {
                    match changed.try_send(Event::Changed) {
                        Ok(()) | Err(_) => {}
                    }
                }
            })
            .map_err(|error| crate::node::watching(&watched, &error))?;
        notify::Watcher::watch(&mut notifier, &watched, notify::RecursiveMode::NonRecursive)
            .map_err(|error| crate::node::watching(&watched, &error))?;
        let mut watchers = QueueWatchers::new();
        loop {
            let admission = self.store.admission()?;
            let settings = self.store.settings()?;
            let held = self.store.held_slots()?;
            let predecessor = admission.earliest_waiter(&self.spec)?;
            let decision = decide_admission(settings, held.len(), predecessor);
            let slot = match &decision {
                Admission::Open => OsLock::first_free(&slots)?.map(|(_, lock)| lock),
                Admission::Paused | Admission::EarlierWaiter(_) | Admission::Full => None,
            };
            admission.release()?;
            if slot.is_some() {
                return Ok(slot);
            }
            watchers.observe(&decision, &held, |path| {
                watch_lock_release(path, self.shared.events.clone());
            });
            match events.recv() {
                Ok(Event::Changed) => {}
                Ok(Event::Released(path)) => {
                    watchers.released(&path);
                }
                Ok(Event::LockFailed(error)) => return Err(NodeError::Lock(error)),
                Ok(Event::Kill) => return Ok(None),
                Err(_disconnected) => return Err(NodeError::QueueClosed),
            }
        }
    }

    fn run(
        &self,
        events: &Receiver<Event>,
        held: &mut Held,
        started: &mut Option<Timestamp>,
    ) -> Result<Ending, NodeError> {
        if match self.store.queue_mode(&self.spec.id)? {
            QueueMode::Ordinary => true,
            QueueMode::Immediate => false,
        } {
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
        held.begin_preparing(&self.spec.id, started_at)?;
        let (root, workspace_lock) = match self.prepare() {
            Ok(prepared) => prepared,
            Err(NodeError::Workspace(crate::workspace::WorkspaceError::Stopped)) => {
                return Ok(self.shared.before_start("preparing"));
            }
            Err(other) => return Err(other),
        };
        held.workspace = workspace_lock;
        match self.shared.kill_state() {
            KillState::Asked => return Ok(self.shared.before_start("preparing")),
            KillState::Open => {}
        }
        self.store.set_phase(
            &self.spec.id,
            &Phase::Starting {
                started_at,
                workspace: root.display().to_string(),
            },
        )?;
        let (group, collecting) = self.start(&root)?;
        let group = Arc::new(group);
        if self.shared.group.set(Arc::clone(&group)).is_err() {
            match group.kill() {
                Ok(()) | Err(_) => {}
            }
            return Err(NodeError::AlreadySupervised(self.spec.id.clone()));
        }
        match self.shared.kill_state() {
            KillState::Asked => {
                if let Err(error) = group.kill() {
                    self.shared
                        .say(&format!("stopping the job failed: {error}"));
                }
            }
            KillState::Open => {}
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
        let cwd = WorkingDirectory::for_job(&self.spec.location, root);
        let command = crate::shell::process(&self.spec.command, self.spec.shell.as_deref())
            .in_dir(&cwd)
            .command();
        let (command, omitted) = self
            .store
            .take_launch(&self.spec.id)?
            .prepare(command, &self.spec.id);
        for name in omitted {
            self.shared.say(&format!(
                "the environment variable {} was not passed on because it is not Unicode",
                crate::terminal::neutralize(&name)
            ));
        }
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
        match (status, self.shared.kill_state()) {
            (Ok(_) | Err(_), KillState::Asked) => self.shared.stopped().while_running(),
            (Ok(status), KillState::Open) => match proc::exit_code(status) {
                0 => Outcome::Succeeded,
                code => Outcome::Failed { exit_code: code },
            },
            (Err(error), KillState::Open) => Outcome::Errored {
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
                    Workspace::Warm => {
                        let (slot, lock) = acquire_warm_slot(&locks)?;
                        (slot, Some(lock))
                    }
                    Workspace::Fresh => (SlotIndex::FIRST, None),
                };
                let root = workspace_root(&self.store, &self.spec, slot)
                    .unwrap_or_else(|| self.dirs.home().to_path_buf());
                self.fill(&root, &source.manifest)?;
                (root, lock)
            }
            Location::Home => (self.dirs.home().to_path_buf(), None),
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
        let previous: Applied = applied_file(root).read()?.unwrap_or_default();
        let mut intent = previous.clone();
        for rel in manifest.entries.keys() {
            intent.insert(rel.clone());
        }
        crate::state_file::remove_file(&filled_by_path(root))?;
        applied_file(root).replace(&intent)?;
        let workspace = crate::workspace::Workspace::open(root)?;
        let plan = crate::workspace::Plan {
            manifest: &manifest,
            previous: &previous,
        };
        let (applied, _) = workspace.materialize(&self.cas, plan, &self.shared.killed)?;
        applied_file(root).replace(&applied)?;
        crate::state_file::write_bytes(&filled_by_path(root), self.spec.id.as_str().as_bytes())?;
        Ok(())
    }

    fn record_left(&self) -> Result<(), NodeError> {
        let Some(source) = self.spec.source() else {
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
            if let Some(content) = item.now.as_ref().and_then(crate::snapshot::Entry::file)
                && !self.cas.has(content.blob)?
            {
                let mut file = workspace.open_file(&item.path)?;
                self.cas.receive(&mut file, content.blob, content.size)?;
            }
        }
        Ok(self.store.record_left(&self.spec.id, left)?)
    }

    fn cleanup(&self) -> Result<(), NodeError> {
        let Some(root) = fresh_root(&self.store, &self.spec) else {
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

    fn owner_spec(id: JobId, location: Location, sequence: u64) -> Spec {
        Spec {
            id,
            name: None,
            command: crate::protocol::Command::Script("true".into()),
            location,
            env_names: std::collections::BTreeSet::new(),
            shell: None,
            concurrency: crate::domain::Concurrency::DEFAULT,
            sequence,
            submitted_by: Submitter::Owner,
            submitted_at: Timestamp::at_millis(0),
        }
    }

    #[test]
    fn warm_workspaces_are_separate_for_owner_and_each_peer_key() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&Dirs::for_test(tmp.path())).unwrap();
        let owner = owner_spec(
            "0123456789ABCDEF".parse().unwrap(),
            Location::Snapshot {
                source: crate::protocol::Source {
                    project: "project".parse().unwrap(),
                    manifest: crate::domain::BlobId::of(b"manifest"),
                    revision: crate::protocol::Revision::WorkingDirectory,
                },
                subdir: None,
                workspace: Workspace::Warm,
            },
            1,
        );
        let peer = |byte, label: &str| Spec {
            submitted_by: Submitter::Peer {
                key: crate::trust::PublicKey::from_slice(&[byte; 32]).unwrap(),
                label: label.parse().unwrap(),
            },
            ..owner.clone()
        };
        let owner_root = workspace_root(&store, &owner, SlotIndex::FIRST).unwrap();
        let peer_one_root = workspace_root(&store, &peer(1, "first"), SlotIndex::FIRST).unwrap();
        assert_ne!(owner_root, peer_one_root);
        assert_ne!(
            peer_one_root,
            workspace_root(&store, &peer(2, "second"), SlotIndex::FIRST).unwrap()
        );
        assert_eq!(
            peer_one_root,
            workspace_root(&store, &peer(1, "renamed"), SlotIndex::FIRST).unwrap()
        );
    }

    #[test]
    fn a_job_the_machine_stops_before_it_starts_is_returned_and_one_it_stops_while_running_is_closed()
     {
        assert_eq!(
            Stop::Asked.before_start("queued").0,
            Ending::Concluded(Outcome::Killed)
        );
        let (ending, note) = Stop::Ended.before_start("preparing");
        assert_eq!(ending, Ending::Returned);
        assert!(note.contains("while the job was preparing") && note.contains("domyjob retry"));
        assert_eq!(Stop::Asked.while_running(), Outcome::Killed);
        let ended = Stop::Ended.while_running();
        assert!(
            matches!(&ended, Outcome::Errored { reason } if reason.as_raw_str().contains("told domyjob to stop")),
            "{ended:?}"
        );
    }

    #[test]
    fn each_dequeue_uses_the_current_pause_limit_and_waiter_order() {
        let two = Settings {
            paused: false,
            max_jobs: crate::domain::Concurrency::try_from(2).unwrap(),
        };
        assert_eq!(decide_admission(two, 1, None), Admission::Open);
        assert_eq!(decide_admission(two, 2, None), Admission::Full);
        assert_eq!(
            decide_admission(
                Settings {
                    paused: true,
                    ..two
                },
                0,
                None
            ),
            Admission::Paused
        );
        let predecessor = QueuePredecessor::Waiting(PathBuf::from("earlier.queue.lock"));
        assert_eq!(
            decide_admission(two, 0, Some(predecessor.clone())),
            Admission::EarlierWaiter(predecessor)
        );

        let three = Settings {
            max_jobs: crate::domain::Concurrency::try_from(3).unwrap(),
            ..two
        };
        assert_eq!(decide_admission(three, 2, None), Admission::Open);
    }

    #[test]
    fn queue_watcher_registration_caps_slots_and_replaces_a_changed_predecessor() {
        let mut watchers = QueueWatchers::new();
        for index in 0..crate::domain::Concurrency::MOST {
            assert_eq!(
                watchers.register_slot(Path::new(&format!("{index}.lock"))),
                WatchRegistration::Start
            );
        }
        assert_eq!(
            watchers.register_slot(Path::new("extra.lock")),
            WatchRegistration::Ignore
        );
        assert_eq!(
            watchers.register_slot(Path::new("0.lock")),
            WatchRegistration::Ignore
        );
        assert_eq!(
            watchers.register_earlier(Path::new("first.alive")),
            WatchRegistration::Start
        );
        assert_eq!(
            watchers.register_earlier(Path::new("second.alive")),
            WatchRegistration::Start
        );
        assert_eq!(
            watchers.register_earlier(Path::new("second.alive")),
            WatchRegistration::Ignore
        );
        watchers.released(Path::new("first.alive"));
        watchers.released(Path::new("0.lock"));
        assert_eq!(
            watchers.register_earlier(Path::new("second.alive")),
            WatchRegistration::Ignore
        );
        watchers.released(Path::new("second.alive"));
        assert_eq!(
            watchers.register_earlier(Path::new("third.alive")),
            WatchRegistration::Start
        );
        assert_eq!(
            watchers.register_slot(Path::new("extra.lock")),
            WatchRegistration::Start
        );
    }

    #[test]
    fn a_queued_job_watches_only_the_event_that_can_change_its_admission() {
        let held = [PathBuf::from("0.lock"), PathBuf::from("1.lock")];
        let earlier = PathBuf::from("first.queue.lock");
        for (decision, expected) in [
            (Admission::Paused, Vec::new()),
            (
                Admission::EarlierWaiter(QueuePredecessor::Waiting(earlier.clone())),
                vec![earlier],
            ),
            (Admission::Full, held.to_vec()),
            (Admission::Open, held.to_vec()),
        ] {
            let mut watchers = QueueWatchers::new();
            let mut started = Vec::new();
            watchers.observe(&decision, &held, |path| started.push(path));
            assert_eq!(started, expected, "{decision:?}");
        }
    }

    #[test]
    fn a_predecessor_wakes_its_successor_when_it_leaves_the_queue() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&Dirs::for_test(tmp.path())).unwrap();
        let id: JobId = "0EEEEEEEEEEEEEEE".parse().unwrap();
        let successor: JobId = "0FFFFFFFFFFFFFFF".parse().unwrap();
        let spec = owner_spec(id.clone(), Location::Home, 1);
        let later = Spec {
            id: successor,
            sequence: 2,
            ..spec.clone()
        };
        for staged in [&spec, &later] {
            store
                .stage(
                    staged,
                    (
                        &std::collections::BTreeMap::new(),
                        &crate::store::LaunchEnv::default(),
                    ),
                )
                .unwrap();
            store.publish(&staged.id).unwrap();
        }
        let alive = OsLock::exclusive(&store.alive_path(&id)).unwrap();
        let path = store.queue_wait_path(&id);
        let waiting = OsLock::exclusive(&path).unwrap();
        let before_admission = store.admission().unwrap();
        assert_eq!(
            before_admission.earliest_waiter(&later).unwrap(),
            Some(QueuePredecessor::Waiting(path.clone()))
        );
        before_admission.release().unwrap();
        let mut held = Held {
            waiting: Some(waiting),
            slot: None,
            workspace: None,
            store: store.clone(),
        };
        let (sender, events) = std::sync::mpsc::sync_channel(EVENT_CAPACITY);
        watch_lock_release(path.clone(), sender);
        let started_at = Timestamp::at_millis(1);
        held.begin_preparing(&id, started_at).unwrap();
        match events.recv().unwrap() {
            Event::Released(released) => assert_eq!(released, path),
            Event::Changed => panic!("unexpected queue change"),
            Event::LockFailed(error) => panic!("queue watch failed: {error}"),
            Event::Kill => panic!("unexpected kill event"),
        }
        assert_eq!(
            OsLock::probe(alive.path()).unwrap(),
            crate::lock::Probe::Held
        );
        assert_eq!(store.phase(&id).unwrap(), Phase::Preparing { started_at });
        let after_admission = store.admission().unwrap();
        assert_eq!(after_admission.earliest_waiter(&later).unwrap(), None);
        after_admission.release().unwrap();
        held.release().unwrap();
        alive.release().unwrap();
    }

    #[test]
    fn a_restarting_supervisor_waits_for_a_watcher_to_pass_the_queue_lock() {
        enum Claim {
            Alive,
            Done(Result<SupervisorLocks, NodeError>),
        }
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&Dirs::for_test(tmp.path())).unwrap();
        let id: JobId = "0GGGGGGGGGGGGGGG".parse().unwrap();
        let watcher = OsLock::exclusive(&store.queue_wait_path(&id)).unwrap();
        let (sent, received) = std::sync::mpsc::channel();
        let claiming = {
            let store = store.clone();
            let id = id.clone();
            std::thread::spawn(move || {
                let claimed = sent.clone();
                let result = claim_supervisor_locks(&store, &id, || {
                    claimed.send(Claim::Alive).unwrap();
                });
                sent.send(Claim::Done(result)).unwrap();
            })
        };
        match received.recv().unwrap() {
            Claim::Alive => {}
            Claim::Done(Ok(_)) => panic!("the supervisor did not claim the alive lock first"),
            Claim::Done(Err(error)) => panic!("claiming the supervisor failed: {error}"),
        }
        assert_eq!(
            OsLock::probe(&store.alive_path(&id)).unwrap(),
            crate::lock::Probe::Held
        );
        assert!(matches!(
            received.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        watcher.release().unwrap();
        let locks = match received.recv().unwrap() {
            Claim::Done(Ok(locks)) => locks,
            Claim::Done(Err(error)) => panic!("claiming the supervisor failed: {error}"),
            Claim::Alive => panic!("the supervisor claimed the alive lock twice"),
        };
        claiming.join().unwrap();
        locks.waiting.release().unwrap();
        locks.alive.release().unwrap();
    }

    #[test]
    fn warm_workspace_slots_stop_at_the_job_capacity_and_never_reuse_a_locked_slot() {
        let tmp = tempfile::tempdir().unwrap();
        let locks = tmp.path().join("locks");
        let limit = crate::domain::to_usize(crate::domain::Concurrency::MOST);
        let mut held: Vec<_> = (0..limit)
            .map(|index| OsLock::exclusive(&locks.join(format!("{index}.lock"))).unwrap())
            .collect();
        assert!(matches!(
            acquire_warm_slot(&locks),
            Err(NodeError::WorkspaceSlotsFull(full)) if full == limit
        ));
        assert!(matches!(
            std::fs::symlink_metadata(locks.join(format!("{limit}.lock"))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound
        ));
        held.remove(0).release().unwrap();
        let (slot, free) = acquire_warm_slot(&locks).unwrap();
        assert_eq!(slot, SlotIndex::FIRST);
        free.release().unwrap();
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

        let first = connections.admit().unwrap();
        let second = connections.admit().unwrap();
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

    #[test]
    fn local_control_admission_is_bounded_and_released_on_drop() {
        let connections = Arc::new(Connections::default());
        let open: Vec<_> = (0..MAX_CONTROL_CONNECTIONS)
            .map(|_| connections.admit().unwrap())
            .collect();
        assert!(connections.admit().is_none());
        drop(open);
        assert!(connections.admit().is_some());
    }
}
