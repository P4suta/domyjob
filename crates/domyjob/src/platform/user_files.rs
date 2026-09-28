#![expect(
    clippy::redundant_pub_crate,
    reason = "the binary composition root uses this private module"
)]
#![expect(
    clippy::disallowed_methods,
    reason = "this module owns the user configuration files domyjob installs: service definitions, client settings, and the stable executable"
)]
//! Files outside domyjob's private state that setup installs for the user.

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

const MAX_CONFIG_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
#[error("{action} {path}: {source}")]
pub(crate) struct UserFileError {
    action: &'static str,
    path: PathBuf,
    source: io::Error,
}

fn failed(action: &'static str, path: &Path) -> impl FnOnce(io::Error) -> UserFileError {
    let path = path.to_path_buf();
    move |source| UserFileError {
        action,
        path,
        source,
    }
}

/// Read a configuration file of at most 4 MiB; a missing file reads as `None`.
pub(crate) fn read(path: &Path) -> Result<Option<String>, UserFileError> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(failed("reading", path)(error)),
    };
    let mut text = String::new();
    file.take(MAX_CONFIG_BYTES.saturating_add(1))
        .read_to_string(&mut text)
        .map_err(failed("reading", path))?;
    if u64::try_from(text.len())
        .map_err(|error| failed("reading", path)(io::Error::other(error)))?
        > MAX_CONFIG_BYTES
    {
        return Err(failed("reading", path)(io::Error::other(
            "the file exceeds 4 MiB",
        )));
    }
    Ok(Some(text))
}

/// Replace a file atomically, creating its directory when needed.
pub(crate) fn write(path: &Path, bytes: &[u8]) -> Result<(), UserFileError> {
    let directory = path
        .parent()
        .ok_or_else(|| failed("writing", path)(io::Error::other("the path has no directory")))?;
    fs::create_dir_all(directory).map_err(failed("creating", directory))?;
    let mut staged = tempfile::NamedTempFile::new_in(directory).map_err(failed("writing", path))?;
    staged.write_all(bytes).map_err(failed("writing", path))?;
    staged
        .as_file()
        .sync_all()
        .map_err(failed("writing", path))?;
    staged
        .persist(path)
        .map_err(|error| failed("replacing", path)(error.error))?;
    Ok(())
}

/// Remove a file; a missing file is already removed.
#[cfg_attr(
    windows,
    expect(
        dead_code,
        reason = "Windows keeps its service definition in Task Scheduler"
    )
)]
pub(crate) fn remove(path: &Path) -> Result<bool, UserFileError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(failed("removing", path)(error)),
    }
}

/// Install an executable at a build-specific `target`; an existing target already holds that build.
pub(crate) fn install_executable(source: &Path, target: &Path) -> Result<(), UserFileError> {
    match fs::metadata(target) {
        Ok(_) => return Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(failed("inspecting", target)(error)),
    }
    let directory = target.parent().ok_or_else(|| {
        failed("installing", target)(io::Error::other("the path has no directory"))
    })?;
    fs::create_dir_all(directory).map_err(failed("creating", directory))?;
    let staged =
        tempfile::NamedTempFile::new_in(directory).map_err(failed("installing", target))?;
    fs::copy(source, staged.path()).map_err(failed("copying", source))?;
    crate::platform::make_executable(staged.path()).map_err(failed("installing", target))?;
    staged
        .persist(target)
        .map_err(|error| failed("installing", target)(error.error))?;
    Ok(())
}
