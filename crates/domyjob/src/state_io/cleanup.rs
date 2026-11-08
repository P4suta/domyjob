use std::io;
use std::path::Path;

use cap_std::fs::Dir;
use domyjob_core::retired_cleanup::{Entry, Owner, Preparation, Root, prepare};

use super::{EMPTY_LIMIT, Leftover, StateError, existing_directory_using, io_at};
use crate::platform::{self, Ownership};

#[cfg(test)]
mod tests;

pub(super) fn empty_using(
    trash: &Path,
    captured: impl FnOnce(&Dir) -> io::Result<()>,
) -> Result<Vec<Leftover>, StateError> {
    let Some(retained) = existing_directory_using(trash, platform::cleanup::open_root)? else {
        return Ok(Vec::new());
    };
    let root = retained
        .try_clone()
        .map_err(|error| io_at(trash, error))?
        .into_std_file();
    prepare_directory(&root, Owner::Admitted).map_err(|error| io_at(trash, error))?;
    captured(&retained).map_err(|error| io_at(trash, error))?;
    let entries = retained.entries().map_err(|error| io_at(trash, error))?;
    let mut leftovers = Vec::new();
    for entry in entries.take(EMPTY_LIMIT) {
        let name = entry.map_err(|error| io_at(trash, error))?.file_name();
        if let Err(error) = remove_entry(&retained, &name) {
            leftovers.push(Leftover {
                path: trash.join(name),
                error,
            });
        }
    }
    Ok(leftovers)
}

fn remove_entry(parent: &Dir, name: &std::ffi::OsStr) -> io::Result<()> {
    remove_entry_using(parent, name, || Ok(()))
}

fn remove_entry_using(
    parent: &Dir,
    name: &std::ffi::OsStr,
    observed: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    let before = parent.symlink_metadata(name)?;
    observed()?;
    if platform::cleanup::link(&before) {
        return match prepare(Root::PrivateRetired, Entry::Link, Owner::Unknown) {
            Preparation::Unchanged => platform::cleanup::remove_link(parent, name, &before),
            Preparation::DirectoryAccess | Preparation::Refuse => {
                Err(io::Error::other("the retired link is not admitted"))
            }
        };
    }
    let file = if before.is_dir() {
        let start = parent.try_clone()?.into_std_file();
        platform::cleanup::capture_directory(&start, Path::new(name))?
    } else if before.is_file() {
        parent
            .open_with(name, &platform::cleanup::child_options())?
            .into_std()
    } else {
        return Err(io::Error::other("the retired entry type is not admitted"));
    };
    let metadata = file.metadata()?;
    if platform::reparse_point(&metadata) {
        return Err(io::Error::other(
            "a retired entry changed to a reparse point",
        ));
    }
    if before.is_file() && !metadata.is_file() {
        return Err(io::Error::other("the retired file changed its type"));
    }
    let kind = if metadata.is_dir() {
        Entry::Directory
    } else if metadata.is_file() {
        Entry::File
    } else {
        Entry::Other
    };
    let owner = match platform::ownership(&file)? {
        Ownership::Private | Ownership::Exposed(_) => Owner::Admitted,
        Ownership::Foreign => Owner::Foreign,
    };
    match prepare(Root::PrivateRetired, kind, owner) {
        Preparation::DirectoryAccess => {
            platform::cleanup::prepare_directory(&file)?;
            let child = Dir::from_std_file(file);
            for entry in child.entries()? {
                remove_entry(&child, &entry?.file_name())?;
            }
            drop(child);
            parent.remove_dir(name)
        }
        Preparation::Unchanged => {
            drop(file);
            parent.remove_file(name)
        }
        Preparation::Refuse => Err(io::Error::other(
            "the retired entry owner or type is not admitted",
        )),
    }
}

fn prepare_directory(file: &std::fs::File, owner: Owner) -> io::Result<()> {
    match prepare(Root::PrivateRetired, Entry::Directory, owner) {
        Preparation::DirectoryAccess => platform::cleanup::prepare_directory(file),
        Preparation::Unchanged | Preparation::Refuse => Err(io::Error::other(
            "the retired directory owner is not admitted",
        )),
    }
}
