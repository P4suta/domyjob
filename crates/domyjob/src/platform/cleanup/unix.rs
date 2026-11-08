use std::fs::{File, Permissions};
use std::io;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;

#[cfg(test)]
mod tests;

pub(crate) fn open_root(path: &Path) -> io::Result<File> {
    crate::platform::open_private_dir(path)
}

pub(crate) fn capture_directory(parent: &File, name: &Path) -> io::Result<File> {
    let retained = rustix::fs::openat(
        parent,
        name,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::DIRECTORY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )?;
    Ok(File::from(retained))
}

pub(crate) fn child_options() -> cap_std::fs::OpenOptions {
    let mut options = crate::platform::private_relative_options();
    options.read(true);
    options
}

pub(crate) fn link(metadata: &cap_std::fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

pub(crate) fn prepare_directory(file: &File) -> io::Result<()> {
    let mode = file.metadata()?.permissions().mode();
    file.set_permissions(Permissions::from_mode(
        domyjob_core::retired_cleanup::directory_mode(mode),
    ))
}

pub(crate) fn remove_link(
    directory: &cap_std::fs::Dir,
    name: &std::ffi::OsStr,
    _metadata: &cap_std::fs::Metadata,
) -> io::Result<()> {
    directory.remove_file(name)
}

#[cfg(test)]
pub(crate) fn replace_root(root: &Path, held: &Path, outside: &Path) -> io::Result<()> {
    super::rename_fixture_directory(root, held)?;
    crate::file_kind::directory_link(outside, root)
}

#[cfg(test)]
pub(crate) fn linked_directory_refusal(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(errno) if errno == rustix::io::Errno::LOOP.raw_os_error()
            || errno == rustix::io::Errno::NOTDIR.raw_os_error()
    )
}
