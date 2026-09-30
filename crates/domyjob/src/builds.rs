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
const STARTED: &str = "started";
const PINNED: &str = "pinned";
const ORDER: &str = "order";
const ORDER_LOCK: &str = "order.lock";
const KEEP_IDLE: usize = 2;

fn installed_directory(executable: &Path) -> Option<PathBuf> {
    let bin = executable.parent()?;
    let build = bin.parent()?;
    let versions = build.parent()?;
    (bin.file_name()? == "bin" && versions.file_name()? == "versions").then(|| build.to_path_buf())
}

fn versions_of(directory: &Path) -> io::Result<&Path> {
    directory
        .parent()
        .ok_or_else(|| io::Error::other("an installed build has no versions directory"))
}

fn lock_file(directory: &Path) -> io::Result<File> {
    raw::open_lock(&directory.join(LOCK))
}

pub(crate) fn hold_current() -> io::Result<Option<File>> {
    let executable = fs::canonicalize(std::env::current_exe()?)?;
    let Some(directory) = installed_directory(&executable) else {
        return Ok(None);
    };
    let file = lock_file(&directory)?;
    file.lock_shared()?;
    let place = next_place(versions_of(&directory)?)?;
    raw::write(&directory.join(STARTED), place.to_string().as_bytes())?;
    Ok(Some(file))
}

fn next_place(versions: &Path) -> io::Result<u64> {
    let order = raw::open_lock(&versions.join(ORDER_LOCK))?;
    order.lock()?;
    let next = number(&versions.join(ORDER))?
        .unwrap_or(0)
        .saturating_add(1);
    raw::write(&versions.join(ORDER), next.to_string().as_bytes())?;
    Ok(next)
}

fn record(path: &Path) -> io::Result<Option<String>> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    match crate::bounded::read(file, 32)?.map(String::from_utf8) {
        Some(Ok(text)) => Ok(Some(text.trim().to_owned())),
        Some(Err(_)) | None => Ok(None),
    }
}

fn number(path: &Path) -> io::Result<Option<u64>> {
    Ok(match record(path)?.map(|text| text.parse()) {
        Some(Ok(number)) => Some(number),
        Some(Err(_)) | None => None,
    })
}

fn running(directory: &Path) -> io::Result<bool> {
    match lock_file(directory)?.try_lock() {
        Ok(()) => Ok(false),
        Err(TryLockError::WouldBlock) => Ok(true),
        Err(TryLockError::Error(error)) => Err(error),
    }
}

pub(crate) fn pin(program: &Path) -> io::Result<()> {
    let directory = installed_directory(program).ok_or_else(|| {
        io::Error::other(format!("{} is not an installed build", program.display()))
    })?;
    let tag = directory
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    raw::write(&versions_of(&directory)?.join(PINNED), tag.as_bytes())
}

pub(crate) fn prune(versions: &Path, keep: &str) -> io::Result<Vec<String>> {
    let entries = match fs::read_dir(versions) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let pinned = record(&versions.join(PINNED))?;
    let mut idle = Vec::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == keep
            || pinned.as_deref() == Some(name.as_str())
            || !entry.file_type()?.is_dir()
            || !is_build_tag(&name)
        {
            continue;
        }
        let directory = entry.path();
        if running(&directory)? {
            continue;
        }
        idle.push((
            number(&directory.join(STARTED))?.unwrap_or(0),
            name,
            directory,
        ));
    }
    let mut removed = Vec::new();
    for (_, name, directory) in crate::retention::beyond_newest(idle, KEEP_IDLE) {
        match raw::remove_dir_all(&directory) {
            Ok(()) => removed.push(name),
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {}
            Err(error) => return Err(error),
        }
    }
    removed.sort();
    Ok(removed)
}

fn is_build_tag(name: &str) -> bool {
    name.len() == 16 && name.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::{PINNED, STARTED, installed_directory, lock_file, next_place, pin, prune};
    use crate::testing;

    #[test]
    fn builds_stay_while_current_running_pinned_or_among_the_last_started() {
        let root = tempfile::tempdir().unwrap();
        let versions = root.path().join("versions");
        for (tag, started) in [
            (1, Some(1)),
            (2, Some(2)),
            (3, Some(3)),
            (4, Some(4)),
            (5, Some(5)),
            (6, None),
            (7, Some(7)),
        ] {
            let directory = versions.join(format!("{tag:016}"));
            testing::mkdir(&directory.join("bin"));
            if let Some(place) = started {
                testing::write(&directory.join(STARTED), format!("{place}"));
            }
        }
        testing::write(&versions.join(PINNED), format!("{:016}", 4));
        testing::mkdir(&versions.join("notes"));
        let running = lock_file(&versions.join(format!("{:016}", 2))).unwrap();
        running.lock_shared().unwrap();
        assert_eq!(
            prune(&versions, &format!("{:016}", 7)).unwrap(),
            [format!("{:016}", 1), format!("{:016}", 6)]
        );
        for kept in [2, 3, 4, 5, 7] {
            assert!(testing::is_dir(&versions.join(format!("{kept:016}"))));
        }
        assert!(testing::is_dir(&versions.join("notes")));
        running.unlock().unwrap();
        drop(running);
        assert_eq!(
            prune(&versions, &format!("{:016}", 7)).unwrap(),
            [format!("{:016}", 2)]
        );
    }

    #[test]
    fn each_start_takes_the_next_place_and_setup_pins_one_build() {
        let root = tempfile::tempdir().unwrap();
        let versions = root.path().join("versions");
        testing::mkdir(&versions);
        assert_eq!(next_place(&versions).unwrap(), 1);
        assert_eq!(next_place(&versions).unwrap(), 2);
        let program = |tag: u64| {
            versions
                .join(format!("{tag:016}"))
                .join("bin")
                .join("domyjob")
        };
        for tag in [1, 2] {
            testing::mkdir(&versions.join(format!("{tag:016}")).join("bin"));
        }
        pin(&program(1)).unwrap();
        pin(&program(2)).unwrap();
        assert_eq!(testing::read(&versions.join(PINNED)), format!("{:016}", 2));
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
