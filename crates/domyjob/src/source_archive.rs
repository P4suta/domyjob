#![expect(
    clippy::redundant_pub_crate,
    reason = "the archive builder is compiled into both the build script and the application"
)]

use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Write};
use std::path::Path;

use domyjob_core::domain::{Invalid, RelativePath};
use domyjob_core::wire::MAX_SNAPSHOT_BYTES;
use thiserror::Error;

use crate::file_kind;

const MAX_FILES: usize = 100_000;
const MAX_PATH_BYTES: usize = 16_777_216;

#[derive(Debug, Error)]
#[expect(
    variant_size_differences,
    reason = "I/O is the only payload-bearing archive error variant"
)]
pub(crate) enum ArchiveError {
    #[error(transparent)]
    Invalid(#[from] Invalid),
    #[error("source I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("the source contains a symlink or a non-file entry")]
    NonFile,
    #[error("the source contains paths that collide on a case-insensitive filesystem")]
    Collision,
    #[error("the source has too many files or path bytes")]
    Capacity,
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
        let relative = path
            .strip_prefix(root)
            .map_err(|_prefix| io::Error::other("source is outside its root"))?;
        let mut parts = Vec::new();
        for component in relative.components() {
            parts.push(
                component
                    .as_os_str()
                    .to_str()
                    .ok_or_else(|| io::Error::other("a source path is not UTF-8"))?,
            );
        }
        let name = RelativePath::try_from(parts.join("/"))?;
        self.path_bytes = self.path_bytes.saturating_add(name.as_str().len());
        if self.path_bytes > MAX_PATH_BYTES || self.names.len() >= MAX_FILES {
            return Err(ArchiveError::Capacity);
        }
        if !self.names.insert(name.as_str().to_lowercase()) {
            return Err(ArchiveError::Collision);
        }
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.is_file() || file_kind::reparse_point(&metadata) {
            return Err(ArchiveError::NonFile);
        }
        let mode = mode(&metadata);
        let mut content = fs::File::open(path)?;
        if content.metadata()?.len() != metadata.len() {
            return Err(io::Error::other("a source file changed during archiving").into());
        }
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Regular);
        header.set_size(metadata.len());
        header.set_mode(mode);
        header.set_mtime(0);
        header.set_cksum();
        self.builder
            .append_data(&mut header, name.as_str(), &mut content)?;
        Ok(())
    }

    pub(crate) fn finish(self) -> Result<Vec<u8>, ArchiveError> {
        Ok(self.builder.into_inner()?.0)
    }
}
