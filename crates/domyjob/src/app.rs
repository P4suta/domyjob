use std::fmt::Display;
use std::fs::File;
use std::io::{Read, Write};
use std::process::{Command as Process, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;

use domyjob_core::domain::{Command, JobId, RemoteText};
use domyjob_core::state::{Event, PhaseKind};
use domyjob_core::wire::{Input, Reply, Request};
use notify::Watcher;
use thiserror::Error;

use crate::identity;
use crate::platform;
use crate::process::{self, Group, ProcessError, ReadyToken, Stop as _};
use crate::state_io as state_file;
use crate::store::{ReceivedArchive, Store, StoreError};
use crate::watch_event::{self, Notice};

const MAX_LOG_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Debug, Error)]
pub(crate) enum AppError {
    #[error(transparent)]
    Chat(Box<crate::chat::sync::SyncError>),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Proc(#[from] ProcessError),
    #[error(transparent)]
    Notify(#[from] notify::Error),
    #[error("starting or running the job failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("the command error could not be represented as remote text")]
    InvalidErrorText,
}

enum Wake {
    Changed,
    Broken(String),
    Finished,
}

struct CancellationWatch {
    _watcher: notify::RecommendedWatcher,
    sender: mpsc::SyncSender<Wake>,
    receiver: mpsc::Receiver<Wake>,
}

type WatchSignal = Box<dyn FnMut(Notice) + Send>;

impl CancellationWatch {
    fn start(store: &Store, job: &JobId) -> Result<Self, AppError> {
        Self::start_using(store, job, watch_event::watcher, |watcher, directory| {
            watcher.watch(directory, notify::RecursiveMode::NonRecursive)
        })
    }

    fn start_using(
        store: &Store,
        job: &JobId,
        create: impl FnOnce(WatchSignal) -> notify::Result<notify::RecommendedWatcher>,
        register: impl FnOnce(&mut notify::RecommendedWatcher, &std::path::Path) -> notify::Result<()>,
    ) -> Result<Self, AppError> {
        let (sender, receiver) = mpsc::sync_channel(1);
        let callback = sender.clone();
        let mut watcher = create(Box::new(move |notice| {
            let wake = match notice {
                Notice::Changed => Some(Wake::Changed),
                Notice::Unrelated => None,
                Notice::Failed(error) => Some(Wake::Broken(error.to_string())),
            };
            if let Some(wake) = wake {
                let _sent = callback.try_send(wake);
            }
        }))?;
        register(&mut watcher, &store.watch_dir(job)?)?;
        Ok(Self {
            _watcher: watcher,
            sender,
            receiver,
        })
    }

    fn wait(self, store: &Store, job: &JobId, group: &Group) -> Result<Event, AppError> {
        self.wait_using((store, job), || group.wait(), || group.kill())
    }

    fn wait_using(
        self,
        (store, job): (&Store, &JobId),
        wait: impl FnOnce() -> Result<std::process::ExitStatus, ProcessError>,
        kill: impl Fn() -> Result<(), ProcessError> + Send,
    ) -> Result<Event, AppError> {
        let cancelled = AtomicBool::new(false);
        let status = std::thread::scope(|scope| -> Result<_, AppError> {
            let cancelled_by_request = &cancelled;
            let watch_thread = scope.spawn(move || -> Result<(), AppError> {
                loop {
                    match self.receiver.recv() {
                        Ok(Wake::Changed) => match store.cancel_requested(job) {
                            Ok(true) => {
                                cancelled_by_request.store(true, Ordering::Release);
                                kill()?;
                                return Ok(());
                            }
                            Ok(false) => {}
                            Err(error) => {
                                let _stopped = kill();
                                return Err(error.into());
                            }
                        },
                        Ok(Wake::Broken(message)) => {
                            let _stopped = kill();
                            return Err(AppError::Io(std::io::Error::other(message)));
                        }
                        Ok(Wake::Finished) => return Ok(()),
                        Err(_closed) => {
                            let _stopped = kill();
                            return Err(AppError::Io(std::io::Error::other(
                                "cancellation watcher disconnected",
                            )));
                        }
                    }
                }
            });
            let status = wait();
            let _sent = self.sender.send(Wake::Finished);
            watch_thread.join().map_err(|_panic| {
                AppError::Io(std::io::Error::other("cancellation watcher panicked"))
            })??;
            Ok(status?)
        })?;
        Ok(if cancelled.load(Ordering::Acquire) {
            Event::Killed
        } else {
            Event::Exited {
                code: status.code().unwrap_or(-1),
            }
        })
    }
}

fn job_command(command: &Command, home: &std::path::Path) -> Process {
    let mut process = process::command(command.program());
    platform::prepare_job_environment(&mut process);
    process.args(command.arguments());
    process.current_dir(home);
    process.stdin(Stdio::null());
    process
}

#[derive(Debug)]
struct Log<W> {
    file: W,
    open_line: bool,
}

impl<W: Write> Write for Log<W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let count = self.file.write(bytes)?;
        if let Some(last) = bytes.get(..count).and_then(<[u8]>::last) {
            self.open_line = *last != b'\n';
        }
        Ok(count)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

fn relay_output(output: impl Read, log: impl Write, limit: u64) -> std::io::Result<(u64, u64)> {
    let mut log = Log {
        file: log,
        open_line: false,
    };
    let mut kept = output.take(limit);
    let stored = std::io::copy(&mut kept, &mut log);
    let discarded = std::io::copy(&mut kept.into_inner(), &mut std::io::sink())?;
    let written = stored?;
    if discarded != 0 {
        let separator = if log.open_line { "\n" } else { "" };
        writeln!(
            log,
            "{separator}[domyjob: {discarded} bytes of output were discarded after the 256 MiB limit]"
        )?;
    }
    Ok((written, discarded))
}

type Relay = JoinHandle<std::io::Result<(u64, u64)>>;
type RelayResult = std::thread::Result<std::io::Result<(u64, u64)>>;

fn finish_logged(
    (store, job): (&Store, &JobId),
    completion: &Event,
    (stopped, relayed): (std::io::Result<()>, RelayResult),
) -> Result<(), AppError> {
    stopped?;
    store.transition(job, completion)?;
    relayed
        .map_err(|_panic| AppError::Io(std::io::Error::other("the job output relay panicked")))??;
    Ok(())
}

fn spawn_logged(
    process: Process,
    log: File,
    limit: u64,
) -> Result<(Group, Relay, process::OutputStop), ProcessError> {
    spawn_logged_using(
        (process, log),
        limit,
        process::output_pipe,
        std::io::PipeWriter::try_clone,
    )
}

fn spawn_logged_using(
    (process, log): (Process, File),
    limit: u64,
    pipe: impl FnOnce() -> std::io::Result<(
        process::OutputReader,
        std::io::PipeWriter,
        process::OutputStop,
    )>,
    clone: impl FnOnce(&std::io::PipeWriter) -> std::io::Result<std::io::PipeWriter>,
) -> Result<(Group, Relay, process::OutputStop), ProcessError> {
    let piping = |source: std::io::Error| ProcessError::Spawn {
        what: "the job output pipe",
        source,
    };
    let (reader, writer, stop) = pipe().map_err(piping)?;
    let errors = clone(&writer).map_err(piping)?;
    let group = Group::spawn_stdio(process, Stdio::from(writer), Stdio::from(errors))?;
    Ok((
        group,
        std::thread::spawn(move || relay_output(reader, log, limit)),
        stop,
    ))
}

fn launch_failure_reason(error: &impl Display) -> Result<RemoteText, AppError> {
    let detail: String = error.to_string().chars().take(4096).collect();
    RemoteText::try_from(detail).map_err(|_invalid| AppError::InvalidErrorText)
}

fn start_worker(store: &Store, job: &JobId) -> Result<(), AppError> {
    start_worker_using(store, job, process::launch_worker)
}

fn start_worker_using(
    store: &Store,
    job: &JobId,
    launch: impl FnOnce(&JobId) -> Result<(), ProcessError>,
) -> Result<(), AppError> {
    let _launch = store.launch_lock(job)?;
    if store.status(job)?.kind() != PhaseKind::Accepted {
        return Ok(());
    }
    if let Err(error) = launch(job) {
        let reason = launch_failure_reason(&error)?;
        store.finish_launch_failure(job, reason)?;
    }
    Ok(())
}

pub(crate) fn handle(
    request: Request,
    archive: Option<&ReceivedArchive>,
    abandoned: &AtomicBool,
) -> Result<Reply, AppError> {
    handle_using((request, archive, abandoned), Store::open, start_worker)
}

fn handle_using(
    (request, archive, abandoned): (Request, Option<&ReceivedArchive>, &AtomicBool),
    open_store: impl Fn() -> Result<Store, StoreError>,
    start: impl Fn(&Store, &JobId) -> Result<(), AppError>,
) -> Result<Reply, AppError> {
    match request {
        Request::Chat(request) => {
            let chat = |error: crate::chat::sync::SyncError| AppError::Chat(Box::new(error));
            let store = crate::chat::store::Store::open().map_err(|error| chat(error.into()))?;
            Ok(Reply::Chat(
                crate::chat::sync::serve(&store, request, abandoned).map_err(chat)?,
            ))
        }
        Request::Hello => Ok(Reply::Hello {
            build: identity::current(),
        }),
        Request::Run { ref submission, .. } => {
            let store = open_store()?;
            let job = store.reserve(submission, &request, archive)?;
            start(&store, &job)?;
            Ok(Reply::Accepted { job })
        }
        Request::List => Ok(Reply::Jobs {
            jobs: open_store()?.list()?,
        }),
        Request::Status { job } => {
            let store = open_store()?;
            start(&store, &job)?;
            Ok(Reply::Status {
                state: store.status(&job)?,
            })
        }
        Request::Wait { job } => {
            let store = open_store()?;
            start(&store, &job)?;
            Ok(Reply::Status {
                state: store.wait(&job)?,
            })
        }
        Request::Logs { job } => {
            let (text, omitted) = open_store()?.log_tail(&job)?;
            Ok(Reply::Logs { text, omitted })
        }
        Request::Kill { job } => {
            let store = open_store()?;
            let _launch = store.launch_lock(&job)?;
            let state = store.status(&job)?;
            let stopped = match state.kind() {
                PhaseKind::Accepted => store.transition(&job, &Event::Killed)?,
                PhaseKind::Starting | PhaseKind::Running => {
                    store.request_cancel(&job)?;
                    store.wait(&job)?
                }
                PhaseKind::Finished => state,
            };
            Ok(Reply::Status { state: stopped })
        }
        Request::Clean { target } => Ok(Reply::Cleaned {
            count: open_store()?.clean(&target)?,
        }),
    }
}

fn run_worker_using(
    store: &Store,
    job: &JobId,
    (ready_event, announce): (
        Option<&ReadyToken>,
        impl FnOnce(Option<&ReadyToken>) -> Result<(), ProcessError>,
    ),
    start_watch: impl FnOnce(&Store, &JobId) -> Result<CancellationWatch, AppError>,
) -> Result<(), AppError> {
    let Some(_alive) = store.worker_lock(job)? else {
        return Ok(());
    };
    if store.status(job)?.kind() != PhaseKind::Accepted {
        return Ok(());
    }
    let request = store.request(job)?;
    let Request::Run { command, input, .. } = request else {
        return Err(AppError::Io(std::io::Error::other(
            "stored request is not a run",
        )));
    };
    let cancellation = start_watch(store, job)?;
    store.transition(job, &Event::Starting)?;
    announce(ready_event)?;
    if store.cancel_requested(job)? {
        store.transition(job, &Event::Killed)?;
        return Ok(());
    }
    let directory = match input {
        Input::Home => platform::home()?,
        Input::Snapshot(_) => store.workspace(job)?,
    };
    let process = job_command(&command, &directory);
    let log = state_file::open_append(&store.log_path(job)).map_err(StoreError::from)?;
    match spawn_logged(process, log, MAX_LOG_BYTES) {
        Ok((child, relay, stop)) => {
            store.transition(job, &Event::Spawned { pid: child.id() })?;
            let completion = cancellation.wait(store, job, &child)?;
            let stopped = stop.stop();
            let relayed = relay.join();
            finish_logged((store, job), &completion, (stopped, relayed))?;
        }
        Err(error) => {
            let reason = launch_failure_reason(&error)?;
            store.transition(job, &Event::LaunchFailed { reason })?;
        }
    }
    Ok(())
}

pub(crate) fn worker(job: &JobId, event: Option<&ReadyToken>) -> Result<(), AppError> {
    worker_using(job, event, Store::open, CancellationWatch::start)
}

fn worker_using(
    job: &JobId,
    event: Option<&ReadyToken>,
    open_store: impl Fn() -> Result<Store, StoreError>,
    start_watch: impl FnOnce(&Store, &JobId) -> Result<CancellationWatch, AppError>,
) -> Result<(), AppError> {
    let result = (|| {
        let store = open_store()?;
        run_worker_using(&store, job, (event, process::announce_ready), start_watch)
    })();
    if let Err(error) = &result
        && let Ok(store) = open_store()
    {
        if let Ok(path) = store.supervisor_log_path(job)
            && let Ok(mut file) = state_file::open_append(&path)
        {
            let detail: String = error.to_string().chars().take(4096).collect();
            let _recorded = writeln!(file, "{detail}");
        }
        if let Ok(reason) = launch_failure_reason(error) {
            let _recorded = store.finish_launch_failure(job, reason);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use std::process::Stdio;
    use std::sync::atomic::AtomicBool;
    use std::sync::mpsc;

    use domyjob_core::domain::{Command, JobId, SubmissionId};
    use domyjob_core::state::{Event, Outcome, PhaseKind};
    use domyjob_core::wire::{CleanTarget, Input, Reply, Request};

    use super::{
        AppError, CancellationWatch, Wake, finish_logged, handle_using, relay_output,
        run_worker_using, spawn_logged, spawn_logged_using, start_worker_using, worker_using,
    };
    use crate::process::{self, ProcessError, Stop as _};
    use crate::state_io as state_file;
    use crate::store::{Store, StoreError};
    use crate::watch_event;

    const MARKER: &str = "[domyjob: 3 bytes of output were discarded after the 256 MiB limit]\n";

    fn accepted_job() -> (tempfile::TempDir, Store, JobId) {
        accepted_job_using(|temporary| {
            Command::try_from(vec![
                temporary
                    .path()
                    .join("nonexistent-command")
                    .to_string_lossy()
                    .into_owned(),
            ])
            .expect("command")
        })
    }

    fn accepted_job_using(
        command: impl FnOnce(&tempfile::TempDir) -> Command,
    ) -> (tempfile::TempDir, Store, JobId) {
        let temporary = tempfile::tempdir().expect("temporary state root");
        let store = Store::fixture(temporary.path().join("state")).expect("private store");
        let submission = SubmissionId::try_from("2".repeat(32)).expect("submission ID");
        let request = Request::Run {
            submission: submission.clone(),
            command: command(&temporary),
            input: Input::Home,
        };
        let job = store.reserve(&submission, &request, None).expect("job");
        (temporary, store, job)
    }

    fn controlled_watcher(store: &Store, job: &JobId) -> Result<CancellationWatch, AppError> {
        CancellationWatch::start_using(store, job, watch_event::watcher, |_watcher, _directory| {
            Ok(())
        })
    }

    fn controlled_watch(cancel: bool) -> (tempfile::TempDir, Store, JobId, CancellationWatch) {
        let (temporary, store, job) = accepted_job();
        store.transition(&job, &Event::Starting).expect("starting");
        store
            .transition(&job, &Event::Spawned { pid: 42 })
            .expect("running");
        if cancel {
            store.request_cancel(&job).expect("cancel request");
        }
        let watcher = controlled_watcher(&store, &job).expect("controlled watcher");
        if cancel {
            watcher.sender.send(Wake::Changed).expect("cancel notice");
        }
        (temporary, store, job, watcher)
    }

    fn job_fixture_file(
        temporary: &tempfile::TempDir,
        job: &JobId,
        name: &str,
    ) -> std::path::PathBuf {
        temporary
            .path()
            .join("state/jobs")
            .join(job.as_str())
            .join(name)
    }

    fn assert_killed(store: &Store, job: &JobId) {
        let state = store.status(job).expect("job state");
        assert!(
            matches!(state.outcome(), Some(Outcome::Killed)),
            "{state:?}"
        );
    }

    fn successful_exit() -> std::process::ExitStatus {
        process::stdout_then_stderr()
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("completed command")
    }

    fn wait_for_cancellation(
        watcher: CancellationWatch,
        store: &Store,
        job: &JobId,
        signal: impl Fn() -> Result<(), ProcessError> + Send,
    ) -> Result<Event, AppError> {
        let status = successful_exit();
        let (stopped, observed) = mpsc::sync_channel(1);
        watcher.wait_using(
            (store, job),
            || {
                observed.recv().expect("signal was attempted");
                Ok(status)
            },
            move || {
                stopped.send(()).expect("report the signal");
                signal()
            },
        )
    }

    #[test]
    fn a_requested_cancellation_remains_killed_when_the_process_exits_successfully() {
        let (_temporary, store, job, watcher) = controlled_watch(true);
        let completion = wait_for_cancellation(watcher, &store, &job, || Ok(()))
            .expect("cancellation completion");
        let state = store.transition(&job, &completion).expect("terminal state");
        assert!(matches!(state.outcome(), Some(Outcome::Killed)));
    }

    #[test]
    fn a_failed_cancellation_signal_preserves_its_process_error() {
        let (_temporary, store, job, watcher) = controlled_watch(true);
        let error = wait_for_cancellation(watcher, &store, &job, || {
            Err(ProcessError::Signal(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "signal refused",
            )))
        })
        .expect_err("signal error");
        assert!(
            matches!(error, AppError::Proc(ProcessError::Signal(ref cause))
                if cause.kind() == std::io::ErrorKind::PermissionDenied
                    && cause.to_string() == "signal refused"),
            "{error:?}"
        );
    }

    #[test]
    fn a_failed_process_wait_is_returned_after_the_watcher_stops() {
        let (_temporary, store, job, watcher) = controlled_watch(false);
        let error = watcher
            .wait_using(
                (&store, &job),
                || Err(ProcessError::Wait(std::io::Error::other("wait refused"))),
                || panic!("a completed wait does not request a stop"),
            )
            .expect_err("wait error");
        assert!(
            matches!(error, AppError::Proc(ProcessError::Wait(ref cause))
                if cause.to_string() == "wait refused"),
            "{error:?}"
        );
    }

    #[test]
    fn a_panicked_cancellation_thread_becomes_a_diagnostic_error() {
        let (_temporary, store, job, watcher) = controlled_watch(true);
        let error = wait_for_cancellation(watcher, &store, &job, || {
            panic!("signal callback panicked");
        })
        .expect_err("watcher panic");
        assert!(
            matches!(error, AppError::Io(ref cause)
                if cause.to_string() == "cancellation watcher panicked"),
            "{error:?}"
        );
    }

    #[test]
    fn an_uncancelled_process_terminated_by_a_signal_has_no_exit_code() {
        let (mut command, supported) = process::signal_termination_fixture();
        if !supported {
            return;
        }
        let status = command.status().expect("signal-terminated child");
        let (_temporary, store, job, watcher) = controlled_watch(false);
        assert_eq!(status.code(), None);
        let completion = watcher
            .wait_using(
                (&store, &job),
                || Ok(status),
                || panic!("no cancellation was requested"),
            )
            .expect("process completion");
        let state = store.transition(&job, &completion).expect("terminal state");
        assert!(matches!(
            state.outcome(),
            Some(Outcome::Failed { code }) if code.get() == -1
        ));
    }

    #[test]
    fn cancellation_watch_returns_a_missing_job_before_registration() {
        let temporary = tempfile::tempdir().expect("temporary state root");
        let store = Store::fixture(temporary.path().join("state")).expect("private store");
        let job = JobId::try_from("0".repeat(32)).expect("job ID");
        let result = CancellationWatch::start_using(
            &store,
            &job,
            watch_event::watcher,
            |_watcher, _directory| panic!("a missing job cannot be registered"),
        );
        assert!(matches!(result, Err(AppError::Store(StoreError::Missing))));
    }

    #[test]
    fn failed_watcher_setup_finishes_the_job_and_keeps_its_diagnostic() {
        for creating in [true, false] {
            let temporary = tempfile::tempdir().expect("temporary state root");
            let root = temporary.path().join("state");
            let store = Store::fixture(root.clone()).expect("private store");
            let submission = SubmissionId::try_from("1".repeat(32)).expect("submission ID");
            let request = Request::Run {
                submission: submission.clone(),
                command: Command::try_from(vec!["never-launched".to_owned()]).expect("command"),
                input: Input::Home,
            };
            let job = store
                .reserve(&submission, &request, None)
                .expect("reserved job");
            let message = if creating {
                "watcher initialization failed"
            } else {
                "watch registration failed"
            };
            let error = worker_using(
                &job,
                None,
                || Store::fixture(root.clone()),
                |working_store, reserved_job| {
                    CancellationWatch::start_using(
                        working_store,
                        reserved_job,
                        |signal| {
                            if creating {
                                Err(notify::Error::generic(message))
                            } else {
                                watch_event::watcher(signal)
                            }
                        },
                        |_watcher, _directory| Err(notify::Error::generic(message)),
                    )
                },
            )
            .expect_err("watcher failure remains an error");
            assert!(matches!(error, AppError::Notify(_)));
            let state = store.wait(&job).expect("terminal job");
            let Some(Outcome::LaunchFailed { reason }) = state.outcome() else {
                panic!("watcher failure must finish admission: {state:?}");
            };
            assert!(reason.for_terminal().contains(message));
            let diagnostic = store.supervisor_log_path(&job).expect("supervisor log");
            assert!(crate::testing::read(&diagnostic).contains(message));
            assert_eq!(store.log_tail(&job).expect("job log").0.for_terminal(), "");
        }
    }

    #[test]
    fn output_past_the_limit_is_drained_counted_and_marked_on_its_own_line() {
        for (output, limit, counts, expected) in [
            ("", 4, (0, 0), String::new()),
            ("0123", 4, (4, 0), "0123".to_owned()),
            ("0123abc", 4, (4, 3), format!("0123\n{MARKER}")),
            ("012\nabc", 4, (4, 3), format!("012\n{MARKER}")),
            ("abc", 0, (0, 3), MARKER.to_owned()),
        ] {
            let mut log = Vec::new();
            let relayed = relay_output(output.as_bytes(), &mut log, limit).expect("relayed output");
            assert_eq!(relayed, counts, "{output:?}");
            assert_eq!(String::from_utf8(log).expect("text log"), expected);
        }
    }

    #[test]
    fn a_failed_log_write_still_drains_the_job_output() {
        let mut output: &[u8] = b"0123456789";
        let mut log = [0_u8; 4];
        let error = relay_output(&mut output, log.as_mut_slice(), 8).expect_err("full log");
        assert_eq!(error.kind(), std::io::ErrorKind::WriteZero);
        assert!(output.is_empty());
        assert_eq!(&log, b"0123");
    }

    struct FailedLog {
        remaining: usize,
        written: Vec<u8>,
    }

    impl std::io::Write for FailedLog {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if self.remaining == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "log refused",
                ));
            }
            let count = bytes.len().min(self.remaining);
            self.written
                .extend_from_slice(bytes.get(..count).expect("bounded write"));
            self.remaining = self.remaining.checked_sub(count).expect("write budget");
            Ok(count)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn errors_writing_output_or_its_discard_marker_are_preserved() {
        for remaining in [0, 4] {
            let mut output: &[u8] = b"0123abc";
            let mut log = FailedLog {
                remaining,
                written: Vec::new(),
            };
            let error = relay_output(&mut output, &mut log, 4).expect_err("log error");
            assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
            assert_eq!(error.to_string(), "log refused");
            assert!(output.is_empty(), "failed logging must still drain output");
            assert_eq!(log.written, b"0123".get(..remaining).expect("kept prefix"));
        }
    }

    struct FailedTail<'a>(&'a [u8]);

