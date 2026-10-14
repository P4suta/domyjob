use std::io;
use std::path::Path;

mod raw {
    #![expect(
        clippy::disallowed_methods,
        reason = "staged replacement uses standard rename after syncing and closing the writer"
    )]

    pub(super) fn rename(from: &std::path::Path, to: &std::path::Path) -> std::io::Result<()> {
        std::fs::rename(from, to)
    }

    pub(super) fn close(file: tempfile::NamedTempFile) -> tempfile::TempPath {
        file.into_temp_path()
    }

    pub(super) fn retain(path: &mut tempfile::TempPath) {
        path.disable_cleanup(true);
    }

    pub(super) fn retain_directory(directory: &mut tempfile::TempDir) {
        directory.disable_cleanup(true);
    }

    pub(super) fn remove_empty_directory(path: &std::path::Path) -> std::io::Result<()> {
        std::fs::remove_dir(path)
    }
}

const CANDIDATE_DIRECTORY: &str = "candidate";

pub(crate) struct PrivateCandidate {
    outer: tempfile::TempDir,
    path: std::path::PathBuf,
}

impl PrivateCandidate {
    pub(crate) fn new(outer: tempfile::TempDir) -> io::Result<Self> {
        let path = outer.path().join(CANDIDATE_DIRECTORY);
        crate::state_io::private_dir(&path).map_err(io::Error::other)?;
        Ok(Self { outer, path })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

#[must_use = "replace the destination or drop the staged file to remove it"]
pub(crate) struct StagedFile(tempfile::TempPath);

pub(crate) fn publish_owned_directory(
    directory: PrivateCandidate,
    target: &Path,
) -> io::Result<()> {
    publish_owned_directory_using(directory, target, raw::rename, raw::remove_empty_directory)
}

fn publish_owned_directory_using(
    mut directory: PrivateCandidate,
    target: &Path,
    rename: impl FnOnce(&Path, &Path) -> io::Result<()>,
    remove_empty: impl FnOnce(&Path) -> io::Result<()>,
) -> io::Result<()> {
    rename(directory.path(), target).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "publishing {} to {}: {error}",
                directory.path().display(),
                target.display()
            ),
        )
    })?;
    raw::retain_directory(&mut directory.outer);
    remove_empty(directory.outer.path()).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "removing the empty staging directory {}: {error}",
                directory.outer.path().display()
            ),
        )
    })?;
    drop(directory);
    Ok(())
}

impl StagedFile {
    pub(crate) fn sync_and_close(file: tempfile::NamedTempFile) -> io::Result<Self> {
        file.as_file().sync_all()?;
        Ok(Self(raw::close(file)))
    }

