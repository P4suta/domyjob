#![expect(
    clippy::disallowed_methods,
    reason = "the one module that writes domyjob's own state, always owner-only and atomically"
)]

use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use serde::Serialize;

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
}

fn io(action: &'static str, path: &Path) -> impl FnOnce(std::io::Error) -> StateError + use<> {
    let path = path.to_path_buf();
    move |source| StateError::Io {
        action,
        path,
        source,
    }
}

fn check_owner_only(path: &Path, meta: &std::fs::Metadata) -> Result<(), StateError> {
    match crate::platform::ownership(meta) {
        crate::platform::Ownership::Private => Ok(()),
        crate::platform::Ownership::OtherOwner => Err(StateError::Foreign {
            path: path.to_path_buf(),
        }),
        crate::platform::Ownership::Exposed(mode) => Err(StateError::Exposed {
            path: path.to_path_buf(),
            mode,
        }),
    }
}

fn create_private_dir(path: &Path) -> Result<(), StateError> {
    crate::platform::create_private_dir(path).map_err(io("securing", path))
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

fn create_private_file(path: &Path) -> std::io::Result<std::fs::File> {
    private_options().write(true).create_new(true).open(path)
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

fn sync_dir(dir: &Path) -> Result<(), StateError> {
    crate::platform::sync_dir(dir).map_err(io("syncing", dir))
}

fn private_options() -> std::fs::OpenOptions {
    crate::platform::private_options()
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

pub fn open_existing_lock(path: &Path) -> Result<Option<std::fs::File>, StateError> {
    crate::faults::at("state_file::lock", path).map_err(io("opening", path))?;
    match private_options().read(true).write(true).open(path) {
        Ok(file) => Ok(Some(file)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(io("opening", path)(error)),
    }
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
    crate::platform::let_owner_change(&mut perms);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[expect(
        clippy::disallowed_methods,
        reason = "the test locks a job's directory the way a careless job would"
    )]
    fn a_tree_a_job_locked_down_is_still_removed() {
        let tmp = tempfile::tempdir().unwrap();
        let tree = tmp.path().join("trash").join("job");
        let locked = tree.join("target").join("locked");
        std::fs::create_dir_all(&locked).unwrap();
        std::fs::write(locked.join("file"), b"x").unwrap();
        crate::platform::lock_down(&locked.join("file")).unwrap();
        crate::platform::lock_down(&locked).unwrap();
        crate::platform::lock_down(&tree.join("target")).unwrap();
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
        let meta = std::fs::metadata(&path).unwrap();
        assert_eq!(
            crate::platform::ownership(&meta),
            crate::platform::Ownership::Private
        );
        if crate::platform::expose(&path).unwrap() {
            assert!(matches!(
                read_json::<Vec<u8>>(&path),
                Err(StateError::Exposed { .. })
            ));
        }
    }
}
