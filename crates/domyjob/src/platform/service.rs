//! Installing the per-user chat service with each operating system's service manager.
//!
//! Every manager compiles on every system, and [`MANAGER`] picks this system's,
//! so each system type-checks the others' installers too.

use std::path::{Path, PathBuf};

use domyjob_core::service as render;

use crate::platform::user_files::{self, UserFileError};
use crate::process::{Tool, ToolError, run_tool};

#[derive(Debug, thiserror::Error)]
pub(crate) enum ServiceError {
    #[error("the service definition cannot represent this path: {0:?}")]
    Render(render::ServiceError),
    #[error(transparent)]
    File(#[from] UserFileError),
    #[error(transparent)]
    Tool(#[from] ToolError),
    #[error("the program path is not UTF-8")]
    Path,
    #[error("{command} failed: {detail}")]
    Failed { command: String, detail: String },
    #[error("the home directory is unknown: {0}")]
    Home(std::io::Error),
    #[error("this system does not name its user by a number, which {0} needs")]
    NoUserId(&'static str),
    #[error(
        "this operating system has no supported service manager; run `domyjob chat serve` yourself"
    )]
    Unsupported,
}

impl From<render::ServiceError> for ServiceError {
    fn from(error: render::ServiceError) -> Self {
        Self::Render(error)
    }
}

/// A per-user service manager.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Manager {
    Launchd,
    Systemd,
    TaskScheduler,
    Missing,
}

/// The service manager of the system this build runs on.
const MANAGER: Manager = if cfg!(target_os = "macos") {
    Manager::Launchd
} else if cfg!(target_os = "linux") {
    Manager::Systemd
} else if cfg!(windows) {
    Manager::TaskScheduler
} else {
    Manager::Missing
};

const LABEL: &str = "dev.domyjob.chat";
const UNIT: &str = "domyjob-chat.service";
const TASK: &str = "domyjob-chat";

/// Install the service that runs `program chat serve`, logging to `log` where the manager allows,
/// and start it; returns where the definition lives.
pub(crate) fn install(program: &Path, log: &Path) -> Result<PathBuf, ServiceError> {
    match MANAGER {
        Manager::Launchd => launchd_install(program, log),
        Manager::Systemd => systemd_install(program),
        Manager::TaskScheduler => task_install(program),
        Manager::Missing => Err(ServiceError::Unsupported),
    }
}

/// Stop and remove the service; returns whether a definition was removed.
pub(crate) fn uninstall() -> Result<bool, ServiceError> {
    match MANAGER {
        Manager::Launchd => launchd_uninstall(),
        Manager::Systemd => systemd_uninstall(),
        Manager::TaskScheduler => task_uninstall(),
        Manager::Missing => Err(ServiceError::Unsupported),
    }
}

fn text(path: &Path) -> Result<&str, ServiceError> {
    path.to_str().ok_or(ServiceError::Path)
}

fn owned(words: &[&str]) -> Vec<String> {
    words.iter().map(|word| (*word).to_owned()).collect()
}

/// Run a service-manager command; failures are errors unless `tolerated`.
fn run(tool: &Tool<'_>, tolerated: bool) -> Result<(), ServiceError> {
    let output = run_tool(tool)?;
    if output.success || tolerated {
        return Ok(());
    }
    Err(ServiceError::Failed {
        command: format!("{tool:?}"),
        detail: output.stderr.trim().to_owned(),
    })
}

fn step(program: &str, arguments: &[String], tolerated: bool) -> Result<(), ServiceError> {
    run(&Tool::new(program, arguments), tolerated)
}

fn home() -> Result<PathBuf, ServiceError> {
    crate::platform::home().map_err(ServiceError::Home)
}

fn launchd_definition() -> Result<PathBuf, ServiceError> {
    Ok(home()?.join(format!("Library/LaunchAgents/{LABEL}.plist")))
}

fn launchd_domain() -> Result<String, ServiceError> {
    let user = crate::platform::user_id().ok_or(ServiceError::NoUserId("launchd"))?;
    Ok(format!("gui/{user}"))
}

