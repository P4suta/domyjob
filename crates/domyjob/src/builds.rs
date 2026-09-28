#![expect(
    clippy::redundant_pub_crate,
    reason = "the binary composition root uses this private module"
)]
#![expect(
    clippy::disallowed_methods,
    reason = "this module owns the build-specific install directories and their removal"
)]
//! Build-specific install directories, their in-use locks, and pruning of unused ones.
//!
//! Every process started from an installed build holds a shared lock inside that build's directory, so a build is removed only when no process of it runs.

use std::fs::{self, File, TryLockError};
use std::io;
use std::path::{Path, PathBuf};

const LOCK: &str = "in-use.lock";

/// The installed build directory this executable runs from: `versions/TAG/bin/domyjob`.
fn installed_directory(executable: &Path) -> Option<PathBuf> {
    let bin = executable.parent()?;
    let build = bin.parent()?;
    let versions = build.parent()?;
    (bin.file_name()? == "bin" && versions.file_name()? == "versions").then(|| build.to_path_buf())
}

fn lock_file(directory: &Path) -> io::Result<File> {
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(directory.join(LOCK))
}

/// Mark this process as a user of its installed build for as long as the returned file lives.
pub(crate) fn hold_current() -> io::Result<Option<File>> {
    let executable = fs::canonicalize(std::env::current_exe()?)?;
    let Some(directory) = installed_directory(&executable) else {
        return Ok(None);
    };
    let file = lock_file(&directory)?;
    file.lock_shared()?;
    Ok(Some(file))
}

/// Remove installed builds other than `keep` that no process uses; report what was removed.
pub(crate) fn prune(versions: &Path, keep: &str) -> io::Result<Vec<String>> {
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
        let lock = lock_file(&directory)?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => continue,
            Err(TryLockError::Error(error)) => return Err(error),
        }
        // Windows cannot delete a file that is still open, and a running build's files stay locked.
        drop(lock);
        match fs::remove_dir_all(&directory) {
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
    use std::fs;

    use super::{LOCK, installed_directory, lock_file, prune};

    #[test]
    fn only_unused_builds_other_than_the_current_one_are_removed() {
        let root = tempfile::tempdir().unwrap();
        let versions = root.path().join("versions");
        for tag in ["0000000000000001", "0000000000000002", "0000000000000003"] {
            fs::create_dir_all(versions.join(tag).join("bin")).unwrap();
        }
        fs::create_dir_all(versions.join("notes")).unwrap();
        let running = lock_file(&versions.join("0000000000000002")).unwrap();
        running.lock_shared().unwrap();
        let removed = prune(&versions, "0000000000000003").unwrap();
        assert_eq!(removed, ["0000000000000001"]);
        assert!(versions.join("0000000000000002").join(LOCK).is_file());
        assert!(versions.join("0000000000000003").is_dir());
        assert!(versions.join("notes").is_dir());
        drop(running);
        assert_eq!(
            prune(&versions, "0000000000000003").unwrap(),
            ["0000000000000002"]
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
