#![expect(
    clippy::redundant_pub_crate,
    reason = "the platform file check is compiled into both the build script and the application"
)]

use std::fs;

#[cfg(unix)]
pub(crate) fn reparse_point(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(windows)]
pub(crate) fn reparse_point(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt as _;

    metadata.file_attributes() & 0x400 != 0
}
