use std::path::Path;

use cap_std::fs::Dir;

const PUBLIC: &[u8] = b"public";
const SECRET: &[u8] = b"secret";

fn assert_original(result: std::io::Result<super::File>) {
    let file = result.unwrap();
    assert_eq!(
        crate::bounded::read(file, PUBLIC.len()).unwrap().unwrap(),
        PUBLIC
    );
}

#[test]
fn renamed_captured_ancestors_cannot_redirect_the_final_open_to_metadata() {
    let root = tempfile::tempdir().unwrap();
    crate::testing::write(&root.path().join("a/b/file"), PUBLIC);
    crate::testing::write(&root.path().join(".git/b/file"), SECRET);
    let directory = Dir::open_ambient_dir(root.path(), cap_std::ambient_authority()).unwrap();
    assert_original(super::open_using(
        &directory,
        Path::new("a/b/file"),
        || match directory.rename("a", &directory, "before") {
            Ok(()) => {
                crate::file_kind::directory_link(Path::new(".git"), &root.path().join("a"))
                    .unwrap();
            }
            Err(error) => assert!(super::native::rename_blocked(&error), "{error}"),
        },
    ));
}

#[test]
fn replacing_the_root_name_cannot_redirect_a_captured_file_open() {
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("checkout");
    crate::testing::write(&root.join("file"), PUBLIC);
    let outer = Dir::open_ambient_dir(parent.path(), cap_std::ambient_authority()).unwrap();
    let directory = Dir::open_ambient_dir(&root, cap_std::ambient_authority()).unwrap();
    assert_original(super::open_using(
        &directory,
        Path::new("file"),
        || match outer.rename("checkout", &outer, "before") {
            Ok(()) => crate::testing::write(&root.join("file"), SECRET),
            Err(error) => assert!(super::native::rename_blocked(&error), "{error}"),
        },
    ));
}
