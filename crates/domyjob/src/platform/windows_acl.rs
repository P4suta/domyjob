use std::fs;
use std::io;
use std::path::Path;

mod descriptor;

use super::{Exposure, Ownership};
use descriptor::{Ace, Acl, FileSecurity, SecurityDescriptor, Sid, SidRef, current_user_sid};

pub(super) fn ownership(file: &fs::File) -> io::Result<Ownership> {
    ownership_using(file, current_user_sid, descriptor::sid_text)
}

fn ownership_using(
    file: &fs::File,
    current_user: impl FnOnce() -> io::Result<Sid>,
    mut sid_text: impl FnMut(SidRef<'_>) -> io::Result<Sid>,
) -> io::Result<Ownership> {
    const ADMINISTRATORS: &str = "S-1-5-32-544";
    let security = FileSecurity::read(file)?;
    let user = current_user()?;
    let owner = sid_text(security.owner())?;
    if owner != user && owner.as_str() != ADMINISTRATORS {
        return Ok(Ownership::Foreign);
    }
    acl_ownership(security.acl(), &user, sid_text)
}

fn acl_ownership(
    dacl: Option<Acl<'_>>,
    user: &Sid,
    mut ace_sid: impl FnMut(SidRef<'_>) -> io::Result<Sid>,
) -> io::Result<Ownership> {
    const SYSTEM: &str = "S-1-5-18";
    let Some(dacl) = dacl else {
        return Ok(Ownership::Exposed(Exposure));
    };
    let count = dacl.ace_count()?;
    if too_many_aces(count) {
        return Ok(Ownership::Exposed(Exposure));
    }
    for index in 0..count {
        let Ace::Allowed(sid) = dacl.ace(index)? else {
            return Ok(Ownership::Exposed(Exposure));
        };
        let sid = ace_sid(sid)?;
        if &sid != user && sid.as_str() != SYSTEM {
            return Ok(Ownership::Exposed(Exposure));
        }
    }
    Ok(Ownership::Private)
}

const fn too_many_aces(count: u32) -> bool {
    const MAX_ACES: u32 = 32;
    count > MAX_ACES
}

pub(super) fn no_follow(options: &mut fs::OpenOptions) {
    descriptor::no_follow(options);
}

pub(super) fn open_private_dir(path: &Path) -> io::Result<fs::File> {
    descriptor::open_private_dir(path, &mut super::raw::options())
}

pub(super) fn create_private_dir(path: &Path) -> io::Result<()> {
    create_private_dir_using(
        path,
        current_user_sid,
        SecurityDescriptor::for_user,
        |descriptor, ancestor| descriptor.create_dir(ancestor).map(|_success| ()),
    )
}

fn create_private_dir_using(
    path: &Path,
    current_user: impl FnOnce() -> io::Result<Sid>,
    descriptor_for_user: impl FnOnce(&Sid) -> io::Result<SecurityDescriptor>,
    mut create_dir: impl FnMut(&SecurityDescriptor, &Path) -> io::Result<()>,
) -> io::Result<()> {
    let user = current_user()?;
    let descriptor = descriptor_for_user(&user)?;
    for ancestor in path.ancestors().collect::<Vec<_>>().into_iter().rev() {
        if ancestor.as_os_str().is_empty() {
            continue;
        }
        match fs::symlink_metadata(ancestor) {
            Ok(metadata) if metadata.is_dir() && !super::reparse_point(&metadata) => continue,
            Ok(_blocked) => return Err(io::ErrorKind::AlreadyExists.into()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        match create_dir(&descriptor, ancestor) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                let metadata = fs::symlink_metadata(ancestor)?;
                if !metadata.is_dir() || super::reparse_point(&metadata) {
                    return Err(error);
                }
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::fs;
    use std::path::Path;

    use super::descriptor::validated_sid;
    use super::{
        SecurityDescriptor, acl_ownership, create_private_dir, create_private_dir_using,
        current_user_sid, open_private_dir, ownership, ownership_using, too_many_aces,
    };

    const ERROR_ACCESS_DENIED: u32 = 5;
    const ERROR_ALREADY_EXISTS: u32 = 183;
    const ERROR_NOT_ENOUGH_MEMORY: u32 = 8;

    fn assert_os_error(error: &std::io::Error, code: u32) {
        assert_eq!(error.raw_os_error(), Some(code.cast_signed()));
    }

    #[test]
    fn a_directory_name_with_nul_never_creates_its_truncated_prefix() {
        let root = tempfile::tempdir().expect("owned directory");
        let user = current_user_sid().expect("current user");
        let descriptor = SecurityDescriptor::for_user(&user).expect("security descriptor");
        let prefix = root.path().join("should-not-exist");
        let mut name = prefix.as_os_str().to_os_string();
        name.push("\0ignored");
        let error = descriptor
            .create_dir(Path::new(&name))
            .expect_err("interior NUL");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        let absent = fs::symlink_metadata(&prefix).expect_err("prefix was not created");
        assert_eq!(absent.kind(), std::io::ErrorKind::NotFound);
    }

    #[test]
    fn an_existing_directory_preserves_the_creation_error() {
        let root = tempfile::tempdir().expect("owned directory");
        let user = current_user_sid().expect("current user");
        let descriptor = SecurityDescriptor::for_user(&user).expect("security descriptor");
        let error = descriptor
            .create_dir(root.path())
            .expect_err("directory already exists");
        assert_eq!(
            error.raw_os_error(),
            Some(ERROR_ALREADY_EXISTS.cast_signed())
        );
        assert!(
            fs::symlink_metadata(root.path())
                .expect("owned directory")
                .is_dir()
        );
    }

    #[test]
    fn a_failed_user_lookup_stops_before_converting_the_owner() {
        let file = tempfile::NamedTempFile::new().expect("owned file");
        let called = Cell::new(false);
        let error = ownership_using(
            file.as_file(),
            || {
                Err(std::io::Error::from_raw_os_error(
                    ERROR_ACCESS_DENIED.cast_signed(),
                ))
            },
            |_sid| {
                called.set(true);
                Err(std::io::Error::other("owner conversion was not requested"))
            },
        )
        .expect_err("user lookup failed");
        assert_eq!(
            error.raw_os_error(),
            Some(ERROR_ACCESS_DENIED.cast_signed())
        );
        assert!(!called.get());
    }

    #[test]
    fn a_failed_owner_conversion_preserves_its_error() {
        let file = tempfile::NamedTempFile::new().expect("owned file");
        let error = ownership_using(file.as_file(), current_user_sid, |sid| {
            sid.text().expect("valid borrowed owner SID");
            Err(std::io::Error::from_raw_os_error(
                ERROR_NOT_ENOUGH_MEMORY.cast_signed(),
            ))
        })
        .expect_err("owner conversion failed");
        assert_eq!(
            error.raw_os_error(),
            Some(ERROR_NOT_ENOUGH_MEMORY.cast_signed())
        );
    }

    #[test]
    fn an_absent_acl_is_exposed_without_inspecting_aces() {
        let called = Cell::new(false);
        let user = validated_sid("S-1-5-18".to_owned()).expect("system SID");
        let result = acl_ownership(None, &user, |_sid| {
            called.set(true);
            Err(std::io::Error::other("an absent ACL has no ACE"))
        })
        .expect("absent ACL policy");
        assert!(matches!(result, super::Ownership::Exposed(_)));
        assert!(!called.get());
    }

    #[test]
    fn the_acl_count_limit_accepts_its_last_permitted_ace() {
        assert!(!too_many_aces(31));
        assert!(!too_many_aces(32));
        assert!(too_many_aces(33));
    }

    #[test]
    fn a_failed_ace_sid_conversion_preserves_its_error() {
        let root = tempfile::tempdir().expect("owned directory");
        let private = root.path().join("private");
        create_private_dir(&private).expect("private directory");
        let file = tempfile::NamedTempFile::new_in(&private).expect("owned private file");
        assert!(matches!(
            ownership(file.as_file()).expect("private ownership"),
            super::Ownership::Private
        ));
        let calls = Cell::new(0);
        let error = ownership_using(file.as_file(), current_user_sid, |sid| {
            calls.set(calls.get() + 1);
            if calls.get() == 1 {
                sid.text()
            } else {
                Err(std::io::Error::from_raw_os_error(
                    ERROR_NOT_ENOUGH_MEMORY.cast_signed(),
                ))
            }
        })
        .expect_err("ACE conversion failed");
        assert_os_error(&error, ERROR_NOT_ENOUGH_MEMORY);
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn a_failed_directory_user_lookup_stops_before_descriptor_creation() {
        let called = Cell::new(false);
        let error = create_private_dir_using(
            Path::new(""),
            || {
                Err(std::io::Error::from_raw_os_error(
                    ERROR_ACCESS_DENIED.cast_signed(),
                ))
            },
            |_user| {
                called.set(true);
                Err(std::io::Error::other(
                    "descriptor creation was not requested",
                ))
            },
            |_descriptor, _path| {
                Err(std::io::Error::other(
                    "directory creation was not requested",
                ))
            },
        )
        .expect_err("user lookup failed");
        assert_os_error(&error, ERROR_ACCESS_DENIED);
        assert!(!called.get());
    }

    #[test]
    fn a_failed_directory_descriptor_preserves_its_error() {
        let called = Cell::new(false);
        let error = create_private_dir_using(
            Path::new(""),
            current_user_sid,
            |_user| {
                Err(std::io::Error::from_raw_os_error(
                    ERROR_NOT_ENOUGH_MEMORY.cast_signed(),
                ))
            },
            |_descriptor, _path| {
                called.set(true);
                Err(std::io::Error::other(
                    "directory creation was not requested",
                ))
            },
        )
        .expect_err("descriptor creation failed");
        assert_os_error(&error, ERROR_NOT_ENOUGH_MEMORY);
        assert!(!called.get());
    }

    #[test]
    fn a_relative_directory_skips_only_the_empty_ancestor() {
        let root = tempfile::TempDir::new_in(".").expect("owned current directory fixture");
        let current = std::env::current_dir().expect("current directory");
        let relative = root
            .path()
            .strip_prefix(&current)
            .expect("relative fixture");
        assert!(!relative.is_absolute());
        let leaf = relative.join("first").join("second");
        create_private_dir(&leaf).expect("relative private directory");
        let file = open_private_dir(&leaf).expect("created leaf");
        assert!(matches!(
            ownership(&file).expect("private ownership"),
            super::Ownership::Private
        ));
    }

    #[test]
    fn a_regular_file_blocks_private_directory_creation() {
        let root = tempfile::tempdir().expect("owned directory");
        let file = tempfile::NamedTempFile::new_in(root.path()).expect("owned blocker");
        let error = create_private_dir(file.path()).expect_err("a regular file is not a directory");
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        let metadata = file.as_file().metadata().expect("blocker unchanged");
        assert!(metadata.is_file());
        assert_eq!(metadata.len(), 0);
    }

    #[test]
    fn a_directory_creation_error_is_not_treated_as_an_existing_directory() {
        let root = tempfile::tempdir().expect("owned directory");
        let path = root.path().join("uncreated");
        let calls = Cell::new(0);
        let error = create_private_dir_using(
            &path,
            current_user_sid,
            SecurityDescriptor::for_user,
            |_descriptor, ancestor| {
                calls.set(calls.get() + 1);
                assert_eq!(ancestor, path);
                Err(std::io::Error::from_raw_os_error(
                    ERROR_ACCESS_DENIED.cast_signed(),
                ))
            },
        )
        .expect_err("creation denied");
        assert_os_error(&error, ERROR_ACCESS_DENIED);
        assert_eq!(calls.get(), 1);
        assert_eq!(
            fs::symlink_metadata(&path)
                .expect_err("directory was not created")
                .kind(),
            std::io::ErrorKind::NotFound
        );
    }

    enum Inspected {
        Directory,
        Missing,
        File,
    }

    fn with_directory_race(
        path: &Path,
        inspected: &Inspected,
        check: impl FnOnce(std::io::Result<()>),
    ) {
        let calls = Cell::new(0_u32);
        let result = create_private_dir_using(
            path,
            current_user_sid,
            SecurityDescriptor::for_user,
            |descriptor, ancestor| {
                calls.set(calls.get().saturating_add(1));
                assert_eq!(ancestor, path);
                descriptor.create_dir(ancestor).expect("competing creation");
                let error = descriptor
                    .create_dir(ancestor)
                    .expect_err("already created");
                match inspected {
                    Inspected::Directory => {}
                    Inspected::Missing | Inspected::File => {
                        fs::remove_dir(ancestor).expect("remove the owned empty directory");
                        if matches!(inspected, Inspected::File) {
                            crate::testing::write(ancestor, b"");
                        }
                    }
                }
                Err(error)
            },
        );
        assert_eq!(calls.get(), 1);
        check(result);
    }

    #[test]
    fn a_directory_created_during_inspection_is_checked_again() {
        let root = tempfile::tempdir().expect("owned directory");
        let path = root.path().join("created-concurrently");
        with_directory_race(&path, &Inspected::Directory, |result| {
            result.expect("a concurrently created directory is accepted after inspection");
        });
        let file = open_private_dir(&path).expect("created directory");
        assert!(matches!(
            ownership(&file).expect("private ownership"),
            super::Ownership::Private
        ));
    }

    #[test]
    fn a_directory_that_disappears_before_reinspection_preserves_the_error() {
        let root = tempfile::tempdir().expect("owned directory");
        let path = root.path().join("removed-during-inspection");
        with_directory_race(&path, &Inspected::Missing, |result| {
            let error = result.expect_err("reinspection lost the directory");
            assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
        });
    }

    #[test]
    fn a_file_that_replaces_a_directory_during_inspection_is_rejected() {
        let root = tempfile::tempdir().expect("owned directory");
        let path = root.path().join("replaced-during-inspection");
        with_directory_race(&path, &Inspected::File, |result| {
            let error = result.expect_err("a regular file replaced the directory");
            assert_os_error(&error, ERROR_ALREADY_EXISTS);
        });
        assert!(
            fs::symlink_metadata(&path)
                .expect("owned blocker")
                .is_file()
        );
    }

    #[test]
    fn only_windows_sid_syntax_enters_an_acl() {
        assert_eq!(
            validated_sid("S-1-5-18".to_owned())
                .expect("system SID")
                .as_str(),
            "S-1-5-18"
        );
        for text in ["not a SID", "S-1-5-18)(A;;FA;;;WD)", "S-1-"] {
            let _error = validated_sid(text.to_owned()).expect_err("invalid SID");
        }
    }
}
