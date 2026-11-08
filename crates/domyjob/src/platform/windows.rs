use std::fs;
use std::io;
use std::path::Path;

use super::{Ownership, System, windows_acl};

#[derive(Debug)]
pub(super) struct Windows;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Acl;

impl std::fmt::Display for Acl {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("its access control list admits other users")
    }
}

impl System for Windows {
    const JOB_ENVIRONMENT: &'static [&'static str] = &[
        "PATHEXT",
        "SystemRoot",
        "WINDIR",
        "COMSPEC",
        "LOCALAPPDATA",
        "APPDATA",
        "PROGRAMDATA",
        "HOMEDRIVE",
        "HOMEPATH",
        "USERNAME",
        "NUMBER_OF_PROCESSORS",
        "PROCESSOR_ARCHITECTURE",
    ];

    fn user_id() -> Option<u32> {
        None
    }

    fn executable(metadata: &fs::Metadata) -> bool {
        metadata.is_file()
    }

    fn make_executable(_path: &Path) -> io::Result<()> {
        Ok(())
    }

    fn file_mode(_metadata: &fs::Metadata) -> u32 {
        const NON_EXECUTABLE_FILE_MODE: u32 = 0o644;
        NON_EXECUTABLE_FILE_MODE
    }

    fn creation_mode(_options: &mut cap_std::fs::OpenOptions, _executable: bool) {}

    fn private_creation_mode(_options: &mut cap_std::fs::OpenOptions) {}

    fn ownership(file: &fs::File) -> io::Result<Ownership> {
        windows_acl::ownership(file)
    }

    fn open_private_dir(path: &Path) -> io::Result<fs::File> {
        windows_acl::open_private_dir(path)
    }

    fn create_private_dir(path: &Path) -> io::Result<()> {
        windows_acl::create_private_dir(path)
    }

    #[cfg(test)]
    fn create_exposed_build_parent(path: &Path) -> io::Result<()> {
        windows_acl::create_exposed_build_parent(path)
    }

    fn owner_only(_options: &mut fs::OpenOptions) {}

    fn no_follow(options: &mut fs::OpenOptions) {
        windows_acl::no_follow(options);
    }

    fn no_follow_relative(options: &mut cap_std::fs::OpenOptions) {
        windows_acl::no_follow_relative(options);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn trash_sharing_failures_remain_reported_until_the_handle_closes() {
        const SHARING_VIOLATION: i32 = 32;
        const LOCK_VIOLATION: i32 = 33;
        use std::os::windows::fs::OpenOptionsExt as _;

        let root = tempfile::tempdir().unwrap();
        let trash = root.path().join("trash");
        let file = trash.join("job").join("tracked");
        crate::testing::write(&file, "remove");
        let held = crate::platform::private_options()
            .read(true)
            .share_mode(0)
            .open(&file)
            .unwrap();
        let leftovers = crate::state_io::empty(&trash).unwrap();
        assert_eq!(leftovers.len(), 1);
        assert!(matches!(
            leftovers.first().unwrap().error.raw_os_error(),
            Some(SHARING_VIOLATION | LOCK_VIOLATION)
        ));
        drop(held);
        assert!(crate::state_io::empty(&trash).unwrap().is_empty());
    }
}
