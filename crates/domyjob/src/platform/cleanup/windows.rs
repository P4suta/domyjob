use std::fs::File;
use std::io;
use std::path::Path;

#[cfg(test)]
mod tests;

pub(crate) fn open_root(path: &Path) -> io::Result<File> {
    crate::platform::windows_acl::open_cleanup_root(path)
}

pub(crate) fn capture_directory(parent: &File, name: &Path) -> io::Result<File> {
    cap_primitives::fs::open_dir_nofollow(parent, name)
}

pub(crate) fn child_options() -> cap_std::fs::OpenOptions {
    crate::platform::windows_acl::cleanup_child_options()
}

pub(crate) fn link(metadata: &cap_std::fs::Metadata) -> bool {
    crate::platform::windows_acl::cleanup_link(metadata)
}

pub(crate) fn prepare_directory(file: &File) -> io::Result<()> {
    let metadata = file.metadata()?;
    if metadata.is_dir() && !crate::platform::reparse_point(&metadata) {
        Ok(())
    } else {
        Err(io::Error::other(
            "the retained cleanup handle is not a directory",
        ))
    }
}

pub(crate) fn remove_link(
    directory: &cap_std::fs::Dir,
    name: &std::ffi::OsStr,
    metadata: &cap_std::fs::Metadata,
) -> io::Result<()> {
    crate::platform::windows_acl::remove_cleanup_link(directory, name, metadata)
}

#[cfg(test)]
pub(crate) fn replace_root(root: &Path, held: &Path, _outside: &Path) -> io::Result<()> {
    const ERROR_ACCESS_DENIED: i32 = 5;
    const ERROR_SHARING_VIOLATION: i32 = 32;
    match super::rename_fixture_directory(root, held) {
        Err(error)
            if matches!(
                error.raw_os_error(),
                Some(ERROR_ACCESS_DENIED | ERROR_SHARING_VIOLATION)
            ) =>
        {
            Ok(())
        }
        Err(error) => Err(error),
        Ok(()) => Err(io::Error::other("the retained cleanup root was renamed")),
    }
}

#[cfg(test)]
pub(crate) fn linked_directory_refusal(error: &io::Error) -> bool {
    crate::platform::windows_acl::cleanup_link_refusal(error)
}
