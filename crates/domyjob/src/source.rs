use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};

use cap_std::fs::Dir;
use domyjob_core::source_paths::{self, TrackedError, TrackedPaths};
use domyjob_core::wire::{Snapshot, WireError};
use thiserror::Error;

use crate::source_archive::{Archive, ArchiveError};
use crate::{bounded, platform, process};

const MAX_GIT_PATH_BYTES: usize = 32_768;

#[derive(Debug)]
struct Repository {
    root: PathBuf,
    paths: TrackedPaths,
}

#[derive(Debug)]
struct GitOutput {
    status: ExitStatus,
    bytes: Option<Vec<u8>>,
}

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

fn git_output(root: &Path, arguments: &[&str], limit: usize) -> io::Result<Option<GitOutput>> {
    let mut child = match git_command(root)
        .args(arguments)
        .stdout(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("Git did not provide its requested output stream"))?;
    let bytes = bounded::read(stdout, limit);
    let status = if bytes.as_ref().is_ok_and(Option::is_some) {
        child.wait()?
    } else {
        let killed = child.kill();
        let waited = child.wait();
        if let Err(error) = bytes {
            return Err(error);
        }
        killed?;
        waited?
    };
    Ok(Some(GitOutput {
        status,
        bytes: bytes?,
    }))
}

fn tracked(root: &Path) -> Result<Option<Repository>, SourceError> {
    let Some(output) = git_output(root, &["rev-parse", "--show-toplevel"], MAX_GIT_PATH_BYTES)?
    else {
        return Ok(None);
    };
    let bytes = output
        .bytes
        .ok_or_else(|| io::Error::other("the Git root exceeds its path budget"))?;
    if !output.status.success() {
        return Ok(None);
    }
    let bytes = bytes
        .strip_suffix(b"\n")
        .filter(|bytes| !bytes.is_empty() && !bytes.contains(&0))
        .ok_or_else(|| io::Error::other("the Git root path is malformed"))?;
    let repository = std::fs::canonicalize(Path::new(
        std::str::from_utf8(bytes)
            .map_err(|_encoding| io::Error::other("the Git root path is not UTF-8"))?,
    ))?;
    if !root.starts_with(&repository) {
        return Err(
            io::Error::other("the Git root does not contain the selected directory").into(),
        );
    }
    let tracked_output = git_output(
        root,
        &["ls-files", "--cached", "--deduplicate", "-z", "--", "."],
        source_paths::MAX_TRACKED_BYTES,
    )?
    .ok_or_else(|| io::Error::other("Git became unavailable after repository discovery"))?;
    let tracked_bytes = tracked_output.bytes.ok_or(TrackedError::Capacity)?;
    if !tracked_output.status.success() {
        return Err(io::Error::other("Git could not enumerate the tracked source paths").into());
    }
    Ok(Some(Repository {
        root: repository,
        paths: source_paths::tracked_paths(&tracked_bytes)?,
    }))
}

fn global_excludes(root: &Path) -> Result<Option<PathBuf>, SourceError> {
    let Some(output) = git_output(
        root,
        &["config", "--path", "--null", "--get", "core.excludesFile"],
        MAX_GIT_PATH_BYTES,
    )?
    else {
        return Ok(ignore::gitignore::gitconfig_excludes_path());
    };
    let bytes = output
        .bytes
        .ok_or_else(|| io::Error::other("the Git excludes path exceeds its byte budget"))?;
    if output.status.code() == Some(1) {
        return Ok(platform::variable("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                platform::variable("HOME")
                    .or_else(|| platform::variable("USERPROFILE"))
                    .map(|home| PathBuf::from(home).join(".config"))
            })
            .map(|configuration| configuration.join("git/ignore")));
    }
    if !output.status.success() {
        return Err(io::Error::other("Git could not read its excludes path").into());
    }
    let bytes = bytes
        .strip_suffix(&[0])
        .filter(|bytes| !bytes.contains(&0))
        .ok_or_else(|| io::Error::other("the Git excludes path is malformed"))?;
    if bytes.is_empty() {
        return Ok(None);
    }
    let path = PathBuf::from(
        std::str::from_utf8(bytes)
            .map_err(|_encoding| io::Error::other("the Git excludes path is not UTF-8"))?,
    );
    Ok(Some(if path.is_absolute() {
        path
    } else {
        root.join(path)
    }))
}

