use std::fs::{self, File};
use std::io::{self, ErrorKind, Read, Write};
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::platform::{self, Ownership};

mod raw {
    #![expect(
        clippy::disallowed_methods,
        reason = "private state is created, replaced, and deleted only through `state_io`"
    )]

    use std::fs::OpenOptions;
    use std::io;
    use std::path::Path;

    pub(super) fn exclusive(options: &mut OpenOptions) -> &mut OpenOptions {
        options.create_new(true)
    }

    pub(super) fn remove_file(path: &Path) -> io::Result<()> {
        std::fs::remove_file(path)
    }

    pub(super) fn rename(from: &super::PrivateDirectory<'_>, to: &Path) -> io::Result<()> {
        std::fs::rename(from.0, to)
    }

    pub(super) fn exclusive_relative(options: &mut cap_std::fs::OpenOptions) {
        options.create_new(true);
    }
}

const MAX_RECORD_BYTES: u64 = 1_048_576;

#[derive(Debug, Error)]
pub(crate) enum StateError {
    #[error(transparent)]
    Io(#[from] IoFailure),
    #[error("{path} belongs to another user")]
    Foreign { path: PathBuf },
    #[error("{path} is accessible to other users: {how}")]
    Exposed {
        path: PathBuf,
        how: platform::Exposure,
    },
    #[error("{path} is not a regular private file")]
    NotFile { path: PathBuf },
    #[error("{path} is not a private directory")]
    NotDirectory { path: PathBuf },
    #[error("{path} exceeds the 1 MiB record limit")]
    TooLarge { path: PathBuf },
}

#[derive(Debug, Error)]
#[error("{action} {path}: {source}")]
pub(crate) struct IoFailure {
    action: &'static str,
    path: PathBuf,
    source: io::Error,
}

fn io_at(path: &Path, source: io::Error) -> StateError {
    IoFailure {
        action: "accessing state at",
        path: path.to_path_buf(),
        source,
    }
    .into()
}

fn check_owner(path: &Path, file: &File) -> Result<(), StateError> {
    match platform::ownership(file).map_err(|error| io_at(path, error))? {
        Ownership::Private => Ok(()),
        Ownership::Foreign => Err(StateError::Foreign {
            path: path.to_path_buf(),
        }),
        Ownership::Exposed(how) => Err(StateError::Exposed {
            path: path.to_path_buf(),
            how,
        }),
    }
}

fn check_file(path: &Path, file: &File) -> Result<fs::Metadata, StateError> {
    let metadata = file.metadata().map_err(|error| io_at(path, error))?;
    if !metadata.is_file() || platform::reparse_point(&metadata) {
        return Err(StateError::NotFile {
            path: path.to_path_buf(),
        });
    }
    check_owner(path, file)?;
    Ok(metadata)
}

fn classify_open(path: &Path, error: io::Error) -> StateError {
    match fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.is_file() || platform::reparse_point(&metadata) => {
            StateError::NotFile {
                path: path.to_path_buf(),
            }
        }
        Ok(_) | Err(_) => io_at(path, error),
    }
}

pub(crate) fn private_dir(path: &Path) -> Result<(), StateError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !platform::reparse_point(&metadata) => {}
        Ok(_blocked) => {
            return Err(StateError::NotDirectory {
                path: path.to_path_buf(),
            });
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {
            platform::create_private_dir(path).map_err(|creating| io_at(path, creating))?;
        }
        Err(error) => return Err(io_at(path, error)),
    }
    let directory = platform::open_private_dir(path).map_err(|error| io_at(path, error))?;
    let metadata = directory.metadata().map_err(|error| io_at(path, error))?;
    if !metadata.is_dir() || platform::reparse_point(&metadata) {
        return Err(StateError::NotDirectory {
            path: path.to_path_buf(),
        });
    }
    check_owner(path, &directory)
}

struct PrivateDirectory<'a>(&'a Path);

impl<'a> PrivateDirectory<'a> {
    fn checked(path: &'a Path) -> Result<Self, StateError> {
        private_dir(path)?;
        Ok(Self(path))
    }
}

