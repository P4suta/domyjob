use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};

use domyjob_core::domain::{Command, JobId, RelativePath, RemoteText};
use domyjob_core::resource_policy::Policy;
use domyjob_core::state::{CommandCompletionError, Event, JobState};

use super::{AdmissionPermit, Launch, Scope};
use crate::process::{Tool, ToolOutput};

const POLICY_LIMIT: usize = 16_384;

pub(super) fn load(path: &Path) -> io::Result<Option<Policy>> {
    let file = match crate::platform::private_options().read(true).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let metadata = file.metadata()?;
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("policy has no parent"))?;
    let directory = fs::symlink_metadata(parent)?;
    if !metadata.is_file()
        || metadata.uid() != 0
        || metadata.mode() & 0o022 != 0
        || !directory.is_dir()
        || directory.uid() != 0
        || directory.mode() & 0o022 != 0
    {
        return Err(io::Error::other(
            "resource policy and its directory must be root-owned and not writable by group or others",
        ));
    }
    let bytes = crate::bounded::read(file, POLICY_LIMIT)?
        .ok_or_else(|| io::Error::other("resource policy exceeds 16 KiB"))?;
    let policy = domyjob_core::ingress::json(&bytes, POLICY_LIMIT).map_err(io::Error::other)?;
    let controllers = crate::bounded::read(
        fs::File::open("/sys/fs/cgroup/cgroup.controllers")?,
        POLICY_LIMIT,
    )?
    .ok_or_else(|| io::Error::other("cgroup controller inventory is too large"))?;
    if String::from_utf8(controllers)
        .map_err(io::Error::other)?
        .split_whitespace()
        .any(|name| name == "memory")
    {
        Ok(Some(policy))
    } else {
        Err(io::Error::other(
            "resource policy requires the cgroup v2 memory controller",
        ))
    }
}

fn systemctl(arguments: &[String]) -> io::Result<ToolOutput> {
    let runtime = crate::platform::user_runtime_dir()
        .ok_or_else(|| io::Error::other("user systemd runtime directory is unavailable"))?;
    crate::process::run_tool(
        &Tool::new("/usr/bin/systemctl", arguments).env("XDG_RUNTIME_DIR", runtime),
    )
    .map_err(io::Error::other)
}

fn properties(scope: &str) -> io::Result<ToolOutput> {
    systemctl(&[
        "--user".into(),
        "show".into(),
        scope.into(),
        "--property=LoadState,ActiveState,Result,ControlGroup,MemoryHigh,MemoryMax,MemorySwapMax"
            .into(),
    ])
}

fn value<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    text.lines().find_map(|line| {
        let (key, value) = line.split_once('=')?;
        (key == name).then_some(value)
    })
}

pub(super) fn prepare(
    permit: AdmissionPermit,
    policy: Policy,
    job: &JobId,
    original: &std::process::Command,
) -> io::Result<Launch> {
    let started = systemctl(&["--user".into(), "start".into(), "domyjob.slice".into()])?;
    if !started.success {
        return Err(io::Error::other(started.stderr));
    }
    let effective = properties("domyjob.slice")?;
    let budget = policy.budget();
    for (key, expected) in [
        ("MemoryHigh", budget.high),
        ("MemoryMax", budget.max),
        ("MemorySwapMax", budget.swap),
    ] {
        if !effective.success
            || value(&effective.stdout, key) != Some(expected.to_string().as_str())
        {
            return Err(io::Error::other(format!(
                "domyjob.slice {key} does not match the required resource policy"
            )));
        }
    }
    let identity = format!("{}:{}", crate::identity::tag(), job.as_str());
    let name: String = blake3::hash(identity.as_bytes())
        .to_hex()
        .chars()
        .take(32)
        .collect();
    let scope = Scope::parse(format!("domyjob-job-{name}.scope"))?;
    if let Some(record) = &permit.record {
        crate::state_io::write_bytes(record, scope.name().as_bytes()).map_err(io::Error::other)?;
    }
    crate::state_io::remove_file(&scope.marker()?).map_err(io::Error::other)?;
    let mut command = crate::process::command("/usr/bin/systemd-run");
    crate::platform::prepare_job_environment(&mut command);
    command.args([
        "--user",
        "--scope",
        "--quiet",
        "--no-ask-password",
        "--expand-environment=no",
        "--slice=domyjob.slice",
        "--property=OOMPolicy=kill",
        "--property=TimeoutStopSec=5s",
    ]);
    command.arg(format!("--unit={}", scope.name()));
    command.arg(format!("--property=MemoryHigh={}", budget.high));
    command.arg(format!("--property=MemoryMax={}", budget.max));
    command.arg(format!("--property=MemorySwapMax={}", budget.swap));
    command
        .arg("--")
        .arg(std::env::current_exe()?)
        .arg("job-exec")
        .arg(scope.name())
        .arg("--")
        .arg(original.get_program())
        .args(original.get_args());
    if let Some(directory) = original.get_current_dir() {
        command.current_dir(directory);
    }
    let runtime = crate::platform::user_runtime_dir()
        .ok_or_else(|| io::Error::other("user systemd runtime directory is unavailable"))?;
    command.env("XDG_RUNTIME_DIR", runtime);
    Ok(Launch {
        command,
        scope: Some(scope),
        permit,
    })
}

