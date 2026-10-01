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
}

#[must_use = "replace the destination or drop the staged file to remove it"]
pub(crate) struct StagedFile(tempfile::TempPath);

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
    use super::StagedFile;
    use std::io::Write as _;

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