fn parent(path: &Path) -> Result<&Path, StateError> {
    path.parent()
        .ok_or_else(|| io_at(path, io::Error::other("state path has no parent")))
}

#[derive(Debug, Clone, Copy)]
enum ExistingFile {
    Read,
    Lock,
}

fn open_existing(path: &Path, kind: ExistingFile) -> Result<Option<File>, StateError> {
    let mut options = platform::private_options();
    match kind {
        ExistingFile::Read => {
            options.read(true);
        }
        ExistingFile::Lock => {
            options.read(true).write(true);
        }
    }
    let file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(classify_open(path, error)),
    };
    check_file(path, &file)?;
    Ok(Some(file))
}

pub(crate) fn open_read(path: &Path) -> Result<Option<File>, StateError> {
    open_existing(path, ExistingFile::Read)
}

pub(crate) fn read_bytes(path: &Path) -> Result<Option<Vec<u8>>, StateError> {
    let Some(mut file) = open_read(path)? else {
        return Ok(None);
    };
    let size = file.metadata().map_err(|error| io_at(path, error))?.len();
    if size > MAX_RECORD_BYTES {
        return Err(StateError::TooLarge {
            path: path.to_path_buf(),
        });
    }
    let length = usize::try_from(size).map_err(|error| io_at(path, io::Error::other(error)))?;
    let mut bytes = vec![0_u8; length];
    file.read_exact(&mut bytes)
        .map_err(|error| io_at(path, error))?;
    let mut extra = [0_u8; 1];
    if file.read(&mut extra).map_err(|error| io_at(path, error))? != 0 {
        return Err(StateError::TooLarge {
            path: path.to_path_buf(),
        });
    }
    Ok(Some(bytes))
}

pub(crate) fn write_bytes(path: &Path, bytes: &[u8]) -> Result<(), StateError> {
    if bytes.len()
        > usize::try_from(MAX_RECORD_BYTES).map_err(|error| io_at(path, io::Error::other(error)))?
    {
        return Err(StateError::TooLarge {
            path: path.to_path_buf(),
        });
    }
    let directory = parent(path)?;
    private_dir(directory)?;
    let mut staged =
        tempfile::NamedTempFile::new_in(directory).map_err(|error| io_at(path, error))?;
    check_file(staged.path(), staged.as_file())?;
    staged
        .write_all(bytes)
        .map_err(|error| io_at(path, error))?;
    platform::replacement::StagedFile::sync_and_close(staged)
        .and_then(|staged| staged.replace(path))
        .map_err(|error| io_at(path, error))?;
    platform::sync_dir(directory).map_err(|error| io_at(directory, error))?;
    Ok(())
}

pub(crate) fn existing_directory(path: &Path) -> Result<Option<cap_std::fs::Dir>, StateError> {
    existing_directory_using(path, platform::open_private_dir)
}

fn existing_directory_using(
    path: &Path,
    open: impl FnOnce(&Path) -> io::Result<File>,
) -> Result<Option<cap_std::fs::Dir>, StateError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !platform::reparse_point(&metadata) => {}
        Ok(_) => return Err(StateError::NotDirectory { path: path.into() }),
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io_at(path, error)),
    }
    let retained = match open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io_at(path, error)),
    };
    let metadata = retained.metadata().map_err(|error| io_at(path, error))?;
    if !metadata.is_dir() || platform::reparse_point(&metadata) {
        return Err(StateError::NotDirectory { path: path.into() });
    }
    check_owner(path, &retained)?;
    Ok(Some(cap_std::fs::Dir::from_std_file(retained)))
}

