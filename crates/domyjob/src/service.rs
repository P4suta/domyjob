use std::path::{Path, PathBuf};

use clap::{Args, Subcommand};
use serde::{Deserialize, Serialize};

use crate::layout::{self, State};
use crate::lock::{OsLock, Probe};
use crate::output::Output;
use crate::platform::{service as manager, user_files};
use crate::state_io;

#[derive(Debug, Args)]
pub(crate) struct ServiceArgs {
    #[command(subcommand)]
    command: Operation,
    #[arg(long, global = true, help = "Print the service state as JSON")]
    json: bool,
}

#[derive(Debug, Subcommand)]
enum Operation {
    #[command(about = "Install the macOS login service and remove owned chat integrations")]
    Install,
    #[command(about = "Stop the service and remove owned chat integrations")]
    Uninstall,
    #[command(about = "Inspect the service without changing configuration")]
    Status,
    #[command(hide = true)]
    Serve,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ServiceError {
    #[error(transparent)]
    Manager(#[from] manager::ServiceError),
    #[error(transparent)]
    File(#[from] user_files::UserFileError),
    #[error(transparent)]
    State(#[from] state_io::StateError),
    #[error(transparent)]
    Lock(#[from] crate::lock::LockError),
    #[error(transparent)]
    Retirement(#[from] crate::chat_retirement::RetirementError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("the service record is invalid: {0}")]
    Record(#[from] domyjob_core::ingress::JsonError),
    #[error("encoding the service state failed: {0}")]
    Encode(#[from] serde_json::Error),
    #[error("the service program path is not UTF-8")]
    Path,
    #[error("the login service is already running")]
    Running,
    #[error("the login service is only required on macOS")]
    NotRequired,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Installed {
    program: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
enum ServiceState {
    Missing,
    Installed,
    Running,
    UpdateRequired,
    NotRequired,
    Uninstalled,
}

#[derive(Debug, Serialize)]
struct Report {
    state: ServiceState,
    #[serde(skip_serializing_if = "Option::is_none")]
    program: Option<String>,
    changed_configurations: Vec<PathBuf>,
}

fn installed(paths: &layout::Service) -> Result<Option<Installed>, ServiceError> {
    state_io::read_bytes(&paths.service_record())?
        .map(|bytes| domyjob_core::ingress::json(&bytes, domyjob_core::wire::MAX_CONTROL_BYTES))
        .transpose()
        .map_err(ServiceError::from)
}

fn service_state(
    required: bool,
    program: Option<&str>,
    current: &Path,
    running: Probe,
) -> ServiceState {
    if !required {
        ServiceState::NotRequired
    } else if program.is_some_and(|program| Path::new(program) != current)
        || (program.is_none() && running == Probe::Held)
    {
        ServiceState::UpdateRequired
    } else if running == Probe::Held {
        ServiceState::Running
    } else if program.is_some() {
        ServiceState::Installed
    } else {
        ServiceState::Missing
    }
}

fn status(paths: &layout::Service) -> Result<Report, ServiceError> {
    let record = installed(paths)?;
    let program = record.map(|record| record.program);
    Ok(Report {
        state: service_state(
            cfg!(target_os = "macos"),
            program.as_deref(),
            &crate::platform::stable_program(&crate::identity::tag())?,
            OsLock::probe(&paths.service_lock())?,
        ),
        program,
        changed_configurations: Vec::new(),
    })
}

fn uninstall(paths: &layout::Service) -> Result<Report, ServiceError> {
    let record = installed(paths)?;
    manager::uninstall(
        record.as_ref().map(|record| Path::new(&record.program)),
        &paths.service_log(),
    )?;
    state_io::remove_file(&paths.service_record())?;
    Ok(Report {
        state: ServiceState::Uninstalled,
        program: None,
        changed_configurations: crate::chat_retirement::retire()?,
    })
}

fn install(paths: &layout::Service) -> Result<Report, ServiceError> {
    if !cfg!(target_os = "macos") {
        let mut report = uninstall(paths)?;
        report.state = ServiceState::NotRequired;
        return Ok(report);
    }
    let program = crate::platform::stable_program(&crate::identity::tag())?;
    user_files::install_executable(&std::env::current_exe()?, &program)?;
    crate::builds::pin(&program)?;
    let record = Installed {
        program: program.to_str().ok_or(ServiceError::Path)?.to_owned(),
    };
    let previous = installed(paths)?;
    manager::install(
        &program,
        previous
            .as_ref()
            .map(|previous_record| Path::new(&previous_record.program)),
        &paths.service_log(),
    )?;
    state_io::write_bytes(&paths.service_record(), &serde_json::to_vec(&record)?)?;
    let mut report = status(paths)?;
    report.changed_configurations = crate::chat_retirement::retire()?;
    Ok(report)
}

fn serve(paths: &layout::Service) -> Result<(), ServiceError> {
    if !cfg!(target_os = "macos") {
        return Err(ServiceError::NotRequired);
    }
    state_io::private_dir(paths.root())?;
    let _running = OsLock::try_exclusive(&paths.service_lock())?.ok_or(ServiceError::Running)?;
    crate::process::start_login_agent()?;
    eprintln!("domyjob: login-session listener ready");
    loop {
        std::thread::park();
    }
}

pub(crate) fn run(args: &ServiceArgs, output: &Output) -> Result<(), ServiceError> {
    let paths = State::here()?.service();
    let report = match args.command {
        Operation::Serve => return serve(&paths),
        Operation::Install => install(&paths)?,
        Operation::Uninstall => uninstall(&paths)?,
        Operation::Status => status(&paths)?,
    };
    if args.json {
        output.line(serde_json::to_string(&report)?)?;
    } else {
        let state = match report.state {
            ServiceState::Missing => {
                "not installed; run `domyjob service install` in the macOS login session"
            }
            ServiceState::Installed => "installed; waiting for the login service to start",
            ServiceState::Running => "running",
            ServiceState::UpdateRequired => {
                "an older build is installed; run `domyjob service install`"
            }
            ServiceState::NotRequired => {
                "no background service is required on this operating system"
            }
            ServiceState::Uninstalled => "uninstalled",
        };
        output.line(state)?;
        for path in report.changed_configurations {
            output.line(format!(
                "removed retired chat registration from {}",
                path.display()
            ))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Probe, ServiceState, service_state};
    use std::path::Path;

    #[test]
    fn a_stale_chat_service_cannot_report_a_current_login_service() {
        let current = Path::new("/current/domyjob");
        assert_eq!(
            service_state(true, Some("/previous/domyjob"), current, Probe::Held),
            ServiceState::UpdateRequired
        );
        assert_eq!(
            service_state(true, Some("/current/domyjob"), current, Probe::Free),
            ServiceState::Installed
        );
        assert_eq!(
            service_state(true, Some("/current/domyjob"), current, Probe::Held),
            ServiceState::Running
        );
        assert_eq!(
            service_state(true, None, current, Probe::Absent),
            ServiceState::Missing
        );
        assert_eq!(
            service_state(true, None, current, Probe::Held),
            ServiceState::UpdateRequired
        );
        assert_eq!(
            service_state(false, None, current, Probe::Absent),
            ServiceState::NotRequired
        );
    }
}