fn launchd_install(program: &Path, log: &Path) -> Result<PathBuf, ServiceError> {
    let path = launchd_definition()?;
    let plist = render::launchd_plist(LABEL, text(program)?, &["chat", "serve"], text(log)?)?;
    user_files::write(&path, plist.as_bytes())?;
    let domain = launchd_domain()?;
    let plist_path = text(&path)?.to_owned();
    step(
        "launchctl",
        &[String::from("bootout"), domain.clone(), plist_path.clone()],
        true,
    )?;
    step(
        "launchctl",
        &[String::from("bootstrap"), domain, plist_path],
        false,
    )?;
    Ok(path)
}

fn launchd_uninstall() -> Result<bool, ServiceError> {
    let path = launchd_definition()?;
    step(
        "launchctl",
        &[
            String::from("bootout"),
            launchd_domain()?,
            text(&path)?.to_owned(),
        ],
        true,
    )?;
    Ok(user_files::remove(&path)?)
}

fn systemd_definition() -> Result<PathBuf, ServiceError> {
    let config = match std::env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty()) {
        Some(config) => PathBuf::from(config),
        None => home()?.join(".config"),
    };
    Ok(config.join("systemd/user").join(UNIT))
}

fn systemctl(arguments: &[&str], tolerated: bool) -> Result<(), ServiceError> {
    let runtime = crate::platform::user_runtime_dir().ok_or(ServiceError::NoUserId("systemd"))?;
    let mut words = owned(&["--user"]);
    words.extend(owned(arguments));
    run(
        &Tool::new("systemctl", &words).env("XDG_RUNTIME_DIR", runtime),
        tolerated,
    )
}

fn systemd_install(program: &Path) -> Result<PathBuf, ServiceError> {
    let path = systemd_definition()?;
    let unit = render::systemd_unit("domyjob chat service", text(program)?, &["chat", "serve"])?;
    user_files::write(&path, unit.as_bytes())?;
    systemctl(&["daemon-reload"], false)?;
    systemctl(&["enable", UNIT], false)?;
    systemctl(&["restart", UNIT], false)?;
    step("loginctl", &owned(&["enable-linger"]), true)?;
    Ok(path)
}

fn systemd_uninstall() -> Result<bool, ServiceError> {
    systemctl(&["disable", "--now", UNIT], true)?;
    let removed = user_files::remove(&systemd_definition()?)?;
    systemctl(&["daemon-reload"], true)?;
    Ok(removed)
}

fn powershell(script: &str, tolerated: bool) -> Result<(), ServiceError> {
    let utf16: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    step(
        "powershell.exe",
        &[
            String::from("-NoProfile"),
            String::from("-NonInteractive"),
            String::from("-EncodedCommand"),
            data_encoding::BASE64.encode(&utf16),
        ],
        tolerated,
    )
}

fn task_install(program: &Path) -> Result<PathBuf, ServiceError> {
    let command = render::windows_task_command(text(program)?, &["chat", "serve"])?;
    let argument = format!("--headless {command}").replace('\'', "''");
    powershell(
        &format!(
            "$ErrorActionPreference='Stop'; \
$action = New-ScheduledTaskAction -Execute 'conhost.exe' -Argument '{argument}'; \
$trigger = New-ScheduledTaskTrigger -AtLogOn -User $env:USERNAME; \
$settings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit ([TimeSpan]::Zero) -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -MultipleInstances IgnoreNew -RestartCount 999 -RestartInterval (New-TimeSpan -Minutes 1); \
$principal = New-ScheduledTaskPrincipal -UserId $env:USERNAME -LogonType Interactive -RunLevel Limited; \
Register-ScheduledTask -TaskName '{TASK}' -Action $action -Trigger $trigger -Settings $settings -Principal $principal -Force | Out-Null; \
Start-ScheduledTask -TaskName '{TASK}'"
        ),
        false,
    )?;
    Ok(PathBuf::from(format!("scheduled task {TASK}")))
}

fn task_uninstall() -> Result<bool, ServiceError> {
    powershell(
        &format!(
            "Unregister-ScheduledTask -TaskName '{TASK}' -Confirm:$false -ErrorAction SilentlyContinue"
        ),
        true,
    )?;
    Ok(true)
}