pub(super) fn exec(scope: &Scope, original: &Command) -> io::Result<()> {
    let membership = crate::bounded::read(fs::File::open("/proc/self/cgroup")?, POLICY_LIMIT)?
        .ok_or_else(|| io::Error::other("cgroup membership is too large"))?;
    let suffix = format!("/{}", scope.name());
    if !String::from_utf8(membership)
        .map_err(io::Error::other)?
        .lines()
        .any(|line| line.starts_with("0::") && line.ends_with(&suffix))
    {
        return Err(io::Error::other(
            "scoped execution is outside its assigned cgroup",
        ));
    }
    let adjusted = crate::process::run_tool(&Tool::new(
        "/usr/bin/choom",
        &[
            "--pid".into(),
            std::process::id().to_string(),
            "--adjust".into(),
            "100".into(),
        ],
    ))
    .map_err(io::Error::other)?;
    if !adjusted.success {
        return Err(io::Error::other(adjusted.stderr));
    }
    let mut command = crate::process::command(original.program());
    crate::platform::prepare_job_environment(&mut command);
    command
        .args(original.arguments())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit());
    let mut execution = JobState::accepted();
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            let detail: String = error.to_string().chars().take(4096).collect();
            let reason = RemoteText::try_from(detail).map_err(io::Error::other)?;
            execution
                .advance(&Event::LaunchFailed { reason })
                .map_err(io::Error::other)?;
            return write_execution(scope, &execution);
        }
    };
    execution
        .advance(&Event::Starting)
        .map_err(io::Error::other)?;
    execution
        .advance(&Event::Spawned { pid: child.id() })
        .map_err(io::Error::other)?;
    write_execution(scope, &execution)?;
    let status = child.wait()?;
    execution
        .advance(&Event::Exited {
            code: status.code().unwrap_or(-1),
        })
        .map_err(io::Error::other)?;
    write_execution(scope, &execution)
}

fn write_execution(scope: &Scope, state: &JobState) -> io::Result<()> {
    let bytes = serde_json::to_vec(state).map_err(io::Error::other)?;
    crate::state_io::write_bytes(&scope.marker()?, &bytes).map_err(io::Error::other)
}

fn execution_event(scope: &Scope) -> io::Result<Event> {
    if let Some(bytes) = crate::state_io::read_bytes(&scope.marker()?).map_err(io::Error::other)? {
        let state = domyjob_core::ingress::stored_job(&bytes).map_err(io::Error::other)?;
        match state.command_completion() {
            Ok(event) => return Ok(event),
            Err(CommandCompletionError::InvalidOutcome) => {
                return Err(io::Error::other("invalid command execution outcome"));
            }
            Err(CommandCompletionError::Unfinished) => {}
        }
    }
    Ok(Event::LaunchFailed {
        reason: RemoteText::try_from(String::from(
            "the scoped launcher did not collect a command outcome",
        ))
        .map_err(io::Error::other)?,
    })
}

fn oom_result(output: &ToolOutput) -> io::Result<bool> {
    if !output.success {
        return Err(io::Error::other(output.stderr.clone()));
    }
    match value(&output.stdout, "Result") {
        Some("oom-kill") => Ok(true),
        Some("success") => Ok(false),
        None if value(&output.stdout, "LoadState") == Some("not-found") => Ok(false),
        other => Err(io::Error::other(format!(
            "unexpected scope result: {other:?}"
        ))),
    }
}

