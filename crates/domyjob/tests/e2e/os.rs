#[cfg(windows)]
#[path = "os/windows.rs"]
mod windows;

#[path = "os/macos.rs"]
mod macos;

use std::io;

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

pub(crate) fn parent_of(pid: u32) -> io::Result<u32> {
    #[cfg(windows)]
    {
        windows::parent_of(pid)
    }
    #[cfg(unix)]
    {
        let output = std::process::Command::new("ps")
            .args(["-o", "ppid=", "-p", &pid.to_string()])
            .stdin(std::process::Stdio::null())
            .output()?;
        String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
    }
}
