use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use cap_std::fs::Dir;
use domyjob_core::source_paths::{self, TrackedError, TrackedPaths};
use domyjob_core::wire::{Snapshot, WireError};
use thiserror::Error;

use crate::source_archive::{Archive, ArchiveError};
use crate::{bounded, platform, process};

#[derive(Debug, Error)]
pub(crate) enum SourceError {
    #[error(transparent)]
    Archive(#[from] ArchiveError),
    #[error(transparent)]
    Wire(#[from] WireError),
    #[error("walking the source failed: {0}")]
    Walk(#[from] ignore::Error),
    #[error("reading the tracked source paths: {0}")]
    Git(#[from] io::Error),
    #[error(transparent)]
    Tracked(#[from] TrackedError),
}

fn git_command(root: &Path) -> std::process::Command {
    let mut command = process::command("git");
    command.current_dir(root).stderr(Stdio::null());
    for name in source_paths::GIT_CONTEXT_VARIABLES {
        command.env_remove(name);
    }
    command
}

fn tracked(root: &Path) -> Result<Option<TrackedPaths>, SourceError> {
    let status = match git_command(root)
        .args(["rev-parse", "--show-prefix"])
        .status()
    {
        Ok(status) => status,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !status.success() {
        return Ok(None);
    }
    let mut child = git_command(root)
        .args(["ls-files", "--cached", "--deduplicate", "-z", "--", "."])
        .stdout(Stdio::piped())
        .spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("Git did not provide its tracked path stream"))?;
    let bytes = bounded::read(stdout, source_paths::MAX_TRACKED_BYTES);
    let bytes = match bytes {
        Ok(Some(bytes)) => bytes,
        Ok(None) => {
            child.kill()?;
            child.wait()?;
            return Err(TrackedError::Capacity.into());
        }
        Err(error) => {
            child.kill()?;
            child.wait()?;
            return Err(error.into());
        }
    };
    if !child.wait()?.success() {
        return Err(io::Error::other("Git could not enumerate the tracked source paths").into());
    }
    Ok(Some(source_paths::tracked_paths(&bytes)?))
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
                .is_none_or(|name| !source_paths::metadata_component(name))
        });
    builder.build()
}

pub(crate) fn working_directory(root: &Path) -> Result<(Vec<u8>, Snapshot), SourceError> {
    let source = Dir::open_ambient_dir(root, cap_std::ambient_authority())?;
    let mut paths = BTreeSet::<PathBuf>::new();
    if let Some(tracked) = tracked(root)? {
        for path in tracked.paths() {
            let path = root.join(path.as_str());
            match std::fs::symlink_metadata(&path) {
                Ok(metadata)
                    if metadata.is_dir() && !crate::file_kind::reparse_point(&metadata) => {}
                Ok(_) => {
                    paths.insert(path);
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
    let mut archive = Archive::new();
    for item in walker(root) {
        let item = item?;
        let Some(kind) = item.file_type() else {
            continue;
        };
        if kind.is_dir() {
            continue;
        }
        paths.insert(item.path().to_path_buf());
    }
    for path in paths {
        archive.add((root, &source), &path, platform::file_mode)?;
    }
    let bytes = archive.finish()?;
    let length = u64::try_from(bytes.len()).map_err(|_length| ArchiveError::Capacity)?;
    let digest = blake3::hash(&bytes).to_hex().to_string();
    let descriptor = Snapshot::new(length, digest)?;
    Ok((bytes, descriptor))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::working_directory;
    use crate::testing;

    #[test]
    fn tracked_queries_remove_repository_context_without_overriding_global_configuration() {
        let root = tempfile::tempdir().unwrap();
        let command = super::git_command(root.path());
        let removed = command
            .get_envs()
            .map(|(name, value)| {
                assert!(value.is_none());
                name.to_str().unwrap()
            })
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            removed,
            domyjob_core::source_paths::GIT_CONTEXT_VARIABLES
                .into_iter()
                .collect()
        );
        assert!(!removed.contains("GIT_CONFIG_GLOBAL"));
        assert_eq!(command.get_current_dir(), Some(root.path()));
    }

    fn git_command(root: &Path) -> std::process::Command {
        let mut command = crate::process::command("git");
        for (name, _) in std::env::vars_os() {
            if name
                .to_str()
                .is_some_and(|name| name.starts_with("GIT_") && name != "GIT_CONFIG_GLOBAL")
            {
                command.env_remove(name);
            }
        }
        command.current_dir(root);
        command
    }

    fn git(root: &Path, arguments: &[&str]) {
        let status = git_command(root).args(arguments).status().unwrap();
        assert!(status.success(), "Git fixture failed: {status}");
    }

    fn contents(root: &Path) -> Vec<(String, Vec<u8>)> {
        let (bytes, _) = working_directory(root).expect("source archive");
        let mut archive = tar::Archive::new(bytes.as_slice());
        let mut entries = archive
            .entries()
            .unwrap()
            .map(|entry| {
                let mut entry = entry.unwrap();
                let path = entry.path().unwrap().to_str().unwrap().to_owned();
                let content = crate::bounded::read(&mut entry, 1024).unwrap().unwrap();
                (path, content)
            })
            .collect::<Vec<_>>();
        entries.sort();
        entries
    }

    #[test]
    fn git_snapshots_keep_current_tracked_bytes_despite_repository_ignores() {
        let source = tempfile::tempdir().unwrap();
        git(source.path(), &["init", "--quiet"]);
        testing::write(&source.path().join("CLAUDE.md"), b"indexed");
        testing::write(&source.path().join("deleted.txt"), b"deleted");
        git(
            source.path(),
            &["add", "--force", "CLAUDE.md", "deleted.txt"],
        );
        testing::write(&source.path().join(".gitignore"), b"*.md\nprivate/\n");
        testing::write(&source.path().join("CLAUDE.md"), b"current working bytes");
        testing::write(&source.path().join("untracked.md"), b"ignored");
        testing::write(&source.path().join("new.txt"), b"untracked visible");
        testing::write(&source.path().join("private/hidden.txt"), b"ignored");
        testing::write(&source.path().join(".hg/private"), b"metadata");
        git(source.path(), &["add", "--force", ".hg/private"]);
        testing::remove(&source.path().join("deleted.txt"));
        assert_eq!(
            contents(source.path()),
            [
                (".gitignore".to_owned(), b"*.md\nprivate/\n".to_vec()),
                ("CLAUDE.md".to_owned(), b"current working bytes".to_vec()),
                ("new.txt".to_owned(), b"untracked visible".to_vec()),
            ]
        );
    }

    #[test]
    fn git_snapshots_keep_tracked_files_matched_by_global_ignores() {
        const CHILD: &str = "DOMYJOB_SOURCE_GLOBAL_IGNORE_TEST";
        if std::env::var_os(CHILD).is_none() {
            let configuration = tempfile::tempdir().unwrap();
            let excludes = configuration.path().join("excludes");
            testing::write(&excludes, b"*.md\n");
            let config = configuration.path().join("gitconfig");
            let configured = git_command(configuration.path())
                .args(["config", "--file"])
                .arg(&config)
                .arg("core.excludesFile")
                .arg(&excludes)
                .status()
                .unwrap();
            assert!(
                configured.success(),
                "global Git configuration failed: {configured}"
            );
            let status = crate::process::command(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "source::tests::git_snapshots_keep_tracked_files_matched_by_global_ignores",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .env("GIT_CONFIG_GLOBAL", config)
                .status()
                .unwrap();
            assert!(status.success(), "global-ignore child failed: {status}");
            return;
        }
        let source = tempfile::tempdir().unwrap();
        git(source.path(), &["init", "--quiet"]);
        testing::write(&source.path().join("CLAUDE.md"), b"indexed");
        git(source.path(), &["add", "--force", "CLAUDE.md"]);
        testing::write(&source.path().join("CLAUDE.md"), b"current tracked");
        testing::write(&source.path().join("private.md"), b"untracked ignored");
        assert_eq!(
            contents(source.path()),
            [("CLAUDE.md".to_owned(), b"current tracked".to_vec())]
        );
    }

    #[test]
    fn git_snapshots_from_a_subdirectory_use_paths_relative_to_that_directory() {
        let source = tempfile::tempdir().unwrap();
        git(source.path(), &["init", "--quiet"]);
        testing::write(&source.path().join("nested/CLAUDE.md"), b"tracked nested");
        testing::write(&source.path().join("outside.txt"), b"outside");
        git(
            source.path(),
            &["add", "--force", "nested/CLAUDE.md", "outside.txt"],
        );
        testing::write(&source.path().join("nested/.gitignore"), b"*.md\n");
        assert_eq!(
            contents(&source.path().join("nested")),
            [
                (".gitignore".to_owned(), b"*.md\n".to_vec()),
                ("CLAUDE.md".to_owned(), b"tracked nested".to_vec()),
            ]
        );
    }

    #[test]
    fn tracked_directories_replaced_with_links_cannot_escape_the_source_root() {
        let source = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        git(source.path(), &["init", "--quiet"]);
        testing::write(&source.path().join("tracked/private.txt"), b"tracked");
        git(source.path(), &["add", "tracked/private.txt"]);
        testing::write(&source.path().join(".gitignore"), b"tracked/\n");
        testing::write(&outside.path().join("private.txt"), b"outside secret");
        testing::remove(&source.path().join("tracked/private.txt"));
        std::fs::remove_dir(source.path().join("tracked")).unwrap();
        crate::file_kind::directory_link(outside.path(), &source.path().join("tracked")).unwrap();
        assert!(matches!(
            working_directory(source.path()),
            Err(super::SourceError::Archive(
                crate::source_archive::ArchiveError::Rejected {
                    reason: crate::source_archive::Rejection::NonFile,
                    ..
                }
            ))
        ));
    }

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