fn cgroup_events(scope: &Scope, reported: Option<&str>, user: u32) -> io::Result<PathBuf> {
    let parent = format!("/user.slice/user-{user}.slice/user@{user}.service/");
    let fallback = format!("{parent}domyjob.slice/{}", scope.name());
    let group = reported
        .filter(|path| !path.is_empty())
        .unwrap_or(&fallback);
    if !group.starts_with(&parent) || !group.ends_with(&format!("/{}", scope.name())) {
        return Err(io::Error::other("scope cgroup is outside its user manager"));
    }
    let relative = RelativePath::try_from(group.trim_start_matches('/').to_owned())
        .map_err(io::Error::other)?;
    Ok(Path::new("/sys/fs/cgroup")
        .join(relative.as_str())
        .join("cgroup.events"))
}

fn populated(scope: &Scope, output: &ToolOutput) -> io::Result<bool> {
    let user =
        crate::platform::user_id().ok_or_else(|| io::Error::other("user UID unavailable"))?;
    let path = cgroup_events(scope, value(&output.stdout, "ControlGroup"), user)?;
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    let bytes = crate::bounded::read(file, POLICY_LIMIT)?
        .ok_or_else(|| io::Error::other("cgroup events are too large"))?;
    let text = String::from_utf8(bytes).map_err(io::Error::other)?;
    match text
        .lines()
        .find_map(|line| line.strip_prefix("populated "))
    {
        Some("0") => Ok(false),
        Some("1") => Ok(true),
        _ => Err(io::Error::other(
            "cgroup populated state is missing or invalid",
        )),
    }
}

pub(super) fn stop(scope: &Scope) -> io::Result<()> {
    let state = properties(scope.name())?;
    if !state.success {
        return Err(io::Error::other(state.stderr));
    }
    if !populated(scope, &state)? {
        return Ok(());
    }
    let killed = systemctl(&[
        "--user".into(),
        "kill".into(),
        "--signal=KILL".into(),
        "--kill-whom=all".into(),
        scope.name().into(),
    ])?;
    let stopped = systemctl(&["--user".into(), "stop".into(), scope.name().into()])?;
    let final_state = properties(scope.name())?;
    if final_state.success && !populated(scope, &final_state)? {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "scope still contains processes after stopping: {}; {}",
            killed.stderr, stopped.stderr
        )))
    }
}

pub(super) fn completion(scope: &Scope, event: &Event) -> io::Result<Event> {
    let before = oom_result(&properties(scope.name())?)?;
    stop(scope)?;
    let after = oom_result(&properties(scope.name())?)?;
    let outcome = if *event == Event::Killed {
        Event::Killed
    } else if before || after {
        Event::MemoryLimitExceeded
    } else {
        execution_event(scope)?
    };
    scope.cleanup()?;
    Ok(outcome)
}

