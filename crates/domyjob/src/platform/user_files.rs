use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

const MAX_CONFIG_BYTES: usize = 4 * 1024 * 1024;

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

pub(crate) fn read(path: &Path) -> Result<Option<String>, UserFileError> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(failed("reading", path)(error)),
    };
    let bytes = crate::bounded::read(file, MAX_CONFIG_BYTES)
        .map_err(failed("reading", path))?
        .ok_or_else(|| failed("reading", path)(io::Error::other("the file exceeds 4 MiB")))?;
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|error| failed("reading", path)(io::Error::new(io::ErrorKind::InvalidData, error)))
}

pub(crate) fn write(path: &Path, bytes: &[u8]) -> Result<(), UserFileError> {
    let (target, permissions) = match fs::canonicalize(path) {
        Ok(real) => {
            let permissions = fs::metadata(&real)
                .map_err(failed("inspecting", &real))?
                .permissions();
            (real, Some(permissions))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => (path.to_path_buf(), None),
        Err(error) => return Err(failed("resolving", path)(error)),
    };
    let directory = target
        .parent()
        .ok_or_else(|| failed("writing", &target)(io::Error::other("the path has no directory")))?;
    super::raw::create_dir_all(directory).map_err(failed("creating", directory))?;
    let mut staged =
        tempfile::NamedTempFile::new_in(directory).map_err(failed("writing", &target))?;
    staged
        .write_all(bytes)
        .map_err(failed("writing", &target))?;
    if let Some(permissions) = permissions {
        staged
            .as_file()
            .set_permissions(permissions)
            .map_err(failed("writing", &target))?;
    }
    staged
        .as_file()
        .sync_all()
        .map_err(failed("writing", &target))?;
    let mut staged = staged.into_temp_path();
    super::raw::rename(&staged, &target).map_err(failed("replacing", &target))?;
    staged.disable_cleanup(true);
    Ok(())
}

pub(crate) fn remove(path: &Path) -> Result<bool, UserFileError> {
    match super::raw::remove_file(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(failed("removing", path)(error)),
    }
}

pub(crate) fn install_executable(source: &Path, target: &Path) -> Result<(), UserFileError> {
    match fs::metadata(target) {
        Ok(_) => return Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(failed("inspecting", target)(error)),
    }
    let directory = target.parent().ok_or_else(|| {
        failed("installing", target)(io::Error::other("the path has no directory"))
    })?;
    super::raw::create_dir_all(directory).map_err(failed("creating", directory))?;
    let staged =
        tempfile::NamedTempFile::new_in(directory).map_err(failed("installing", target))?;
    super::raw::copy(source, staged.path()).map_err(failed("copying", source))?;
    crate::platform::make_executable(staged.path()).map_err(failed("installing", target))?;
    let mut staged = staged.into_temp_path();
    super::raw::rename(&staged, target).map_err(failed("installing", target))?;
    staged.disable_cleanup(true);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::write;
    use crate::testing;

    #[cfg(unix)]
    #[test]
    fn a_linked_configuration_stays_linked_and_keeps_its_mode() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let real = root.path().join("dotfiles").join("config.json");
        testing::write(&real, "{}");
        std::fs::File::open(&real)
            .unwrap()
            .set_permissions(std::fs::Permissions::from_mode(0o640))
            .unwrap();
        let link = root.path().join("config.json");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        write(&link, br#"{"a":1}"#).unwrap();
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(testing::read(&real), r#"{"a":1}"#);
        write(&real, br#"{"a":2}"#).unwrap();
        assert_eq!(testing::read(&link), r#"{"a":2}"#);
        let mode = std::fs::metadata(&real).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o640);
    }

    #[test]
    fn a_new_configuration_is_created_with_its_directory() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("opencode").join("opencode.json");
        write(&path, b"{}").unwrap();
        assert_eq!(testing::read(&path), "{}");
    }
}
