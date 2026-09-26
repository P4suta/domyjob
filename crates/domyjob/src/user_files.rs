use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
#[error("{action} {path}: {source}")]
pub struct UserFileError {
    action: &'static str,
    path: PathBuf,
    source: std::io::Error,
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

#[expect(
    clippy::disallowed_methods,
    reason = "files the user named on the command line or owns in their configuration"
)]
pub fn write(path: &Path, bytes: &[u8]) -> Result<(), UserFileError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(failed("creating", parent))?;
    }
    std::fs::write(path, bytes).map_err(failed("writing", path))
}

fn unique_beside(path: &Path, tag: &str) -> Result<PathBuf, UserFileError> {
    let mut random = [0u8; 8];
    getrandom::fill(&mut random)
        .map_err(|e| failed("naming a file beside", path)(std::io::Error::other(e.to_string())))?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    Ok(path.with_file_name(format!(".{name}.{tag}-{}", crate::trust::hex(&random))))
}

#[derive(Debug)]
pub struct Staged {
    file: std::fs::File,
    temporary: PathBuf,
    destination: PathBuf,
    committed: bool,
}

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
    #[expect(
        clippy::disallowed_methods,
        reason = "files the user named on the command line, written beside them first"
    )]
    pub fn beside(destination: &Path) -> Result<Self, UserFileError> {
        if let Some(parent) = destination.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(failed("creating", parent))?;
        }
        let temporary = unique_beside(destination, "part")?;
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(failed("creating", &temporary))?;
        Ok(Self {
            file,
            temporary,
            destination: destination.to_path_buf(),
            committed: false,
        })
    }

    pub const fn file(&mut self) -> &mut std::fs::File {
        &mut self.file
    }

    #[expect(
        clippy::disallowed_methods,
        reason = "putting a fully received file where the user asked for it"
    )]
    pub fn commit(mut self) -> Result<u64, UserFileError> {
        self.file
            .sync_all()
            .map_err(failed("syncing", &self.temporary))?;
        let bytes = self
            .file
            .metadata()
            .map_err(failed("measuring", &self.temporary))?
            .len();
        std::fs::rename(&self.temporary, &self.destination)
            .map_err(failed("writing", &self.destination))?;
        self.committed = true;
        Ok(bytes)
    }
}

impl Drop for Staged {
    #[expect(
        clippy::disallowed_methods,
        reason = "removing the partial file of a transfer that did not finish"
    )]
    fn drop(&mut self) {
        if !self.committed {
            match std::fs::remove_file(&self.temporary) {
                Ok(()) | Err(_) => {}
            }
        }
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
    let staged = unique_beside(current, "new")?;
    std::fs::copy(fresh, &staged).map_err(failed("staging", &staged))?;
    if cfg!(windows) {
        let retired = unique_beside(current, "old")?;
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