fn mark_directory(directory: &cap_std::fs::Dir, path: &Path) -> Result<(), StateError> {
    let name = path
        .file_name()
        .ok_or_else(|| io_at(path, io::Error::other("marker has no file name")))?;
    let mut options = cap_std::fs::OpenOptions::new();
    options.write(true);
    raw::exclusive_relative(&mut options);
    platform::private_creation_mode(&mut options);
    let file = match directory.open_with(name, &options) {
        Ok(file) => file,
        Err(existing) if existing.kind() == ErrorKind::AlreadyExists => {
            let metadata = directory
                .symlink_metadata(name)
                .map_err(|error| io_at(path, error))?;
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                return Err(StateError::NotFile { path: path.into() });
            }
            let mut existing_options = platform::private_relative_options();
            existing_options.write(true);
            directory
                .open_with(name, &existing_options)
                .map_err(|error| io_at(path, error))?
        }
        Err(error) => return Err(io_at(path, error)),
    }
    .into_std();
    let metadata = check_file(path, &file)?;
    if metadata.len() != 0 {
        return Err(io_at(
            path,
            io::Error::other("completion marker is not empty"),
        ));
    }
    file.sync_all().map_err(|error| io_at(path, error))?;
    let retained = directory
        .try_clone()
        .map_err(|error| io_at(path, error))?
        .into_std_file();
    platform::sync_open_dir(&retained).map_err(|error| io_at(path, error))
}

pub(crate) fn mark_existing(path: &Path) -> Result<bool, StateError> {
    let Some(directory) = existing_directory(parent(path)?)? else {
        return Ok(false);
    };
    mark_directory(&directory, path)?;
    Ok(true)
}

pub(crate) fn open_lock_in_existing_directory(path: &Path) -> Result<Option<File>, StateError> {
    let Some(directory) = existing_directory(parent(path)?)? else {
        return Ok(None);
    };
    open_directory_lock(&directory, path)
}

fn open_directory_lock(
    directory: &cap_std::fs::Dir,
    path: &Path,
) -> Result<Option<File>, StateError> {
    let name = path
        .file_name()
        .ok_or_else(|| io_at(path, io::Error::other("lock has no file name")))?;
    let mut options = platform::private_relative_options();
    options.read(true).write(true).create(true).truncate(false);
    let file = match directory.open_with(name, &options) {
        Ok(file) => file.into_std(),
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(classify_open(path, error)),
    };
    check_file(path, &file)?;
    Ok(Some(file))
}

#[derive(Debug, Clone, Copy)]
enum CreatedFile {
    Append,
    Lock,
    Empty,
}

fn open_created(path: &Path, kind: CreatedFile) -> Result<File, StateError> {
    private_dir(parent(path)?)?;
    let mut options = platform::private_options();
    match kind {
        CreatedFile::Append => {
            options.append(true).create(true);
        }
        CreatedFile::Lock => {
            options.read(true).write(true).create(true).truncate(false);
        }
        CreatedFile::Empty => {
            raw::exclusive(options.write(true));
        }
    }
    let file = options
        .open(path)
        .map_err(|error| classify_open(path, error))?;
    check_file(path, &file)?;
    Ok(file)
}

pub(crate) fn open_append(path: &Path) -> Result<File, StateError> {
    open_created(path, CreatedFile::Append)
}

pub(crate) fn open_lock(path: &Path) -> Result<File, StateError> {
    open_created(path, CreatedFile::Lock)
}

pub(crate) fn open_existing_lock(path: &Path) -> Result<Option<File>, StateError> {
    open_existing(path, ExistingFile::Lock)
}

pub(crate) fn create_empty(path: &Path) -> Result<(), StateError> {
    let file = open_created(path, CreatedFile::Empty)?;
    drop(file);
    Ok(())
}

pub(crate) fn remove_file(path: &Path) -> Result<(), StateError> {
    let mut options = platform::private_options();
    options.read(true);
    let file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(classify_open(path, error)),
    };
    let metadata = file.metadata().map_err(|error| io_at(path, error))?;
    if !metadata.is_file() || platform::reparse_point(&metadata) {
        return Err(StateError::NotFile {
            path: path.to_path_buf(),
        });
    }
    match platform::ownership(&file).map_err(|error| io_at(path, error))? {
        Ownership::Private | Ownership::Exposed(_) => drop(file),
        Ownership::Foreign => {
            return Err(StateError::Foreign {
                path: path.to_path_buf(),
            });
        }
    }
    match raw::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io_at(path, error)),
    }
}

