use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use domyjob_core::chat::card::{MachineCard, Os};
use domyjob_core::chat::id::{Invalid, Line};

pub(crate) use crate::file_kind::reparse_point;
pub(crate) mod clock;
pub(crate) mod service;
pub(crate) mod user_files;

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
mod windows_acl;

#[cfg(unix)]
pub(crate) use unix::Mode as Exposure;
#[cfg(unix)]
use unix::Unix as Native;
#[cfg(windows)]
pub(crate) use windows::Acl as Exposure;
#[cfg(windows)]
use windows::Windows as Native;

mod raw {
    #![expect(
        clippy::disallowed_methods,
        reason = "the platform layer owns raw file options, permissions, and user file effects"
    )]

    use std::fs::OpenOptions;
    use std::io;
    use std::path::Path;

    pub(super) fn options() -> OpenOptions {
        OpenOptions::new()
    }

    #[cfg(unix)]
    pub(super) fn set_permissions(
        path: &Path,
        permissions: std::fs::Permissions,
    ) -> io::Result<()> {
        std::fs::set_permissions(path, permissions)
    }

    pub(super) fn create_dir_all(path: &Path) -> io::Result<()> {
        std::fs::create_dir_all(path)
    }

    pub(super) fn copy(from: &Path, to: &Path) -> io::Result<u64> {
        std::fs::copy(from, to)
    }

    pub(super) fn remove_file(path: &Path) -> io::Result<()> {
        std::fs::remove_file(path)
    }

    pub(super) fn rename(from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(from, to)
    }
}

trait System {
    const OS: Os;
    const JOB_ENVIRONMENT: &'static [&'static str];

    fn host_name() -> String;
    fn user_id() -> Option<u32>;
    fn executable(metadata: &fs::Metadata) -> bool;
    fn make_executable(path: &Path) -> io::Result<()>;
    fn file_mode(metadata: &fs::Metadata) -> u32;
    fn creation_mode(options: &mut cap_std::fs::OpenOptions, executable: bool);
    fn ownership(file: &fs::File) -> io::Result<Ownership>;
    fn open_private_dir(path: &Path) -> io::Result<fs::File>;
    fn create_private_dir(path: &Path) -> io::Result<()>;
    fn owner_only(options: &mut fs::OpenOptions);
    fn no_follow(options: &mut fs::OpenOptions);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Ownership {
    Private,
    Foreign,
    Exposed(Exposure),
}

pub(crate) fn variable(name: &str) -> Option<OsString> {
    std::env::var_os(name).filter(|value| !value.is_empty())
}

pub(crate) fn home() -> io::Result<PathBuf> {
    variable("HOME")
        .or_else(|| variable("USERPROFILE"))
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::other("the user home directory is unavailable"))
}

pub(crate) fn ssh_control_path(
    directory: &Path,
) -> Result<Option<PathBuf>, crate::state_io::StateError> {
    if cfg!(windows) || directory.as_os_str().len().saturating_add(41) > 100 {
        return Ok(None);
    }
    crate::state_io::private_dir(directory)?;
    Ok(Some(directory.join("%C")))
}

pub(crate) fn stable_program(build: &str) -> io::Result<PathBuf> {
    Ok(home()?
        .join(".cargo")
        .join("domyjob")
        .join("versions")
        .join(build)
        .join("bin")
        .join(format!("domyjob{}", std::env::consts::EXE_SUFFIX)))
}

pub(crate) fn make_executable(path: &Path) -> io::Result<()> {
    Native::make_executable(path)
}

pub(crate) fn user_id() -> Option<u32> {
    Native::user_id()
}

pub(crate) fn machine_card() -> Result<MachineCard, Invalid> {
    let name = Native::host_name();
    let label = name.split('.').next().unwrap_or_default().trim().to_owned();
    Ok(MachineCard {
        label: Line::try_from(if label.is_empty() {
            "machine".to_owned()
        } else {
            label.chars().take(64).collect()
        })?,
        os: Native::OS,
    })
}

pub(crate) fn user_runtime_dir() -> Option<String> {
    variable("XDG_RUNTIME_DIR")
        .map(|value| value.to_string_lossy().into_owned())
        .or_else(|| user_id().map(|id| format!("/run/user/{id}")))
}

#[must_use]
pub(crate) fn find_program(name: &str) -> Option<PathBuf> {
    let path = variable("PATH")?;
    std::env::split_paths(&path)
        .filter(|directory| directory.is_absolute())
        .flat_map(|directory| candidates(&directory, name))
        .find(|candidate| fs::metadata(candidate).is_ok_and(|found| Native::executable(&found)))
}