fn walker(root: &Path, repository: Option<&Repository>) -> Result<ignore::Walk, SourceError> {
    let base = repository.map_or(root, |repository| repository.root.as_path());
    let mut builder = ignore::WalkBuilder::new(root);
    builder
        .current_dir(base)
        .hidden(false)
        .parents(repository.is_some())
        .ignore(true)
        .git_ignore(true)
        .git_global(false)
        .git_exclude(true)
        .require_git(false)
        .follow_links(false)
        .filter_entry(|entry| {
            entry
                .file_name()
                .to_str()
                .is_none_or(|name| !source_paths::metadata_component(name))
        });
    if let Some(excludes) = global_excludes(base)? {
        match std::fs::metadata(&excludes) {
            Ok(_) => {
                if let Some(error) = builder.add_ignore(excludes) {
                    return Err(error.into());
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(builder.build())
}

pub(crate) fn working_directory(selected_root: &Path) -> Result<(Vec<u8>, Snapshot), SourceError> {
    let canonical = std::fs::canonicalize(selected_root)?;
    let root = canonical.as_path();
    let source = Dir::open_ambient_dir(root, cap_std::ambient_authority())?;
    let mut paths = BTreeSet::<PathBuf>::new();
    let tracked = tracked(root)?;
    if let Some(tracked) = &tracked {
        for path in tracked.paths.paths() {
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
    for item in walker(root, tracked.as_ref())? {
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

    fn child(test: &str) -> std::process::Command {
        let mut command = crate::process::command(std::env::current_exe().unwrap());
        command.args(["--exact", &format!("source::tests::{test}"), "--nocapture"]);
        command
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
            let excludes = configuration.path().join("excludes with spaces");
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
            let status = child("git_snapshots_keep_tracked_files_matched_by_global_ignores")
                .env(CHILD, "1")
                .env("GIT_CONFIG_GLOBAL", config)
                .env("GIT_CONFIG_COUNT", "1")
                .env("GIT_CONFIG_KEY_0", "core.excludesFile")
                .env("GIT_CONFIG_VALUE_0", "foreign-context-must-be-removed")
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
    fn global_excludes_follow_git_defaults_and_explicit_empty_configuration() {
        const CHILD: &str = "DOMYJOB_SOURCE_DEFAULT_EXCLUDES_TEST";
        let Some(mode) = std::env::var_os(CHILD) else {
            let configuration = tempfile::tempdir().unwrap();
            let xdg = configuration.path().join("xdg");
            testing::write(&xdg.join("git/ignore"), b"*.md\n");
            for mode in ["unset", "empty"] {
                let config = configuration.path().join(mode);
                testing::write(&config, b"");
                if mode == "empty" {
                    let status = git_command(configuration.path())
                        .args(["config", "--file"])
                        .arg(&config)
                        .args(["core.excludesFile", ""])
                        .status()
                        .unwrap();
                    assert!(status.success());
                }
                let status =
                    child("global_excludes_follow_git_defaults_and_explicit_empty_configuration")
                        .env(CHILD, mode)
                        .env("GIT_CONFIG_GLOBAL", config)
                        .env("GIT_CONFIG_NOSYSTEM", "1")
                        .env("XDG_CONFIG_HOME", &xdg)
                        .status()
                        .unwrap();
                assert!(
                    status.success(),
                    "default excludes fixture failed: {status}"
                );
            }
            return;
        };
        let root = tempfile::tempdir().unwrap();
        git(root.path(), &["init", "--quiet"]);
        testing::write(&root.path().join("private.md"), b"private");
        let ignored = git_command(root.path())
            .args(["check-ignore", "--quiet", "private.md"])
            .status()
            .unwrap()
            .success();
        assert_eq!(ignored, mode == "unset");
        let files = contents(root.path());
        assert_eq!(files.is_empty(), ignored);
        if !ignored {
            assert_eq!(files, [("private.md".to_owned(), b"private".to_vec())]);
        }
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
    fn nested_git_snapshots_exclude_parent_ignored_untracked_files_and_keep_tracked_bytes() {
        let source = tempfile::tempdir().unwrap();
        git(source.path(), &["init", "--quiet"]);
        git(source.path(), &["config", "core.excludesFile", ""]);
        testing::write(&source.path().join(".gitignore"), b"nested/*.secret\n");
        testing::write(&source.path().join("nested/tracked.secret"), b"indexed");
        git(source.path(), &["add", "--force", "nested/tracked.secret"]);
        testing::write(
            &source.path().join("nested/tracked.secret"),
            b"current tracked",
        );
        testing::write(
            &source.path().join("nested/private.secret"),
            b"private ignored",
        );
        assert_eq!(
            contents(&source.path().join("nested")),
            [("tracked.secret".to_owned(), b"current tracked".to_vec())]
        );
    }

    #[test]
    fn nested_git_global_excludes_use_the_repository_file_and_pattern_base() {
        let source = tempfile::tempdir().unwrap();
        git(source.path(), &["init", "--quiet"]);
        git(
            source.path(),
            &["config", "core.excludesFile", "relative.ignore"],
        );
        testing::write(
            &source.path().join("relative.ignore"),
            b"root-only.secret\n/nested/anchored.secret\n/nested/tracked.secret\n",
        );
        let nested = source.path().join("nested");
        testing::write(&nested.join("relative.ignore"), b"nested-only.secret\n");
        for (name, bytes, ignored) in [
            ("root-only.secret", b"root ignored".as_slice(), true),
            ("anchored.secret", b"anchor ignored".as_slice(), true),
            ("nested-only.secret", b"nested visible".as_slice(), false),
            ("tracked.secret", b"indexed".as_slice(), true),
        ] {
            testing::write(&nested.join(name), bytes);
            let status = git_command(&nested)
                .args(["check-ignore", "--no-index", "--quiet", name])
                .status()
                .unwrap();
            assert_eq!(
                status.code(),
                Some(i32::from(!ignored)),
                "Git oracle for {name}"
            );
        }
        git(source.path(), &["add", "--force", "nested/tracked.secret"]);
        testing::write(&nested.join("tracked.secret"), b"current tracked");
        assert_eq!(
            contents(&nested),
            [
                ("nested-only.secret".to_owned(), b"nested visible".to_vec()),
                (
                    "relative.ignore".to_owned(),
                    b"nested-only.secret\n".to_vec()
                ),
                ("tracked.secret".to_owned(), b"current tracked".to_vec()),
            ]
        );
    }

    #[test]
    fn repository_metadata_case_aliases_are_excluded_from_real_repository_contents() {
        let root = tempfile::tempdir().unwrap();
        git(root.path(), &["init", "--quiet", "--template="]);
        testing::write(&root.path().join("public.txt"), b"public");
        git(root.path(), &["add", "public.txt"]);
        let directory =
            cap_std::fs::Dir::open_ambient_dir(root.path(), cap_std::ambient_authority()).unwrap();
        directory
            .rename(".git", &directory, "metadata-before")
            .unwrap();
        directory
            .rename("metadata-before", &directory, ".GIT")
            .unwrap();
        assert_eq!(
            contents(root.path()),
            [("public.txt".to_owned(), b"public".to_vec())]
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
