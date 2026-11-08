use std::fs;
use std::path::{Path, PathBuf};

use crate::state_io::{StateError, empty, private_dir};

fn fixture() -> (tempfile::TempDir, PathBuf) {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("fixture");
    private_dir(&root).unwrap();
    (temporary, root)
}

fn retired_tree(root: &Path) -> PathBuf {
    let trash = root.join("trash");
    private_dir(&trash).unwrap();
    let child = trash.join("retired");
    private_dir(&child).unwrap();
    crate::testing::write(&child.join("data"), b"retired-output");
    trash
}

#[test]
fn a_missing_trash_is_not_created_by_cleanup() {
    let root = tempfile::tempdir().unwrap();
    let trash = root.path().join("missing");
    assert!(empty(&trash).unwrap().is_empty());
    assert_eq!(
        fs::symlink_metadata(&trash).unwrap_err().kind(),
        std::io::ErrorKind::NotFound
    );
}

#[test]
fn a_linked_trash_root_is_refused_without_deleting_an_outside_tree() {
    let (_temporary, root) = fixture();
    let outside = crate::testing::outside_retirement(&root);
    let trash = root.join("trash");
    crate::file_kind::directory_link(&outside, &trash).unwrap();
    assert!(matches!(
        empty(&trash),
        Err(StateError::NotDirectory { .. })
    ));
    assert_eq!(
        crate::testing::read(&outside.join("child/canary")),
        "outside-retirement"
    );
}

#[test]
fn a_captured_trash_cannot_remove_the_tree_that_replaces_its_name() {
    let (_temporary, root) = fixture();
    let outside = crate::testing::outside_retirement(&root);
    let trash = retired_tree(&root);
    let child = trash.join("retired");
    let held = root.join("held");
    assert!(
        super::empty_using(&trash, |_retained| {
            crate::platform::cleanup::replace_root(&trash, &held, &outside)
        })
        .unwrap()
        .is_empty()
    );
    assert_eq!(
        crate::testing::read(&outside.join("child/canary")),
        "outside-retirement"
    );
    assert_eq!(
        fs::symlink_metadata(&child).unwrap_err().kind(),
        std::io::ErrorKind::NotFound
    );
}

#[test]
fn a_directory_replaced_by_a_link_is_refused_before_target_permissions_change() {
    let (_temporary, root) = fixture();
    let outside = crate::testing::outside_retirement(&root);
    let trash = retired_tree(&root);
    let original = trash.join("retired");
    let held = trash.join("held");
    let retained = crate::state_io::existing_directory(&trash)
        .unwrap()
        .unwrap();
    let metadata = fs::metadata(&outside).unwrap();
    let error = super::remove_entry_using(&retained, std::ffi::OsStr::new("retired"), || {
        retained.rename("retired", &retained, "held")?;
        crate::file_kind::directory_link(&outside, &original)
    })
    .unwrap_err();
    assert!(
        crate::platform::cleanup::linked_directory_refusal(&error),
        "{error:?}"
    );
    assert_eq!(
        fs::metadata(&outside).unwrap().permissions(),
        metadata.permissions()
    );
    assert_eq!(
        crate::testing::read(&outside.join("child/canary")),
        "outside-retirement"
    );
    assert_eq!(crate::testing::read(&held.join("data")), "retired-output");
}
