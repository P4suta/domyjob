use std::fs;
use std::io;
use std::path::Path;

use domyjob_core::chat::card::Os;

use super::{Ownership, System, variable, windows_acl};

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
    const OS: Os = Os::Windows;
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

    fn host_name() -> String {
        variable("COMPUTERNAME")
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

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
        0o644
    }

    fn creation_mode(_options: &mut cap_std::fs::OpenOptions, _executable: bool) {}

    fn ownership(file: &fs::File) -> io::Result<Ownership> {
        windows_acl::ownership(file)
    }

    fn open_private_dir(path: &Path) -> io::Result<fs::File> {
        windows_acl::open_private_dir(path)
    }

    fn create_private_dir(path: &Path) -> io::Result<()> {
        windows_acl::create_private_dir(path)
    }

    fn owner_only(_options: &mut fs::OpenOptions) {}

    fn no_follow(options: &mut fs::OpenOptions) {
        windows_acl::no_follow(options);
    }
}
