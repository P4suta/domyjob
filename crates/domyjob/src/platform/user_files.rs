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

fn require_absent(
    path: &Path,
    action: &'static str,
    cause: io::Error,
) -> Result<(), UserFileError> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Ok(_) => Err(failed(action, path)(cause)),
        Err(error) => Err(failed("inspecting", path)(error)),
    }
}

pub(crate) fn read(path: &Path) -> Result<Option<String>, UserFileError> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            require_absent(path, "reading", error)?;
            return Ok(None);
        }
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
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            require_absent(path, "resolving", error)?;
            (path.to_path_buf(), None)
        }
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
    super::replacement::StagedFile::sync_and_close(staged)
        .map_err(failed("writing", &target))?
        .replace(&target)
        .map_err(failed("replacing", &target))
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
    super::replacement::StagedFile::sync_and_close(staged)
        .map_err(failed("installing", target))?
        .replace(target)
        .map_err(failed("installing", target))
}

#[cfg(test)]
mod tests {
    use super::write;
    use crate::testing;

    #[cfg(unix)]
    fn dangling_link(root: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
        let target = root.join("missing.json");
        let link = root.join("hooks.json");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        (target, link)
    }

    fn assert_dangling(link: &std::path::Path, target: &std::path::Path, linked: &std::path::Path) {
        assert_eq!(std::fs::read_link(link).unwrap(), linked);
        assert_eq!(
            std::fs::symlink_metadata(target).unwrap_err().kind(),
            std::io::ErrorKind::NotFound
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_dangling_configuration_link_is_refused_on_read() {
        let root = tempfile::tempdir().unwrap();
        let (target, link) = dangling_link(root.path());
        super::read(&link).unwrap_err();
        assert_dangling(&link, &target, &target);
    }

    #[cfg(unix)]
    #[test]
    fn a_dangling_configuration_link_is_preserved_on_write() {
        let root = tempfile::tempdir().unwrap();
        let (target, link) = dangling_link(root.path());
        write(&link, b"{}").unwrap_err();
        assert_dangling(&link, &target, &target);
    }

    #[cfg(windows)]
    #[test]
    fn a_dangling_configuration_junction_is_preserved() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("missing");
        let link = root.path().join("hooks.json");
        testing::mkdir(&target);
        crate::file_kind::directory_link(&target, &link).unwrap();
        let linked = std::fs::read_link(&link).unwrap();
        std::fs::remove_dir(&target).unwrap();
        super::read(&link).unwrap_err();
        write(&link, b"{}").unwrap_err();
        assert_dangling(&link, &target, &linked);
    }

    #[cfg(unix)]
    #[test]
    fn a_linked_configuration_stays_linked_and_keeps_its_mode() {
        use std::os::unix::fs::PermissionsExt;

        const CONFIGURATION_MODE: u32 = 0o640;
        const PERMISSION_BITS: u32 = 0o777;
        let root = tempfile::tempdir().unwrap();
        let real = root.path().join("dotfiles").join("config.json");
        testing::write(&real, "{}");
        std::fs::File::open(&real)
            .unwrap()
            .set_permissions(std::fs::Permissions::from_mode(CONFIGURATION_MODE))
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
        assert_eq!(mode & PERMISSION_BITS, CONFIGURATION_MODE);
    }

    #[test]
    fn a_new_configuration_is_created_with_its_directory() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("opencode").join("opencode.json");
        write(&path, b"{}").unwrap();
        assert_eq!(testing::read(&path), "{}");
    }
}
