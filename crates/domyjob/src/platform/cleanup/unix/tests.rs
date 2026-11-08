use std::fs::{self, File, Permissions};
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use crate::state_io::{empty, private_dir, set_aside};

const READONLY_DIRECTORY_MODE: u32 = 0o500;
const READONLY_FILE_MODE: u32 = 0o400;
const NO_SEARCH_DIRECTORY_MODE: u32 = 0o400;
const UNREADABLE_DIRECTORY_MODE: u32 = 0;

struct RestorePermissions {
    original: Vec<(File, Permissions)>,
}

impl RestorePermissions {
    const fn new() -> Self {
        Self {
            original: Vec::new(),
        }
    }

    fn set(&mut self, path: &Path, mode: u32) {
        let retained = File::open(path).unwrap();
        let permissions = retained.metadata().unwrap().permissions();
        retained
            .set_permissions(Permissions::from_mode(mode))
            .unwrap();
        self.original.push((retained, permissions));
    }
}

impl Drop for RestorePermissions {
    fn drop(&mut self) {
        for (file, permissions) in self.original.drain(..).rev() {
            file.set_permissions(permissions).unwrap();
        }
    }
}

fn retired_tree(root: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let trash = root.join("trash");
    private_dir(&trash).unwrap();
    let workspace = root.join("workspace");
    private_dir(&workspace).unwrap();
    let nested = workspace.join("pkg");
    private_dir(&nested).unwrap();
    let file = nested.join("data");
    crate::testing::write(&file, b"retired-output");
    set_aside(&workspace, &trash).unwrap();
    let retired = fs::read_dir(&trash)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    (trash, retired.join("pkg"), retired.join("pkg/data"))
}

#[test]
fn readonly_regular_files_are_removed_without_changing_their_permissions() {
    let root = tempfile::tempdir().unwrap();
    let (trash, _nested, file) = retired_tree(root.path());
    let mut restore = RestorePermissions::new();
    restore.set(&file, READONLY_FILE_MODE);
    assert!(empty(&trash).unwrap().is_empty());
    assert_eq!(fs::read_dir(&trash).unwrap().count(), 0);
    assert_eq!(
        restore
            .original
            .first()
            .unwrap()
            .0
            .metadata()
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        READONLY_FILE_MODE
    );
}

#[test]
fn retiring_a_readonly_hardlink_preserves_the_external_inode_and_mode() {
    let root = tempfile::tempdir().unwrap();
    let outside = root.path().join("outside");
    crate::testing::write(&outside, b"shared-inode");
    let mut restore = RestorePermissions::new();
    restore.set(&outside, READONLY_FILE_MODE);
    let original = fs::metadata(&outside).unwrap();
    let (trash, nested, _file) = retired_tree(root.path());
    fs::hard_link(&outside, nested.join("linked-file")).unwrap();
    assert!(empty(&trash).unwrap().is_empty());
    let retained = fs::metadata(&outside).unwrap();
    assert_eq!(retained.ino(), original.ino());
    assert_eq!(retained.permissions().mode(), original.permissions().mode());
    assert_eq!(crate::testing::read(&outside), "shared-inode");
}

#[test]
fn readonly_nested_directories_are_removed_without_manual_permission_repair() {
    repairs_directory(READONLY_DIRECTORY_MODE);
}

#[test]
fn an_owned_directory_without_search_permission_is_repaired_through_its_handle() {
    repairs_directory(NO_SEARCH_DIRECTORY_MODE);
}

#[test]
fn directory_permission_repair_stays_bound_to_the_captured_inode_after_rename() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("fixture");
    private_dir(&root).unwrap();
    let outside = crate::testing::outside_retirement(&root);
    let outside_before = fs::metadata(&outside).unwrap();
    let parent = crate::state_io::existing_directory(&root).unwrap().unwrap();
    let child = root.join("retired");
    private_dir(&child).unwrap();
    let mut restore = RestorePermissions::new();
    restore.set(&child, NO_SEARCH_DIRECTORY_MODE);
    let captured = super::capture_directory(
        &parent.try_clone().unwrap().into_std_file(),
        Path::new("retired"),
    )
    .unwrap();
    let original = captured.metadata().unwrap();
    parent.rename("retired", &parent, "held").unwrap();
    crate::file_kind::directory_link(&outside, &child).unwrap();
    super::prepare_directory(&captured).unwrap();
    let repaired = captured.metadata().unwrap();
    assert_eq!(repaired.ino(), original.ino());
    assert_eq!(
        repaired.permissions().mode(),
        domyjob_core::retired_cleanup::directory_mode(original.permissions().mode())
    );
    let outside_after = fs::metadata(&outside).unwrap();
    assert_eq!(outside_after.ino(), outside_before.ino());
    assert_eq!(outside_after.permissions(), outside_before.permissions());
    assert_eq!(
        crate::testing::read(&outside.join("child/canary")),
        "outside-retirement"
    );
}

