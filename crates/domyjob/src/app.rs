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
    sender: mpsc::Sender<Wake>,
    receiver: mpsc::Receiver<Wake>,
}

impl CancellationWatch {
    fn start(store: &Store, job: &JobId) -> Result<Self, AppError> {
        let (sender, receiver) = mpsc::channel();
        let callback = sender.clone();
        let mut watcher = watch_event::watcher(move |notice| {
            let wake = match notice {
                Notice::Changed => Some(Wake::Changed),
                Notice::Unrelated => None,
                Notice::Failed(error) => Some(Wake::Broken(error.to_string())),
            };
            if let Some(wake) = wake {
                let _sent = callback.send(wake);
            }
        })?;
        watcher.watch(&store.watch_dir(job)?, notify::RecursiveMode::NonRecursive)?;
        Ok(Self {
            _watcher: watcher,
            sender,
            receiver,
        })
    }

    fn wait(self, store: &Store, job: &JobId, group: &Group) -> Result<Event, AppError> {
        let cancelled = AtomicBool::new(false);
        let status = std::thread::scope(|scope| -> Result<_, AppError> {
            let cancelled_by_request = &cancelled;
            let watch_thread = scope.spawn(move || -> Result<(), AppError> {
                loop {
                    match self.receiver.recv() {
                        Ok(Wake::Changed) => match store.cancel_requested(job) {
                            Ok(true) => {
                                cancelled_by_request.store(true, Ordering::Release);
                                group.kill()?;
                                return Ok(());
                            }
                            Ok(false) => {}
                            Err(error) => {
                                let _stopped = group.kill();
                                return Err(error.into());
                            }
                        },
                        Ok(Wake::Broken(message)) => {
                            let _stopped = group.kill();
                            return Err(AppError::Io(std::io::Error::other(message)));
                        }
                        Ok(Wake::Finished) => return Ok(()),
                        Err(_closed) => {
                            let _stopped = group.kill();
                            return Err(AppError::Io(std::io::Error::other(
                                "cancellation watcher disconnected",
                            )));
                        }
                    }
                }
            });
            let status = group.wait();
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

/// Writes stored output and remembers whether it stops inside a line.
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

/// Stores at most `limit` bytes of `output` in `log` and returns the bytes written and discarded.
/// Output past the limit is still read, so the job never blocks on a full pipe, and one final line counts it.
fn relay_output(output: impl Read, log: impl Write, limit: u64) -> std::io::Result<(u64, u64)> {
    let mut log = Log {
        file: log,
        open_line: false,
    };
    let mut kept = output.take(limit);
    let stored = std::io::copy(&mut kept, &mut log);
    // A failed log write must not cut the job off from its output, so draining continues and the failure is reported at the end.
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

/// Starts the job with standard output and standard error on one pipe, which a relay thread stores in `log`.
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
    // The command owns the only write ends and drops them once the job starts.
    // The relay ends at end of file, or when stopped after the process tree exits.
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

fn run_worker(job: &JobId, ready_event: Option<&ReadyToken>) -> Result<(), AppError> {
    let store = Store::open()?;
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
    let cancellation = CancellationWatch::start(&store, job)?;
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
            let completion = cancellation.wait(&store, job, &child)?;
            // The process tree is gone, so the relay stores what is already in the pipe and ends,
            // even when an escaped descendant still holds the write end.
            // Its failure is reported after the transition so that the job keeps its own outcome.
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
    let result = run_worker(job, event);
    if let Err(error) = &result
        && let Ok(store) = Store::open()
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
    use super::{relay_output, spawn_logged};
    use crate::process::{self, Stop as _};
    use crate::state_io as state_file;

    const MARKER: &str = "[domyjob: 3 bytes of output were discarded after the 256 MiB limit]\n";

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
        let combined = b"0123456789abcdefghij";
        let mut expected = Vec::new();
        let counts = relay_output(combined.as_slice(), &mut expected, 4).expect("reference relay");
        assert!(counts.1 > 0, "the command must write past the limit");
        let root = tempfile::tempdir().expect("temporary job directory");
        let path = root.path().join("job").join("output.log");
        let log = state_file::open_append(&path).expect("job log");
        let (_group, relay, _stop) =
            spawn_logged(process::stdout_then_stderr(), log, 4).expect("started job");
        // Nothing kills the job here, so the relay reaches EOF only if no write end outlives the job itself.
        let relayed = relay.join().expect("relay thread").expect("relayed output");
        assert_eq!(relayed, counts);
        assert_eq!(std::fs::read(&path).expect("stored log"), expected);
    }

    #[test]
    fn a_stopped_relay_keeps_written_output_even_when_a_writer_outlives_the_job() {
        if cfg!(windows) {
            // A Job Object ends every holder of the write end, so Windows needs no stop.
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
