use crate::failure::io;
use std::collections::BTreeSet;
use std::io::{ErrorKind, Read as _, Write as _};
use std::path::{Path, PathBuf};

use cap_std::fs::{Dir, OpenOptions};

use crate::domain::{BlobId, RelPath};
use crate::paths::Family;
use crate::platform::Moded as _;
use crate::snapshot::{Entry, Mode};

#[derive(Debug, thiserror::Error)]
pub enum TreeError {
    #[error(transparent)]
    Io(#[from] crate::failure::IoFailure),
    #[error("{0} stands where a directory is needed")]
    Blocked(RelPath),
    #[error("{0} still holds files")]
    Occupied(RelPath),
    #[error("{0} links to a target that cannot travel between systems")]
    Unportable(PathBuf),
}

#[derive(Debug)]
pub enum Found {
    Nothing,
    Directory,
    Entry(cap_std::fs::Metadata),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Parents {
    Directories,
    Missing,
    Blocked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Blockers {
    Refuse,
    Replace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Contents {
    EmptyDirectoriesOnly,
    Anything,
}

#[derive(Debug, Clone, Copy)]
pub enum Placed<'a> {
    File(&'a [u8], Mode),
    Link(&'a str),
}

pub type Removed = BTreeSet<RelPath>;

#[must_use]
pub fn ancestors(rel: &RelPath) -> Vec<RelPath> {
    let parts: Vec<&str> = rel.parts().collect();
    (1..parts.len())
        .filter_map(|count| match parts.get(..count)?.join("/").parse() {
            Ok(prefix) => Some(prefix),
            Err(_never_for_a_prefix_of_a_valid_path) => None,
        })
        .collect()
}

fn child_of(rel: &RelPath, name: &std::ffi::OsStr) -> Option<RelPath> {
    match format!("{rel}/{}", name.to_str()?).parse() {
        Ok(child) => Some(child),
        Err(_unportable_or_metadata) => None,
    }
}

#[derive(Debug)]
pub struct Rooted {
    dir: Dir,
    root: PathBuf,
    family: Family,
}

impl Rooted {
    pub fn open(root: &Path) -> Result<Option<Self>, TreeError> {
        Self::open_as(root, crate::platform::FAMILY)
    }

    pub fn open_as(root: &Path, family: Family) -> Result<Option<Self>, TreeError> {
        match Dir::open_ambient_dir(root, cap_std::ambient_authority()) {
            Ok(dir) => Ok(Some(Self {
                dir,
                root: root.to_path_buf(),
                family,
            })),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
            Err(error) => Err(io("opening", root)(error).into()),
        }
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    #[must_use]
    pub fn shown(&self, rel: &RelPath) -> PathBuf {
        self.root.join(rel.to_local())
    }

    fn lstat(&self, rel: &RelPath) -> Result<Option<cap_std::fs::Metadata>, TreeError> {
        match self.dir.symlink_metadata(rel.to_local()) {
            Ok(meta) => Ok(Some(meta)),
            Err(error)
                if matches!(error.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) =>
            {
                Ok(None)
            }
            Err(error) => Err(io("checking", &self.shown(rel))(error).into()),
        }
    }

    pub fn parents(&self, rel: &RelPath, removed: &Removed) -> Result<Parents, TreeError> {
        for ancestor in ancestors(rel) {
            match self.lstat(&ancestor)? {
                None => return Ok(Parents::Missing),
                Some(meta) if meta.is_dir() => {}
                Some(_) if removed.contains(&ancestor) => return Ok(Parents::Missing),
                Some(_) => return Ok(Parents::Blocked),
            }
        }
        Ok(Parents::Directories)
    }

    pub fn found(&self, rel: &RelPath, removed: &Removed) -> Result<Found, TreeError> {
        Ok(match self.parents(rel, removed)? {
            Parents::Blocked | Parents::Missing => Found::Nothing,
            Parents::Directories => match self.lstat(rel)? {
                None => Found::Nothing,
                Some(meta) if meta.is_dir() => Found::Directory,
                Some(meta) => Found::Entry(meta),
            },
        })
    }

    pub fn holds(
        &self,
        rel: &RelPath,
        expected: Option<&Entry>,
        removed: &Removed,
    ) -> Result<bool, TreeError> {
        Ok(match (self.found(rel, removed)?, expected) {
            (Found::Nothing | Found::Directory, Some(_)) | (Found::Entry(_), None) => false,
            (Found::Nothing | Found::Directory, None) => true,
            (Found::Entry(meta), Some(Entry::File { blob, size, mode })) => {
                meta.is_file()
                    && meta.len() == *size
                    && (!self.family.modes() || meta.mode() == *mode)
                    && self.digest(rel)? == *blob
            }
            (Found::Entry(meta), Some(Entry::Symlink { target })) => {
                if meta.is_symlink() {
                    self.link(rel)? == *target
                } else {
                    !self.family.links()
                        && meta.is_file()
                        && meta.len() == crate::domain::len_u64(target.len())
                        && self.digest(rel)? == BlobId::of(target.as_bytes())
                }
            }
        })
    }

    pub fn entry(&self, rel: &RelPath, sent: Option<&Entry>) -> Result<Option<Entry>, TreeError> {
        let meta = match self.found(rel, &Removed::new())? {
            Found::Nothing | Found::Directory => return Ok(None),
            Found::Entry(meta) => meta,
        };
        if meta.is_symlink() {
            return Ok(Some(Entry::Symlink {
                target: self.link(rel)?,
            }));
        }
        if !meta.is_file() {
            return Ok(None);
        }
        if let Some(link @ Entry::Symlink { .. }) = sent
            && self.holds(rel, Some(link), &Removed::new())?
        {
            return Ok(Some(link.clone()));
        }
        let mode = match sent {
            _ if self.family.modes() => meta.mode(),
            Some(Entry::File { mode, .. }) => *mode,
            Some(Entry::Symlink { .. }) | None => Mode::Regular,
        };
        Ok(Some(Entry::File {
            blob: self.digest(rel)?,
            size: meta.len(),
            mode,
        }))
    }

    pub fn digest(&self, rel: &RelPath) -> Result<BlobId, TreeError> {
        let mut file = self
            .dir
            .open(rel.to_local())
            .map_err(io("opening", &self.shown(rel)))?;
        let mut hasher = blake3::Hasher::new();
        std::io::copy(&mut file, &mut hasher).map_err(io("reading", &self.shown(rel)))?;
        Ok(BlobId::from_hash(&hasher.finalize()))
    }

    pub fn read(&self, rel: &RelPath) -> Result<Vec<u8>, TreeError> {
        let mut bytes = Vec::new();
        self.dir
            .open(rel.to_local())
            .and_then(|mut file| file.read_to_end(&mut bytes))
            .map_err(io("reading", &self.shown(rel)))?;
        Ok(bytes)
    }

    pub fn open_file(&self, rel: &RelPath) -> Result<Option<std::fs::File>, TreeError> {
        let meta = self
            .dir
            .metadata(rel.to_local())
            .map_err(io("opening", &self.shown(rel)))?;
        if meta.is_dir() {
            return Ok(None);
        }
        let file = self
            .dir
            .open(rel.to_local())
            .map_err(io("opening", &self.shown(rel)))?;
        Ok(Some(file.into_std()))
    }

    fn link(&self, rel: &RelPath) -> Result<String, TreeError> {
        let target = self
            .dir
            .read_link_contents(rel.to_local())
            .map_err(io("reading", &self.shown(rel)))?;
        target
            .to_str()
            .map(str::to_owned)
            .ok_or_else(|| TreeError::Unportable(self.shown(rel)))
    }

    pub fn placeable(&self, rel: &RelPath, removed: &Removed) -> Result<bool, TreeError> {
        if self.parents(rel, removed)? == Parents::Blocked {
            return Ok(false);
        }
        match self.found(rel, removed)? {
            Found::Directory => self.emptied(rel, removed),
            Found::Nothing | Found::Entry(_) => Ok(true),
        }
    }

    pub fn emptied(&self, rel: &RelPath, removed: &Removed) -> Result<bool, TreeError> {
        let listing = self
            .dir
            .read_dir(rel.to_local())
            .map_err(io("listing", &self.shown(rel)))?;
        for item in listing {
            let item = item.map_err(io("listing", &self.shown(rel)))?;
            let Some(child) = child_of(rel, &item.file_name()) else {
                return Ok(false);
            };
            let meta = item
                .metadata()
                .map_err(io("checking", &self.shown(&child)))?;
            let gone = if meta.is_dir() && !meta.is_symlink() {
                self.emptied(&child, removed)?
            } else {
                removed.contains(&child)
            };
            if !gone {
                return Ok(false);
            }
        }
        Ok(true)
    }

    pub fn make_parents(&self, rel: &RelPath, blockers: Blockers) -> Result<(), TreeError> {
        for ancestor in ancestors(rel) {
            match (self.lstat(&ancestor)?, blockers) {
                (Some(meta), _) if meta.is_dir() => {}
                (Some(_), Blockers::Refuse) => return Err(TreeError::Blocked(ancestor)),
                (Some(_), Blockers::Replace) => {
                    self.remove_file(&ancestor)?;
                    self.create_dir(&ancestor)?;
                }
                (None, _) => self.create_dir(&ancestor)?,
            }
        }
        Ok(())
    }

    fn create_dir(&self, rel: &RelPath) -> Result<(), TreeError> {
        self.dir
            .create_dir(rel.to_local())
            .map_err(io("creating", &self.shown(rel)))
            .map_err(Into::into)
    }

    pub fn clear(&self, rel: &RelPath, contents: Contents) -> Result<(), TreeError> {
        match self.lstat(rel)? {
            None => Ok(()),
            Some(meta) if meta.is_dir() => self.clear_directory(rel, contents),
            Some(_) => self.remove_file(rel),
        }
    }

    pub fn clear_directory(&self, rel: &RelPath, contents: Contents) -> Result<(), TreeError> {
        match (self.lstat(rel)?, contents) {
            (Some(meta), Contents::Anything) if meta.is_dir() => self
                .dir
                .remove_dir_all(rel.to_local())
                .map_err(io("removing", &self.shown(rel)))
                .map_err(Into::into),
            (Some(meta), Contents::EmptyDirectoriesOnly) if meta.is_dir() => {
                self.remove_empty_tree(rel)
            }
            (Some(_) | None, _) => Ok(()),
        }
    }

    fn remove_empty_tree(&self, rel: &RelPath) -> Result<(), TreeError> {
        let listing = self
            .dir
            .read_dir(rel.to_local())
            .map_err(io("listing", &self.shown(rel)))?;
        for item in listing {
            let item = item.map_err(io("listing", &self.shown(rel)))?;
            let kind = item.file_type().map_err(io("checking", &self.shown(rel)))?;
            match child_of(rel, &item.file_name()) {
                Some(child) if kind.is_dir() => self.remove_empty_tree(&child)?,
                Some(_) | None => return Err(TreeError::Occupied(rel.clone())),
            }
        }
        self.dir
            .remove_dir(rel.to_local())
            .map_err(io("removing", &self.shown(rel)))
            .map_err(Into::into)
    }

    pub fn remove_file(&self, rel: &RelPath) -> Result<(), TreeError> {
        match self.dir.remove_file(rel.to_local()) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            Err(error) => Err(io("removing", &self.shown(rel))(error).into()),
        }
    }

    pub fn prune(&self, rel: &RelPath) {
        for ancestor in ancestors(rel).iter().rev() {
            if self.dir.remove_dir(ancestor.to_local()).is_err() {
                break;
            }
        }
    }

    pub fn create(&self, rel: &RelPath, placed: Placed<'_>) -> Result<(), TreeError> {
        match placed {
            Placed::Link(target) if self.family.links() => {
                crate::platform::link(&self.dir, target, &rel.to_local())
                    .map_err(io("linking", &self.shown(rel)))
                    .map_err(Into::into)
            }
            Placed::Link(target) => self.write_new(rel, target.as_bytes(), Mode::Regular),
            Placed::File(bytes, mode) => self.write_new(rel, bytes, mode),
        }
    }

    fn write_new(&self, rel: &RelPath, bytes: &[u8], mode: Mode) -> Result<(), TreeError> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        let kept = if self.family.modes() {
            mode
        } else {
            Mode::Regular
        };
        crate::platform::create_as(&mut options, kept);
        let mut file = self
            .dir
            .open_with(rel.to_local(), &options)
            .map_err(io("creating", &self.shown(rel)))?;
        file.write_all(bytes)
            .map_err(io("writing", &self.shown(rel)))
            .map_err(Into::into)
    }

    pub fn rename(&self, from: &RelPath, to: &RelPath) -> Result<(), TreeError> {
        self.dir
            .rename(from.to_local(), &self.dir, to.to_local())
            .map_err(io("replacing", &self.shown(to)))
            .map_err(Into::into)
    }
}
