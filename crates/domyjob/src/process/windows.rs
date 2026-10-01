use std::io;
use std::process::{Child, Command};

mod kernel;

use super::{Guard, ProcessError, ReadyToken, Stop};
use kernel::{Event, Job, Process, RunningJob, StartWake};

fn event_name(token: &ReadyToken) -> String {
    format!("Local\\domyjob-ready-{}", token.as_str())
}

pub(super) fn announce_ready(token: Option<&ReadyToken>) -> Result<(), ProcessError> {
    let token = token.ok_or(ProcessError::InvalidReadyToken)?;
    let event = Event::open(&event_name(token)).map_err(ProcessError::Ready)?;
    event
        .signal()
        .map(|_success| ())
        .map_err(ProcessError::Ready)
}

pub(super) fn launch_worker(
    arguments: &[&str],
    errors: Option<std::fs::File>,
) -> Result<(), ProcessError> {
    launch_worker_using(arguments, errors, ReadyToken::fresh, std::env::current_exe)
}

fn launch_worker_using(
    arguments: &[&str],
    errors: Option<std::fs::File>,
    fresh_token: impl FnOnce() -> Result<ReadyToken, ProcessError>,
    executable: impl FnOnce() -> io::Result<std::path::PathBuf>,
) -> Result<(), ProcessError> {
    use windows_spawn::{CreationFlags, SpawnOptions, Stdio};

    let token = fresh_token()?;
    let event = Event::create(&event_name(&token)).map_err(ProcessError::Ready)?;
    let executable = executable().map_err(|source| ProcessError::Spawn {
        what: "the worker",
        source,
    })?;
    let mut command = super::raw::detached(&executable);
    command
        .args(arguments)
        .arg("--ready-event")
        .arg(token.as_str())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(errors.map_or_else(Stdio::null, Stdio::from));
    let detached = CreationFlags::NEW_PROCESS_GROUP | CreationFlags::BREAKAWAY_FROM_JOB;
    let child = match command.spawn_with(SpawnOptions::new().creation_flags(detached)) {
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
            command.spawn_with(SpawnOptions::new().creation_flags(CreationFlags::NEW_PROCESS_GROUP))
        }
        other => other,
    }
    .map_err(|source| ProcessError::Spawn {
        what: "the worker",
        source,
    })?;
    let process = Process::duplicate_worker(&child).map_err(ProcessError::Wait)?;
    drop(child);
    match event
        .as_wait()
        .wait_for_start(&process)
        .map_err(ProcessError::Wait)?
    {
        StartWake::Ready => Ok(()),
        StartWake::Exited => Err(ProcessError::NotStarted(exit_description(&process))),
    }
}

fn exit_description(process: &Process<kernel::Watch>) -> String {
    match process.exit_code() {
        Ok(code) => format!("exit code {code:#x}"),
        Err(_unavailable) => "unknown exit code".to_owned(),
    }
}

pub(super) fn isolate(command: &mut Command) {
    kernel::isolate(command);
}

#[derive(Debug)]
pub(super) struct Tree {
    job: RunningJob,
}

impl Tree {
    pub(super) fn adopt(child: &Child) -> io::Result<Self> {
        let job = Job::create()?
            .configure()?
            .assign(child)?
            .watch()?
            .resume()?;
        Ok(Self { job })
    }

    pub(super) fn await_leader(&self) -> Result<(), ProcessError> {
        self.job
            .await_leader()
            .map(|_success| ())
            .map_err(ProcessError::Wait)
    }

    pub(super) fn kill_all(&self) -> Result<(), ProcessError> {
        self.job
            .terminate()
            .map(|_success| ())
            .map_err(ProcessError::Signal)
    }

    pub(super) fn kill_remaining(&self) -> Result<(), ProcessError> {
        self.kill_all()
    }
}

#[derive(Debug)]
pub(super) struct Reaper;

