#![expect(
    clippy::disallowed_methods,
    reason = "the one module that writes domyjob's own state, always owner-only and atomically"
)]

use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use serde::Serialize;

#[cfg(unix)]
const GROUP_AND_OTHER_BITS: u32 = 6;

static STAGED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("{action} {path}: {source}")]
    Io {
        action: &'static str,
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{path} is malformed: {source}")]
    Json {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error(
        "{path} can be read or changed by other users (mode {mode:o}); run `chmod go-rwx {path}` and retry"
    )]
    Exposed { path: PathBuf, mode: u32 },
    #[error("{path} belongs to another user; refusing to trust it")]
    Foreign { path: PathBuf },
    #[error("securing {path} failed: {detail}")]
    Acl { path: PathBuf, detail: String },
}

fn io(action: &'static str, path: &Path) -> impl FnOnce(std::io::Error) -> StateError + use<> {
    let path = path.to_path_buf();
    move |source| StateError::Io {
        action,
        path,
        source,
    }
}

#[cfg(unix)]
fn check_owner_only(path: &Path, meta: &std::fs::Metadata) -> Result<(), StateError> {
    use std::os::unix::fs::MetadataExt;
    if meta.uid() != rustix::process::geteuid().as_raw() {
        return Err(StateError::Foreign {
            path: path.to_path_buf(),
        });
    }
    let mode = meta.mode() & 0o777;
    if mode.trailing_zeros() >= GROUP_AND_OTHER_BITS {
        Ok(())
    } else {
        Err(StateError::Exposed {
            path: path.to_path_buf(),
            mode,
        })
    }
}

#[cfg(not(unix))]
#[expect(
    clippy::unnecessary_wraps,
    reason = "shares the Unix signature; Windows confines the directory with an ACL when it is created"
)]
const fn check_owner_only(_path: &Path, _meta: &std::fs::Metadata) -> Result<(), StateError> {
    Ok(())
}

#[cfg(unix)]
fn create_private_dir(path: &Path) -> Result<(), StateError> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .map_err(io("creating", path))
}

#[cfg(windows)]
#[expect(
    unsafe_code,
    reason = "reading the current user's SID from the process token needs the token API"
)]
fn current_user_sid(path: &Path) -> Result<crate::domain::WindowsSid, StateError> {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, LocalFree};
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    let refuse = |detail: &str| StateError::Acl {
        path: path.to_path_buf(),
        detail: detail.to_owned(),
    };
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
fn create_private_dir(path: &Path) -> Result<(), StateError> {
    use crate::template::Arg;
    std::fs::create_dir_all(path).map_err(io("creating", path))?;
    let sid = current_user_sid(path)?;
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
        .status()
        .map_err(io("running icacls on", path))?;
    if status.success() {
        Ok(())
    } else {
        Err(StateError::Acl {
            path: path.to_path_buf(),
            detail: format!("icacls exited with {status}"),
        })
    }
}

pub fn private_dir(path: &Path) -> Result<(), StateError> {
    crate::faults::at("state_file::dir", path).map_err(io("preparing", path))?;
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => check_owner_only(path, &meta),
        Ok(_) => Err(StateError::Io {
            action: "using",
            path: path.to_path_buf(),
            source: std::io::Error::other("it exists and is not a directory"),
        }),
        Err(e) if e.kind() == ErrorKind::NotFound => create_private_dir(path),
        Err(e) => Err(io("checking", path)(e)),
    }
}

pub fn read_bytes(path: &Path) -> Result<Option<Vec<u8>>, StateError> {
    crate::faults::at("state_file::read", path).map_err(io("reading", path))?;
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io("checking", path)(e)),
    };
    check_owner_only(path, &meta)?;
    std::fs::read(path).map(Some).map_err(io("reading", path))
}

pub fn read_json<T: crate::ingress::Ingress>(path: &Path) -> Result<Option<T>, StateError> {
    match read_bytes(path)? {
        Some(bytes) => crate::ingress::json(&bytes)
            .map(Some)
            .map_err(|source| StateError::Json {
                path: path.to_path_buf(),
                source,
            }),
        None => Ok(None),
    }
}

