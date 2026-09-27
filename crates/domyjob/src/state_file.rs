#![expect(
    clippy::disallowed_methods,
    reason = "the one module that writes domyjob's own state, always owner-only and atomically"
)]

use crate::failure::io;
use std::io::{ErrorKind, Write};
use std::marker::PhantomData;
use std::path::{Path, PathBuf};

use serde::Serialize;

#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error(transparent)]
    Io(#[from] crate::failure::IoFailure),
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
    #[error("{path} is not a regular state file")]
    NotFile { path: PathBuf },
    #[error("{path} exceeds its {limit}-byte state file budget")]
    TooLarge { path: PathBuf, limit: u64 },
}

#[derive(Clone, Copy)]
enum ReadBudget {
    Metadata,
    Audit,
    History,
}

impl ReadBudget {
    const fn bytes(self) -> u64 {
        match self {
            Self::Metadata => 1 << 20,
            Self::Audit => 16 << 20,
            Self::History => 64 << 20,
        }
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

fn check_private_file(path: &Path, file: &std::fs::File) -> Result<std::fs::Metadata, StateError> {
    let meta = file.metadata().map_err(io("checking", path))?;
    if !meta.is_file() || meta.file_type().is_symlink() {
        return Err(StateError::NotFile {
            path: path.to_path_buf(),
        });
    }
    check_owner_only(path, &meta)?;
    Ok(meta)
}

fn classify_open_error(path: &Path, error: std::io::Error) -> StateError {
    if std::fs::symlink_metadata(path).is_ok_and(|meta| !meta.is_file()) {
        StateError::NotFile {
            path: path.to_path_buf(),
        }
    } else {
        StateError::Io(io("opening", path)(error))
    }
}

fn create_private_dir(path: &Path) -> Result<(), StateError> {
    crate::platform::create_private_dir(path)
        .map_err(io("securing", path))
        .map_err(Into::into)
}

pub fn private_dir(path: &Path) -> Result<(), StateError> {
    crate::faults::at("state_file::dir", path).map_err(io("preparing", path))?;
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => check_owner_only(path, &meta),
        Ok(_) => Err(StateError::Io(crate::failure::IoFailure {
            action: "using",
            path: path.to_path_buf(),
            source: std::io::Error::other("it exists and is not a directory"),
        })),
        Err(e) if e.kind() == ErrorKind::NotFound => create_private_dir(path),
        Err(e) => Err(io("checking", path)(e).into()),
    }
}

fn opened_read(path: &Path) -> Result<Option<(std::fs::File, std::fs::Metadata)>, StateError> {
    crate::faults::at("state_file::read", path).map_err(io("reading", path))?;
    let file = match private_options().read(true).open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(classify_open_error(path, e)),
    };
    let meta = check_private_file(path, &file)?;
    Ok(Some((file, meta)))
}

pub fn open_read(path: &Path) -> Result<Option<std::fs::File>, StateError> {
    Ok(opened_read(path)?.map(|(file, _meta)| file))
}

fn read_limited(path: &Path, budget: ReadBudget) -> Result<Option<Vec<u8>>, StateError> {
    let Some((mut file, meta)) = opened_read(path)? else {
        return Ok(None);
    };
    let limit = budget.bytes();
    if meta.len() > limit {
        return Err(StateError::TooLarge {
            path: path.to_path_buf(),
            limit,
        });
    }
    let bytes = crate::bounded::to_end(&mut file, limit).map_err(io("reading", path))?;
    Ok(Some(bytes))
}

pub fn read_bytes(path: &Path) -> Result<Option<Vec<u8>>, StateError> {
    read_limited(path, ReadBudget::Metadata)
}

pub fn read_audit_bytes(path: &Path) -> Result<Option<Vec<u8>>, StateError> {
    read_limited(path, ReadBudget::Audit)
}

