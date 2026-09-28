#![expect(
    clippy::redundant_pub_crate,
    reason = "the binary composition root uses this private module"
)]

use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;

fn variable(name: &str) -> Option<OsString> {
    std::env::var_os(name).filter(|value| !value.is_empty())
}

pub(crate) fn home() -> std::io::Result<PathBuf> {
    variable("HOME")
        .or_else(|| variable("USERPROFILE"))
        .map(PathBuf::from)
        .ok_or_else(|| std::io::Error::other("the user home directory is unavailable"))
}

pub(crate) fn state() -> std::io::Result<PathBuf> {
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

#[cfg(unix)]
pub(crate) fn reparse_point(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(windows)]
pub(crate) fn reparse_point(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt as _;

    metadata.file_attributes() & 0x400 != 0
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