#[cfg(unix)]
fn create_private_file(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn create_private_file(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
}

pub fn write_bytes(path: &Path, bytes: &[u8]) -> Result<(), StateError> {
    crate::faults::at("state_file::write", path).map_err(io("writing", path))?;
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    private_dir(dir)?;
    let mut name = path.file_name().unwrap_or_default().to_owned();
    let unique = STAGED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    name.push(format!(".{}.{unique}.tmp", std::process::id()));
    let staged = dir.join(name);
    let mut file = create_private_file(&staged).map_err(io("creating", &staged))?;
    file.write_all(bytes).map_err(io("writing", &staged))?;
    file.sync_all().map_err(io("syncing", &staged))?;
    drop(file);
    std::fs::rename(&staged, path).map_err(io("replacing", path))?;
    sync_dir(dir)
}

#[cfg(unix)]
fn sync_dir(dir: &Path) -> Result<(), StateError> {
    std::fs::File::open(dir)
        .and_then(|handle| handle.sync_all())
        .map_err(io("syncing", dir))
}

#[cfg(not(unix))]
#[expect(
    clippy::unnecessary_wraps,
    reason = "shares the Unix signature; Windows has no directory handle to sync"
)]
const fn sync_dir(_dir: &Path) -> Result<(), StateError> {
    Ok(())
}

#[cfg(unix)]
fn private_options() -> std::fs::OpenOptions {
    use std::os::unix::fs::OpenOptionsExt;
    let mut options = std::fs::OpenOptions::new();
    options.mode(0o600);
    options
}

#[cfg(not(unix))]
fn private_options() -> std::fs::OpenOptions {
    std::fs::OpenOptions::new()
}

fn prepared_parent(path: &Path) -> Result<(), StateError> {
    match path.parent() {
        Some(parent) => private_dir(parent),
        None => Ok(()),
    }
}

pub fn open_append(path: &Path) -> Result<std::fs::File, StateError> {
    crate::faults::at("state_file::append", path).map_err(io("opening", path))?;
    prepared_parent(path)?;
    private_options()
        .append(true)
        .create(true)
        .open(path)
        .map_err(io("opening", path))
}

pub fn overwrite_in_place(path: &Path, bytes: &[u8]) -> Result<(), StateError> {
    use std::io::Write as _;
    crate::faults::at("state_file::overwrite", path).map_err(io("writing", path))?;
    let mut file = private_options()
        .write(true)
        .open(path)
        .map_err(io("opening", path))?;
    file.write_all(bytes).map_err(io("writing", path))?;
    file.sync_all().map_err(io("syncing", path))
}

pub fn cut_to(path: &Path, len: u64) -> Result<(), StateError> {
    crate::faults::at("state_file::cut", path).map_err(io("cutting", path))?;
    let file = private_options()
        .write(true)
        .open(path)
        .map_err(io("opening", path))?;
    file.set_len(len).map_err(io("cutting", path))?;
    file.sync_all().map_err(io("syncing", path))
}

pub fn open_lock(path: &Path) -> Result<std::fs::File, StateError> {
    crate::faults::at("state_file::lock", path).map_err(io("opening", path))?;
    prepared_parent(path)?;
    private_options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(io("opening", path))
}

pub fn create_empty(path: &Path) -> Result<(), StateError> {
    crate::faults::at("state_file::create", path).map_err(io("creating", path))?;
    prepared_parent(path)?;
    private_options()
        .write(true)
        .create_new(true)
        .open(path)
        .map(drop)
        .map_err(io("creating", path))
}

#[derive(Debug)]
pub struct Staged {
    file: std::fs::File,
    incoming: PathBuf,
    target: PathBuf,
}

pub fn stage(target: &Path) -> Result<Staged, StateError> {
    crate::faults::at("state_file::stage", target).map_err(io("staging", target))?;
    prepared_parent(target)?;
    let mut name = target.as_os_str().to_owned();
    name.push(format!(
        ".{}.{}.incoming",
        std::process::id(),
        STAGED.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let staged = PathBuf::from(name);
    let file = private_options()
        .write(true)
        .create_new(true)
        .open(&staged)
        .map_err(io("creating", &staged))?;
    Ok(Staged {
        file,
        incoming: staged,
        target: target.to_path_buf(),
    })
}

impl Staged {
    pub fn write_all(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        use std::io::Write;
        self.file.write_all(bytes)
    }

    pub fn commit(self) -> Result<(), StateError> {
        crate::faults::at("state_file::commit", &self.target)
            .map_err(io("storing", &self.target))?;
        self.file
            .sync_all()
            .map_err(io("syncing", &self.incoming))?;
        drop(self.file);
        std::fs::rename(&self.incoming, &self.target).map_err(io("storing", &self.target))
    }

    pub fn discard(self) -> Result<(), StateError> {
        drop(self.file);
        remove_file(&self.incoming)
    }
}

pub fn remove_file(path: &Path) -> Result<(), StateError> {
    crate::faults::at("state_file::remove", path).map_err(io("removing", path))?;
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io("removing", path)(e)),
    }
}

