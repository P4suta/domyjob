#![expect(
    clippy::disallowed_methods,
    reason = "this module owns detached supervisor and job process creation"
)]
#![expect(
    clippy::redundant_pub_crate,
    reason = "the binary composition root uses this private module"
)]

use std::io::Write;
use std::process::{Command as Process, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;

use domyjob_core::domain::{Command, JobId, RemoteText};
use domyjob_core::state::{Event, PhaseKind};
use domyjob_core::wire::{Input, Reply, Request};
use notify::Watcher;
use thiserror::Error;

use crate::identity;
use crate::platform;
use crate::store::{ReceivedArchive, Store, StoreError};
use crate::watch_event::{self, Notice};

#[derive(Debug, Error)]
pub(crate) enum AppError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Proc(#[from] domyjob::proc::ProcError),
    #[error(transparent)]
    OldDomain(#[from] domyjob::domain::Invalid),
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

    fn wait(
        self,
        store: &Store,
        job: &JobId,
        group: &domyjob::proc::Group,
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
    let mut process = Process::new(command.program());
    process.args(command.arguments());
    process.current_dir(home);
    process.stdin(Stdio::null());
    process
}

fn start_worker(store: &Store, job: &JobId) -> Result<(), AppError> {
    let _launch = store.launch_lock(job)?;
    if store.status(job)?.kind() != PhaseKind::Accepted {
        return Ok(());
    }
    let executable = domyjob::proc::Executable::current()?;
    let word = domyjob::domain::Nonce::try_from(job.as_str().to_owned())?;
    let invocation = domyjob::spawn::Invocation::new(
        domyjob::template::Arg::path(&executable),
        vec![
            domyjob::template::Arg::literal("worker"),
            domyjob::template::Arg::word(&word),
        ],
    );
    domyjob::proc::launch(&invocation)?;
    Ok(())
}

pub(crate) fn handle(
    request: Request,
    archive: Option<&ReceivedArchive>,
) -> Result<Reply, AppError> {
    match request {
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

fn run_worker(job: &JobId, ready_event: Option<&domyjob::domain::BlobId>) -> Result<(), AppError> {
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
    if let Some(readiness) = domyjob::proc::Readiness::from_parent(ready_event) {
        readiness.announce()?;
    }
    if store.cancel_requested(job)? {
        store.transition(job, &Event::Killed)?;
        return Ok(());
    }
    let directory = match input {
        Input::Home => platform::home()?,
        Input::Snapshot(_) => store.workspace(job)?,
    };
    let process = job_command(&command, &directory);
    let log = domyjob::state_file::open_append(&store.log_path(job)).map_err(StoreError::from)?;
    match domyjob::proc::Group::spawn_stdio(
        process,
        Stdio::from(log.try_clone()?),
        Stdio::from(log),
    ) {
        Ok(child) => {
            store.transition(job, &Event::Spawned { pid: child.id() })?;
            let completion = cancellation.wait(&store, job, &child)?;
            store.transition(job, &completion)?;
        }
        Err(error) => {
            let reason = RemoteText::try_from(error.to_string())
                .map_err(|_invalid| AppError::InvalidErrorText)?;
            store.transition(job, &Event::LaunchFailed { reason })?;
        }
    }
    Ok(())
}

pub(crate) fn worker(job: &JobId, event: Option<&domyjob::domain::BlobId>) -> Result<(), AppError> {
    let result = run_worker(job, event);
    if let Err(error) = &result
        && let Ok(store) = Store::open()
        && let Ok(path) = store.supervisor_log_path(job)
        && let Ok(mut file) = domyjob::state_file::open_append(&path)
    {
        let detail: String = error.to_string().chars().take(4096).collect();
        let _recorded = writeln!(file, "{detail}");
    }
    result
}
