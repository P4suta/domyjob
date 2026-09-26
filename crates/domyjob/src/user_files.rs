use std::path::Path;

pub fn present(path: &Path) -> Result<bool, crate::failure::IoFailure> {
    crate::faults::at("user_files::check", path).map_err(crate::failure::io("checking", path))?;
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(crate::failure::io("checking", path)(error)),
    }
}

pub fn write(path: &Path, bytes: &[u8]) -> Result<(), crate::failure::IoFailure> {
    parents(path)?;
    crate::durable::write(path, bytes, crate::durable::Access::Shared)
}

#[derive(Debug)]
pub struct Staged(crate::durable::Staged);

#[expect(
    clippy::disallowed_methods,
    reason = "directories inside the user's own project, created as the user would"
)]
pub fn parents(path: &Path) -> Result<(), crate::failure::IoFailure> {
    match path.parent().filter(|p| !p.as_os_str().is_empty()) {
        Some(parent) => {
            crate::faults::at("user_files::parents", parent)
                .map_err(crate::failure::io("creating", parent))?;
            std::fs::create_dir_all(parent).map_err(crate::failure::io("creating", parent))
        }
        None => Ok(()),
    }
}

impl Staged {
    pub fn beside(destination: &Path) -> Result<Self, crate::failure::IoFailure> {
        parents(destination)?;
        Ok(Self(crate::durable::Staged::beside(
            destination,
            crate::durable::Access::Shared,
        )?))
    }

    pub const fn file(&mut self) -> &mut std::fs::File {
        self.0.file()
    }

    pub fn commit(self) -> Result<u64, crate::failure::IoFailure> {
        self.0.commit()
    }
}

#[expect(
    clippy::disallowed_methods,
    reason = "removing a file the user asked to uninstall"
)]
pub fn remove(path: &Path) -> Result<(), crate::failure::IoFailure> {
    crate::faults::at("user_files::remove", path).map_err(crate::failure::io("removing", path))?;
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(crate::failure::io("removing", path)(e)),
    }
}

#[expect(
    clippy::disallowed_methods,
    reason = "swapping the running executable for a verified update"
)]
pub fn replace_executable(fresh: &Path, current: &Path) -> Result<(), crate::failure::IoFailure> {
    let staged = crate::durable::beside(current, "new")?;
    crate::faults::at("user_files::copy", &staged)
        .map_err(crate::failure::io("staging", &staged))?;
    std::fs::copy(fresh, &staged).map_err(crate::failure::io("staging", &staged))?;
    if !crate::platform::FAMILY.replaces_running_executables() {
        let retired = crate::durable::beside(current, "old")?;
        crate::faults::at("user_files::retire", current)
            .map_err(crate::failure::io("retiring", current))?;
        std::fs::rename(current, &retired).map_err(crate::failure::io("retiring", current))?;
        crate::faults::at("user_files::install", current)
            .map_err(crate::failure::io("installing", current))?;
        if let Err(error) = std::fs::rename(&staged, current) {
            match std::fs::rename(&retired, current) {
                Ok(()) | Err(_) => {}
            }
            return Err(crate::failure::io("installing", current)(error));
        }
        sweep_retired(current);
        return Ok(());
    }
    crate::faults::at("user_files::install", current)
        .map_err(crate::failure::io("installing", current))?;
    std::fs::rename(&staged, current).map_err(crate::failure::io("installing", current))
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
    fn a_user_file_can_be_written_again_after_a_crash_at_every_step() {
        let steps = {
            let tmp = tempfile::tempdir().unwrap();
            let destination = tmp.path().join("out").join("report.json");
            let crashing = crate::faults::crash_after(tmp.path(), None);
            write(&destination, b"new").unwrap();
            crashing.steps()
        };
        assert!(steps > 0);
        for step in 0..steps {
            let tmp = tempfile::tempdir().unwrap();
            let destination = tmp.path().join("out").join("report.json");
            write(&destination, b"old").unwrap();
            {
                let _crashing = crate::faults::crash_after(tmp.path(), Some(step));
                match write(&destination, b"new") {
                    Ok(()) | Err(_) => {}
                }
            }
            write(&destination, b"new").unwrap();
            assert_eq!(std::fs::read(&destination).unwrap(), b"new");
        }
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
