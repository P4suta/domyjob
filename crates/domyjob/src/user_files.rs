use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
#[error("{action} {path}: {source}")]
pub struct UserFileError {
    action: &'static str,
    path: PathBuf,
    source: std::io::Error,
}

impl From<crate::durable::DurableError> for UserFileError {
    fn from(error: crate::durable::DurableError) -> Self {
        Self {
            action: error.action,
            path: error.path,
            source: error.source,
        }
    }
}

fn failed(
    action: &'static str,
    path: &Path,
) -> impl FnOnce(std::io::Error) -> UserFileError + use<> {
    let path = path.to_path_buf();
    move |source| UserFileError {
        action,
        path,
        source,
    }
}

pub fn present(path: &Path) -> Result<bool, UserFileError> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(failed("checking", path)(error)),
    }
}

pub fn write(path: &Path, bytes: &[u8]) -> Result<(), UserFileError> {
    parents(path)?;
    Ok(crate::durable::write(
        path,
        bytes,
        crate::durable::Access::Shared,
    )?)
}

#[derive(Debug)]
pub struct Staged(crate::durable::Staged);

#[expect(
    clippy::disallowed_methods,
    reason = "directories inside the user's own project, created as the user would"
)]
pub fn parents(path: &Path) -> Result<(), UserFileError> {
    match path.parent().filter(|p| !p.as_os_str().is_empty()) {
        Some(parent) => std::fs::create_dir_all(parent).map_err(failed("creating", parent)),
        None => Ok(()),
    }
}

impl Staged {
    pub fn beside(destination: &Path) -> Result<Self, UserFileError> {
        parents(destination)?;
        Ok(Self(crate::durable::Staged::beside(
            destination,
            crate::durable::Access::Shared,
        )?))
    }

    pub const fn file(&mut self) -> &mut std::fs::File {
        self.0.file()
    }

    pub fn commit(self) -> Result<u64, UserFileError> {
        Ok(self.0.commit()?)
    }
}

#[expect(
    clippy::disallowed_methods,
    reason = "removing a file the user asked to uninstall"
)]
pub fn remove(path: &Path) -> Result<(), UserFileError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(failed("removing", path)(e)),
    }
}

#[expect(
    clippy::disallowed_methods,
    reason = "swapping the running executable for a verified update"
)]
pub fn replace_executable(fresh: &Path, current: &Path) -> Result<(), UserFileError> {
    let staged = crate::durable::beside(current, "new")?;
    std::fs::copy(fresh, &staged).map_err(failed("staging", &staged))?;
    if !crate::platform::FAMILY.replaces_running_executables() {
        let retired = crate::durable::beside(current, "old")?;
        std::fs::rename(current, &retired).map_err(failed("retiring", current))?;
        if let Err(error) = std::fs::rename(&staged, current) {
            match std::fs::rename(&retired, current) {
                Ok(()) | Err(_) => {}
            }
            return Err(failed("installing", current)(error));
        }
        sweep_retired(current);
        return Ok(());
    }
    std::fs::rename(&staged, current).map_err(failed("installing", current))
}

#[expect(
    clippy::disallowed_methods,
    reason = "removing retired executables that no process still runs"
)]
pub fn sweep_retired(current: &Path) {
    let (Some(dir), Some(name)) = (current.parent(), current.file_name()) else {
        return;
    };
    let prefix = format!(".{}.old-", name.to_string_lossy());
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().starts_with(&prefix) {
            match std::fs::remove_file(entry.path()) {
                Ok(()) | Err(_) => {}
            }
        }
    }
}

#[cfg(test)]
#[expect(
    clippy::disallowed_methods,
    reason = "tests build their fixtures directly on disk"
)]
mod tests {
    use super::*;

    #[test]
    fn a_staged_file_appears_only_when_committed_and_leaves_nothing_otherwise() {
        let tmp = tempfile::tempdir().unwrap();
        let destination = tmp.path().join("out").join("report.json");
        std::fs::create_dir_all(destination.parent().unwrap()).unwrap();
        std::fs::write(&destination, b"the old report").unwrap();

        let mut abandoned = Staged::beside(&destination).unwrap();
        std::io::Write::write_all(abandoned.file(), b"half of a new").unwrap();
        drop(abandoned);
        assert_eq!(std::fs::read(&destination).unwrap(), b"the old report");
        assert_eq!(
            std::fs::read_dir(destination.parent().unwrap())
                .unwrap()
                .count(),
            1
        );

        let mut kept = Staged::beside(&destination).unwrap();
        std::io::Write::write_all(kept.file(), b"the new report").unwrap();
        assert_eq!(kept.commit().unwrap(), 14);
        assert_eq!(std::fs::read(&destination).unwrap(), b"the new report");
        assert_eq!(
            std::fs::read_dir(destination.parent().unwrap())
                .unwrap()
                .count(),
            1
        );
    }

    #[test]
    fn only_retired_copies_of_the_executable_are_swept() {
        let tmp = tempfile::tempdir().unwrap();
        let current = tmp.path().join("domyjob.exe");
        let keep = [
            "domyjob.exe",
            ".domyjob.exe.new-1",
            ".other.old-1",
            "domyjob.exe.old-1",
        ];
        for name in keep
            .iter()
            .chain(&[".domyjob.exe.old-1", ".domyjob.exe.old-2"])
        {
            std::fs::write(tmp.path().join(name), b"x").unwrap();
        }
        sweep_retired(&current);
        let mut left: Vec<String> = std::fs::read_dir(tmp.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        let mut expected = keep.map(str::to_owned).to_vec();
        expected.sort();
        assert_eq!(left, expected);
    }

    #[test]
    fn replacing_an_executable_leaves_the_new_one_and_no_litter() {
        let tmp = tempfile::tempdir().unwrap();
        let current = tmp.path().join("domyjob");
        let fresh = tmp.path().join("fresh");
        std::fs::write(&current, b"old").unwrap();
        std::fs::write(&fresh, b"new").unwrap();
        replace_executable(&fresh, &current).unwrap();
        assert_eq!(std::fs::read(&current).unwrap(), b"new");
        let names: Vec<String> = std::fs::read_dir(tmp.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names.len(), 2, "{names:?}");
    }
}
