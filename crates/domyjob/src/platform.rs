#![expect(
    clippy::redundant_pub_crate,
    reason = "the binary composition root uses this private module"
)]

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::Path;
use std::path::PathBuf;

pub(crate) use crate::file_kind::reparse_point;
pub(crate) mod chat_poll;

#[cfg(windows)]
mod windows_acl;

fn variable(name: &str) -> Option<OsString> {
    std::env::var_os(name).filter(|value| !value.is_empty())
}

pub(crate) fn home() -> io::Result<PathBuf> {
    variable("HOME")
        .or_else(|| variable("USERPROFILE"))
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::other("the user home directory is unavailable"))
}

pub(crate) fn state() -> io::Result<PathBuf> {
    if let Some(explicit) = variable("DOMYJOB_STATE") {
        return Ok(PathBuf::from(explicit));
    }
    #[cfg(windows)]
    if let Some(local) = variable("LOCALAPPDATA") {
        return Ok(PathBuf::from(local).join("domyjob").join("state"));
    }
    if let Some(xdg) = variable("XDG_STATE_HOME") {
        return Ok(PathBuf::from(xdg).join("domyjob"));
    }
    Ok(home()?.join(".local").join("state").join("domyjob"))
}

pub(crate) fn cargo_target_dir(checkout: &Path) -> PathBuf {
    match variable("CARGO_TARGET_DIR").map(PathBuf::from) {
        Some(target) if target.is_absolute() => target,
        Some(target) => checkout.join(target),
        None => checkout.join("target"),
    }
}

#[cfg(unix)]
#[expect(
    clippy::unnecessary_wraps,
    reason = "the shared Unix and Windows target selector is fallible on Windows"
)]
pub(crate) fn local_refresh_target(target: &Path) -> io::Result<PathBuf> {
    Ok(target.to_path_buf())
}

#[cfg(windows)]
pub(crate) fn local_refresh_target(target: &Path) -> io::Result<PathBuf> {
    let current = fs::canonicalize(std::env::current_exe()?)?;
    let primary = target.join("debug").join("domyjob.exe");
    let running_primary = match fs::canonicalize(primary) {
        Ok(path) => path == current,
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => return Err(error),
    };
    if running_primary {
        Ok(target.join("domyjob-refresh"))
    } else {
        Ok(target.to_path_buf())
    }
}

pub(crate) fn prepare_job_environment(command: &mut std::process::Command) {
    const COMMON: &[&str] = &["PATH", "HOME", "USERPROFILE", "TEMP", "TMP"];
    #[cfg(unix)]
    const PLATFORM: &[&str] = &[
        "USER",
        "LOGNAME",
        "LANG",
        "LC_ALL",
        "LC_CTYPE",
        "SHELL",
        "TMPDIR",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_CACHE_HOME",
        "XDG_STATE_HOME",
    ];
    #[cfg(windows)]
    const PLATFORM: &[&str] = &[
        "PATHEXT",
        "SystemRoot",
        "WINDIR",
        "COMSPEC",
        "LOCALAPPDATA",
        "APPDATA",
        "PROGRAMDATA",
        "HOMEDRIVE",
        "HOMEPATH",
        "USERNAME",
        "NUMBER_OF_PROCESSORS",
        "PROCESSOR_ARCHITECTURE",
    ];
    command.env_clear();
    for name in COMMON
        .iter()
        .chain(PLATFORM)
        .chain(["MISE_DATA_DIR", "MISE_CONFIG_DIR"].iter())
    {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
}

#[cfg(test)]
#[expect(
    clippy::disallowed_methods,
    reason = "the fixture inspects the environment of an unstarted process"
)]
mod tests {
    use super::prepare_job_environment;

