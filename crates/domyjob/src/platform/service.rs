//! Installing the per-user chat service with each operating system's service manager.

use std::path::{Path, PathBuf};

use domyjob_core::service as render;

use crate::platform::user_files::UserFileError;
use crate::process::{Tool, run_tool};

#[derive(Debug, thiserror::Error)]
pub(crate) enum ServiceError {
    #[error("the service definition cannot represent this path: {0:?}")]
    Render(render::ServiceError),
    #[error(transparent)]
    File(#[from] UserFileError),
    #[error(transparent)]
    Tool(#[from] crate::process::ToolError),
    #[error("the program path is not UTF-8")]
    Path,
    #[error("{command} failed: {detail}")]
    Failed { command: String, detail: String },
    #[cfg_attr(
        any(target_os = "macos", target_os = "linux", windows),
        expect(
            dead_code,
            reason = "only other systems lack a supported service manager"
        )
    )]
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

fn text(path: &Path) -> Result<&str, ServiceError> {
    path.to_str().ok_or(ServiceError::Path)
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

#[cfg(target_os = "linux")]
fn owned(words: &[&str]) -> Vec<String> {
    words.iter().map(|word| (*word).to_owned()).collect()
}

#[cfg(target_os = "macos")]
fn definition() -> Result<PathBuf, ServiceError> {
    Ok(crate::platform::home()
        .map_err(|error| ServiceError::Failed {
            command: "locating the home directory".to_owned(),
            detail: error.to_string(),
        })?
        .join("Library/LaunchAgents/dev.domyjob.chat.plist"))
}

#[cfg(target_os = "macos")]
fn domain() -> String {
    format!("gui/{}", rustix::process::getuid().as_raw())
}

#[cfg(target_os = "macos")]
pub(crate) fn install(program: &Path, log: &Path) -> Result<PathBuf, ServiceError> {
    let path = definition()?;
    let plist = render::launchd_plist(
        "dev.domyjob.chat",
        text(program)?,
        &["chat", "serve"],
        text(log)?,
    )?;
    crate::platform::user_files::write(&path, plist.as_bytes())?;
    let plist_path = text(&path)?.to_owned();
    step(
        "launchctl",
        &[String::from("bootout"), domain(), plist_path.clone()],
        true,
    )?;
    step(
        "launchctl",
        &[String::from("bootstrap"), domain(), plist_path],
        false,
    )?;
    Ok(path)
}

#[cfg(target_os = "macos")]
pub(crate) fn uninstall() -> Result<bool, ServiceError> {
    let path = definition()?;
    step(
        "launchctl",
        &[String::from("bootout"), domain(), text(&path)?.to_owned()],
        true,
    )?;
    Ok(crate::platform::user_files::remove(&path)?)
}

#[cfg(target_os = "linux")]
fn definition() -> Result<PathBuf, ServiceError> {
    let config = match std::env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty()) {
        Some(config) => PathBuf::from(config),
        None => crate::platform::home()
            .map_err(|error| ServiceError::Failed {
                command: "locating the home directory".to_owned(),
                detail: error.to_string(),
            })?
            .join(".config"),
    };
    Ok(config.join("systemd/user/domyjob-chat.service"))
}

#[cfg(target_os = "linux")]
fn systemctl(arguments: &[&str], tolerated: bool) -> Result<(), ServiceError> {
    let mut words = owned(&["--user"]);
    words.extend(owned(arguments));
    run(
        &Tool::new("systemctl", &words).env("XDG_RUNTIME_DIR", crate::platform::user_runtime_dir()),
        tolerated,
    )
}

#[cfg(target_os = "linux")]
pub(crate) fn install(program: &Path, _log: &Path) -> Result<PathBuf, ServiceError> {
    let path = definition()?;
    let unit = render::systemd_unit("domyjob chat service", text(program)?, &["chat", "serve"])?;
    crate::platform::user_files::write(&path, unit.as_bytes())?;
    systemctl(&["daemon-reload"], false)?;
    systemctl(&["enable", "domyjob-chat.service"], false)?;
    systemctl(&["restart", "domyjob-chat.service"], false)?;
    step("loginctl", &owned(&["enable-linger"]), true)?;
    Ok(path)
}

#[cfg(target_os = "linux")]
pub(crate) fn uninstall() -> Result<bool, ServiceError> {
    systemctl(&["disable", "--now", "domyjob-chat.service"], true)?;
    let removed = crate::platform::user_files::remove(&definition()?)?;
    systemctl(&["daemon-reload"], true)?;
    Ok(removed)
}

#[cfg(windows)]
const TASK: &str = "domyjob-chat";

#[cfg(windows)]
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

#[cfg(windows)]
pub(crate) fn install(program: &Path, _log: &Path) -> Result<PathBuf, ServiceError> {
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

#[cfg(windows)]
pub(crate) fn uninstall() -> Result<bool, ServiceError> {
    powershell(
        &format!(
            "Unregister-ScheduledTask -TaskName '{TASK}' -Confirm:$false -ErrorAction SilentlyContinue"
        ),
        true,
    )?;
    Ok(true)
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
pub(crate) fn install(_program: &Path, _log: &Path) -> Result<PathBuf, ServiceError> {
    Err(ServiceError::Unsupported)
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
pub(crate) fn uninstall() -> Result<bool, ServiceError> {
    Err(ServiceError::Unsupported)
}