pub(crate) fn set_aside(path: &Path, trash: &Path) -> Result<(), StateError> {
    let directory = match fs::symlink_metadata(path) {
        Ok(_metadata) => PrivateDirectory::checked(path)?,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(io_at(path, error)),
    };
    private_dir(trash)?;
    let mut entropy = [0_u8; 16];
    getrandom::fill(&mut entropy)
        .map_err(|error| io_at(trash, io::Error::other(error.to_string())))?;
    let name = format!("{:032x}", u128::from_be_bytes(entropy));
    raw::rename(&directory, &trash.join(name)).map_err(|error| io_at(path, error))
}

#[derive(Debug)]
pub(crate) struct Leftover {
    pub(crate) path: PathBuf,
    pub(crate) error: io::Error,
}

const EMPTY_LIMIT: usize = 64;

mod cleanup;

pub(crate) fn empty(trash: &Path) -> Result<Vec<Leftover>, StateError> {
    cleanup::empty_using(trash, |_retained| Ok(()))
}

pub(crate) fn publish_dir(from: &Path, to: &Path) -> Result<(), StateError> {
    let directory = PrivateDirectory::checked(from)?;
    raw::rename(&directory, to).map_err(|error| io_at(to, error))
}

#[cfg(test)]
mod tests {
    use super::{open_read, read_bytes, remove_file, write_bytes};

    #[test]
    fn late_completion_does_not_recreate_a_retired_job() {
        let root = tempfile::tempdir().unwrap();
        let job = root.path().join("job");
        super::private_dir(&job).unwrap();
        super::set_aside(&job, &root.path().join("trash")).unwrap();
        assert!(!super::mark_existing(&job.join("quiesced")).unwrap());
        assert_eq!(
            std::fs::symlink_metadata(job).unwrap_err().kind(),
            std::io::ErrorKind::NotFound
        );
    }

    #[test]
    fn completion_markers_are_private_and_idempotent() {
        let root = tempfile::tempdir().unwrap();
        let job = root.path().join("job");
        super::private_dir(&job).unwrap();
        let marker = job.join("quiesced");
        assert!(super::mark_existing(&marker).unwrap());
        assert!(super::mark_existing(&marker).unwrap());
        assert_eq!(read_bytes(&marker).unwrap(), Some(Vec::new()));
        write_bytes(&marker, b"invalid").unwrap();
        super::mark_existing(&marker).unwrap_err();
    }

    #[test]
    fn a_retained_directory_cannot_mark_its_replacement() {
        let root = tempfile::tempdir().unwrap();
        let job = root.path().join("job");
        super::private_dir(&job).unwrap();
        let retained = super::existing_directory(&job).unwrap().unwrap();
        let moved = root.path().join("moved");
        match super::raw::rename(&super::PrivateDirectory::checked(&job).unwrap(), &moved) {
            Ok(()) => {
                super::private_dir(&job).unwrap();
                super::mark_directory(&retained, &job.join("quiesced")).unwrap();
                assert_eq!(
                    read_bytes(&moved.join("quiesced")).unwrap(),
                    Some(Vec::new())
                );
                assert_eq!(read_bytes(&job.join("quiesced")).unwrap(), None);
            }
            Err(error) => {
                assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
                super::mark_directory(&retained, &job.join("quiesced")).unwrap();
                assert_eq!(read_bytes(&job.join("quiesced")).unwrap(), Some(Vec::new()));
                drop(retained);
                super::raw::rename(&super::PrivateDirectory::checked(&job).unwrap(), &moved)
                    .unwrap();
                assert_eq!(
                    read_bytes(&moved.join("quiesced")).unwrap(),
                    Some(Vec::new())
                );
            }
        }
    }

    #[test]
    fn a_captured_job_lock_cannot_recreate_its_retired_directory() {
        let root = tempfile::tempdir().unwrap();
        let job = root.path().join("job");
        super::private_dir(&job).unwrap();
        let retained = super::existing_directory(&job).unwrap().unwrap();
        let moved = root.path().join("moved");
        match super::raw::rename(&super::PrivateDirectory::checked(&job).unwrap(), &moved) {
            Ok(()) => {
                let lock = super::open_directory_lock(&retained, &job.join("launch.lock")).unwrap();
                assert!(lock.is_some());
                assert_eq!(
                    std::fs::metadata(&job).unwrap_err().kind(),
                    std::io::ErrorKind::NotFound
                );
                assert!(
                    std::fs::metadata(moved.join("launch.lock"))
                        .unwrap()
                        .is_file()
                );
            }
            Err(error) => {
                assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
                drop(retained);
                super::raw::rename(&super::PrivateDirectory::checked(&job).unwrap(), &moved)
                    .unwrap();
                assert!(
                    super::open_lock_in_existing_directory(&job.join("launch.lock"))
                        .unwrap()
                        .is_none()
                );
                assert_eq!(
                    std::fs::metadata(job).unwrap_err().kind(),
                    std::io::ErrorKind::NotFound
                );
            }
        }
    }

