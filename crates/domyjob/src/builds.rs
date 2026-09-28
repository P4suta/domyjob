//! Build-specific install directories, their in-use locks, and pruning of unused ones.
//!
//! Every process started from an installed build holds a shared lock inside that build's directory, so a build is removed only when no process of it runs.

use std::fs::{self, File, TryLockError};
use std::io;
use std::path::{Path, PathBuf};

mod raw {
    #![expect(
        clippy::disallowed_methods,
        reason = "installed build directories are marked and removed only here"
    )]

    use std::fs::{File, OpenOptions};
    use std::io;
    use std::path::Path;

    pub(super) fn open_lock(path: &Path) -> io::Result<File> {
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
    }

    pub(super) fn write(path: &Path, bytes: &[u8]) -> io::Result<()> {
        std::fs::write(path, bytes)
    }

    pub(super) fn remove_dir_all(path: &Path) -> io::Result<()> {
        std::fs::remove_dir_all(path)
    }
}

const LOCK: &str = "in-use.lock";
const LAST_USED: &str = "last-used";
/// Builds used this recently stay installed, so two builds in use never remove each other.
const KEEP_MILLIS: u64 = 7 * 24 * 60 * 60 * 1000;

/// The installed build directory this executable runs from: `versions/TAG/bin/domyjob`.
fn installed_directory(executable: &Path) -> Option<PathBuf> {
    let bin = executable.parent()?;
    let build = bin.parent()?;
    let versions = build.parent()?;
    (bin.file_name()? == "bin" && versions.file_name()? == "versions").then(|| build.to_path_buf())
}

fn lock_file(directory: &Path) -> io::Result<File> {
    raw::open_lock(&directory.join(LOCK))
}

/// Mark this process as a user of its installed build for as long as the returned file lives.
pub(crate) fn hold_current() -> io::Result<Option<File>> {
    let executable = fs::canonicalize(std::env::current_exe()?)?;
    let Some(directory) = installed_directory(&executable) else {
        return Ok(None);
    };
    let file = lock_file(&directory)?;
    file.lock_shared()?;
    raw::write(
        &directory.join(LAST_USED),
        crate::platform::clock::now_millis().to_string().as_bytes(),
    )?;
    Ok(Some(file))
}

/// When a build last started a process, in milliseconds since the Unix epoch;
/// an unreadable record counts as never, and the in-use lock still protects a running build.
fn last_used(directory: &Path) -> io::Result<Option<u64>> {
    let file = match File::open(directory.join(LAST_USED)) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    match crate::bounded::read(file, 32)?.map(String::from_utf8) {
        Some(Ok(text)) => match text.trim().parse() {
            Ok(millis) => Ok(Some(millis)),
            Err(_unreadable) => Ok(None),
        },
        Some(Err(_)) | None => Ok(None),
    }
}

/// Remove installed builds other than `keep` that no process uses and none used for a week.
pub(crate) fn prune(versions: &Path, keep: &str) -> io::Result<Vec<String>> {
    prune_before(
        versions,
        keep,
        crate::platform::clock::now_millis().saturating_sub(KEEP_MILLIS),
    )
}

fn prune_before(versions: &Path, keep: &str, cutoff: u64) -> io::Result<Vec<String>> {
    let entries = match fs::read_dir(versions) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut removed = Vec::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == keep || !entry.file_type()?.is_dir() || !is_build_tag(&name) {
            continue;
        }
        let directory = entry.path();
        if last_used(&directory)?.is_some_and(|used| used > cutoff) {
            continue;
        }
        let lock = lock_file(&directory)?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => continue,
            Err(TryLockError::Error(error)) => return Err(error),
        }
        // Windows cannot delete a file that is still open, and a running build's files stay locked.
        drop(lock);
        match raw::remove_dir_all(&directory) {
            Ok(()) => removed.push(name),
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {}
            Err(error) => return Err(error),
        }
    }
    Ok(removed)
}

fn is_build_tag(name: &str) -> bool {
    name.len() == 16 && name.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::{LAST_USED, LOCK, installed_directory, lock_file, prune_before};
    use crate::testing;

    #[test]
    fn only_unused_builds_other_than_the_current_one_are_removed() {
        let root = tempfile::tempdir().unwrap();
        let versions = root.path().join("versions");
        for tag in ["0000000000000001", "0000000000000002", "0000000000000003"] {
            testing::mkdir(&versions.join(tag).join("bin"));
        }
        testing::mkdir(&versions.join("notes"));
        testing::write(&versions.join("0000000000000004").join(LAST_USED), "900");
        let running = lock_file(&versions.join("0000000000000002")).unwrap();
        running.lock_shared().unwrap();
        let removed = prune_before(&versions, "0000000000000003", 500).unwrap();
        assert_eq!(removed, ["0000000000000001"]);
        assert!(testing::is_file(
            &versions.join("0000000000000002").join(LOCK)
        ));
        assert!(testing::is_dir(&versions.join("0000000000000003")));
        assert!(
            testing::is_dir(&versions.join("0000000000000004")),
            "a recently used build stays"
        );
        assert!(testing::is_dir(&versions.join("notes")));
        drop(running);
        assert_eq!(
            prune_before(&versions, "0000000000000003", 1000).unwrap(),
            ["0000000000000002", "0000000000000004"]
        );
    }

    #[test]
    fn only_executables_inside_a_versions_tree_belong_to_a_build() {
        let installed = std::path::Path::new("/home/me/.cargo/domyjob/versions/00ff/bin/domyjob");
        assert_eq!(
            installed_directory(installed).as_deref(),
            Some(std::path::Path::new(
                "/home/me/.cargo/domyjob/versions/00ff"
            ))
        );
        assert_eq!(
            installed_directory(std::path::Path::new("/repo/target/debug/domyjob")),
            None
        );
    }
}
