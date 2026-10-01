use std::path::Path;

use domyjob_core::wire::{Snapshot, WireError};
use thiserror::Error;

use crate::platform;
use crate::source_archive::{Archive, ArchiveError};

const METADATA: &[&str] = &[
    ".git", ".jj", ".hg", ".svn", ".pijul", "_darcs", ".bzr", "CVS",
];
#[derive(Debug, Error)]
pub(crate) enum SourceError {
    #[error(transparent)]
    Archive(#[from] ArchiveError),
    #[error(transparent)]
    Wire(#[from] WireError),
    #[error("walking the source failed: {0}")]
    Walk(#[from] ignore::Error),
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
        archive.add(root, item.path(), platform::file_mode)?;
    }
    let bytes = archive.finish()?;
    let length = u64::try_from(bytes.len()).map_err(|_length| ArchiveError::Capacity)?;
    let digest = blake3::hash(&bytes).to_hex().to_string();
    let descriptor = Snapshot::new(length, digest)?;
    Ok((bytes, descriptor))
}

#[cfg(test)]
mod tests {
    use super::working_directory;
    use crate::testing;

    #[test]
    fn working_directory_includes_uncommitted_files_but_excludes_repository_metadata() {
        let source = tempfile::tempdir().expect("temporary source");
        testing::write(&source.path().join(".git/config"), b"private");
        testing::write(&source.path().join("new.txt"), b"uncommitted");
        let (bytes, descriptor) = working_directory(source.path()).expect("source archive");
        assert_eq!(
            usize::try_from(descriptor.bytes()).expect("archive length"),
            bytes.len()
        );
        let mut tar = tar::Archive::new(bytes.as_slice());
        let entries = tar
            .entries()
            .expect("archive entries")
            .map(|entry| {
                let entry = entry.expect("archive entry");
                let name = entry
                    .path()
                    .expect("archive path")
                    .to_string_lossy()
                    .into_owned();
                let mode = entry.header().mode().expect("portable file mode");
                (name, mode)
            })
            .collect::<Vec<_>>();
        assert_eq!(entries, [("new.txt".to_owned(), 0o644)]);
    }
}