    #[test]
    fn a_missing_job_directory_is_never_created_by_a_lock_reader() {
        let root = tempfile::tempdir().unwrap();
        let job = root.path().join("retired").join("job");
        assert!(
            super::open_lock_in_existing_directory(&job.join("state.lock"))
                .unwrap()
                .is_none()
        );
        assert_eq!(
            std::fs::metadata(root.path().join("retired"))
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::NotFound
        );
        let existing = root.path().join("existing");
        super::private_dir(&existing).unwrap();
        let lock = existing.join("state.lock");
        write_bytes(&lock, b"held").unwrap();
        for _ in 0..2 {
            drop(
                super::open_lock_in_existing_directory(&lock)
                    .unwrap()
                    .unwrap(),
            );
            assert_eq!(read_bytes(&lock).unwrap(), Some(b"held".to_vec()));
        }
    }

    #[test]
    fn a_linked_job_directory_cannot_grant_a_lock_in_its_target() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target");
        super::private_dir(&target).unwrap();
        let link = root.path().join("job");
        crate::file_kind::directory_link(&target, &link).unwrap();
        assert!(matches!(
            super::open_lock_in_existing_directory(&link.join("state.lock")),
            Err(super::StateError::NotDirectory { path }) if path == link
        ));
        assert_eq!(read_bytes(&target.join("state.lock")).unwrap(), None);
    }

    #[test]
    fn state_readers_refuse_directories_with_their_paths() {
        let root = tempfile::tempdir().expect("temporary state root");
        let path = root.path().join("state");
        super::private_dir(&path).expect("private directory");
        assert!(matches!(
            open_read(&path),
            Err(super::StateError::NotFile { path: blocked }) if blocked == path
        ));
        assert!(matches!(
            read_bytes(&path),
            Err(super::StateError::NotFile { path: blocked }) if blocked == path
        ));
    }

    #[test]
    fn a_state_file_is_replaced_while_another_handle_holds_it_open() {
        let root = tempfile::tempdir().expect("temporary state root");
        let path = root.path().join("state").join("generation");
        write_bytes(&path, b"1").expect("first generation");
        let held = std::fs::File::open(&path).expect("a reader of the generation");
        write_bytes(&path, b"2").expect("replacement while a reader holds the file");
        write_bytes(&path, b"3").expect("replacement of a replacement");
        drop(held);
        assert_eq!(
            read_bytes(&path).expect("latest generation").as_deref(),
            Some(&b"3"[..])
        );
    }

    #[test]
    fn state_replacement_keeps_a_private_regular_file() {
        let root = tempfile::tempdir().expect("temporary state root");
        let path = root.path().join("state").join("record");
        write_bytes(&path, b"first").expect("first record");
        write_bytes(&path, b"second").expect("replacement record");
        assert_eq!(
            read_bytes(&path).expect("read record"),
            Some(b"second".to_vec())
        );
        let file = open_read(&path)
            .expect("private file")
            .expect("record exists");
        assert!(file.metadata().expect("file metadata").is_file());
    }

    #[test]
    fn a_file_open_to_other_users_can_still_be_removed() {
        if cfg!(windows) {
            return;
        }
        let root = tempfile::tempdir().expect("temporary state root");
        let path = root.path().join("state").join("record");
        write_bytes(&path, b"record").expect("private record");
        crate::platform::make_executable(&path).expect("open the record to others");
        read_bytes(&path).expect_err("an exposed record is never read");
        remove_file(&path).expect("an exposed record of this user's is removed");
        assert_eq!(read_bytes(&path).expect("absent record"), None);
    }
}