impl Guard for Reaper {
    fn stand_guard(_tree: &Tree) -> Result<Self, ProcessError> {
        Ok(Self)
    }

    fn stand_down(self) {}

    fn reap(_group: i32) {}
}

pub(crate) type OutputReader = io::PipeReader;

#[derive(Debug)]
pub(crate) struct OutputStop;

pub(super) fn output_pipe() -> io::Result<(OutputReader, io::PipeWriter, OutputStop)> {
    let (reader, writer) = io::pipe()?;
    Ok((reader, writer, OutputStop))
}

impl Stop for OutputStop {
    fn stop(self) -> io::Result<()> {
        Ok(())
    }
}

pub(super) fn terminate(pid: u32) -> Result<(), ProcessError> {
    let status = super::command("taskkill.exe")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_err(ProcessError::Signal)?;
    if status.success() || status.code() == Some(128) {
        Ok(())
    } else {
        Err(ProcessError::Signal(io::Error::other(format!(
            "taskkill failed: {status}"
        ))))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Event, ProcessError, ReadyToken, announce_ready, event_name, launch_worker,
        launch_worker_using,
    };

    #[test]
    fn a_readiness_event_is_signalled_before_success_is_returned() {
        let token = ReadyToken::fresh().expect("owned readiness token");
        let event = Event::create(&event_name(&token)).expect("owned readiness event");
        announce_ready(Some(&token)).expect("signal the owned event");
        assert!(
            event
                .as_wait()
                .is_signalled()
                .expect("inspect the owned event")
        );
    }

    #[test]
    fn missing_entropy_stops_before_a_worker_is_prepared() {
        let failure = getrandom::Error::new_custom(73);
        let error = launch_worker_using(
            &[],
            None,
            || Err(ProcessError::Entropy(failure)),
            || panic!("an executable must not be requested without entropy"),
        )
        .expect_err("retain the entropy error");
        let ProcessError::Entropy(source) = error else {
            panic!("unexpected entropy failure: {error}");
        };
        assert_eq!(source, failure);
    }

    #[test]
    fn an_executable_lookup_error_keeps_the_worker_context() {
        let error = launch_worker_using(&[], None, ReadyToken::fresh, || {
            Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
        })
        .expect_err("retain the executable lookup error");
        let ProcessError::Spawn { what, source } = error else {
            panic!("unexpected executable lookup failure: {error}");
        };
        assert_eq!(what, "the worker");
        assert_eq!(source.kind(), std::io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn a_missing_executable_keeps_the_real_spawn_error() {
        let root = tempfile::tempdir().expect("owned executable directory");
        let missing = root.path().join("missing-worker.exe");
        let error = launch_worker_using(&[], None, ReadyToken::fresh, || Ok(missing))
            .expect_err("a missing executable cannot start");
        let ProcessError::Spawn { what, source } = error else {
            panic!("unexpected spawn failure: {error}");
        };
        assert_eq!(what, "the worker");
        assert_eq!(source.kind(), std::io::ErrorKind::NotFound);
    }

    #[test]
    fn missing_readiness_inputs_retain_their_errors() {
        assert!(matches!(
            announce_ready(None),
            Err(ProcessError::InvalidReadyToken)
        ));
        let token = ReadyToken::fresh().expect("unused readiness token");
        let error = announce_ready(Some(&token)).expect_err("the event has not been created");
        let ProcessError::Ready(source) = error else {
            panic!("unexpected readiness failure: {error}");
        };
        assert_eq!(source.kind(), std::io::ErrorKind::NotFound);
    }

    #[test]
    fn a_worker_that_exits_without_readiness_reports_its_exit_code() {
        let error = launch_worker(&["--help"], None).expect_err("help exits without readiness");
        let ProcessError::NotStarted(reason) = error else {
            panic!("unexpected worker startup failure: {error}");
        };
        assert!(reason.starts_with("exit code "), "{reason}");
    }
}
