#[cfg(windows)]
#[path = "os/windows.rs"]
mod windows;

#[path = "os/macos.rs"]
mod macos;

#[cfg(windows)]
pub(crate) fn input_child_role() -> Option<std::process::ExitCode> {
    windows::input_child_role()
}

#[cfg(unix)]
pub(crate) const fn input_child_role() -> Option<std::process::ExitCode> {
    None
}
pub(crate) type BootstrapInputContract = fn(&std::path::Path) -> Result<(), crate::Failure>;

pub(crate) const fn bootstrap_input_contracts() -> &'static [BootstrapInputContract] {
    #[cfg(windows)]
    {
        &[windows::bootstrap_input_contract]
    }
    #[cfg(unix)]
    {
        &[]
    }
}

pub(crate) fn service_sampler(pid: u32, report: &std::path::Path) -> Option<std::process::Command> {
    if cfg!(target_os = "macos") {
        let mut command = std::process::Command::new("/usr/bin/sample");
        command.args(crate::diagnostics::sample_arguments(pid, report));
        Some(command)
    } else {
        None
    }
}
pub(crate) fn bootstrap_shell(command: &str) -> std::process::Command {
    #[cfg(windows)]
    {
        let mut shell = std::process::Command::new("powershell.exe");
        shell.args(["-NoProfile", "-Command", command]);
        shell
    }
    #[cfg(unix)]
    {
        let mut shell = std::process::Command::new("sh");
        shell.args(["-c", command]);
        shell
    }
}

pub(crate) const fn bootstrap_os() -> &'static str {
    if cfg!(windows) { "Windows_NT" } else { ":OS" }
}

pub(crate) fn bootstrap_cache(home: &std::path::Path) -> std::path::PathBuf {
    if cfg!(windows) {
        home.join("AppData/Local")
    } else {
        home.join(".cache")
    }
}

pub(crate) const fn extra_fakes() -> &'static [&'static str] {
    if cfg!(target_os = "macos") {
        &["launchctl"]
    } else {
        &[]
    }
}

pub(crate) fn fake_tool(executable: &std::path::Path) -> Option<std::process::ExitCode> {
    (cfg!(target_os = "macos") && executable.file_stem()?.to_str()? == "launchctl")
        .then(macos::launchctl)
}

pub(crate) fn job_handoff(
    world: &crate::World,
    client: &crate::world::Machine<'_>,
) -> Result<(), crate::Failure> {
    if cfg!(target_os = "macos") {
        macos::job_handoff(world, client)
    } else {
        Ok(())
    }
}