pub(super) fn cleanup(scope: &Scope) -> io::Result<()> {
    let state = properties(scope.name())?;
    if !state.success {
        return Err(io::Error::other(state.stderr));
    }
    if populated(scope, &state)? {
        return Err(io::Error::other("cannot discard a populated scope"));
    }
    if value(&state.stdout, "LoadState") == Some("not-found") {
        return Ok(());
    }
    let reset = systemctl(&["--user".into(), "reset-failed".into(), scope.name().into()])?;
    if reset.success {
        Ok(())
    } else {
        let final_state = properties(scope.name())?;
        if final_state.success && value(&final_state.stdout, "LoadState") == Some("not-found") {
            Ok(())
        } else {
            Err(io::Error::other(reset.stderr))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Scope, Tool};
    use crate::process::{Group, resources::Launch};
    use domyjob_core::state::Event;
    use std::io::Read as _;
    use std::process::Stdio;

    #[test]
    fn scope_accounting_paths_cannot_escape_the_user_manager() {
        let scope = Scope::parse(format!("domyjob-job-{}.scope", "a".repeat(32))).unwrap();
        let valid = format!(
            "/user.slice/user-1000.slice/user@1000.service/app.slice/{}",
            scope.name()
        );
        assert_eq!(
            super::cgroup_events(&scope, Some(&valid), 1000).unwrap(),
            std::path::Path::new("/sys/fs/cgroup")
                .join(valid.strip_prefix('/').unwrap())
                .join("cgroup.events")
        );
        for invalid in [
            valid.replace("1000", "0"),
            valid.replace("app.slice", "../system.slice"),
            valid.replace(scope.name(), "another.scope"),
            valid.replacen("/user.slice", "//user.slice", 1),
        ] {
            super::cgroup_events(&scope, Some(&invalid), 1000).unwrap_err();
        }
        assert!(
            super::cgroup_events(&scope, None, 1000)
                .unwrap()
                .ends_with(format!("domyjob.slice/{}/cgroup.events", scope.name()))
        );
    }

    fn native_scope() -> (tempfile::TempDir, Scope) {
        let directory = tempfile::tempdir().unwrap();
        let unique = blake3::hash(directory.path().to_string_lossy().as_bytes())
            .to_hex()
            .to_string();
        let name: String = unique.chars().take(32).collect();
        (
            directory,
            Scope::parse(format!("domyjob-job-{name}.scope")).unwrap(),
        )
    }

    fn native_command(scope: &Scope) -> std::process::Command {
        let mut command = crate::process::command("/usr/bin/systemd-run");
        command
            .args([
                "--user",
                "--scope",
                "--quiet",
                "--no-ask-password",
                "--property=MemoryHigh=64M",
                "--property=MemoryMax=96M",
                "--property=MemorySwapMax=0",
                "--property=OOMPolicy=kill",
                "--property=TimeoutStopSec=5s",
            ])
            .arg(format!("--unit={}", scope.name()));
        command
    }

    #[test]
    fn scope_memory_fixture() {
        if std::env::var_os("DOMYJOB_RESOURCE_FIXTURE").is_some() {
            let mut pages = Vec::new();
            pages.resize(128 * 1024 * 1024, 1_u8);
            std::hint::black_box(pages);
        }
    }

    #[test]
    fn scope_exec_fixture() {
        if let Some(name) = std::env::var_os("DOMYJOB_EXEC_FIXTURE") {
            let scope = Scope::parse(name.to_string_lossy().into_owned()).unwrap();
            let kind = std::env::var("DOMYJOB_EXEC_KIND").unwrap();
            let arguments = match kind.as_str() {
                "missing" => vec!["/does-not-exist/domyjob-fixture".to_owned()],
                "exit" => vec![
                    "/bin/sh".to_owned(),
                    "-c".to_owned(),
                    "printf stdout; printf stderr >&2; exit 7".to_owned(),
                ],
                "signal" => vec![
                    "/bin/sh".to_owned(),
                    "-c".to_owned(),
                    "kill -TERM $$".to_owned(),
                ],
                _ => panic!("invalid scope fixture"),
            };
            super::exec(
                &scope,
                &domyjob_core::domain::Command::try_from(arguments).unwrap(),
            )
            .unwrap();
        }
    }

    #[test]
    #[ignore = "requires a native Linux user systemd manager; run mise run resource-contracts"]
    fn native_resource_preserves_launch_failure_exit_code_and_signal() {
        for kind in ["missing", "exit", "signal"] {
            let (_directory, scope) = native_scope();
            let mut command = native_command(&scope);
            command
                .arg("--")
                .arg(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "process::resources::linux::tests::scope_exec_fixture",
                    "--nocapture",
                ]);
            let arguments: Vec<_> = command
                .get_args()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect();
            let output = crate::process::run_tool(
                &Tool::new("/usr/bin/systemd-run", &arguments)
                    .env("DOMYJOB_EXEC_FIXTURE", scope.name().to_owned())
                    .env("DOMYJOB_EXEC_KIND", kind.to_owned())
                    .env(
                        "XDG_RUNTIME_DIR",
                        crate::platform::user_runtime_dir().unwrap(),
                    ),
            )
            .unwrap();
            let event = scope.completion(&Event::Exited { code: 0 }).unwrap();
            assert!(output.success);
            match kind {
                "missing" => assert!(matches!(event, Event::LaunchFailed { .. })),
                "exit" => {
                    assert_eq!(event, Event::Exited { code: 7 });
                    assert!(output.stdout.contains("stdout"));
                    assert!(output.stderr.contains("stderr"));
                }
                "signal" => assert_eq!(event, Event::Exited { code: -1 }),
                _ => panic!("invalid scope fixture"),
            }
        }
    }

    #[test]
    #[ignore = "requires a native Linux user systemd manager; run mise run resource-contracts"]
    fn native_resource_oom_is_not_an_ordinary_exit() {
        let (_directory, scope) = native_scope();
        let mut command = native_command(&scope);
        command
            .arg("--property=MemoryHigh=96M")
            .arg("--")
            .arg("/usr/bin/choom")
            .args(["--adjust", "100", "--"])
            .arg(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "process::resources::linux::tests::scope_memory_fixture",
            ])
            .env("DOMYJOB_RESOURCE_FIXTURE", "1");
        let arguments: Vec<_> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        let runtime = crate::platform::user_runtime_dir().unwrap();
        let result = crate::process::run_tool(
            &Tool::new("/usr/bin/systemd-run", &arguments)
                .env("DOMYJOB_RESOURCE_FIXTURE", "1".to_owned())
                .env("XDG_RUNTIME_DIR", runtime),
        )
        .unwrap();
        let completed = scope.completion(&Event::Exited { code: 1 }).unwrap();
        assert!(!result.success);
        assert_eq!(completed, Event::MemoryLimitExceeded);
        scope.stop().unwrap();
        scope.cleanup().unwrap();
    }

    #[test]
    #[ignore = "requires a native Linux user systemd manager; run mise run resource-contracts"]
    fn native_resource_cancel_stops_session_escaped_descendants() {
        let (_directory, scope) = native_scope();
        let mut command = native_command(&scope);
        command.args([
            "--",
            "/bin/sh",
            "-c",
            "setsid sleep 600 & printf ready; wait",
        ]);
        let (mut reader, writer) = std::io::pipe().unwrap();
        let mut launch = Launch::fixture(command);
        launch.scope = Some(scope.clone());
        let group = Group::spawn_job_stdio(launch, Stdio::from(writer), Stdio::null()).unwrap();
        let mut ready = [0; 5];
        reader.read_exact(&mut ready).unwrap();
        assert_eq!(&ready, b"ready");
        group.kill().unwrap();
        group.wait().unwrap();
        assert_eq!(group.completion(Event::Killed).unwrap(), Event::Killed);
        let final_state = super::properties(scope.name()).unwrap();
        assert_eq!(
            super::value(&final_state.stdout, "LoadState"),
            Some("not-found")
        );
    }

    #[test]
    #[ignore = "requires a native Linux user systemd manager; run mise run resource-contracts"]
    fn native_resource_permit_drop_cleans_up_before_slot_reuse() {
        let (directory, scope) = native_scope();
        let policy = domyjob_core::ingress::foreign_json(
            r#"{"version":1,"max_concurrent_jobs":2,"slice":"domyjob.slice","memory_high_bytes":67108864,"memory_max_bytes":100663296,"memory_swap_max_bytes":0}"#,
        ).unwrap();
        let limits =
            super::super::Limits::fixture(Some(policy), directory.path().join("admission"));
        let permit = limits.try_admit().unwrap().unwrap();
        crate::state_io::write_bytes(permit.record.as_ref().unwrap(), scope.name().as_bytes())
            .unwrap();
        let mut command = native_command(&scope);
        command.args([
            "--",
            "/bin/sh",
            "-c",
            "setsid sleep 600 & printf ready; wait",
        ]);
        let (mut reader, writer) = std::io::pipe().unwrap();
        let group =
            Group::spawn_job_stdio(Launch::fixture(command), Stdio::from(writer), Stdio::null())
                .unwrap();
        let mut ready = [0; 5];
        reader.read_exact(&mut ready).unwrap();
        assert_eq!(&ready, b"ready");
        drop(permit);
        group.wait().unwrap();
        assert_eq!(
            super::value(
                &super::properties(scope.name()).unwrap().stdout,
                "LoadState"
            ),
            Some("not-found")
        );
        assert!(limits.try_admit().unwrap().is_some());
    }

    #[test]
    fn user_owned_or_malformed_policy_is_rejected() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("resource-policy.json");
        crate::testing::write(&path, b"{}");
        super::load(&path).unwrap_err();
        assert!(super::load(&root.path().join("missing")).unwrap().is_none());
    }
}
