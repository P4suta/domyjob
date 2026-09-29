//! Test fixtures: files that tests create, change, and inspect directly.

use std::path::Path;

mod raw {
    #![expect(
        clippy::disallowed_methods,
        reason = "test fixtures create, change, and inspect files directly"
    )]

    use std::io;
    use std::path::Path;

    pub(super) fn create_dir_all(path: &Path) -> io::Result<()> {
        std::fs::create_dir_all(path)
    }

    pub(super) fn write(path: &Path, bytes: &[u8]) -> io::Result<()> {
        std::fs::write(path, bytes)
    }

    pub(super) fn remove_file(path: &Path) -> io::Result<()> {
        std::fs::remove_file(path)
    }

    pub(super) fn is_dir(path: &Path) -> bool {
        path.is_dir()
    }

    pub(super) fn is_file(path: &Path) -> bool {
        path.is_file()
    }

    pub(super) fn read_to_string(path: &Path) -> io::Result<String> {
        std::fs::read_to_string(path)
    }

    pub(super) fn set_permissions(
        path: &Path,
        permissions: std::fs::Permissions,
    ) -> io::Result<()> {
        std::fs::set_permissions(path, permissions)
    }
}

/// Create a directory and its parents.
pub(crate) fn mkdir(path: &Path) {
    raw::create_dir_all(path).expect("create a fixture directory");
}

/// Write a file, creating its directories.
pub(crate) fn write(path: &Path, bytes: impl AsRef<[u8]>) {
    if let Some(parent) = path.parent() {
        mkdir(parent);
    }
    raw::write(path, bytes.as_ref()).expect("write a fixture file");
}

pub(crate) fn remove(path: &Path) {
    raw::remove_file(path).expect("remove a fixture file");
}

pub(crate) fn is_dir(path: &Path) -> bool {
    raw::is_dir(path)
}

pub(crate) fn is_file(path: &Path) -> bool {
    raw::is_file(path)
}

pub(crate) fn read(path: &Path) -> String {
    raw::read_to_string(path).expect("read a fixture file")
}

/// Make a fixture read-only, as a container's files owned by root are to this user,
/// and return the permissions that [`restore`] gives back.
pub(crate) fn protect(path: &Path) -> std::fs::Permissions {
    let original = std::fs::metadata(path)
        .expect("read a fixture's permissions")
        .permissions();
    let mut readonly = original.clone();
    readonly.set_readonly(true);
    raw::set_permissions(path, readonly).expect("protect a fixture");
    original
}

pub(crate) fn restore(path: &Path, permissions: std::fs::Permissions) {
    raw::set_permissions(path, permissions).expect("restore a fixture's permissions");
}
