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

    pub(super) fn remove_dir_all(path: &Path) -> io::Result<()> {
        std::fs::remove_dir_all(path)
    }

    pub(super) fn rename(from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(from, to)
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
    staged
        .as_file()
        .sync_all()
        .map_err(|error| io_at(path, error))?;
    let mut staged = staged.into_temp_path();
    raw::rename(&staged, path).map_err(|error| io_at(path, error))?;
    staged.disable_cleanup(true);
    platform::sync_dir(directory).map_err(|error| io_at(directory, error))?;
    Ok(())
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
    match fs::symlink_metadata(path) {
        Ok(_metadata) => private_dir(path)?,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(io_at(path, error)),
    }
    private_dir(trash)?;
    let mut entropy = [0_u8; 16];
    getrandom::fill(&mut entropy)
        .map_err(|error| io_at(trash, io::Error::other(error.to_string())))?;
    let name = format!("{:032x}", u128::from_be_bytes(entropy));
    raw::rename(path, &trash.join(name)).map_err(|error| io_at(path, error))
}

#[derive(Debug)]
pub(crate) struct Leftover {
    pub(crate) path: PathBuf,
    pub(crate) error: io::Error,
}

const EMPTY_LIMIT: usize = 64;

pub(crate) fn empty(trash: &Path) -> Result<Vec<Leftover>, StateError> {
    let entries = match fs::read_dir(trash) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(io_at(trash, error)),
    };
    let mut leftovers = Vec::new();
    for entry in entries.take(EMPTY_LIMIT) {
        let path = entry.map_err(|error| io_at(trash, error))?.path();
        if let Err(error) = raw::remove_dir_all(&path) {
            leftovers.push(Leftover { path, error });
        }
    }
    Ok(leftovers)
}

pub(crate) fn publish_dir(from: &Path, to: &Path) -> Result<(), StateError> {
    private_dir(from)?;
    raw::rename(from, to).map_err(|error| io_at(to, error))
}

#[cfg(test)]
mod tests {
    use super::{open_read, read_bytes, remove_file, write_bytes};

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
