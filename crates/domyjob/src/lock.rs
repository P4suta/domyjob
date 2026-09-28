use std::fs::{File, TryLockError};
use std::io;
use std::path::Path;

use thiserror::Error;

use crate::state_io::{self, StateError};

#[derive(Debug, Error)]
pub(crate) enum LockError {
    #[error(transparent)]
    State(#[from] StateError),
    #[error("locking the state file failed: {0}")]
    Io(#[from] io::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Probe {
    Absent,
    Free,
    Held,
}

#[derive(Debug)]
pub(crate) struct OsLock {
    _file: File,
}

impl OsLock {
    pub(crate) fn probe(path: &Path) -> Result<Probe, LockError> {
        let Some(file) = state_io::open_existing_lock(path)? else {
            return Ok(Probe::Absent);
        };
        match file.try_lock() {
            Ok(()) => Ok(Probe::Free),
            Err(TryLockError::WouldBlock) => Ok(Probe::Held),
            Err(TryLockError::Error(error)) => Err(error.into()),
        }
    }

    pub(crate) fn try_exclusive(path: &Path) -> Result<Option<Self>, LockError> {
        let file = state_io::open_lock(path)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self { _file: file })),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(error)) => Err(error.into()),
        }
    }

    pub(crate) fn exclusive(path: &Path) -> Result<Self, LockError> {
        let file = state_io::open_lock(path)?;
        file.lock()?;
        Ok(Self { _file: file })
    }
}

#[cfg(test)]
mod tests {
    use super::{OsLock, Probe};

    #[test]
    fn a_held_lock_cannot_be_taken_or_mistaken_for_a_dead_worker() {
        let root = tempfile::tempdir().expect("temporary lock root");
        let path = root.path().join("state").join("worker.lock");
        assert_eq!(OsLock::probe(&path).expect("absent lock"), Probe::Absent);
        let first = OsLock::exclusive(&path).expect("first lock");
        assert_eq!(OsLock::probe(&path).expect("held lock"), Probe::Held);
        assert!(
            OsLock::try_exclusive(&path)
                .expect("second attempt")
                .is_none()
        );
        drop(first);
        assert_eq!(OsLock::probe(&path).expect("released lock"), Probe::Free);
    }
}
