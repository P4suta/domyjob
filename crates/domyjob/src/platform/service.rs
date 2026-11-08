use std::path::{Path, PathBuf};

use domyjob_core::service as render;

use crate::platform::user_files::{self, UserFileError};
use crate::process::{Tool, ToolError, run_tool};

#[derive(Debug, thiserror::Error)]
pub(crate) enum ServiceError {
    #[error("the service definition cannot represent this path: {0:?}")]
    Render(#[from] render::ServiceError),
    #[error(transparent)]
    File(#[from] UserFileError),
    #[error(transparent)]
    Tool(#[from] ToolError),
    #[error("the program path is not UTF-8")]
    Path,
    #[error("{command} failed: {detail}")]
    Failed { command: String, detail: String },
    #[error("the home directory is unknown: {0}")]
    Home(#[from] std::io::Error),
    #[error("the user ID is unavailable")]
    NoUserId,
    #[error("the existing service definition is not owned by domyjob: {0}")]
    Foreign(PathBuf),
    #[error("the login service is only available on macOS")]
    Unsupported,
}

const LABEL: &str = "dev.domyjob.chat";
const UNIT: &str = "domyjob-chat.service";
const TASK: &str = "domyjob-chat";

fn text(path: &Path) -> Result<&str, ServiceError> {
    path.to_str().ok_or(ServiceError::Path)
}

fn step(program: &str, arguments: &[String], tolerated: bool) -> Result<String, ServiceError> {
    let output = run_tool(
        &Tool::new(program, arguments)
            .output(domyjob_core::capture_policy::CapturePolicy::Complete),
    )?;
    if output.success || tolerated {
        return Ok(output.stdout);
    }
    Err(ServiceError::Failed {
        command: format!("{program} {}", arguments.join(" ")),
        detail: output.stderr.trim().to_owned(),
    })
}

fn launchd_definition() -> Result<PathBuf, ServiceError> {
    Ok(crate::platform::home()?.join(format!("Library/LaunchAgents/{LABEL}.plist")))
}

fn launchd_domain() -> Result<String, ServiceError> {
    let user = crate::platform::user_id().ok_or(ServiceError::NoUserId)?;
    Ok(format!("gui/{user}"))
}

fn require_owned(path: &Path, expected: &[String]) -> Result<bool, ServiceError> {
    let Some(document) = user_files::read(path)? else {
        return Ok(false);
    };
    if expected.iter().any(|value| value == &document) {
        Ok(true)
    } else {
        Err(ServiceError::Foreign(path.to_path_buf()))
    }
}

fn launchd_owned(path: &Path, program: Option<&Path>, log: &Path) -> Result<bool, ServiceError> {
    let expected = program
        .map(|program| {
            [["chat", "serve"], ["service", "serve"]]
                .map(|arguments| {
                    render::launchd_plist(LABEL, text(program)?, &arguments, text(log)?)
                        .map_err(ServiceError::from)
                })
                .into_iter()
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()?
        .unwrap_or_default();
    require_owned(path, &expected)
}

fn launchd_update_owned(
    path: &Path,
    program: &Path,
    previous: Option<&Path>,
    log: &Path,
) -> Result<bool, ServiceError> {
    match launchd_owned(path, previous, log) {
        Err(ServiceError::Foreign(_)) => launchd_owned(path, Some(program), log),
        result => result,
    }
}

pub(crate) fn install(
    program: &Path,
    previous: Option<&Path>,
    log: &Path,
) -> Result<PathBuf, ServiceError> {
    if !cfg!(target_os = "macos") {
        return Err(ServiceError::Unsupported);
    }
    let path = launchd_definition()?;
    launchd_update_owned(&path, program, previous, log)?;
    let domain = launchd_domain()?;
    let plist_path = text(&path)?.to_owned();
    let plist = render::launchd_plist(LABEL, text(program)?, &["service", "serve"], text(log)?)?;
    user_files::write(&path, plist.as_bytes())?;
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

fn launchd_uninstall(program: Option<&Path>, log: &Path) -> Result<bool, ServiceError> {
    let path = launchd_definition()?;
    if !launchd_owned(&path, program, log)? {
        return Ok(false);
    }
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
    let config = match crate::platform::variable("XDG_CONFIG_HOME") {
        Some(config) => PathBuf::from(config),
        None => crate::platform::home()?.join(".config"),
    };
    Ok(config.join("systemd/user").join(UNIT))
}

fn systemctl(arguments: &[&str]) -> Result<(), ServiceError> {
    let runtime = crate::platform::user_runtime_dir().ok_or(ServiceError::NoUserId)?;
    let mut words = vec![String::from("--user")];
    words.extend(arguments.iter().map(|word| (*word).to_owned()));
    let output = run_tool(&Tool::new("systemctl", &words).env("XDG_RUNTIME_DIR", runtime))?;
    if output.success {
        Ok(())
    } else {
        Err(ServiceError::Failed {
            command: format!("systemctl {}", words.join(" ")),
            detail: output.stderr,
        })
    }
}

fn systemd_uninstall(program: Option<&Path>) -> Result<bool, ServiceError> {
    let path = systemd_definition()?;
    let expected = program
        .map(|program| {
            render::systemd_unit("domyjob chat service", text(program)?, &["chat", "serve"])
                .map_err(ServiceError::from)
        })
        .transpose()?
        .into_iter()
        .collect::<Vec<_>>();
    if !require_owned(&path, &expected)? {
        return Ok(false);
    }
    systemctl(&["disable", "--now", UNIT])?;
    let removed = user_files::remove(&path)?;
    systemctl(&["daemon-reload"])?;
    Ok(removed)
}

fn task_uninstall(program: Option<&Path>) -> Result<bool, ServiceError> {
    let expected = program
        .map(|program| {
            render::windows_task_command(text(program)?, &["chat", "serve"])
                .map_err(ServiceError::from)
        })
        .transpose()?;
    let expected = expected
        .map(|command| format!("--headless {command}"))
        .unwrap_or_default()
        .replace('\'', "''");
    let script = format!(
        "$ErrorActionPreference='Stop'; $task = Get-ScheduledTask -TaskName '{TASK}' -ErrorAction SilentlyContinue; \
if ($null -eq $task) {{ Write-Output 'absent'; exit 0 }}; \
$actions = @($task.Actions); \
if ($actions.Count -ne 1 -or $actions[0].Execute -ne 'conhost.exe' -or $actions[0].Arguments -cne '{expected}') {{ throw 'The scheduled task is not owned by domyjob' }}; \
Stop-ScheduledTask -InputObject $task; \
Unregister-ScheduledTask -InputObject $task -Confirm:$false; Write-Output 'removed'"
    );
    let utf16: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let output = step(
        "powershell.exe",
        &[
            String::from("-NoProfile"),
            String::from("-NonInteractive"),
            String::from("-EncodedCommand"),
            data_encoding::BASE64.encode(&utf16),
        ],
        false,
    )?;
    Ok(output.trim() == "removed")
}

pub(crate) fn uninstall(program: Option<&Path>, log: &Path) -> Result<bool, ServiceError> {
    if cfg!(target_os = "macos") {
        launchd_uninstall(program, log)
    } else if cfg!(target_os = "linux") {
        systemd_uninstall(program)
    } else if cfg!(windows) {
        task_uninstall(program)
    } else {
        Err(ServiceError::Unsupported)
    }
}

#[cfg(test)]
mod tests {
    use super::{LABEL, launchd_owned, launchd_update_owned};
    use crate::platform::user_files;
    use domyjob_core::service::launchd_plist;
    use std::path::Path;

    #[test]
    fn migration_recognizes_the_previous_service_and_refuses_foreign_definitions() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("service.plist");
        let log = root.path().join("service.log");
        let program = Path::new("/bin/domyjob");
        for arguments in [["chat", "serve"], ["service", "serve"]] {
            let definition =
                launchd_plist(LABEL, "/bin/domyjob", &arguments, log.to_str().unwrap()).unwrap();
            user_files::write(&path, definition.as_bytes()).unwrap();
            assert!(launchd_owned(&path, Some(program), &log).unwrap());
            launchd_owned(&path, None, &log).unwrap_err();
            launchd_owned(&path, Some(Path::new("/foreign/domyjob")), &log).unwrap_err();
            assert!(
                launchd_update_owned(&path, program, Some(Path::new("/previous/domyjob")), &log)
                    .unwrap()
            );
        }
        user_files::write(&path, b"foreign service").unwrap();
        launchd_owned(&path, Some(program), &log).unwrap_err();
        launchd_update_owned(&path, program, Some(Path::new("/previous/domyjob")), &log)
            .unwrap_err();
        assert_eq!(user_files::read(&path).unwrap().unwrap(), "foreign service");
    }
}
