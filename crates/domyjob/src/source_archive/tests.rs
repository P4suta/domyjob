use std::path::Path;

use super::Archive;

const SOURCE_MODE: u32 = 0o644;

#[derive(Debug)]
pub(super) struct FileFixture {
    pub(super) directory: cap_std::fs::Dir,
    pub(super) path: std::path::PathBuf,
    pub(super) root: tempfile::TempDir,
}

pub(super) fn file_fixture(relative: &str) -> FileFixture {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join(relative);
    crate::testing::write(&path, b"public");
    let directory =
        cap_std::fs::Dir::open_ambient_dir(root.path(), cap_std::ambient_authority()).unwrap();
    FileFixture {
        directory,
        path,
        root,
    }
}

impl FileFixture {
    pub(super) fn replaced(&self, replace: impl FnOnce(&Path)) -> super::ArchiveError {
        Archive::new()
            .add(
                (self.root.path(), &self.directory),
                &self.path,
                |_metadata| {
                    let relative = self.path.strip_prefix(self.root.path()).unwrap();
                    self.directory.remove_file(relative).unwrap();
                    replace(&self.path);
                    SOURCE_MODE
                },
            )
            .unwrap_err()
    }
}

#[test]
fn replacing_an_inspected_parent_with_an_in_root_metadata_link_never_reads_its_target() {
    let root = tempfile::tempdir().unwrap();
    crate::testing::write(&root.path().join("tracked/config"), b"public");
    crate::testing::write(&root.path().join(".git/config"), b"secret");
    let directory =
        cap_std::fs::Dir::open_ambient_dir(root.path(), cap_std::ambient_authority()).unwrap();
    let mut archive = Archive::new();
    let result = archive.add(
        (root.path(), &directory),
        &root.path().join("tracked/config"),
        |_metadata| {
            directory.rename("tracked", &directory, "before").unwrap();
            crate::file_kind::directory_link(Path::new(".git"), &root.path().join("tracked"))
                .unwrap();
            SOURCE_MODE
        },
    );
    if result.is_ok() {
        let bytes = archive.finish().unwrap();
        let mut tar = tar::Archive::new(bytes.as_slice());
        let mut entries = tar.entries().unwrap();
        let mut entry = entries.next().unwrap().unwrap();
        let content = crate::bounded::read(&mut entry, b"secret".len())
            .unwrap()
            .unwrap();
        assert_eq!(
            content, b"secret",
            "the old opening boundary followed the replacement"
        );
    }
    assert!(
        result.is_err(),
        "the inspected path must refuse its replacement link"
    );
}

#[test]
fn a_final_regular_file_replaced_by_a_directory_is_refused_before_reading() {
    let fixture = file_fixture("file");
    let error = fixture.replaced(|_path| fixture.directory.create_dir("file").unwrap());
    assert!(matches!(error, super::ArchiveError::Io { .. }));
}
