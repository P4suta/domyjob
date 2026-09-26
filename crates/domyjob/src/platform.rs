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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ownership {
    Private,
    OtherOwner,
    Exposed(u32),
}

#[cfg(unix)]
#[must_use]
pub fn ownership(meta: &std::fs::Metadata) -> Ownership {
    use std::os::unix::fs::MetadataExt;
    const GROUP_AND_OTHER_BITS: u32 = 6;
    if meta.uid() != rustix::process::geteuid().as_raw() {
        return Ownership::OtherOwner;
    }
    let mode = MetadataExt::mode(meta) & 0o777;
    if mode.trailing_zeros() >= GROUP_AND_OTHER_BITS {
        Ownership::Private
    } else {
        Ownership::Exposed(mode)
    }
}

#[cfg(not(unix))]
#[must_use]
pub const fn ownership(_meta: &std::fs::Metadata) -> Ownership {
    Ownership::Private
}

pub fn create_private_dir(path: &Path) -> std::io::Result<()> {
    create_private_dir_in(path)
}

#[cfg(unix)]
fn create_private_dir_in(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
}

#[cfg(windows)]
#[expect(
    unsafe_code,
    reason = "reading the current user's SID from the process token needs the token API"
)]
fn current_user_sid() -> std::io::Result<crate::domain::WindowsSid> {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, LocalFree};
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    let refuse = |detail: &str| std::io::Error::other(detail.to_owned());
    let mut token: HANDLE = std::ptr::null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token) } == 0 {
        return Err(refuse("the process token cannot be opened"));
    }
    let mut needed = 0u32;
    unsafe { GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &raw mut needed) };
    let Ok(capacity) = usize::try_from(needed) else {
        return Err(refuse("the token is too large"));
    };
    let mut buffer = vec![0u8; capacity];
    let asked = unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            buffer.as_mut_ptr().cast(),
            needed,
            &raw mut needed,
        )
    };
    unsafe { CloseHandle(token) };
    if asked == 0 || buffer.len() < size_of::<TOKEN_USER>() {
        return Err(refuse("the token has no user"));
    }
    let user: TOKEN_USER = unsafe { std::ptr::read_unaligned(buffer.as_ptr().cast()) };
    let mut wide: *mut u16 = std::ptr::null_mut();
    if unsafe { ConvertSidToStringSidW(user.User.Sid, &raw mut wide) } == 0 || wide.is_null() {
        return Err(refuse("the SID cannot be rendered"));
    }
    let mut len = 0usize;
    while unsafe { *wide.add(len) } != 0 {
        len = len.saturating_add(1);
    }
    let text = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(wide, len) });
    unsafe { LocalFree(wide.cast()) };
    crate::domain::WindowsSid::try_from(text).map_err(|e| refuse(&e.to_string()))
}

#[cfg(windows)]
#[expect(
    clippy::disallowed_methods,
    reason = "the one place that creates a private directory, then confines it with an ACL"
)]
fn create_private_dir_in(path: &Path) -> std::io::Result<()> {
    use crate::template::Arg;
    std::fs::create_dir_all(path)?;
    let sid = current_user_sid()?;
    let args = vec![
        Arg::path(path),
        Arg::literal("/inheritance:r"),
        Arg::literal("/grant:r"),
        Arg::concat(&[
            Arg::literal("*"),
            Arg::word(&sid),
            Arg::literal(":(OI)(CI)F"),
        ]),
        Arg::literal("/grant:r"),
        Arg::literal("*S-1-5-18:(OI)(CI)F"),
        Arg::literal("/Q"),
    ];
    let status = crate::spawn::Invocation::new(Arg::literal("icacls"), args)
        .command()
        .stdout(std::process::Stdio::null())
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "icacls exited with {status}"
        )))
    }
}

#[expect(
    clippy::disallowed_methods,
    reason = "the options every private state file is opened with"
)]
#[must_use]
pub fn private_options() -> std::fs::OpenOptions {
    let mut options = std::fs::OpenOptions::new();
    owner_only(&mut options);
    options
}

#[cfg(unix)]
fn owner_only(options: &mut std::fs::OpenOptions) {
    std::os::unix::fs::OpenOptionsExt::mode(options, 0o600);
}

#[cfg(not(unix))]
const fn owner_only(_options: &mut std::fs::OpenOptions) {}

#[cfg(unix)]
pub fn sync_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::File::open(dir).and_then(|handle| handle.sync_all())
}

#[cfg(not(unix))]
#[expect(
    clippy::unnecessary_wraps,
    reason = "shares the signature of systems that can sync a directory"
)]
pub const fn sync_dir(_dir: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(unix)]
pub fn let_owner_change(perms: &mut std::fs::Permissions) {
    std::os::unix::fs::PermissionsExt::set_mode(perms, 0o700);
}

#[cfg(not(unix))]
#[expect(
    clippy::permissions_set_readonly_false,
    reason = "clearing the read-only attribute a job left is exactly what removing its files needs"
)]
pub fn let_owner_change(perms: &mut std::fs::Permissions) {
    perms.set_readonly(false);
}

#[cfg(test)]
#[expect(
    clippy::disallowed_methods,
    reason = "tests lock a path down the way a careless job would"
)]
pub fn lock_down(path: &Path) -> std::io::Result<()> {
    let mut perms = std::fs::symlink_metadata(path)?.permissions();
    forbid(&mut perms);
    std::fs::set_permissions(path, perms)
}

#[cfg(all(test, unix))]
fn forbid(perms: &mut std::fs::Permissions) {
    std::os::unix::fs::PermissionsExt::set_mode(perms, 0o500);
}

#[cfg(all(test, not(unix)))]
fn forbid(perms: &mut std::fs::Permissions) {
    perms.set_readonly(true);
}

#[cfg(all(test, unix))]
#[expect(
    clippy::disallowed_methods,
    reason = "tests widen a state file the way a careless user would"
)]
pub fn expose(path: &Path) -> std::io::Result<bool> {
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o644))?;
    Ok(true)
}

#[cfg(all(test, not(unix)))]
#[expect(
    clippy::unnecessary_wraps,
    reason = "shares the signature of systems where group and other bits exist"
)]
pub const fn expose(_path: &Path) -> std::io::Result<bool> {
    Ok(false)
}

#[cfg(all(test, unix))]
pub(crate) struct ReadOnly(std::path::PathBuf);

#[cfg(all(test, unix))]
#[expect(
    clippy::disallowed_methods,
    reason = "tests make a directory read-only and must give it back even when they panic"
)]
impl ReadOnly {
    pub(crate) fn make(dir: &Path) -> std::io::Result<Self> {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::create_dir_all(dir)?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o500))?;
        Ok(Self(dir.to_path_buf()))
    }
}

#[cfg(all(test, unix))]
#[expect(
    clippy::disallowed_methods,
    reason = "tests make a directory read-only and must give it back even when they panic"
)]
impl Drop for ReadOnly {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt as _;
        match std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o700)) {
            Ok(()) | Err(_) => {}
        }
    }
}
