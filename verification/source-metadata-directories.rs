#[path = "../crates/domyjob/src/source/metadata_directories.rs"]
mod metadata_directories;

const CUSTOM_DIRECTORY_BYTES: usize = 3;

enum Case {
    GitRoot,
    GitDescendant,
    CommonDescendant,
    Sibling,
}

fn metadata_case(case: Case) -> bool {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Component;

    let name: [u8; CUSTOM_DIRECTORY_BYTES] = kani::any();
    for byte in name {
        kani::assume(byte.is_ascii_lowercase());
    }
    let name = Component::Normal(OsStr::from_bytes(&name));
    let source = Component::Normal(OsStr::new("source"));
    let shared = Component::Normal(OsStr::new("shared"));
    let private = Component::Normal(OsStr::new("private"));
    let objects = Component::Normal(OsStr::new("objects"));
    let refs = Component::Normal(OsStr::new("refs"));
    let git = [source, name];
    let common = [source, shared];
    match case {
        Case::GitRoot => metadata_directories::contains_components(
            git.iter().copied(),
            common.iter().copied(),
            git.iter().copied(),
        ),
        Case::GitDescendant => metadata_directories::contains_components(
            git.iter().copied(),
            common.iter().copied(),
            [source, name, objects, private].iter().copied(),
        ),
        Case::CommonDescendant => metadata_directories::contains_components(
            git.iter().copied(),
            common.iter().copied(),
            [source, shared, refs, private].iter().copied(),
        ),
        Case::Sibling => {
            let mut sibling_name = [b'x'; 4];
            sibling_name[..CUSTOM_DIRECTORY_BYTES]
                .copy_from_slice(name.as_os_str().as_encoded_bytes());
            let sibling_name = Component::Normal(OsStr::from_bytes(&sibling_name));
            metadata_directories::contains_components(
                git.iter().copied(),
                common.iter().copied(),
                [source, sibling_name, private].iter().copied(),
            )
        }
    }
}

#[kani::proof]
#[kani::unwind(32)]
fn actual_git_metadata_root_is_excluded() {
    let excluded = metadata_case(Case::GitRoot);
    assert!(excluded);
    kani::cover!(excluded);
}

#[kani::proof]
#[kani::unwind(32)]
fn actual_git_metadata_descendants_are_excluded() {
    let excluded = metadata_case(Case::GitDescendant);
    assert!(excluded);
    kani::cover!(excluded);
}

#[kani::proof]
#[kani::unwind(32)]
fn actual_common_metadata_descendants_are_excluded() {
    let excluded = metadata_case(Case::CommonDescendant);
    assert!(excluded);
    kani::cover!(excluded);
}

#[kani::proof]
#[kani::unwind(32)]
fn component_prefix_siblings_remain_source_inputs() {
    let excluded = metadata_case(Case::Sibling);
    assert!(!excluded);
    kani::cover!(!excluded);
}

#[kani::proof]
fn rejects_custom_git_metadata_delivery() {
    let directories = metadata_directories::MetadataDirectories::new(
        "source/gitdb".into(),
        "source/common".into(),
    );
    kani::assert(
        !directories.contains(std::path::Path::new("source/gitdb/private")),
        "NEGATIVE_GIT_DIRECTORY_CONTROL",
    );
}