pub fn publish_dir(staged: &Path, target: &Path) -> Result<(), StateError> {
    crate::faults::at("state_file::publish", target).map_err(io("publishing", target))?;
    prepared_parent(target)?;
    match std::fs::symlink_metadata(target) {
        Ok(_) => {
            return Err(StateError::Io {
                action: "publishing",
                path: target.to_path_buf(),
                source: std::io::Error::new(ErrorKind::AlreadyExists, "it already exists"),
            });
        }
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => return Err(io("checking", target)(e)),
    }
    std::fs::rename(staged, target).map_err(io("publishing", target))?;
    sync_dir(target.parent().unwrap_or_else(|| Path::new(".")))
}

pub fn remove_dir_all(path: &Path) -> Result<(), StateError> {
    crate::faults::at("state_file::remove", path).map_err(io("removing", path))?;
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io("removing", path)(e)),
    }
}

pub fn replace_with(fresh: &Path, target: &Path) -> Result<(), StateError> {
    crate::faults::at("state_file::rename", target).map_err(io("replacing", target))?;
    std::fs::rename(fresh, target).map_err(io("replacing", target))
}

pub fn move_aside(from: &Path, to: &Path) -> Result<(), StateError> {
    crate::faults::at("state_file::rename", from).map_err(io("moving aside", from))?;
    prepared_parent(to)?;
    std::fs::rename(from, to).map_err(io("moving aside", from))
}

pub fn remove_tree_forcibly(path: &Path) -> Result<(), StateError> {
    if remove_dir_all(path).is_ok() {
        return Ok(());
    }
    make_removable(path);
    remove_dir_all(path)
}

fn make_removable(path: &Path) {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return;
    };
    if !meta.is_dir() {
        return;
    }
    let mut perms = meta.permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        perms.set_mode(0o700);
    }
    #[cfg(not(unix))]
    #[expect(
        clippy::permissions_set_readonly_false,
        reason = "clearing the read-only attribute a job left is exactly what removing its files needs"
    )]
    {
        perms.set_readonly(false);
    }
    match std::fs::set_permissions(path, perms) {
        Ok(()) | Err(_) => {}
    }
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            make_removable(&entry.path());
        }
    }
}

pub fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), StateError> {
    let bytes = serde_json::to_vec_pretty(value).map_err(|source| StateError::Json {
        path: path.to_path_buf(),
        source,
    })?;
    write_bytes(path, &bytes)
}

pub fn update_json<T, R>(
    path: &Path,
    empty: impl FnOnce() -> T,
    change: impl FnOnce(&mut T) -> R,
) -> Result<R, StateError>
where
    T: Serialize + crate::ingress::Ingress,
{
    let mut lock_path = path.as_os_str().to_owned();
    lock_path.push(".lock");
    let lock =
        crate::lock::OsLock::exclusive(Path::new(&lock_path)).map_err(|e| StateError::Io {
            action: "locking",
            path: path.to_path_buf(),
            source: std::io::Error::other(e.to_string()),
        })?;
    let mut value = read_json(path)?.unwrap_or_else(empty);
    let result = change(&mut value);
    write_json(path, &value)?;
    lock.release().map_err(|e| StateError::Io {
        action: "unlocking",
        path: path.to_path_buf(),
        source: std::io::Error::other(e.to_string()),
    })?;
    Ok(result)
}

#[cfg(all(test, unix))]
pub(crate) struct ReadOnly(PathBuf);

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

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    #[expect(
        clippy::disallowed_methods,
        reason = "the test locks a job's directory the way a careless job would"
    )]
    fn a_tree_a_job_locked_down_is_still_removed() {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = tempfile::tempdir().unwrap();
        let tree = tmp.path().join("trash").join("job");
        let locked = tree.join("target").join("locked");
        std::fs::create_dir_all(&locked).unwrap();
        std::fs::write(locked.join("file"), b"x").unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        std::fs::set_permissions(tree.join("target"), std::fs::Permissions::from_mode(0o500))
            .unwrap();
        remove_tree_forcibly(&tree).unwrap();
        assert!(!tree.exists());
    }

    #[test]
    fn state_is_private_atomic_and_refused_when_exposed() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state").join("trust.json");
        write_json(&path, &vec![1, 2, 3]).unwrap();
        assert_eq!(read_json::<Vec<u8>>(&path).unwrap(), Some(vec![1, 2, 3]));
        let total = update_json(&path, Vec::new, |values: &mut Vec<u8>| {
            values.push(4);
            values.len()
        })
        .unwrap();
        assert_eq!(total, 4);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert!(matches!(
                read_json::<Vec<u8>>(&path),
                Err(StateError::Exposed { .. })
            ));
        }
    }
}
