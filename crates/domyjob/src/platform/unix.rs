use std::fs;
use std::io;
use std::os::unix::fs::{
    DirBuilderExt as _, MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _,
};
use std::path::Path;

use domyjob_core::chat::card::Os;

use super::{Ownership, System, raw};

const PRIVATE_FILE_MODE: u32 = 0o600;
const PRIVATE_DIRECTORY_MODE: u32 = 0o700;
const PUBLIC_EXECUTABLE_MODE: u32 = 0o755;
const PUBLIC_FILE_MODE: u32 = 0o644;
const EXECUTE_BITS: u32 = 0o111;
const PERMISSION_BITS: u32 = 0o777;
const NON_OWNER_PERMISSION_BITS: u32 = 6;

#[derive(Debug)]
pub(super) struct Unix;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Mode(u32);

impl std::fmt::Display for Mode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "mode {:o}", self.0)
    }
}

impl System for Unix {
    const OS: Os = if cfg!(target_os = "macos") {
        Os::Macos
    } else if cfg!(target_os = "linux") {
        Os::Linux
    } else {
        Os::Other
    };
    const JOB_ENVIRONMENT: &'static [&'static str] = &[
        "USER",
        "LOGNAME",
        "LANG",
        "LC_ALL",
        "LC_CTYPE",
        "SHELL",
        "TMPDIR",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_CACHE_HOME",
        "XDG_STATE_HOME",
    ];

    fn host_name() -> String {
        rustix::system::uname()
            .nodename()
            .to_string_lossy()
            .into_owned()
    }

    fn user_id() -> Option<u32> {
        Some(rustix::process::getuid().as_raw())
    }

    fn executable(metadata: &fs::Metadata) -> bool {
        metadata.is_file() && metadata.permissions().mode() & EXECUTE_BITS != 0
    }

    fn make_executable(path: &Path) -> io::Result<()> {
        raw::set_permissions(path, fs::Permissions::from_mode(PUBLIC_EXECUTABLE_MODE))
    }

    fn file_mode(metadata: &fs::Metadata) -> u32 {
        if metadata.permissions().mode() & EXECUTE_BITS == 0 {
            PUBLIC_FILE_MODE
        } else {
            PUBLIC_EXECUTABLE_MODE
        }
    }

    fn creation_mode(options: &mut cap_std::fs::OpenOptions, executable: bool) {
        use cap_std::fs::OpenOptionsExt as _;

        options.mode(if executable {
            PUBLIC_EXECUTABLE_MODE
        } else {
            PUBLIC_FILE_MODE
        });
    }

    fn ownership(file: &fs::File) -> io::Result<Ownership> {
        let metadata = file.metadata()?;
        if metadata.uid() != rustix::process::geteuid().as_raw() {
            return Ok(Ownership::Foreign);
        }
        let mode = metadata.mode() & PERMISSION_BITS;
        if mode.trailing_zeros() >= NON_OWNER_PERMISSION_BITS {
            Ok(Ownership::Private)
        } else {
            Ok(Ownership::Exposed(Mode(mode)))
        }
    }

    fn open_private_dir(path: &Path) -> io::Result<fs::File> {
        let mut options = raw::options();
        options.read(true);
        Self::no_follow(&mut options);
        options.open(path)
    }

    fn create_private_dir(path: &Path) -> io::Result<()> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(PRIVATE_DIRECTORY_MODE)
            .create(path)
    }

    fn owner_only(options: &mut fs::OpenOptions) {
        options.mode(PRIVATE_FILE_MODE);
    }

    #[cfg(test)]
    fn create_exposed_build_parent(path: &Path) -> io::Result<()> {
        const PUBLIC_BUILD_PARENT_MODE: u32 = 0o755;
        fs::DirBuilder::new()
            .mode(PUBLIC_BUILD_PARENT_MODE)
            .create(path)?;
        raw::set_permissions(path, fs::Permissions::from_mode(PUBLIC_BUILD_PARENT_MODE))
    }

    fn no_follow(options: &mut fs::OpenOptions) {
        let flags = rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK;
        options.custom_flags(flags.bits().cast_signed());
    }
}
