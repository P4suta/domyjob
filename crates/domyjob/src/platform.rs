#[cfg(unix)]
pub use std::os::unix::net::{UnixListener, UnixStream};

#[cfg(windows)]
pub use uds_windows::{UnixListener, UnixStream};

pub const SOCKET_PATH_LIMIT: usize = if cfg!(windows) { 107 } else { 103 };

use std::path::Path;

use crate::snapshot::Mode;

pub const LINKS: bool = cfg!(unix);
pub const MODES: bool = cfg!(unix);

const REGULAR: u32 = 0o644;
const EXECUTABLE: u32 = 0o755;

const fn bits(mode: Mode) -> u32 {
    match mode {
        Mode::Regular => REGULAR,
        Mode::Executable => EXECUTABLE,
    }
}

const fn mode_from(bits: u32) -> Mode {
    if bits & 0o111 == 0 {
        Mode::Regular
    } else {
        Mode::Executable
    }
}

pub trait Moded {
    fn mode(&self) -> Mode;
}

impl Moded for std::fs::Metadata {
    fn mode(&self) -> Mode {
        mode_from(std_bits(self))
    }
}

impl Moded for cap_std::fs::Metadata {
    fn mode(&self) -> Mode {
        mode_from(cap_bits(self))
    }
}

#[cfg(unix)]
fn std_bits(meta: &std::fs::Metadata) -> u32 {
    std::os::unix::fs::PermissionsExt::mode(&meta.permissions())
}

#[cfg(not(unix))]
const fn std_bits(_meta: &std::fs::Metadata) -> u32 {
    REGULAR
}

#[cfg(unix)]
fn cap_bits(meta: &cap_std::fs::Metadata) -> u32 {
    cap_std::fs::PermissionsExt::mode(&meta.permissions())
}

#[cfg(not(unix))]
const fn cap_bits(_meta: &cap_std::fs::Metadata) -> u32 {
    REGULAR
}

pub fn create_as(options: &mut cap_std::fs::OpenOptions, mode: Mode) {
    create_with_bits(options, bits(mode));
}

#[cfg(unix)]
fn create_with_bits(options: &mut cap_std::fs::OpenOptions, bits: u32) {
    cap_std::fs::OpenOptionsExt::mode(options, bits);
}

#[cfg(not(unix))]
const fn create_with_bits(_options: &mut cap_std::fs::OpenOptions, _bits: u32) {}

pub fn link(dir: &cap_std::fs::Dir, target: &str, at: &Path) -> std::io::Result<()> {
    link_in(dir, target, at)
}

#[cfg(unix)]
fn link_in(dir: &cap_std::fs::Dir, target: &str, at: &Path) -> std::io::Result<()> {
    dir.symlink_contents(target, at)
}

#[cfg(not(unix))]
fn link_in(_dir: &cap_std::fs::Dir, target: &str, _at: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        format!("a symbolic link to {target} cannot be made on this system"),
    ))
}

#[cfg(test)]
pub fn make_link(target: &str, at: &Path) -> std::io::Result<()> {
    make_link_in(target, at)
}

#[cfg(all(test, unix))]
fn make_link_in(target: &str, at: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, at)
}

#[cfg(all(test, not(unix)))]
fn make_link_in(target: &str, _at: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        format!("a symbolic link to {target} cannot be made on this system"),
    ))
}

#[cfg(all(test, unix))]
#[expect(
    clippy::disallowed_methods,
    reason = "tests give fixture files the mode a project would have"
)]
pub fn set_mode(path: &Path, mode: Mode) -> std::io::Result<()> {
    std::fs::set_permissions(
        path,
        std::os::unix::fs::PermissionsExt::from_mode(bits(mode)),
    )
}

#[cfg(all(test, not(unix)))]
#[expect(
    clippy::unnecessary_wraps,
    reason = "shares the signature of systems that keep an executable bit"
)]
pub const fn set_mode(_path: &Path, _mode: Mode) -> std::io::Result<()> {
    Ok(())
}
