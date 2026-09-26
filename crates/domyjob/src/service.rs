use std::path::{Path, PathBuf};

use crate::paths::Dirs;
use crate::template::Arg;

const UNIT: &str = "domyjob.service";
const AGENT: &str = "dev.domyjob.serve";
const TASK: &str = "domyjob-serve";
const KEEP_THE_TASK_RUNNING: &str = "$s = New-ScheduledTaskSettingsSet -ExecutionTimeLimit ([TimeSpan]::Zero) -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -StartWhenAvailable; Set-ScheduledTask -TaskName domyjob-serve -Settings $s | Out-Null";

#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    #[error("{action} {path}: {source}")]
    Io {
        action: &'static str,
        path: PathBuf,
        source: std::io::Error,
    },
    #[error(transparent)]
    Files(crate::user_files::UserFileError),
    #[error("{program} could not start: {source}")]
    Start {
        program: String,
        source: std::io::Error,
    },
    #[error("{command} failed")]
    Failed { command: String },
    #[error(
        "starting with the session is not supported on {0}; run `domyjob serve` from your own service manager"
    )]
    Unsupported(&'static str),
}

fn run(program: &'static str, args: &[Arg]) -> Result<(), ServiceError> {
    let status = crate::spawn::Invocation::new(Arg::literal(program), args.to_vec())
        .command()
        .status()
        .map_err(|source| ServiceError::Start {
            program: program.to_owned(),
            source,
        })?;
    if status.success() {
        Ok(())
    } else {
        Err(ServiceError::Failed {
            command: crate::spawn::Invocation::new(Arg::literal(program), args.to_vec()).display(),
        })
    }
}

fn write(path: &Path, text: &str) -> Result<(), ServiceError> {
    crate::user_files::write(path, text.as_bytes()).map_err(ServiceError::Files)
}

fn remove(path: &Path) -> Result<(), ServiceError> {
    crate::user_files::remove(path).map_err(ServiceError::Files)
}

fn xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[must_use]
pub fn systemd_unit(exe: &Path, args: &[Arg]) -> String {
    format!(
        "[Unit]\nDescription=domyjob serve\nAfter=network-online.target\n\n[Service]\nExecStart=\"{}\" serve {}\nRestart=on-failure\nKillMode=process\n\n[Install]\nWantedBy=default.target\n",
        exe.display(),
        Arg::spaced(args).as_arg_str()
    )
}

#[must_use]
pub fn launch_agent(exe: &Path, args: &[Arg], log: &Path) -> String {
    let mut extra = String::new();
    for arg in args {
        extra.push_str("<string>");
        extra.push_str(&xml(arg.as_arg_str()));
        extra.push_str("</string>");
    }
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict>\n<key>Label</key><string>{AGENT}</string>\n<key>ProgramArguments</key><array><string>{}</string><string>serve</string>{extra}</array>\n<key>RunAtLoad</key><true/>\n<key>KeepAlive</key><true/>\n<key>StandardOutPath</key><string>{}</string>\n<key>StandardErrorPath</key><string>{}</string>\n</dict></plist>\n",
        xml(&exe.display().to_string()),
        xml(&log.display().to_string()),
        xml(&log.display().to_string()),
    )
}

fn systemd_path(dirs: &Dirs) -> PathBuf {
    dirs.home
        .join(".config")
        .join("systemd")
        .join("user")
        .join(UNIT)
}

fn agent_path(dirs: &Dirs) -> PathBuf {
    dirs.home
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{AGENT}.plist"))
}

#[cfg(unix)]
fn gui_domain() -> Arg {
    Arg::concat(&[
        Arg::literal("gui/"),
        Arg::number(u64::from(rustix::process::getuid().as_raw())),
    ])
}

#[cfg(not(unix))]
fn gui_domain() -> Arg {
    Arg::literal("")
}

fn install_windows(exe: &Path, args: &[Arg]) -> Result<String, ServiceError> {
    let action = Arg::concat(&[
        Arg::literal("\""),
        Arg::path(exe),
        Arg::literal("\" serve "),
        Arg::spaced(args),
    ]);
    let create = [
        "/Create", "/F", "/SC", "ONLOGON", "/RL", "LIMITED", "/TN", TASK, "/TR",
    ];
    let mut create: Vec<Arg> = create.into_iter().map(Arg::literal).collect();
    create.push(action);
    run("schtasks", &create)?;
    if let Err(error) = run(
        "powershell",
        &[
            Arg::literal("-NoProfile"),
            Arg::literal("-NonInteractive"),
            Arg::literal("-Command"),
            Arg::literal(KEEP_THE_TASK_RUNNING),
        ],
    ) {
        eprintln!(
            "domyjob: the task may stop after 72 hours or on battery, because relaxing its limits failed: {error}"
        );
    }
    run(
        "schtasks",
        &[
            Arg::literal("/Run"),
            Arg::literal("/TN"),
            Arg::literal(TASK),
        ],
    )?;
    Ok(format!(
        "scheduled task {TASK}; Windows Defender Firewall will ask before the port is reachable"
    ))
}

