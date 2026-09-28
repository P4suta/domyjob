#![expect(
    clippy::redundant_pub_crate,
    reason = "the binary composition root uses this private module"
)]

use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Write};
use std::path::Path;

use domyjob_core::domain::{Invalid, RelativePath};
use domyjob_core::wire::{self, Snapshot, WireError};
use thiserror::Error;

use crate::platform;

const METADATA: &[&str] = &[
    ".git", ".jj", ".hg", ".svn", ".pijul", "_darcs", ".bzr", "CVS",
];
const MAX_FILES: usize = 100_000;
const MAX_PATH_BYTES: usize = 16_777_216;

#[derive(Debug, Error)]
pub(crate) enum SourceError {
    #[error(transparent)]
    Invalid(#[from] Invalid),
    #[error(transparent)]
    Wire(#[from] WireError),
    #[error("source I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("walking the source failed: {0}")]
    Walk(#[from] ignore::Error),
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
        if length > wire::MAX_SNAPSHOT_BYTES {
            return Err(io::Error::other("source archive exceeds 64 MiB"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct Archive {
    builder: tar::Builder<LimitedArchive>,
    names: BTreeSet<String>,
    path_bytes: usize,
}

impl Archive {
    fn new() -> Self {
        Self {
            builder: tar::Builder::new(LimitedArchive::default()),
            names: BTreeSet::new(),
            path_bytes: 0,
        }
    }

    fn add(&mut self, root: &Path, path: &Path) -> Result<(), SourceError> {
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
            return Err(SourceError::Capacity);
        }
        if !self.names.insert(name.as_str().to_lowercase()) {
            return Err(SourceError::Collision);
        }
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.is_file() || platform::reparse_point(&metadata) {
            return Err(SourceError::NonFile);
        }
        let mut content = fs::File::open(path)?;
        if content.metadata()?.len() != metadata.len() {
            return Err(io::Error::other("a source file changed during archiving").into());
        }
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Regular);
        header.set_size(metadata.len());
        header.set_mode(platform::file_mode(&metadata));
        header.set_mtime(0);
        header.set_cksum();
        self.builder
            .append_data(&mut header, name.as_str(), &mut content)?;
        Ok(())
    }

    fn finish(self) -> Result<Vec<u8>, SourceError> {
        Ok(self.builder.into_inner()?.0)
    }
}

fn walker(root: &Path) -> ignore::Walk {
    let mut builder = ignore::WalkBuilder::new(root);
    builder
        .hidden(false)
        .parents(false)
        .ignore(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .require_git(false)
        .follow_links(false)
        .filter_entry(|entry| {
            entry
                .file_name()
                .to_str()
                .is_none_or(|name| !METADATA.contains(&name))
        });
    builder.build()
}

pub(crate) fn working_directory(root: &Path) -> Result<(Vec<u8>, Snapshot), SourceError> {
    let mut archive = Archive::new();
    for item in walker(root) {
        let item = item?;
        let Some(kind) = item.file_type() else {
            continue;
        };
        if kind.is_dir() {
            continue;
        }
        archive.add(root, item.path())?;
    }
    let bytes = archive.finish()?;
    let length = u64::try_from(bytes.len()).map_err(|_length| SourceError::Capacity)?;
    let digest = blake3::hash(&bytes).to_hex().to_string();
    let descriptor = Snapshot::new(length, digest)?;
    Ok((bytes, descriptor))
}

#[cfg(test)]
#[expect(
    clippy::disallowed_methods,
    reason = "the fixture owns its temporary source directory"
)]
mod tests {
    use std::fs;

    use super::working_directory;

    #[test]
    fn working_directory_includes_uncommitted_files_but_excludes_repository_metadata() {
        let source = tempfile::tempdir().expect("temporary source");
        fs::create_dir_all(source.path().join(".git")).expect("metadata directory");
        fs::write(source.path().join(".git/config"), b"private").expect("metadata file");
        fs::write(source.path().join("new.txt"), b"uncommitted").expect("source file");
        let (bytes, descriptor) = working_directory(source.path()).expect("source archive");
        assert_eq!(
            usize::try_from(descriptor.bytes()).expect("archive length"),
            bytes.len()
        );
        let mut tar = tar::Archive::new(bytes.as_slice());
        let names = tar
            .entries()
            .expect("archive entries")
            .map(|entry| {
                entry
                    .expect("archive entry")
                    .path()
                    .expect("archive path")
                    .to_string_lossy()
                    .into_owned()
            })
            .collect::<Vec<_>>();
        assert_eq!(names, ["new.txt"]);
    }
}