pub fn read_history_bytes(path: &Path) -> Result<Option<Vec<u8>>, StateError> {
    read_limited(path, ReadBudget::History)
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

pub fn write_bytes(path: &Path, bytes: &[u8]) -> Result<(), StateError> {
    crate::faults::at("state_file::write", path).map_err(io("writing", path))?;
    private_dir(path.parent().unwrap_or_else(|| Path::new(".")))?;
    Ok(crate::durable::write(
        path,
        bytes,
        crate::durable::Access::Private,
    )?)
}

fn sync_dir(dir: &Path) -> Result<(), StateError> {
    crate::platform::sync_dir(dir)
        .map_err(io("syncing", dir))
        .map_err(Into::into)
}

fn private_options() -> std::fs::OpenOptions {
    let mut options = crate::platform::private_options();
    crate::platform::no_follow(&mut options);
    options
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
    let file = private_options()
        .append(true)
        .create(true)
        .open(path)
        .map_err(|error| classify_open_error(path, error))?;
    check_private_file(path, &file)?;
    Ok(file)
}

pub fn overwrite_in_place(path: &Path, bytes: &[u8]) -> Result<(), StateError> {
    use std::io::Write as _;
    crate::faults::at("state_file::overwrite", path).map_err(io("writing", path))?;
    let mut file = private_options()
        .write(true)
        .open(path)
        .map_err(|error| classify_open_error(path, error))?;
    check_private_file(path, &file)?;
    file.write_all(bytes).map_err(io("writing", path))?;
    file.sync_all()
        .map_err(io("syncing", path))
        .map_err(Into::into)
}

pub fn cut_to(path: &Path, len: u64) -> Result<(), StateError> {
    crate::faults::at("state_file::cut", path).map_err(io("cutting", path))?;
    let file = private_options()
        .write(true)
        .open(path)
        .map_err(|error| classify_open_error(path, error))?;
    check_private_file(path, &file)?;
    file.set_len(len).map_err(io("cutting", path))?;
    file.sync_all()
        .map_err(io("syncing", path))
        .map_err(Into::into)
}

pub fn open_lock(path: &Path) -> Result<std::fs::File, StateError> {
    crate::faults::at("state_file::lock", path).map_err(io("opening", path))?;
    prepared_parent(path)?;
    let file = private_options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|error| classify_open_error(path, error))?;
    check_private_file(path, &file)?;
    Ok(file)
}

pub fn open_existing_lock(path: &Path) -> Result<Option<std::fs::File>, StateError> {
    crate::faults::at("state_file::lock", path).map_err(io("opening", path))?;
    match private_options().read(true).write(true).open(path) {
        Ok(file) => {
            check_private_file(path, &file)?;
            Ok(Some(file))
        }
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(classify_open_error(path, error)),
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
        .map_err(Into::into)
}

#[derive(Debug)]
pub struct Staged {
    staged: crate::durable::Staged,
    target: PathBuf,
}

pub fn stage(target: &Path) -> Result<Staged, StateError> {
    crate::faults::at("state_file::stage", target).map_err(io("staging", target))?;
    prepared_parent(target)?;
    Ok(Staged {
        staged: crate::durable::Staged::beside(target, crate::durable::Access::Private)?,
        target: target.to_path_buf(),
    })
}

impl Staged {
    pub fn write_all(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.staged.file().write_all(bytes)
    }

    pub fn commit(self) -> Result<(), StateError> {
        crate::faults::at("state_file::commit", &self.target)
            .map_err(io("storing", &self.target))?;
        self.staged.commit().map(drop).map_err(StateError::from)
    }

    pub fn discard(self) {
        drop(self.staged);
    }
}

pub fn remove_file(path: &Path) -> Result<(), StateError> {
    crate::faults::at("state_file::remove", path).map_err(io("removing", path))?;
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io("removing", path)(e).into()),
    }
}

pub fn publish_dir(staged: &Path, target: &Path) -> Result<(), StateError> {
    crate::faults::at("state_file::publish", target).map_err(io("publishing", target))?;
    prepared_parent(target)?;
    match std::fs::symlink_metadata(target) {
        Ok(_) => {
            return Err(StateError::Io(crate::failure::IoFailure {
                action: "publishing",
                path: target.to_path_buf(),
                source: std::io::Error::new(ErrorKind::AlreadyExists, "it already exists"),
            }));
        }
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => return Err(io("checking", target)(e).into()),
    }
    std::fs::rename(staged, target).map_err(io("publishing", target))?;
    sync_dir(target.parent().unwrap_or_else(|| Path::new(".")))
}

pub fn remove_dir_all(path: &Path) -> Result<(), StateError> {
    crate::faults::at("state_file::remove", path).map_err(io("removing", path))?;
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io("removing", path)(e).into()),
    }
}

pub fn replace_with(fresh: &Path, target: &Path) -> Result<(), StateError> {
    crate::faults::at("state_file::rename", target).map_err(io("replacing", target))?;
    std::fs::rename(fresh, target)
        .map_err(io("replacing", target))
        .map_err(Into::into)
}

pub fn move_aside(from: &Path, to: &Path) -> Result<(), StateError> {
    crate::faults::at("state_file::rename", from).map_err(io("moving aside", from))?;
    prepared_parent(to)?;
    std::fs::rename(from, to)
        .map_err(io("moving aside", from))
        .map_err(Into::into)
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

#[derive(Debug)]
pub struct StateFile<T> {
    path: PathBuf,
    value: PhantomData<fn() -> T>,
}

#[derive(Debug)]
pub struct LockedStateFile<T> {
    file: StateFile<T>,
    lock: crate::lock::OsLock,
}

impl<T> StateFile<T> {
    #[must_use]
    pub fn at(path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
            value: PhantomData,
        }
    }

    pub fn lock(self) -> Result<LockedStateFile<T>, StateError> {
        let mut lock_path = self.path.as_os_str().to_owned();
        lock_path.push(".lock");
        let lock = crate::lock::OsLock::exclusive(Path::new(&lock_path)).map_err(|error| {
            StateError::Io(crate::failure::IoFailure {
                action: "locking",
                path: self.path.clone(),
                source: std::io::Error::other(error.to_string()),
            })
        })?;
        Ok(LockedStateFile { file: self, lock })
    }
}