pub fn install(dirs: &Dirs, exe: &Path, args: &[Arg]) -> Result<String, ServiceError> {
    match std::env::consts::OS {
        "linux" => {
            let path = systemd_path(dirs);
            write(&path, &systemd_unit(exe, args))?;
            run(
                "systemctl",
                &[Arg::literal("--user"), Arg::literal("daemon-reload")],
            )?;
            run(
                "systemctl",
                &[
                    Arg::literal("--user"),
                    Arg::literal("enable"),
                    Arg::literal("--now"),
                    Arg::literal(UNIT),
                ],
            )?;
            Ok(format!(
                "systemd user unit {}; `loginctl enable-linger` keeps it running without a login",
                path.display()
            ))
        }
        "macos" => {
            let path = agent_path(dirs);
            let log = dirs.state.join("serve.log");
            write(&path, &launch_agent(exe, args, &log))?;
            let target = path.display().to_string();
            if run(
                "launchctl",
                &[Arg::literal("bootout"), gui_domain(), Arg::path(&path)],
            )
            .is_err()
            {
                eprintln!("domyjob: no earlier agent was loaded");
            }
            run(
                "launchctl",
                &[Arg::literal("bootstrap"), gui_domain(), Arg::path(&path)],
            )?;
            Ok(format!(
                "launch agent {target}, logging to {}",
                log.display()
            ))
        }
        "windows" => install_windows(exe, args),
        _ => Err(ServiceError::Unsupported(std::env::consts::OS)),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Uninstalled {
    Nothing,
    Service,
}

fn defined(path: &Path) -> Result<bool, ServiceError> {
    crate::user_files::present(path).map_err(ServiceError::Files)
}

pub fn uninstall(dirs: &Dirs) -> Result<Uninstalled, ServiceError> {
    match std::env::consts::OS {
        "linux" => {
            let path = systemd_path(dirs);
            if !defined(&path)? {
                return Ok(Uninstalled::Nothing);
            }
            run(
                "systemctl",
                &[
                    Arg::literal("--user"),
                    Arg::literal("disable"),
                    Arg::literal("--now"),
                    Arg::literal(UNIT),
                ],
            )?;
            remove(&path).map(|()| Uninstalled::Service)
        }
        "macos" => {
            let path = agent_path(dirs);
            if !defined(&path)? {
                return Ok(Uninstalled::Nothing);
            }
            run(
                "launchctl",
                &[Arg::literal("bootout"), gui_domain(), Arg::path(&path)],
            )?;
            remove(&path).map(|()| Uninstalled::Service)
        }
        "windows" => {
            let task = [Arg::literal("/TN"), Arg::literal(TASK)];
            if run("schtasks", &[&[Arg::literal("/Query")][..], &task].concat()).is_err() {
                return Ok(Uninstalled::Nothing);
            }
            if run("schtasks", &[&[Arg::literal("/End")][..], &task].concat()).is_err() {
                eprintln!("domyjob: the task was not running");
            }
            run(
                "schtasks",
                &[&[Arg::literal("/Delete"), Arg::literal("/F")][..], &task].concat(),
            )
            .map(|()| Uninstalled::Service)
        }
        _ => Err(ServiceError::Unsupported(std::env::consts::OS)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_definitions_name_the_binary_and_address() {
        let args = vec![Arg::literal("--expose"), Arg::literal("tailnet")];
        let unit = systemd_unit(Path::new("/home/me/.local/bin/domyjob"), &args);
        assert!(unit.contains("ExecStart=\"/home/me/.local/bin/domyjob\" serve --expose tailnet"));
        let plist = launch_agent(Path::new("/opt/a&b/domyjob"), &args, Path::new("/tmp/log"));
        assert!(plist.contains("<string>--expose</string><string>tailnet</string>"));
        assert!(plist.contains("<string>/opt/a&amp;b/domyjob</string>"));
        assert!(plist.contains("<key>KeepAlive</key><true/>"));
    }
}
