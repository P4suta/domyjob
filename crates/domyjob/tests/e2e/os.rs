#[cfg(windows)]
#[path = "os/windows.rs"]
mod windows;

use std::io;

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