#[test]
fn an_unreadable_directory_refuses_capture_without_changing_its_mode_or_bytes() {
    let root = tempfile::tempdir().unwrap();
    let (_trash, nested, file) = retired_tree(root.path());
    let mut restore = RestorePermissions::new();
    restore.set(&nested, UNREADABLE_DIRECTORY_MODE);
    let parent = crate::state_io::existing_directory(nested.parent().unwrap())
        .unwrap()
        .unwrap();
    let error = super::capture_directory(&parent.into_std_file(), Path::new("pkg")).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    assert_eq!(
        fs::metadata(&nested).unwrap().permissions().mode() & 0o777,
        UNREADABLE_DIRECTORY_MODE
    );
    drop(restore);
    assert_eq!(crate::testing::read(&file), "retired-output");
}

fn repairs_directory(mode: u32) {
    let root = tempfile::tempdir().unwrap();
    let (trash, nested, _file) = retired_tree(root.path());
    let mut restore = RestorePermissions::new();
    restore.set(&nested, mode);
    assert!(empty(&trash).unwrap().is_empty());
    assert_eq!(fs::read_dir(&trash).unwrap().count(), 0);
}

#[test]
fn nested_and_top_level_symlinks_are_unlinked_without_modifying_their_targets() {
    let root = tempfile::tempdir().unwrap();
    let outside = crate::testing::outside_retirement(root.path());
    let canary = outside.join("child/canary");
    let original = fs::metadata(&canary).unwrap();
    let mut restore = RestorePermissions::new();
    restore.set(&outside, READONLY_DIRECTORY_MODE);
    let (trash, nested, _file) = retired_tree(root.path());
    std::os::unix::fs::symlink(&outside, nested.join("escape")).unwrap();
    std::os::unix::fs::symlink(&outside, trash.join("escape")).unwrap();
    assert!(empty(&trash).unwrap().is_empty());
    assert_eq!(fs::read_dir(&trash).unwrap().count(), 0);
    let retained = fs::metadata(&canary).unwrap();
    assert_eq!(retained.ino(), original.ino());
    assert_eq!(retained.permissions().mode(), original.permissions().mode());
    assert_eq!(crate::testing::read(&canary), "outside-retirement");
    assert_eq!(
        fs::metadata(&outside).unwrap().permissions().mode() & 0o777,
        READONLY_DIRECTORY_MODE
    );
}

#[test]
fn acknowledging_a_quiesced_job_cannot_leave_readonly_output_in_trash_forever() {
    use domyjob_core::domain::SubmissionId;
    use domyjob_core::state::Event;

    let root = tempfile::tempdir().unwrap();
    let runner = root.path().join("runner");
    let store = crate::store::Store::fixture(runner.clone()).unwrap();
    let submission = SubmissionId::try_from("a".repeat(32)).unwrap();
    let (_, job) = crate::testing::home_job(&store, &submission);
    let workspace = runner.join("jobs").join(job.as_str()).join("workspace");
    private_dir(&workspace).unwrap();
    let nested = workspace.join("pkg");
    private_dir(&nested).unwrap();
    crate::testing::write(&nested.join("data"), b"completed-output");
    let mut restore = RestorePermissions::new();
    restore.set(&nested, READONLY_DIRECTORY_MODE);
    store.transition(&job, &Event::Starting).unwrap();
    store.transition(&job, &Event::Spawned { pid: 42 }).unwrap();
    store.transition(&job, &Event::Exited { code: 0 }).unwrap();
    crate::state_io::mark_existing(&store.quiescence_marker(&job).unwrap()).unwrap();
    store.acknowledge(&job).unwrap();
    assert_eq!(
        store.status(&job).unwrap().outcome(),
        Some(&domyjob_core::state::Outcome::Succeeded)
    );
    assert_eq!(
        fs::symlink_metadata(runner.join("jobs").join(job.as_str()))
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::NotFound
    );
    assert_eq!(empty(&runner.join("trash")).unwrap().len(), 0);
}
