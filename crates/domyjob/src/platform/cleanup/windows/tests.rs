use std::fs::{self, File, Permissions};
use std::os::windows::fs::MetadataExt as _;
use std::path::PathBuf;

const ERROR_SHARING_VIOLATION: i32 = 32;

struct RestorePermissions {
    file: File,
    original: Permissions,
}

impl Drop for RestorePermissions {
    fn drop(&mut self) {
        self.file.set_permissions(self.original.clone()).unwrap();
    }
}

struct ReadonlyRetirement {
    restored: RestorePermissions,
    outside: PathBuf,
    trash: PathBuf,
    retired: PathBuf,
    _temporary: tempfile::TempDir,
}

impl ReadonlyRetirement {
    fn assert_outside_unchanged(&self, attributes: u32) {
        let observed = self.restored.file.metadata().unwrap();
        assert_eq!(observed.file_attributes(), attributes);
        assert!(observed.permissions().readonly());
        assert_eq!(crate::testing::read(&self.outside), "shared-inode");
    }

    fn assert_retired_absent(&self) {
        assert_eq!(
            fs::symlink_metadata(&self.retired).unwrap_err().kind(),
            std::io::ErrorKind::NotFound
        );
    }
}

fn readonly_retirement() -> ReadonlyRetirement {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("fixture");
    crate::state_io::private_dir(&root).unwrap();
    let outside = root.join("outside");
    crate::testing::write(&outside, b"shared-inode");
    let retained = File::options()
        .read(true)
        .write(true)
        .open(&outside)
        .unwrap();
    let original = retained.metadata().unwrap().permissions();
    let mut readonly = original.clone();
    readonly.set_readonly(true);
    let restored = RestorePermissions {
        file: retained,
        original,
    };
    restored.file.set_permissions(readonly).unwrap();
    let trash = root.join("trash");
    crate::state_io::private_dir(&trash).unwrap();
    let retired = trash.join("retired");
    crate::state_io::private_dir(&retired).unwrap();
    fs::hard_link(&outside, retired.join("linked-file")).unwrap();
    ReadonlyRetirement {
        restored,
        outside,
        trash,
        retired,
        _temporary: temporary,
    }
}

#[test]
fn readonly_hardlink_retirement_preserves_the_external_file_attributes() {
    let fixture = readonly_retirement();
    let original = fixture.restored.file.metadata().unwrap().file_attributes();
    assert!(crate::state_io::empty(&fixture.trash).unwrap().is_empty());
    fixture.assert_retired_absent();
    fixture.assert_outside_unchanged(original);
}

#[test]
fn a_delete_sharing_denial_preserves_the_original_error_and_allows_retry_after_release() {
    let fixture = readonly_retirement();
    let blocked = fixture.retired.join("linked-file");
    let retained = crate::state_io::existing_directory(&fixture.retired)
        .unwrap()
        .unwrap();
    let held_file = retained
        .open_with("linked-file", &super::child_options())
        .unwrap();
    let original = fixture.restored.file.metadata().unwrap().file_attributes();
    let leftovers = crate::state_io::empty(&fixture.trash).unwrap();
    assert_eq!(leftovers.len(), 1);
    assert_eq!(
        leftovers.first().unwrap().error.raw_os_error(),
        Some(ERROR_SHARING_VIOLATION)
    );
    fixture.assert_outside_unchanged(original);
    assert_eq!(crate::testing::read(&blocked), "shared-inode");
    drop(held_file);
    drop(retained);
    assert!(crate::state_io::empty(&fixture.trash).unwrap().is_empty());
    fixture.assert_outside_unchanged(original);
    fixture.assert_retired_absent();
}
