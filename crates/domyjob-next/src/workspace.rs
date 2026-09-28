#![expect(
    clippy::redundant_pub_crate,
    reason = "the binary composition root uses this private module"
)]
#![expect(
    clippy::disallowed_methods,
    reason = "the workspace effect creates files exclusively beneath an open directory capability"
)]

use std::io;
use std::path::{Path, PathBuf};

use cap_std::fs::{Dir, OpenOptions};
use domyjob_core::domain::RelativePath;
use thiserror::Error;

use crate::platform;

#[derive(Debug, Error)]
pub(crate) enum WorkspaceError {
    #[error("workspace I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("a file or link blocks a workspace directory")]
    Blocked,
}

#[derive(Debug)]
pub(crate) struct Rooted(Dir);

impl Rooted {
    pub(crate) fn open(root: &Path) -> Result<Self, WorkspaceError> {
        Ok(Self(Dir::open_ambient_dir(
            root,
            cap_std::ambient_authority(),
        )?))
    }

    pub(crate) fn make_parents(&self, path: &RelativePath) -> Result<(), WorkspaceError> {
        let mut current = PathBuf::new();
        let mut parts = path.as_str().split('/').peekable();
        while let Some(part) = parts.next() {
            if parts.peek().is_none() {
                break;
            }
            current.push(part);
            match self.0.symlink_metadata(&current) {
                Ok(metadata) if metadata.is_dir() && !metadata.is_symlink() => {}
                Ok(_blocked) => return Err(WorkspaceError::Blocked),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    self.0.create_dir(&current)?;
                }
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    pub(crate) fn create_file(
        &self,
        path: &RelativePath,
        executable: bool,
    ) -> Result<cap_std::fs::File, WorkspaceError> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        platform::creation_mode(&mut options, executable);
        Ok(self.0.open_with(path.as_str(), &options)?)
    }
}