fn candidates(directory: &Path, name: &str) -> Vec<PathBuf> {
    candidates_on(cfg!(windows), directory, name)
}

fn candidates_on(windows: bool, directory: &Path, name: &str) -> Vec<PathBuf> {
    if !windows {
        return vec![directory.join(name)];
    }
    let mut found = Vec::new();
    if Path::new(name).extension().is_some() {
        found.push(directory.join(name));
    }
    found.extend(
        ["exe", "cmd", "bat"]
            .iter()
            .map(|extension| directory.join(format!("{name}.{extension}"))),
    );
    found
}

pub(crate) fn cargo_target_dir(checkout: &Path) -> PathBuf {
    match variable("CARGO_TARGET_DIR").map(PathBuf::from) {
        Some(target) if target.is_absolute() => target,
        Some(target) => checkout.join(target),
        None => checkout.join("target"),
    }
}

fn replaceable(executable: &Path) -> bool {
    raw::options().write(true).open(executable).is_ok()
}

pub(crate) fn local_refresh_target(target: &Path) -> io::Result<PathBuf> {
    if !cfg!(windows) {
        return Ok(target.to_path_buf());
    }
    let current = fs::canonicalize(std::env::current_exe()?)?;
    for slot in [
        target.to_path_buf(),
        target.join("domyjob-refresh"),
        target.join("domyjob-refresh-2"),
    ] {
        let executable = slot.join("debug").join("domyjob.exe");
        match fs::canonicalize(&executable) {
            Ok(path) if path == current => {}
            Ok(_) if replaceable(&executable) => return Ok(slot),
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(slot),
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::other(
        "every local build slot is running; close other domyjob clients and retry",
    ))
}

pub(crate) fn prepare_job_environment(command: &mut std::process::Command) {
    const COMMON: &[&str] = &[
        "PATH",
        "HOME",
        "USERPROFILE",
        "TEMP",
        "TMP",
        "MISE_DATA_DIR",
        "MISE_CONFIG_DIR",
    ];
    command.env_clear();
    for name in COMMON.iter().chain(Native::JOB_ENVIRONMENT) {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
}

pub(crate) fn file_mode(metadata: &fs::Metadata) -> u32 {
    Native::file_mode(metadata)
}

pub(crate) fn creation_mode(options: &mut cap_std::fs::OpenOptions, executable: bool) {
    Native::creation_mode(options, executable);
}

pub(crate) fn ownership(file: &fs::File) -> io::Result<Ownership> {
    Native::ownership(file)
}

pub(crate) fn open_private_dir(path: &Path) -> io::Result<fs::File> {
    Native::open_private_dir(path)
}

pub(crate) fn create_private_dir(path: &Path) -> io::Result<()> {
    Native::create_private_dir(path)
}

pub(crate) fn private_options() -> fs::OpenOptions {
    let mut options = raw::options();
    Native::owner_only(&mut options);
    Native::no_follow(&mut options);
    options
}

pub(crate) fn sync_dir(path: &Path) -> io::Result<()> {
    if cfg!(windows) {
        return Ok(());
    }
    fs::File::open(path)?.sync_all()
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{candidates_on, prepare_job_environment};

    #[test]
    fn a_program_named_with_its_extension_is_found_as_written() {
        let directory = Path::new("bin");
        assert_eq!(
            candidates_on(true, directory, "powershell.exe").first(),
            Some(&PathBuf::from("bin").join("powershell.exe"))
        );
        assert_eq!(
            candidates_on(true, directory, "codex"),
            ["codex.exe", "codex.cmd", "codex.bat"].map(|file| directory.join(file))
        );
        assert_eq!(
            candidates_on(false, directory, "sh"),
            [directory.join("sh")]
        );
    }

    #[test]
    fn a_job_does_not_inherit_ssh_connection_credentials() {
        let mut command = crate::process::command("unused");
        command.env("SSH_AUTH_SOCK", "should-not-be-forwarded");
        command.env("GIT_ASKPASS", "should-not-be-forwarded");
        prepare_job_environment(&mut command);
        let names: Vec<_> = command.get_envs().map(|(name, _value)| name).collect();
        assert!(!names.contains(&std::ffi::OsStr::new("SSH_AUTH_SOCK")));
        assert!(!names.contains(&std::ffi::OsStr::new("GIT_ASKPASS")));
    }
}