    impl std::io::Read for FailedTail<'_> {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            if self.0.is_empty() {
                Err(std::io::Error::other("discarded output refused"))
            } else {
                std::io::Read::read(&mut self.0, buffer)
            }
        }
    }

    #[test]
    fn a_failed_read_after_the_output_limit_preserves_the_kept_log() {
        let mut log = Vec::new();
        let error = relay_output(FailedTail(b"0123"), &mut log, 4).expect_err("drain error");
        assert_eq!(error.to_string(), "discarded output refused");
        assert_eq!(log, b"0123");
    }

    #[test]
    fn failed_pipe_creation_or_cloning_returns_its_spawn_error() {
        for creating in [true, false] {
            let temporary = tempfile::tempdir().expect("temporary job directory");
            let log = state_file::open_append(&temporary.path().join("job").join("output.log"))
                .expect("job log");
            let refused = || std::io::Error::other("pipe refused");
            let result = spawn_logged_using(
                (process::stdout_then_stderr(), log),
                4,
                || {
                    if creating {
                        Err(refused())
                    } else {
                        process::output_pipe()
                    }
                },
                |_writer| Err(refused()),
            );
            assert!(
                matches!(result, Err(ProcessError::Spawn { what: "the job output pipe", ref source })
                    if source.to_string() == "pipe refused"),
                "pipe failure must remain a typed spawn error"
            );
        }
    }

    #[test]
    fn a_command_that_cannot_start_returns_a_spawn_error_without_a_relay() {
        let temporary = tempfile::tempdir().expect("temporary job directory");
        let log = state_file::open_append(&temporary.path().join("job").join("output.log"))
            .expect("job log");
        let result = spawn_logged(process::command(""), log, 4);
        assert!(matches!(result, Err(ProcessError::Spawn { .. })));
    }

    #[test]
    fn a_missing_job_cannot_start_a_worker() {
        let temporary = tempfile::tempdir().expect("temporary state root");
        let store = Store::fixture(temporary.path().join("state")).expect("private store");
        let job = JobId::try_from("0".repeat(32)).expect("job ID");
        let result = start_worker_using(&store, &job, |_job| {
            panic!("a missing job cannot launch a worker");
        });
        assert!(matches!(result, Err(AppError::Store(StoreError::Missing))));
    }

    #[test]
    fn a_finished_job_cannot_launch_another_worker() {
        let (_temporary, store, job, _watcher) = controlled_watch(false);
        store
            .transition(&job, &Event::Killed)
            .expect("finished job");
        start_worker_using(&store, &job, |_job| {
            panic!("a finished job cannot launch a worker");
        })
        .expect("finished jobs need no worker");
    }

    fn job_queries(job: &JobId) -> [Request; 5] {
        [
            Request::Status { job: job.clone() },
            Request::Wait { job: job.clone() },
            Request::Logs { job: job.clone() },
            Request::Kill { job: job.clone() },
            Request::Clean {
                target: CleanTarget::Job(job.clone()),
            },
        ]
    }

    #[test]
    fn every_job_request_preserves_a_store_open_failure() {
        let job = JobId::try_from("0".repeat(32)).expect("job ID");
        let submission = SubmissionId::try_from("0".repeat(32)).expect("submission ID");
        let requests = [
            Request::Run {
                submission,
                command: Command::try_from(vec!["never-launched".to_owned()]).expect("command"),
                input: Input::Home,
            },
            Request::List,
        ]
        .into_iter()
        .chain(job_queries(&job));
        let abandoned = AtomicBool::new(false);
        for request in requests {
            let error = handle_using(
                (request, None, &abandoned),
                || {
                    Err(StoreError::Io(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "state root refused",
                    )))
                },
                |_store, _job| panic!("an unopened store cannot start a worker"),
            )
            .expect_err("store error");
            assert!(
                matches!(error, AppError::Store(StoreError::Io(ref cause))
                    if cause.kind() == std::io::ErrorKind::PermissionDenied
                        && cause.to_string() == "state root refused"),
                "{error:?}"
            );
        }
    }

    #[test]
    fn requests_for_unknown_jobs_preserve_the_missing_job_error() {
        let temporary = tempfile::tempdir().expect("temporary state root");
        let root = temporary.path().join("state");
        let job = JobId::try_from("0".repeat(32)).expect("job ID");
        let abandoned = AtomicBool::new(false);
        for request in job_queries(&job) {
            let result = handle_using(
                (request, None, &abandoned),
                || Store::fixture(root.clone()),
                super::start_worker,
            );
            assert!(matches!(result, Err(AppError::Store(StoreError::Missing))));
        }
    }

    #[test]
    fn a_conflicting_submission_cannot_report_acceptance_or_start_a_worker() {
        let (temporary, _, _, _) = controlled_watch(false);
        let request = Request::Run {
            submission: SubmissionId::try_from("2".repeat(32)).expect("submission ID"),
            command: Command::try_from(vec!["another-command".to_owned()]).expect("command"),
            input: Input::Home,
        };
        let abandoned = AtomicBool::new(false);
        let result = handle_using(
            (request, None, &abandoned),
            || Store::fixture(temporary.path().join("state")),
            |_store, _job| panic!("a conflicting request cannot start a worker"),
        );
        assert!(matches!(result, Err(AppError::Store(StoreError::Conflict))));
    }

    #[test]
    fn a_startup_failure_cannot_report_an_accepted_job() {
        let temporary = tempfile::tempdir().expect("temporary state root");
        let request = Request::Run {
            submission: SubmissionId::try_from("3".repeat(32)).expect("submission ID"),
            command: Command::try_from(vec!["never-launched".to_owned()]).expect("command"),
            input: Input::Home,
        };
        let abandoned = AtomicBool::new(false);
        let error = handle_using(
            (request, None, &abandoned),
            || Store::fixture(temporary.path().join("state")),
            |_store, _job| Err(ProcessError::NotStarted("startup refused".to_owned()).into()),
        )
        .expect_err("startup error");
        assert!(
            matches!(error, AppError::Proc(ProcessError::NotStarted(ref cause))
            if cause == "startup refused")
        );
    }

    #[test]
    fn a_job_cleaned_after_startup_cannot_report_status_or_completion() {
        for waiting in [false, true] {
            let (temporary, store, job, _watcher) = controlled_watch(false);
            store
                .transition(&job, &Event::Killed)
                .expect("finished job");
            let request = if waiting {
                Request::Wait { job }
            } else {
                Request::Status { job }
            };
            let abandoned = AtomicBool::new(false);
            let result = handle_using(
                (request, None, &abandoned),
                || Store::fixture(temporary.path().join("state")),
                |working_store, requested_job| {
                    super::start_worker(working_store, requested_job)?;
                    working_store.clean(&CleanTarget::Job(requested_job.clone()))?;
                    Ok(())
                },
            );
            assert!(matches!(result, Err(AppError::Store(StoreError::Missing))));
        }
    }

    #[test]
    fn a_hello_reply_does_not_require_a_store_or_a_worker() {
        let abandoned = AtomicBool::new(false);
        let reply = handle_using(
            (Request::Hello, None, &abandoned),
            || panic!("a hello does not open the store"),
            |_store, _job| panic!("a hello does not start a worker"),
        )
        .expect("hello reply");
        assert!(matches!(reply, Reply::Hello { .. }));
    }

    #[test]
    fn killing_a_job_with_a_missing_state_preserves_the_record_error() {
        let (temporary, store, job, _watcher) = controlled_watch(false);
        crate::testing::remove(&job_fixture_file(&temporary, &job, "state.json"));
        let abandoned = AtomicBool::new(false);
        let result = handle_using(
            (Request::Kill { job }, None, &abandoned),
            || Ok(store.clone()),
            super::start_worker,
        );
        assert!(matches!(result, Err(AppError::Store(StoreError::Missing))));
    }

    #[test]
    fn listing_a_store_with_an_invalid_job_entry_preserves_the_record_error() {
        let temporary = tempfile::tempdir().expect("temporary state root");
        let root = temporary.path().join("state");
        let store = Store::fixture(root.clone()).expect("private store");
        crate::testing::mkdir(&root.join("jobs/invalid"));
        let abandoned = AtomicBool::new(false);
        let result = handle_using(
            (Request::List, None, &abandoned),
            || Ok(store.clone()),
            |_store, _job| panic!("listing jobs cannot start a worker"),
        );
        assert!(matches!(result, Err(AppError::Store(StoreError::Corrupt))));
    }

    #[test]
    fn cleaning_an_active_job_preserves_the_active_job_error() {
        let (_temporary, store, job, _watcher) = controlled_watch(false);
        let _alive = store.worker_lock(&job).expect("worker lock");
        let abandoned = AtomicBool::new(false);
        let result = handle_using(
            (
                Request::Clean {
                    target: CleanTarget::Job(job.clone()),
                },
                None,
                &abandoned,
            ),
            || Ok(store.clone()),
            |_store, _job| panic!("cleaning cannot start a worker"),
        );
        assert!(matches!(result, Err(AppError::Store(StoreError::Active))));
        assert!(!store.cancel_requested(&job).expect("cancellation marker"));
    }

    #[test]
    fn a_missing_worker_job_fails_before_starting_a_watcher() {
        let temporary = tempfile::tempdir().expect("temporary state root");
        let store = Store::fixture(temporary.path().join("state")).expect("private store");
        let job = JobId::try_from("0".repeat(32)).expect("job ID");
        let result = run_worker_using(
            &store,
            &job,
            (None, process::announce_ready),
            |_store, _job| {
                panic!("a missing worker job cannot start a watcher");
            },
        );
        assert!(matches!(result, Err(AppError::Store(StoreError::Missing))));
    }

    #[test]
    fn a_finished_worker_job_does_not_start_a_watcher_or_a_command() {
        let (_temporary, store, job) = accepted_job();
        store
            .transition(&job, &Event::Killed)
            .expect("finished job");
        run_worker_using(
            &store,
            &job,
            (None, process::announce_ready),
            |_store, _job| {
                panic!("a finished worker job cannot start a watcher");
            },
        )
        .expect("nothing remains to launch");
        assert_killed(&store, &job);
    }

    #[test]
    fn a_missing_stored_request_preserves_the_record_error() {
        let (temporary, store, job) = accepted_job();
        crate::testing::remove(&job_fixture_file(&temporary, &job, "request.json"));
        let result = run_worker_using(
            &store,
            &job,
            (None, process::announce_ready),
            |_store, _job| {
                panic!("a missing request cannot start a watcher");
            },
        );
        assert!(matches!(result, Err(AppError::Store(StoreError::Missing))));
    }

    #[test]
    fn a_missing_state_cannot_start_a_worker_or_a_command() {
        let (temporary, store, job) = accepted_job();
        crate::testing::remove(&job_fixture_file(&temporary, &job, "state.json"));
        let started = start_worker_using(&store, &job, |_job| {
            panic!("an incomplete record cannot start a worker");
        });
        assert!(matches!(started, Err(AppError::Store(StoreError::Missing))));
        let running = run_worker_using(
            &store,
            &job,
            (None, process::announce_ready),
            |_store, _job| {
                panic!("an incomplete record cannot start a watcher");
            },
        );
        assert!(matches!(running, Err(AppError::Store(StoreError::Missing))));
    }

    #[test]
    fn a_stored_non_run_request_is_rejected_before_a_command_starts() {
        let (temporary, store, job) = accepted_job();
        let encoded = domyjob_core::wire::frame(&Request::Hello).expect("encoded hello");
        let payload = domyjob_core::wire::payload(&encoded).expect("hello payload");
        state_file::write_bytes(&job_fixture_file(&temporary, &job, "request.json"), payload)
            .expect("replaced request fixture");
        let error = run_worker_using(
            &store,
            &job,
            (None, process::announce_ready),
            |_store, _job| {
                panic!("a non-run request cannot start a watcher");
            },
        )
        .expect_err("invalid stored request");
        assert!(
            matches!(error, AppError::Io(ref cause) if cause.to_string() == "stored request is not a run")
        );
    }

    #[test]
    fn cancellation_before_command_launch_finishes_the_job_as_killed() {
        let (_temporary, store, job) = accepted_job();
        store.request_cancel(&job).expect("cancel request");
        run_worker_using(
            &store,
            &job,
            (None, process::announce_ready),
            controlled_watcher,
        )
        .expect("cancelled worker");
        assert_killed(&store, &job);
        assert_eq!(store.log_tail(&job).expect("job log").0.for_terminal(), "");
    }

    #[test]
    fn cancellation_while_a_watcher_starts_preserves_the_transition_error() {
        let (_temporary, store, job) = accepted_job();
        let result = run_worker_using(
            &store,
            &job,
            (None, process::announce_ready),
            |working_store, watched_job| {
                let watcher = controlled_watcher(working_store, watched_job)?;
                working_store.transition(watched_job, &Event::Killed)?;
                Ok(watcher)
            },
        );
        assert!(matches!(
            result,
            Err(AppError::Store(StoreError::Transition(_)))
        ));
        assert_killed(&store, &job);
    }

    #[test]
    fn a_cancel_marker_with_the_wrong_file_kind_preserves_its_state_error() {
        let (temporary, store, job) = accepted_job();
        crate::testing::mkdir(&job_fixture_file(&temporary, &job, "cancel"));
        let result = run_worker_using(
            &store,
            &job,
            (None, process::announce_ready),
            controlled_watcher,
        );
        assert!(matches!(result, Err(AppError::Store(StoreError::State(_)))));
    }

    #[test]
    fn a_log_that_cannot_be_opened_prevents_command_launch() {
        let (_temporary, store, job) = accepted_job();
        let path = store.log_path(&job);
        state_file::write_bytes(&path, b"").expect("empty log fixture");
        let permissions = crate::testing::protect(&path);
        let result = run_worker_using(
            &store,
            &job,
            (None, process::announce_ready),
            controlled_watcher,
        );
        crate::testing::restore(&path, permissions);
        assert!(matches!(result, Err(AppError::Store(StoreError::State(_)))));
    }

    #[test]
    fn failed_readiness_prevents_command_launch_and_preserves_its_error() {
        let (_temporary, store, job) = accepted_job();
        let error = run_worker_using(
            &store,
            &job,
            (None, |_event| {
                Err(ProcessError::Ready(std::io::Error::other(
                    "readiness refused",
                )))
            }),
            controlled_watcher,
        )
        .expect_err("readiness error");
        assert!(
            matches!(error, AppError::Proc(ProcessError::Ready(ref cause))
            if cause.to_string() == "readiness refused")
        );
        assert_eq!(store.log_tail(&job).expect("job log").0.for_terminal(), "");
    }

    #[test]
    fn a_broken_cancellation_watch_preserves_its_error_after_a_real_command() {
        let (_temporary, store, job) = accepted_job_using(|_temporary| {
            let command = process::stdout_then_stderr();
            let words = std::iter::once(command.get_program())
                .chain(command.get_args())
                .map(|word| word.to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            Command::try_from(words).expect("real command")
        });
        let error = run_worker_using(
            &store,
            &job,
            (None, process::announce_ready),
            |working_store, watched_job| {
                let watcher = controlled_watcher(working_store, watched_job)?;
                watcher
                    .sender
                    .send(Wake::Broken("watch refused".to_owned()))
                    .expect("failure notice");
                Ok(watcher)
            },
        )
        .expect_err("watch error");
        assert!(matches!(error, AppError::Io(ref cause) if cause.to_string() == "watch refused"));
    }

    #[test]
    fn failed_output_preserves_the_completed_process_state_and_its_error() {
        for panicked in [false, true] {
            let (_temporary, store, job, _watcher) = controlled_watch(false);
            let relayed = std::thread::spawn(move || -> std::io::Result<(u64, u64)> {
                assert!(!panicked, "output relay failed");
                Err(std::io::Error::new(
                    std::io::ErrorKind::StorageFull,
                    "log disk full",
                ))
            })
            .join();
            let error = finish_logged(
                (&store, &job),
                &Event::Exited { code: 0 },
                (Ok(()), relayed),
            )
            .expect_err("output error");
            let expected = if panicked {
                "the job output relay panicked"
            } else {
                "log disk full"
            };
            assert!(
                matches!(error, AppError::Io(ref cause) if cause.to_string() == expected),
                "{error:?}"
            );
            assert!(matches!(
                store.status(&job).expect("terminal state").outcome(),
                Some(Outcome::Succeeded)
            ));
        }
    }

    #[test]
    fn failed_relay_shutdown_cannot_commit_process_completion() {
        let (_temporary, store, job, _watcher) = controlled_watch(false);
        let _alive = store.worker_lock(&job).expect("worker lock");
        let error = finish_logged(
            (&store, &job),
            &Event::Exited { code: 0 },
            (
                Err(std::io::Error::other("shutdown refused")),
                Ok(Ok((0, 0))),
            ),
        )
        .expect_err("shutdown error");
        assert!(
            matches!(error, AppError::Io(ref cause) if cause.to_string() == "shutdown refused")
        );
        assert_eq!(
            store.status(&job).expect("job state").kind(),
            PhaseKind::Running
        );
    }

    #[test]
    fn a_missing_state_preserves_the_completion_publication_error() {
        let (temporary, store, job, _watcher) = controlled_watch(false);
        crate::testing::remove(&job_fixture_file(&temporary, &job, "state.json"));
        let result = finish_logged(
            (&store, &job),
            &Event::Exited { code: 0 },
            (Ok(()), Ok(Ok((0, 0)))),
        );
        assert!(matches!(result, Err(AppError::Store(StoreError::Missing))));
    }

    #[test]
    fn a_missing_state_preserves_the_worker_launch_failure_publication_error() {
        let (temporary, store, job) = accepted_job();
        let result = start_worker_using(&store, &job, |_job| {
            crate::testing::remove(&job_fixture_file(&temporary, &job, "state.json"));
            Err(ProcessError::NotStarted("worker refused".to_owned()))
        });
        assert!(matches!(result, Err(AppError::Store(StoreError::Missing))));
    }

    #[test]
    fn a_worker_preserves_the_store_open_error_when_diagnostics_cannot_open_it() {
        let job =
            JobId::try_from("00000000000000000000000000000001".to_owned()).expect("job identity");
        let result = worker_using(
            &job,
            None,
            || Err(StoreError::Io(std::io::Error::other("store unavailable"))),
            controlled_watcher,
        );
        assert!(
            matches!(result, Err(AppError::Store(StoreError::Io(ref cause)))
            if cause.to_string() == "store unavailable")
        );
    }

    #[test]
    fn a_real_job_logs_both_streams_through_one_bounded_pipe() {
        let mut expected = Vec::new();
        let counts =
            relay_output(process::STDOUT_THEN_STDERR, &mut expected, 4).expect("reference relay");
        assert!(counts.1 > 0, "the command must write past the limit");
        let root = tempfile::tempdir().expect("temporary job directory");
        let path = root.path().join("job").join("output.log");
        let log = state_file::open_append(&path).expect("job log");
        let (_group, relay, _stop) =
            spawn_logged(process::stdout_then_stderr(), log, 4).expect("started job");
        let relayed = relay.join().expect("relay thread").expect("relayed output");
        assert_eq!(relayed, counts);
        assert_eq!(crate::testing::read(&path).into_bytes(), expected);
    }

    #[test]
    fn stopping_a_relay_that_already_reached_the_end_succeeds() {
        let (reader, writer, stop) = process::output_pipe().expect("output pipe");
        drop(writer);
        let mut stored = Vec::new();
        relay_output(reader, &mut stored, 4).expect("relay to the end");
        stop.stop().expect("nothing is left to stop");
    }

    #[test]
    fn a_stopped_relay_keeps_written_output_even_when_a_writer_outlives_the_job() {
        if cfg!(windows) {
            return;
        }
        let (reader, writer, stop) = process::output_pipe().expect("output pipe");
        let mut escaped = writer.try_clone().expect("escaped descendant's copy");
        std::io::Write::write_all(&mut escaped, b"before the job ended\n").expect("write");
        drop(writer);
        let relay = std::thread::spawn(move || {
            let mut stored = Vec::new();
            relay_output(reader, &mut stored, 1024).map(|counts| (counts, stored))
        });
        stop.stop().expect("stop the relay");
        let (counts, stored) = relay.join().expect("relay thread").expect("relayed output");
        assert_eq!(stored, b"before the job ended\n");
        assert_eq!(counts, (21, 0));
        drop(escaped);
    }
}