    #[test]
    fn a_job_does_not_inherit_ssh_connection_credentials() {
        let mut command = std::process::Command::new("unused");
        command.env("SSH_AUTH_SOCK", "should-not-be-forwarded");
        command.env("GIT_ASKPASS", "should-not-be-forwarded");
        prepare_job_environment(&mut command);
        let names: Vec<_> = command.get_envs().map(|(name, _value)| name).collect();
        assert!(!names.contains(&std::ffi::OsStr::new("SSH_AUTH_SOCK")));
        assert!(!names.contains(&std::ffi::OsStr::new("GIT_ASKPASS")));
    }
}

#[cfg(unix)]
pub(crate) fn file_mode(metadata: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt as _;

    if metadata.permissions().mode() & 0o111 == 0 {
        0o644
    } else {
        0o755
    }
}

#[cfg(windows)]
pub(crate) const fn file_mode(_metadata: &fs::Metadata) -> u32 {
    0o644
}

#[cfg(unix)]
pub(crate) fn creation_mode(options: &mut cap_std::fs::OpenOptions, executable: bool) {
    use cap_std::fs::OpenOptionsExt as _;

    options.mode(if executable { 0o755 } else { 0o644 });
}

#[cfg(windows)]
pub(crate) const fn creation_mode(_options: &mut cap_std::fs::OpenOptions, _executable: bool) {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Ownership {
    Private,
    Foreign,
    #[cfg_attr(
        windows,
        expect(dead_code, reason = "Unix file modes use this classification")
    )]
    Exposed(u32),
    #[cfg_attr(
        unix,
        expect(dead_code, reason = "Windows ACLs use this classification")
    )]
    ExposedAcl,
}

#[cfg(unix)]
pub(crate) fn ownership(file: &fs::File) -> io::Result<Ownership> {
    use std::os::unix::fs::MetadataExt as _;

    let metadata = file.metadata()?;
    if metadata.uid() != rustix::process::geteuid().as_raw() {
        return Ok(Ownership::Foreign);
    }
    let mode = metadata.mode() & 0o777;
    if mode.trailing_zeros() >= 6 {
        Ok(Ownership::Private)
    } else {
        Ok(Ownership::Exposed(mode))
    }
}

#[cfg(windows)]
pub(crate) fn ownership(file: &fs::File) -> io::Result<Ownership> {
    windows_acl::ownership(file)
}

#[cfg(unix)]
#[expect(
    clippy::disallowed_methods,
    reason = "the state owner check opens a directory without following a link"
)]
pub(crate) fn open_private_dir(path: &Path) -> io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    no_follow(&mut options);
    options.open(path)
}

#[cfg(windows)]
pub(crate) fn open_private_dir(path: &Path) -> io::Result<fs::File> {
    windows_acl::open_private_dir(path)
}

#[cfg(unix)]
pub(crate) fn create_private_dir(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;

    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
}

#[cfg(windows)]
pub(crate) fn create_private_dir(path: &Path) -> io::Result<()> {
    windows_acl::create_private_dir(path)
}

#[expect(
    clippy::disallowed_methods,
    reason = "private state files are created only through these options"
)]
pub(crate) fn private_options() -> fs::OpenOptions {
    let mut options = fs::OpenOptions::new();
    owner_only(&mut options);
    no_follow(&mut options);
    options
}

#[cfg(unix)]
fn owner_only(options: &mut fs::OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt as _;

    options.mode(0o600);
}

#[cfg(windows)]
const fn owner_only(_options: &mut fs::OpenOptions) {}

#[cfg(unix)]
fn no_follow(options: &mut fs::OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt as _;

    let flags = rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK;
    options.custom_flags(flags.bits().cast_signed());
}

#[cfg(windows)]
fn no_follow(options: &mut fs::OpenOptions) {
    use std::os::windows::fs::OpenOptionsExt as _;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;

    options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
}

#[cfg(unix)]
pub(crate) fn sync_dir(path: &Path) -> io::Result<()> {
    fs::File::open(path)?.sync_all()
}

#[cfg(windows)]
#[expect(
    clippy::unnecessary_wraps,
    reason = "the shared state writer treats directory sync as a fallible platform operation"
)]
pub(crate) const fn sync_dir(_path: &Path) -> io::Result<()> {
    Ok(())
}
