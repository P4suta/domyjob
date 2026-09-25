use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use crate::domain::{BlobId, RelPath};
use crate::snapshot::{Change, Entry, Mode};

#[derive(Debug, thiserror::Error)]
pub enum PullError {
    #[error("{} changed here since the job was sent: {}", .0.len(), list(.0))]
    Diverged(Vec<RelPath>),
    #[error("{action} {path}: {source}")]
    Io {
        action: &'static str,
        path: PathBuf,
        source: std::io::Error,
    },
    #[error(transparent)]
    Written(#[from] crate::user_files::UserFileError),
}

fn list(paths: &[RelPath]) -> String {
    paths
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

fn io(action: &'static str, path: &Path) -> impl FnOnce(std::io::Error) -> PullError + use<> {
    let path = path.to_path_buf();
    move |source| PullError::Io {
        action,
        path,
        source,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Added,
    Modified,
    Removed,
}

impl Kind {
    #[must_use]
    pub const fn letter(self) -> char {
        match self {
            Self::Added => 'A',
            Self::Modified => 'M',
            Self::Removed => 'D',
        }
    }
}

#[must_use]
pub const fn kind(change: &Change) -> Kind {
    match (&change.before, &change.after) {
        (None, _) => Kind::Added,
        (Some(_), None) => Kind::Removed,
        (Some(_), Some(_)) => Kind::Modified,
    }
}

pub fn diverged(root: &Path, changes: &[Change]) -> Result<Vec<RelPath>, PullError> {
    let mut diverged = Vec::new();
    for change in changes {
        if !as_sent(&root.join(change.path.as_str()), change.before.as_ref())? {
            diverged.push(change.path.clone());
        }
    }
    Ok(diverged)
}

fn as_sent(path: &Path, sent: Option<&Entry>) -> Result<bool, PullError> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => Some(meta),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(io("checking", path)(error)),
    };
    Ok(match (sent, meta) {
        (None, None) => true,
        (None, Some(_)) | (Some(_), None) => false,
        (Some(Entry::File { blob, .. }), Some(meta)) => {
            meta.is_file()
                && BlobId::of(&std::fs::read(path).map_err(io("reading", path))?) == *blob
        }
        (Some(Entry::Symlink { target }), Some(meta)) => {
            meta.is_symlink()
                && std::fs::read_link(path)
                    .map_err(io("reading", path))?
                    .as_os_str()
                    .to_string_lossy()
                    == target.as_str()
        }
    })
}

pub fn apply(
    root: &Path,
    changes: &[Change],
    contents: &BTreeMap<RelPath, Vec<u8>>,
) -> Result<(), PullError> {
    let diverged = diverged(root, changes)?;
    if !diverged.is_empty() {
        return Err(PullError::Diverged(diverged));
    }
    for change in changes {
        let path = root.join(change.path.as_str());
        match &change.after {
            None => remove(&path)?,
            Some(Entry::File { mode, .. }) => {
                let bytes = contents.get(&change.path).map_or(&[][..], Vec::as_slice);
                write(&path, bytes, *mode)?;
            }
            Some(Entry::Symlink { target }) => link(&path, target)?,
        }
    }
    Ok(())
}

#[expect(
    clippy::disallowed_methods,
    reason = "removing a file the job removed, after checking it is still the one that was sent"
)]
fn remove(path: &Path) -> Result<(), PullError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io("removing", path)(error)),
    }
}

fn write(path: &Path, bytes: &[u8], mode: Mode) -> Result<(), PullError> {
    if std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_symlink()) {
        remove(path)?;
    }
    let mut staged = crate::user_files::Staged::beside(path)?;
    staged
        .file()
        .write_all(bytes)
        .map_err(io("writing", path))?;
    executable(staged.file(), mode, path)?;
    staged.commit()?;
    Ok(())
}

#[cfg(unix)]
fn executable(file: &std::fs::File, mode: Mode, path: &Path) -> Result<(), PullError> {
    use std::os::unix::fs::PermissionsExt as _;
    let bits = match mode {
        Mode::Executable => 0o755,
        Mode::Regular => 0o644,
    };
    file.set_permissions(std::fs::Permissions::from_mode(bits))
        .map_err(io("setting the mode of", path))
}

#[cfg(not(unix))]
#[expect(
    clippy::unnecessary_wraps,
    clippy::missing_const_for_fn,
    reason = "Windows has no executable bit to carry"
)]
fn executable(_file: &std::fs::File, _mode: Mode, _path: &Path) -> Result<(), PullError> {
    Ok(())
}

#[cfg(unix)]
fn link(path: &Path, target: &str) -> Result<(), PullError> {
    remove(path)?;
    crate::user_files::parents(path)?;
    std::os::unix::fs::symlink(target, path).map_err(io("linking", path))
}

#[cfg(not(unix))]
fn link(path: &Path, target: &str) -> Result<(), PullError> {
    Err(io("linking", path)(std::io::Error::other(format!(
        "a symbolic link to {target} cannot be recreated here"
    ))))
}

#[cfg(test)]
#[expect(
    clippy::disallowed_methods,
    reason = "tests build the local project on disk directly"
)]
mod tests {
    use super::*;

    fn file(text: &[u8], mode: Mode) -> Entry {
        Entry::File {
            blob: BlobId::of(text),
            size: crate::domain::len_u64(text.len()),
            mode,
        }
    }

    fn change(path: &str, before: Option<Entry>, after: Option<Entry>) -> Change {
        Change {
            path: path.parse().unwrap(),
            before,
            after,
        }
    }

    #[test]
    fn changes_land_only_where_nothing_moved_since_sending() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("edited.txt"), b"old").unwrap();
        std::fs::write(root.join("gone.txt"), b"bye").unwrap();
        let changes = vec![
            change(
                "new/born.sh",
                None,
                Some(file(b"echo hi", Mode::Executable)),
            ),
            change(
                "edited.txt",
                Some(file(b"old", Mode::Regular)),
                Some(file(b"new", Mode::Regular)),
            ),
            change("gone.txt", Some(file(b"bye", Mode::Regular)), None),
        ];
        let contents = BTreeMap::from([
            ("new/born.sh".parse().unwrap(), b"echo hi".to_vec()),
            ("edited.txt".parse().unwrap(), b"new".to_vec()),
        ]);
        assert_eq!(
            changes.iter().map(|c| kind(c).letter()).collect::<String>(),
            "AMD"
        );

        std::fs::write(root.join("edited.txt"), b"mine").unwrap();
        let refused = apply(root, &changes, &contents).unwrap_err();
        assert!(
            matches!(&refused, PullError::Diverged(paths) if paths.len() == 1),
            "{refused}"
        );
        assert!(!root.join("new/born.sh").try_exists().unwrap());
        assert!(root.join("gone.txt").try_exists().unwrap());

        std::fs::write(root.join("edited.txt"), b"old").unwrap();
        apply(root, &changes, &contents).unwrap();
        assert_eq!(std::fs::read(root.join("new/born.sh")).unwrap(), b"echo hi");
        assert_eq!(std::fs::read(root.join("edited.txt")).unwrap(), b"new");
        assert!(!root.join("gone.txt").try_exists().unwrap());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(root.join("new/born.sh"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o111, 0o111);
        }
        assert_eq!(diverged(root, &changes).unwrap().len(), 3);
    }
}
