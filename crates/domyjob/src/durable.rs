#![expect(
    clippy::disallowed_methods,
    reason = "the one place a file is staged beside its destination and moved into place"
)]

use std::io::Write as _;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Private,
    Shared,
}

#[derive(Debug, thiserror::Error)]
#[error("{action} {path}: {source}")]
pub struct DurableError {
    pub action: &'static str,
    pub path: PathBuf,
    pub source: std::io::Error,
}

fn failed(
    action: &'static str,
    path: &Path,
) -> impl FnOnce(std::io::Error) -> DurableError + use<> {
    let path = path.to_path_buf();
    move |source| DurableError {
        action,
        path,
        source,
    }
}

pub fn beside(destination: &Path, tag: &str) -> Result<PathBuf, DurableError> {
    let mut random = [0u8; 8];
    getrandom::fill(&mut random).map_err(|error| {
        failed("naming a file beside", destination)(std::io::Error::other(error.to_string()))
    })?;
    let name = destination
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    Ok(destination.with_file_name(format!(".{name}.{tag}-{}", crate::trust::hex(&random))))
}

#[derive(Debug)]
pub struct Staged {
    file: std::fs::File,
    temporary: PathBuf,
    destination: PathBuf,
    committed: bool,
}

impl Staged {
    pub fn beside(destination: &Path, access: Access) -> Result<Self, DurableError> {
        let temporary = beside(destination, "staged")?;
        let mut options = match access {
            Access::Private => crate::platform::private_options(),
            Access::Shared => std::fs::OpenOptions::new(),
        };
        let file = options
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

    pub fn commit(mut self) -> Result<u64, DurableError> {
        self.file
            .sync_all()
            .map_err(failed("syncing", &self.temporary))?;
        let bytes = self
            .file
            .metadata()
            .map_err(failed("measuring", &self.temporary))?
            .len();
        std::fs::rename(&self.temporary, &self.destination)
            .map_err(failed("replacing", &self.destination))?;
        self.committed = true;
        let dir = self.destination.parent().unwrap_or_else(|| Path::new("."));
        crate::platform::sync_dir(dir).map_err(failed("syncing", dir))?;
        Ok(bytes)
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        if !self.committed {
            match std::fs::remove_file(&self.temporary) {
                Ok(()) | Err(_) => {}
            }
        }
    }
}

pub fn write(destination: &Path, bytes: &[u8], access: Access) -> Result<(), DurableError> {
    let mut staged = Staged::beside(destination, access)?;
    staged
        .file()
        .write_all(bytes)
        .map_err(failed("writing", destination))?;
    staged.commit().map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_staged_file_appears_whole_or_not_at_all_and_leaves_nothing_beside_it() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("file");
        write(&target, b"first", Access::Private).unwrap();
        let mut abandoned = Staged::beside(&target, Access::Shared).unwrap();
        abandoned.file().write_all(b"half").unwrap();
        drop(abandoned);
        let mut kept = Staged::beside(&target, Access::Shared).unwrap();
        kept.file().write_all(b"second").unwrap();
        assert_eq!(kept.commit().unwrap(), 6);
        let names: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .map(|item| item.unwrap().file_name())
            .collect();
        assert_eq!(names, [std::ffi::OsString::from("file")]);
        assert_eq!(std::fs::read(&target).unwrap(), b"second");
    }
}