impl<T> LockedStateFile<T> {
    #[expect(
        clippy::needless_pass_by_ref_mut,
        reason = "an exclusive borrow prevents concurrent initialization through one lock guard"
    )]
    pub fn load_or_create<E>(
        &mut self,
        decode: impl FnOnce(&[u8]) -> Result<T, E>,
        create: impl FnOnce() -> Result<T, E>,
        encode: impl FnOnce(&T) -> Vec<u8>,
    ) -> Result<T, E>
    where
        E: From<StateError>,
    {
        if let Some(bytes) = read_bytes(&self.file.path)? {
            return decode(&bytes);
        }
        let value = create()?;
        write_bytes(&self.file.path, &encode(&value))?;
        Ok(value)
    }

    #[expect(
        clippy::needless_pass_by_ref_mut,
        reason = "an exclusive borrow prevents concurrent reads and writes through one lock guard"
    )]
    pub fn read(&mut self) -> Result<Option<T>, StateError>
    where
        T: crate::ingress::Ingress,
    {
        read_json(&self.file.path)
    }

    #[expect(
        clippy::needless_pass_by_ref_mut,
        reason = "an exclusive borrow prevents concurrent reads and writes through one lock guard"
    )]
    pub fn write(&mut self, value: &T) -> Result<(), StateError>
    where
        T: Serialize,
    {
        write_json(&self.file.path, value)
    }

    pub fn release(self) -> Result<(), StateError> {
        self.lock.release().map_err(|error| {
            StateError::Io(crate::failure::IoFailure {
                action: "unlocking",
                path: self.file.path,
                source: std::io::Error::other(error.to_string()),
            })
        })
    }
}

pub fn update_json<T, R>(
    path: &Path,
    empty: impl FnOnce() -> T,
    change: impl FnOnce(&mut T) -> R,
) -> Result<R, StateError>
where
    T: Serialize + crate::ingress::Ingress,
{
    let mut file = StateFile::<T>::at(path).lock()?;
    let mut value = file.read()?.unwrap_or_else(empty);
    let result = change(&mut value);
    file.write(&value)?;
    file.release()?;
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

    #[test]
    fn typed_state_file_serializes_concurrent_updates() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state").join("counter.json");
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let path = &path;
                scope.spawn(move || {
                    for _ in 0..8 {
                        let mut file = StateFile::<Vec<u8>>::at(path).lock().unwrap();
                        let mut values = file.read().unwrap().unwrap_or_default();
                        values.push(1);
                        file.write(&values).unwrap();
                        file.release().unwrap();
                    }
                });
            }
        });
        assert_eq!(read_json::<Vec<u8>>(&path).unwrap().unwrap().len(), 64);
    }

    #[test]
    fn a_state_file_name_cannot_redirect_reads_writes_or_locks() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("state");
        let target = dir.join("target");
        write_bytes(&target, b"safe").unwrap();
        let alias = dir.join("alias");
        if crate::platform::make_link("target", &alias).is_err() {
            return;
        }

        read_bytes(&alias).unwrap_err();
        open_append(&alias).unwrap_err();
        open_lock(&alias).unwrap_err();
        open_existing_lock(&alias).unwrap_err();
        overwrite_in_place(&alias, b"changed").unwrap_err();
        cut_to(&alias, 0).unwrap_err();
        assert_eq!(read_bytes(&target).unwrap(), Some(b"safe".to_vec()));
    }

    #[test]
    fn a_directory_as_a_state_file_has_the_same_error_on_each_system() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state").join("directory");
        private_dir(&path).unwrap();
        assert!(matches!(read_bytes(&path), Err(StateError::NotFile { .. })));
    }

    #[test]
    fn each_state_read_budget_refuses_a_larger_file_before_allocation() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state").join("oversized");
        write_bytes(&path, b"").unwrap();
        let file = private_options().write(true).open(&path).unwrap();
        for budget in [ReadBudget::Metadata, ReadBudget::Audit, ReadBudget::History] {
            file.set_len(budget.bytes() + 1).unwrap();
            let read = match budget {
                ReadBudget::Metadata => read_bytes(&path),
                ReadBudget::Audit => read_audit_bytes(&path),
                ReadBudget::History => read_history_bytes(&path),
            };
            assert!(matches!(
                read,
                Err(StateError::TooLarge { limit, .. }) if limit == budget.bytes()
            ));
        }
    }
}
