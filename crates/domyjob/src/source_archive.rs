use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use domyjob_core::domain::{Invalid, RelativePath};
use domyjob_core::wire::MAX_SNAPSHOT_BYTES;
use thiserror::Error;

use crate::file_kind;

const MAX_FILES: usize = 100_000;
const MAX_PATH_BYTES: usize = 16_777_216;

/// Why the source could not be archived, naming the file where one is to blame.
#[derive(Debug, Error)]
pub(crate) enum ArchiveError {
    #[error("reading {path}: {source}")]
    Io { path: PathBuf, source: io::Error },
    #[error("{path} cannot be sent: {reason}")]
    Rejected { path: PathBuf, reason: Rejection },
    #[error("the source exceeds 64 MiB, {MAX_FILES} files, or its path budget")]
    Capacity,
}

/// Why one file of the source cannot be sent.
#[derive(Debug, Error)]
pub(crate) enum Rejection {
    #[error(transparent)]
    Invalid(Invalid),
    #[error("it lies outside the source root")]
    Outside,
    #[error("its path is not UTF-8")]
    NotUtf8,
    #[error("it is a symlink or not a regular file")]
    NonFile,
    #[error("its path collides with another on a case-insensitive filesystem")]
    Collision,
    #[error("it changed while it was archived")]
    Changed,
}

#[derive(Debug, Default)]
struct LimitedArchive(Vec<u8>);

impl Write for LimitedArchive {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let length = self
            .0
            .len()
            .checked_add(bytes.len())
            .ok_or_else(|| io::Error::other("source archive size overflowed"))?;
        let length = u64::try_from(length)
            .map_err(|_overflow| io::Error::other("source archive size overflowed"))?;
        if length > MAX_SNAPSHOT_BYTES {
            return Err(io::Error::other("source archive exceeds 64 MiB"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(crate) struct Archive {
    builder: tar::Builder<LimitedArchive>,
    names: BTreeSet<String>,
    path_bytes: usize,
}

impl Archive {
    pub(crate) fn new() -> Self {
        Self {
            builder: tar::Builder::new(LimitedArchive::default()),
            names: BTreeSet::new(),
            path_bytes: 0,
        }
    }

    pub(crate) fn add(
        &mut self,
        root: &Path,
        path: &Path,
        mode: impl FnOnce(&fs::Metadata) -> u32,
    ) -> Result<(), ArchiveError> {
        let rejected = |reason| ArchiveError::Rejected {
            path: path.to_path_buf(),
            reason,
        };
        let failed = |source| ArchiveError::Io {
            path: path.to_path_buf(),
            source,
        };
        let relative = path
            .strip_prefix(root)
            .map_err(|_prefix| rejected(Rejection::Outside))?;
        let mut parts = Vec::new();
        for component in relative.components() {
            parts.push(
                component
                    .as_os_str()
                    .to_str()
                    .ok_or_else(|| rejected(Rejection::NotUtf8))?,
            );
        }
        let name = RelativePath::try_from(parts.join("/"))
            .map_err(|invalid| rejected(Rejection::Invalid(invalid)))?;
        self.path_bytes = self.path_bytes.saturating_add(name.as_str().len());
        if self.path_bytes > MAX_PATH_BYTES || self.names.len() >= MAX_FILES {
            return Err(ArchiveError::Capacity);
        }
        if !self.names.insert(name.as_str().to_lowercase()) {
            return Err(rejected(Rejection::Collision));
        }
        let metadata = fs::symlink_metadata(path).map_err(failed)?;
        if !metadata.is_file() || file_kind::reparse_point(&metadata) {
            return Err(rejected(Rejection::NonFile));
        }
        let mode = mode(&metadata);
        let mut content = fs::File::open(path).map_err(failed)?;
        if content.metadata().map_err(failed)?.len() != metadata.len() {
            return Err(rejected(Rejection::Changed));
        }
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Regular);
        header.set_size(metadata.len());
        header.set_mode(mode);
        header.set_mtime(0);
        header.set_cksum();
        self.builder
            .append_data(&mut header, name.as_str(), &mut content)
            .map_err(failed)?;
        Ok(())
    }

    /// The finished archive; writing its end can only fail on the size limit.
    pub(crate) fn finish(self) -> Result<Vec<u8>, ArchiveError> {
        match self.builder.into_inner() {
            Ok(archive) => Ok(archive.0),
            Err(_limit) => Err(ArchiveError::Capacity),
        }
    }
}
