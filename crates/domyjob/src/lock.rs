use std::fs::{File, TryLockError};
use std::io;
use std::path::Path;

use thiserror::Error;

use crate::state_io::{self, StateError};

mod raw {
    #![expect(
        clippy::disallowed_methods,
        reason = "the lock adapter alone acquires and releases file locks"
    )]

    use std::fs::{File, TryLockError};
    use std::io::{self, Write as _};

    pub(super) fn shared(file: &File) -> io::Result<()> {
        file.lock_shared()
    }

    pub(super) fn exclusive(file: &File) -> io::Result<()> {
        file.lock()
    }

    pub(super) fn try_exclusive(file: &File) -> Result<(), TryLockError> {
        file.try_lock()
    }

    pub(super) fn unlock(file: &File) -> io::Result<()> {
        file.unlock()
    }

    pub(super) fn report(error: &io::Error) -> io::Result<()> {
        writeln!(
            io::stderr().lock(),
            "domyjob: releasing a file lock failed: {error}"
        )
    }
}

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
#[must_use = "keep the lock guard alive for the protected operation"]
pub(crate) struct OsLock {
    file: Option<File>,
}

impl OsLock {
    pub(crate) fn shared_file(file: File) -> io::Result<Self> {
        raw::shared(&file)?;
        Ok(Self { file: Some(file) })
    }

    pub(crate) fn exclusive_file(file: File) -> io::Result<Self> {
        raw::exclusive(&file)?;
        Ok(Self { file: Some(file) })
    }

    pub(crate) fn try_file(file: File) -> io::Result<Option<Self>> {
        match raw::try_exclusive(&file) {
            Ok(()) => Ok(Some(Self { file: Some(file) })),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(error)) => Err(error),
        }
    }

    pub(crate) fn release(mut self) -> io::Result<()> {
        match self.file.take() {
            Some(file) => raw::unlock(&file),
            None => Ok(()),
        }
    }

    pub(crate) fn probe(path: &Path) -> Result<Probe, LockError> {
        let Some(file) = state_io::open_existing_lock(path)? else {
            return Ok(Probe::Absent);
        };
        match Self::try_file(file)? {
            Some(guard) => {
                guard.release()?;
                Ok(Probe::Free)
            }
            None => Ok(Probe::Held),
        }
    }

    pub(crate) fn try_exclusive(path: &Path) -> Result<Option<Self>, LockError> {
        let file = state_io::open_lock(path)?;
        Self::try_file(file).map_err(LockError::from)
    }

    pub(crate) fn exclusive(path: &Path) -> Result<Self, LockError> {
        let file = state_io::open_lock(path)?;
        Self::exclusive_file(file).map_err(LockError::from)
    }
}

impl Drop for OsLock {
    fn drop(&mut self) {
        if let Some(file) = self.file.take()
            && let Err(error) = raw::unlock(&file)
        {
            match raw::report(&error) {
                Ok(()) | Err(_) => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{OsLock, Probe};
    use crate::state_io;
    use std::fs::File;
    use std::io;

    #[test]
    fn dropping_a_guard_releases_shared_and_exclusive_locks_with_a_live_duplicate() {
        let constructors: [fn(File) -> io::Result<OsLock>; 2] =
            [OsLock::shared_file, OsLock::exclusive_file];
        for acquire in constructors {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("state/guard.lock");
            let guard = acquire(state_io::open_lock(&path).unwrap()).unwrap();
            let duplicate = guard.file.as_ref().unwrap().try_clone().unwrap();
            assert_eq!(OsLock::probe(&path).unwrap(), Probe::Held);
            drop(guard);
            assert_eq!(OsLock::probe(&path).unwrap(), Probe::Free);
            assert!(duplicate.metadata().unwrap().is_file());
        }
    }

    #[test]
    fn explicitly_releasing_a_guard_unlocks_before_closing_a_live_duplicate() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("state/released.lock");
        let guard = OsLock::exclusive(&path).unwrap();
        let duplicate = guard.file.as_ref().unwrap().try_clone().unwrap();
        guard.release().unwrap();
        assert_eq!(OsLock::probe(&path).unwrap(), Probe::Free);
        assert!(duplicate.metadata().unwrap().is_file());
    }

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
        assert_eq!(
            OsLock::probe(&path).expect("released lock"),
            Probe::Free,
            "a dropped lock is released"
        );
    }
}
