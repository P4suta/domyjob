use std::fs::{File, TryLockError};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotIndex(u32);

impl SlotIndex {
    pub const FIRST: Self = Self(0);

    pub fn all() -> impl Iterator<Item = Self> {
        (0..crate::domain::Concurrency::MOST).map(Self)
    }

    #[must_use]
    pub fn lock_path(self, dir: &Path) -> PathBuf {
        dir.join(format!("{}.lock", self.0))
    }
}

impl std::fmt::Display for SlotIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

#[cfg(test)]
mod tests {
    use super::SlotIndex;

    #[test]
    fn slot_paths_cover_exactly_the_canonical_range() {
        let names: Vec<_> = SlotIndex::all().map(|slot| slot.to_string()).collect();
        assert_eq!(names.len(), 64);
        assert_eq!(names.first().map(String::as_str), Some("0"));
        assert_eq!(names.last().map(String::as_str), Some("63"));
        assert!(
            !names
                .iter()
                .any(|name| ["01", "64"].contains(&name.as_str()))
        );
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LockError {
    #[error(transparent)]
    Io(#[from] crate::failure::IoFailure),
}

#[derive(Debug)]
pub struct OsLock {
    file: File,
    path: PathBuf,
}

fn open(path: &Path) -> Result<File, LockError> {
    crate::state_file::open_lock(path).map_err(|error| opening(path, &error))
}

fn opening(path: &Path, error: &crate::state_file::StateError) -> LockError {
    LockError::Io(crate::failure::IoFailure {
        action: "opening",
        path: path.to_path_buf(),
        source: std::io::Error::other(error.to_string()),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Probe {
    Absent,
    Free,
    Held,
}

enum Availability {
    Free,
    Held,
}

fn availability(file: &File, path: &Path) -> Result<Availability, LockError> {
    match file.try_lock() {
        Ok(()) => Ok(Availability::Free),
        Err(TryLockError::WouldBlock) => Ok(Availability::Held),
        Err(TryLockError::Error(source)) => Err(LockError::Io(crate::failure::IoFailure {
            action: "locking",
            path: path.to_path_buf(),
            source,
        })),
    }
}

impl OsLock {
    pub fn probe(path: &Path) -> Result<Probe, LockError> {
        let opened =
            crate::state_file::open_existing_lock(path).map_err(|error| opening(path, &error))?;
        let Some(file) = opened else {
            return Ok(Probe::Absent);
        };
        match availability(&file, path)? {
            Availability::Free => Ok(Probe::Free),
            Availability::Held => Ok(Probe::Held),
        }
    }

    pub fn try_exclusive(path: &Path) -> Result<Option<Self>, LockError> {
        let file = open(path)?;
        match availability(&file, path)? {
            Availability::Free => Ok(Some(Self {
                file,
                path: path.to_path_buf(),
            })),
            Availability::Held => Ok(None),
        }
    }

    pub fn exclusive(path: &Path) -> Result<Self, LockError> {
        let file = open(path)?;
        file.lock().map_err(|source| {
            LockError::Io(crate::failure::IoFailure {
                action: "locking",
                path: path.to_path_buf(),
                source,
            })
        })?;
        Ok(Self {
            file,
            path: path.to_path_buf(),
        })
    }

    pub fn first_free(dir: &Path) -> Result<Option<(SlotIndex, Self)>, LockError> {
        for index in SlotIndex::all() {
            if let Some(lock) = Self::try_exclusive(&index.lock_path(dir))? {
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
        crate::faults::at("lock::release", &self.path)
            .and_then(|()| self.file.unlock())
            .map_err(|source| {
                LockError::Io(crate::failure::IoFailure {
                    action: "unlocking",
                    path: self.path.clone(),
                    source,
                })
            })
    }
}
