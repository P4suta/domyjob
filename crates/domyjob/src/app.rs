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

fn spawn_logged(
    process: Process,
    log: File,
    limit: u64,
) -> Result<(Group, Relay, process::OutputStop), ProcessError> {
    let piping = |source: std::io::Error| ProcessError::Spawn {
        what: "the job output pipe",
        source,
    };
    let (reader, writer, stop) = process::output_pipe().map_err(piping)?;
    let errors = writer.try_clone().map_err(piping)?;
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
    let _launch = store.launch_lock(job)?;
    if store.status(job)?.kind() != PhaseKind::Accepted {
        return Ok(());
    }
    if let Err(error) = process::launch_worker(job) {
        store.finish_launch_failure(job, launch_failure_reason(&error)?)?;
    }
    Ok(())
}

pub(crate) fn handle(
    request: Request,
    archive: Option<&ReceivedArchive>,
    abandoned: &AtomicBool,
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
            let store = Store::open()?;
            let job = store.reserve(submission, &request, archive)?;
            start_worker(&store, &job)?;
            Ok(Reply::Accepted { job })
        }
        Request::List => Ok(Reply::Jobs {
            jobs: Store::open()?.list()?,
        }),
        Request::Status { job } => {
            let store = Store::open()?;
            start_worker(&store, &job)?;
            Ok(Reply::Status {
                state: store.status(&job)?,
            })
        }
        Request::Wait { job } => {
            let store = Store::open()?;
            start_worker(&store, &job)?;
            Ok(Reply::Status {
                state: store.wait(&job)?,
            })
        }
        Request::Logs { job } => {
            let (text, omitted) = Store::open()?.log_tail(&job)?;
            Ok(Reply::Logs { text, omitted })
        }
        Request::Kill { job } => {
            let store = Store::open()?;
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
            count: Store::open()?.clean(&target)?,
        }),
    }
}

fn run_worker_using(
    store: &Store,
    job: &JobId,
    ready_event: Option<&ReadyToken>,
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
    process::announce_ready(ready_event)?;
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
            stopped?;
            store.transition(job, &completion)?;
            relayed.map_err(|_panic| {
                AppError::Io(std::io::Error::other("the job output relay panicked"))
            })??;
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
        run_worker_using(&store, job, event, start_watch)
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
    use std::sync::mpsc;

    use domyjob_core::domain::{Command, JobId, SubmissionId};
    use domyjob_core::state::{Event, Outcome};
    use domyjob_core::wire::{Input, Request};

    use super::{AppError, CancellationWatch, Wake, relay_output, spawn_logged, worker_using};
    use crate::process::{self, ProcessError, Stop as _};
    use crate::state_io as state_file;
    use crate::store::{Store, StoreError};
    use crate::watch_event;

    const MARKER: &str = "[domyjob: 3 bytes of output were discarded after the 256 MiB limit]\n";

    fn requested_cancellation() -> (tempfile::TempDir, Store, JobId, CancellationWatch) {
        let temporary = tempfile::tempdir().expect("temporary state root");
        let store = Store::fixture(temporary.path().join("state")).expect("private store");
        let submission = SubmissionId::try_from("2".repeat(32)).expect("submission ID");
        let request = Request::Run {
            submission: submission.clone(),
            command: Command::try_from(vec!["controlled-job".to_owned()]).expect("command"),
            input: Input::Home,
        };
        let job = store.reserve(&submission, &request, None).expect("job");
        store.transition(&job, &Event::Starting).expect("starting");
        store
            .transition(&job, &Event::Spawned { pid: 42 })
            .expect("running");
        store.request_cancel(&job).expect("cancel request");
        let watcher = CancellationWatch::start_using(
            &store,
            &job,
            watch_event::watcher,
            |_watcher, _directory| Ok(()),
        )
        .expect("controlled watcher");
        watcher.sender.send(Wake::Changed).expect("cancel notice");
        (temporary, store, job, watcher)
    }

    fn successful_exit() -> std::process::ExitStatus {
        process::stdout_then_stderr()
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("completed command")
    }

    #[test]
    fn a_requested_cancellation_remains_killed_when_the_process_exits_successfully() {
        let (_temporary, store, job, watcher) = requested_cancellation();
        let status = successful_exit();
        let (stopped, observed) = mpsc::sync_channel(1);
        let completion = watcher
            .wait_using(
                (&store, &job),
                || {
                    observed.recv().expect("process was stopped");
                    Ok(status)
                },
                || {
                    stopped.send(()).expect("report the stop");
                    Ok(())
                },
            )
            .expect("cancellation completion");
        let state = store.transition(&job, &completion).expect("terminal state");
        assert!(matches!(state.outcome(), Some(Outcome::Killed)));
    }

    #[test]
    fn a_failed_cancellation_signal_preserves_its_process_error() {
        let (_temporary, store, job, watcher) = requested_cancellation();
        let status = successful_exit();
        let (stopped, observed) = mpsc::sync_channel(1);
        let error = watcher
            .wait_using(
                (&store, &job),
                || {
                    observed.recv().expect("signal was attempted");
                    Ok(status)
                },
                || {
                    stopped.send(()).expect("report the signal");
                    Err(ProcessError::Signal(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "signal refused",
                    )))
                },
            )
            .expect_err("signal error");
        assert!(
            matches!(error, AppError::Proc(ProcessError::Signal(ref cause))
                if cause.kind() == std::io::ErrorKind::PermissionDenied
                    && cause.to_string() == "signal refused"),
            "{error:?}"
        );
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
