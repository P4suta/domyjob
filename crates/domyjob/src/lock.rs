use std::fs::{File, TryLockError};
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum LockError {
    #[error("{action} {path}: {source}")]
    Io {
        action: &'static str,
        path: PathBuf,
        source: std::io::Error,
    },
}

#[derive(Debug)]
pub struct OsLock {
    file: File,
    path: PathBuf,
}

fn open(path: &Path) -> Result<File, LockError> {
    crate::state_file::open_lock(path).map_err(|error| LockError::Io {
        action: "opening",
        path: path.to_path_buf(),
        source: std::io::Error::other(error.to_string()),
    })
}

impl OsLock {
    pub fn try_exclusive(path: &Path) -> Result<Option<Self>, LockError> {
        let file = open(path)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self {
                file,
                path: path.to_path_buf(),
            })),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(source)) => Err(LockError::Io {
                action: "locking",
                path: path.to_path_buf(),
                source,
            }),
        }
    }

    pub fn exclusive(path: &Path) -> Result<Self, LockError> {
        let file = open(path)?;
        file.lock().map_err(|source| LockError::Io {
            action: "locking",
            path: path.to_path_buf(),
            source,
        })?;
        Ok(Self {
            file,
            path: path.to_path_buf(),
        })
    }

    pub fn first_free(dir: &Path, slots: usize) -> Result<Option<(usize, Self)>, LockError> {
        for index in 0..slots {
            if let Some(lock) = Self::try_exclusive(&dir.join(format!("{index}.lock")))? {
                return Ok(Some((index, lock)));
            }
        }
        Ok(None)
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn release(self) -> Result<(), LockError> {
        self.file.unlock().map_err(|source| LockError::Io {
            action: "unlocking",
            path: self.path.clone(),
            source,
        })
    }
}
