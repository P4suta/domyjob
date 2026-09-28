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
