use std::fs;
use std::io::{self, Read};
use std::path::{Component, Path};

use cap_std::fs::Dir;

use crate::file_kind;

#[cfg(unix)]
#[path = "source_file/unix.rs"]
mod unix;
#[cfg(windows)]
#[path = "source_file/windows.rs"]
mod windows;
#[cfg(unix)]
use unix as native;
#[cfg(windows)]
use windows as native;

#[derive(Debug)]
pub(crate) struct Directory {
    directory: Dir,
    _parents: Vec<fs::File>,
}

impl Directory {
    pub(crate) fn entries(&self) -> io::Result<cap_std::fs::ReadDir> {
        self.directory.entries()
    }
}

#[derive(Debug)]
pub(crate) struct File {
    content: fs::File,
    _parents: Vec<fs::File>,
}

impl File {
    pub(crate) fn metadata(&self) -> io::Result<fs::Metadata> {
        self.content.metadata()
    }
}

impl Read for File {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.content.read(bytes)
    }
}

fn parents(root: &Dir, path: &Path) -> io::Result<Vec<fs::File>> {
    let mut parents = vec![root.try_clone()?.into_std_file()];
    for component in path.components() {
        let Component::Normal(name) = component else {
            return Err(io::Error::other(
                "a source component is not a relative name",
            ));
        };
        let parent = parents
            .last()
            .ok_or_else(|| io::Error::other("the captured source parent is missing"))?;
        let directory = native::directory(parent, name)?;
        let metadata = directory.metadata()?;
        if !metadata.is_dir() || file_kind::reparse_point(&metadata) {
            return Err(io::Error::other(
                "a source component is not an ordinary directory",
            ));
        }
        parents.push(directory);
    }
    Ok(parents)
}

pub(crate) fn directory(root: &Dir, path: &Path) -> io::Result<Directory> {
    let parents = parents(root, path)?;
    let captured = parents
        .last()
        .ok_or_else(|| io::Error::other("the captured source directory is missing"))?
        .try_clone()?;
    Ok(Directory {
        directory: Dir::from_std_file(captured),
        _parents: parents,
    })
}

pub(crate) fn open(root: &Dir, path: &Path) -> io::Result<File> {
    open_using(root, path, || {})
}

fn open_using(root: &Dir, path: &Path, before_file: impl FnOnce()) -> io::Result<File> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("the source parent is missing"))?;
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::other("the source filename is missing"))?;
    let parents = parents(root, parent)?;
    let captured = parents
        .last()
        .ok_or_else(|| io::Error::other("the captured source parent is missing"))?;
    before_file();
    let content = native::open(captured, name)?;
    let metadata = content.metadata()?;
    if !metadata.is_file() || file_kind::reparse_point(&metadata) {
        return Err(io::Error::other("a source path is not a regular file"));
    }
    Ok(File {
        content,
        _parents: parents,
    })
}

#[cfg(test)]
#[path = "source_file/tests.rs"]
mod tests;
