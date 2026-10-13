use std::fs;

#[cfg(all(test, unix))]
pub(crate) fn directory_link(
    target: &std::path::Path,
    link: &std::path::Path,
) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(all(test, windows))]
pub(crate) fn directory_link(
    target: &std::path::Path,
    link: &std::path::Path,
) -> std::io::Result<()> {
    let status = crate::process::command("cmd.exe")
        .current_dir(
            link.parent()
                .ok_or_else(|| std::io::Error::other("the junction parent is missing"))?,
        )
        .args(["/d", "/c", "mklink", "/j"])
        .arg(link)
        .arg(target)
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "junction creation failed: {status}"
        )))
    }
}

#[cfg(unix)]
pub(crate) fn reparse_point(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(windows)]
pub(crate) fn reparse_point(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt as _;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;

    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(test)]
mod tests {
    use super::reparse_point;

    #[test]
    fn ordinary_files_and_directories_are_not_reparse_points() {
        let root = tempfile::tempdir().expect("temporary directory");
        let file = root.path().join("file");
        crate::testing::write(&file, "plain file");
        for path in [root.path(), file.as_path()] {
            let metadata = std::fs::symlink_metadata(path).expect("fixture metadata");
            assert!(!reparse_point(&metadata), "{}", path.display());
        }
    }

    #[cfg(unix)]
    #[test]
    fn symbolic_links_are_reparse_points_even_when_their_target_is_ordinary() {
        let root = tempfile::tempdir().expect("temporary directory");
        let target = root.path().join("target");
        crate::testing::write(&target, "plain file");
        let link = root.path().join("link");
        std::os::unix::fs::symlink(&target, &link).expect("symbolic link");
        assert!(reparse_point(
            &std::fs::symlink_metadata(&link).expect("link metadata")
        ));
        assert!(!reparse_point(
            &std::fs::metadata(&link).expect("target metadata")
        ));
    }

    #[cfg(windows)]
    #[test]
    fn directory_junctions_are_reparse_points_and_cannot_be_private_state() {
        let root = tempfile::tempdir().expect("temporary directory");
        crate::testing::mkdir(&root.path().join("target"));
        let status = crate::process::command("cmd.exe")
            .current_dir(root.path())
            .args(["/d", "/c", "mklink", "/j", "link", "target"])
            .status()
            .expect("create a directory junction");
        assert!(status.success(), "junction command failed: {status}");
        let link = root.path().join("link");
        assert!(reparse_point(
            &std::fs::symlink_metadata(&link).expect("junction metadata")
        ));
        assert!(!reparse_point(
            &std::fs::metadata(&link).expect("target metadata")
        ));
        assert!(crate::state_io::private_dir(&link).is_err());
    }
}