    pub(crate) fn replace(mut self, target: &Path) -> io::Result<()> {
        raw::rename(&self.0, target)?;
        raw::retain(&mut self.0);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{PrivateCandidate, StagedFile};
    use std::io::Write as _;

    fn directory_fixture() -> (tempfile::TempDir, PrivateCandidate, std::path::PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let stage = PrivateCandidate::new(tempfile::tempdir_in(root.path()).unwrap()).unwrap();
        let target = root.path().join("published");
        (root, stage, target)
    }

    fn publish_then_reuse(
        stage: PrivateCandidate,
        target: &std::path::Path,
        reuse: impl FnOnce(&std::path::Path) -> std::io::Result<()>,
    ) -> std::io::Result<()> {
        super::publish_owned_directory_using(
            stage,
            target,
            |source, target| {
                super::raw::rename(source, target)?;
                reuse(source)
            },
            super::raw::remove_empty_directory,
        )
    }

    fn assert_reused_name_preserved(error: &std::io::Error, name: &std::path::Path) {
        assert_eq!(error.kind(), std::io::ErrorKind::DirectoryNotEmpty);
        assert_eq!(crate::testing::read(&name.join("replacement")), "keep");
    }

    #[test]
    fn publishing_a_directory_never_removes_a_replacement_of_its_staging_path() {
        let (_root, stage, target) = directory_fixture();
        let old_path = stage.path().to_path_buf();
        crate::state_io::create_empty(&stage.path().join("usage.lock")).unwrap();
        let error = publish_then_reuse(stage, &target, |source| {
            crate::testing::write(&source.join("replacement"), "keep");
            Ok(())
        })
        .unwrap_err();
        assert_reused_name_preserved(&error, &old_path);
        assert_eq!(
            crate::lock::OsLock::probe(&target.join("usage.lock")).unwrap(),
            crate::lock::Probe::Free
        );
    }

    #[test]
    fn refused_directory_publication_preserves_the_target_and_removes_the_stage() {
        let (_root, stage, target) = directory_fixture();
        let staged_path = stage.path().to_path_buf();
        let lock = staged_path.join("usage.lock");
        crate::state_io::create_empty(&lock).unwrap();
        let reader = std::fs::File::open(&lock).unwrap();
        crate::testing::write(&target.join("before"), "keep");
        let error = super::publish_owned_directory_using(
            stage,
            &target,
            |_source, _target| {
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "injected refusal",
                ))
            },
            super::raw::remove_empty_directory,
        )
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(error.to_string().contains("injected refusal"));
        assert_eq!(crate::testing::read(&target.join("before")), "keep");
        assert_eq!(
            std::fs::metadata(staged_path).unwrap_err().kind(),
            std::io::ErrorKind::NotFound
        );
        assert!(crate::lock::OsLock::try_file(reader).unwrap().is_some());
    }

    #[test]
    fn published_candidates_remove_only_the_empty_outer_directory() {
        let (_root, stage, target) = directory_fixture();
        let outer = stage.outer.path().to_path_buf();
        crate::state_io::create_empty(&stage.path().join("usage.lock")).unwrap();
        super::publish_owned_directory(stage, &target).unwrap();
        assert_eq!(
            std::fs::symlink_metadata(outer).unwrap_err().kind(),
            std::io::ErrorKind::NotFound
        );
        assert!(
            std::fs::symlink_metadata(target.join("usage.lock"))
                .unwrap()
                .is_file()
        );
        assert_eq!(
            crate::platform::ownership(
                &crate::state_io::open_existing_lock(&target.join("usage.lock"))
                    .unwrap()
                    .unwrap()
            )
            .unwrap(),
            crate::platform::Ownership::Private
        );
    }

    #[test]
    fn failed_empty_outer_cleanup_preserves_the_published_candidate_and_reports_failure() {
        let (_root, stage, target) = directory_fixture();
        let outer = stage.outer.path().to_path_buf();
        crate::state_io::create_empty(&stage.path().join("usage.lock")).unwrap();
        let error =
            super::publish_owned_directory_using(stage, &target, super::raw::rename, |_empty| {
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "injected empty-parent cleanup refusal",
                ))
            })
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(
            error
                .to_string()
                .contains("injected empty-parent cleanup refusal")
        );
        assert!(
            std::fs::symlink_metadata(target.join("usage.lock"))
                .unwrap()
                .is_file()
        );
        assert!(std::fs::symlink_metadata(&outer).unwrap().is_dir());
        assert_eq!(std::fs::read_dir(outer).unwrap().count(), 0);
    }

    #[test]
    fn a_reused_outer_directory_is_never_recursively_removed_after_publication() {
        let (_root, stage, target) = directory_fixture();
        let outer = stage.outer.path().to_path_buf();
        let error = publish_then_reuse(stage, &target, |source| {
            let parent = source.parent().unwrap();
            super::raw::remove_empty_directory(parent)?;
            crate::testing::write(&parent.join("replacement"), "keep");
            Ok(())
        })
        .unwrap_err();
        assert_reused_name_preserved(&error, &outer);
        assert!(std::fs::symlink_metadata(target).unwrap().is_dir());
    }

    #[test]
    fn replacement_succeeds_with_a_live_reader_and_preserves_its_snapshot() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("state.json");
        crate::testing::write(&target, "before");
        let reader = std::fs::File::open(&target).unwrap();
        let mut file = tempfile::NamedTempFile::new_in(root.path()).unwrap();
        file.write_all(b"after").unwrap();
        StagedFile::sync_and_close(file)
            .unwrap()
            .replace(&target)
            .unwrap();
        assert_eq!(crate::testing::read(&target), "after");
        assert_eq!(
            crate::bounded::read(reader, 32).unwrap().unwrap(),
            b"before"
        );
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    }

    #[test]
    fn failed_replacement_removes_the_staged_file_and_keeps_the_destination() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("directory");
        crate::testing::mkdir(&target);
        let mut file = tempfile::NamedTempFile::new_in(root.path()).unwrap();
        file.write_all(b"after").unwrap();
        let staged_path = file.path().to_path_buf();
        assert!(
            StagedFile::sync_and_close(file)
                .unwrap()
                .replace(&target)
                .is_err()
        );
        assert!(std::fs::metadata(&target).unwrap().is_dir());
        assert_eq!(
            std::fs::metadata(staged_path).unwrap_err().kind(),
            std::io::ErrorKind::NotFound
        );
    }
}
