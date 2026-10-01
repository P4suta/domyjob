#![expect(
    unsafe_code,
    reason = "Windows ACL and token APIs require raw handles, pointers, and ownership transfer"
)]

use std::fs;
use std::io;
use std::path::Path;
use std::ptr::NonNull;

use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo,
    SDDL_REVISION_1, SE_FILE_OBJECT,
};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, ACL_SIZE_INFORMATION, AclSizeInformation,
    DACL_SECURITY_INFORMATION, GetAce, GetAclInformation, GetTokenInformation,
    OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES, TOKEN_QUERY,
    TOKEN_USER, TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateDirectoryW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, READ_CONTROL,
};
use windows_sys::Win32::System::SystemServices::ACCESS_ALLOWED_ACE_TYPE;
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use super::{Exposure, Ownership};

#[derive(Debug, Clone, PartialEq, Eq)]
struct Sid(String);

impl Sid {
    fn as_str(&self) -> &str {
        &self.0
    }
}

fn validated_sid(text: String) -> io::Result<Sid> {
    if text.strip_prefix("S-1-").is_some_and(|parts| {
        !parts.is_empty()
            && parts
                .chars()
                .all(|character| character.is_ascii_digit() || character == '-')
    }) {
        Ok(Sid(text))
    } else {
        Err(io::Error::other("the SID is malformed"))
    }
}

fn sid_text(sid: PSID) -> io::Result<Sid> {
    converted_sid(
        sid,
        |value, output| unsafe { ConvertSidToStringSidW(value, output) },
        |value| {
            unsafe { LocalFree(value.cast()) };
        },
    )
}

fn converted_sid(
    sid: PSID,
    convert: impl FnOnce(PSID, &mut *mut u16) -> i32,
    release: impl FnOnce(*mut u16),
) -> io::Result<Sid> {
    if sid.is_null() {
        return Err(io::Error::other("the SID is missing"));
    }
    let mut wide: *mut u16 = std::ptr::null_mut();
    if convert(sid, &mut wide) == 0 || wide.is_null() {
        return Err(io::Error::last_os_error());
    }
    let mut length = 0_usize;
    while length < 256 && unsafe { *wide.add(length) } != 0 {
        length = length.saturating_add(1);
    }
    let result = if length == 256 {
        Err(io::Error::other("the SID is too long"))
    } else {
        let text = String::from_utf16(unsafe { std::slice::from_raw_parts(wide, length) })
            .map_err(io::Error::other)?;
        validated_sid(text)
    };
    release(wide);
    result
}

fn current_user_sid() -> io::Result<Sid> {
    use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};

    let mut raw = std::ptr::null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut raw) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let token = unsafe { OwnedHandle::from_raw_handle(raw) };
    token_user(|buffer, length, needed| unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            buffer.map_or(std::ptr::null_mut(), |value| value.as_mut_ptr().cast()),
            length,
            needed,
        )
    })
}

fn token_user(mut query: impl FnMut(Option<&mut [u8]>, u32, &mut u32) -> i32) -> io::Result<Sid> {
    let mut needed = 0_u32;
    query(None, 0, &mut needed);
    if needed == 0 || needed > 4096 {
        return Err(io::Error::other(
            "the token user is unavailable or too large",
        ));
    }
    let size = usize::try_from(needed).map_err(io::Error::other)?;
    let mut buffer = vec![0_u8; size];
    if query(Some(&mut buffer), needed, &mut needed) == 0 || buffer.len() < size_of::<TOKEN_USER>()
    {
        return Err(io::Error::other("the token has no user"));
    }
    let user: TOKEN_USER = unsafe { std::ptr::read_unaligned(buffer.as_ptr().cast()) };
    sid_text(user.User.Sid)
}

struct SecurityDescriptor(PSECURITY_DESCRIPTOR);

impl SecurityDescriptor {
    fn for_user(user: &Sid) -> io::Result<Self> {
        let sddl = format!(
            "O:{}D:P(A;OICI;FA;;;{})(A;OICI;FA;;;SY)",
            user.as_str(),
            user.as_str()
        );
        let wide: Vec<u16> = sddl.encode_utf16().chain(std::iter::once(0)).collect();
        let mut descriptor = std::ptr::null_mut();
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                wide.as_ptr(),
                SDDL_REVISION_1,
                &raw mut descriptor,
                std::ptr::null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(descriptor))
    }

    fn create_dir(&self, path: &Path) -> io::Result<()> {
        use std::os::windows::ffi::OsStrExt as _;

        let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
        if wide.contains(&0) {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        wide.push(0);
        let size = u32::try_from(size_of::<SECURITY_ATTRIBUTES>()).map_err(io::Error::other)?;
        let attributes = SECURITY_ATTRIBUTES {
            nLength: size,
            lpSecurityDescriptor: self.0,
            bInheritHandle: 0,
        };
        if unsafe { CreateDirectoryW(wide.as_ptr(), &raw const attributes) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        unsafe { LocalFree(self.0) };
    }
}

pub(super) fn ownership(file: &fs::File) -> io::Result<Ownership> {
    ownership_using(file, current_user_sid, sid_text)
}

fn ownership_using(
    file: &fs::File,
    current_user: impl FnOnce() -> io::Result<Sid>,
    mut sid_text_using: impl FnMut(PSID) -> io::Result<Sid>,
) -> io::Result<Ownership> {
    use std::os::windows::io::AsRawHandle as _;

    const ADMINISTRATORS: &str = "S-1-5-32-544";
    let mut owner = std::ptr::null_mut();
    let mut dacl = std::ptr::null_mut();
    let mut descriptor = std::ptr::null_mut();
    let status = unsafe {
        GetSecurityInfo(
            file.as_raw_handle(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &raw mut owner,
            std::ptr::null_mut(),
            &raw mut dacl,
            std::ptr::null_mut(),
            &raw mut descriptor,
        )
    };
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status.cast_signed()));
    }
    let _descriptor = SecurityDescriptor(descriptor);
    let user = current_user()?;
    let owner = sid_text_using(owner)?;
    if owner != user && owner.as_str() != ADMINISTRATORS {
        return Ok(Ownership::Foreign);
    }
    acl_ownership(NonNull::new(dacl), &user, sid_text_using)
}

fn acl_ownership(
    dacl: Option<NonNull<ACL>>,
    user: &Sid,
    mut ace_sid: impl FnMut(PSID) -> io::Result<Sid>,
) -> io::Result<Ownership> {
    const SYSTEM: &str = "S-1-5-18";
    let Some(dacl) = dacl else {
        return Ok(Ownership::Exposed(Exposure));
    };
    let mut size = ACL_SIZE_INFORMATION::default();
    let bytes = u32::try_from(size_of::<ACL_SIZE_INFORMATION>()).map_err(io::Error::other)?;
    if unsafe {
        GetAclInformation(
            dacl.as_ptr(),
            (&raw mut size).cast(),
            bytes,
            AclSizeInformation,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    if too_many_aces(size.AceCount) {
        return Ok(Ownership::Exposed(Exposure));
    }
    for index in 0..size.AceCount {
        let mut raw = std::ptr::null_mut();
        if unsafe { GetAce(dacl.as_ptr(), index, &raw mut raw) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let header: &ACE_HEADER = unsafe { &*raw.cast() };
        if unsupported_ace(*header) {
            return Ok(Ownership::Exposed(Exposure));
        }
        let ace: &ACCESS_ALLOWED_ACE = unsafe { &*raw.cast() };
        let sid = ace_sid((&raw const ace.SidStart).cast_mut().cast())?;
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

fn unsupported_ace(header: ACE_HEADER) -> bool {
    u32::from(header.AceType) != ACCESS_ALLOWED_ACE_TYPE
        || usize::from(header.AceSize) < size_of::<ACCESS_ALLOWED_ACE>()
}

pub(super) fn open_private_dir(path: &Path) -> io::Result<fs::File> {
    use std::os::windows::fs::OpenOptionsExt as _;

    let mut options = super::raw::options();
    options.access_mode(READ_CONTROL);
    options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS);
    options.open(path)
}

pub(super) fn create_private_dir(path: &Path) -> io::Result<()> {
    create_private_dir_using(
        path,
        current_user_sid,
        SecurityDescriptor::for_user,
        SecurityDescriptor::create_dir,
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
    use std::os::windows::fs::OpenOptionsExt as _;
    use std::path::Path;

    use windows_sys::Win32::Foundation::{
        ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, ERROR_NOT_ENOUGH_MEMORY, SetLastError,
    };
    use windows_sys::Win32::Security::{
        ACCESS_ALLOWED_ACE, ACE_HEADER, SECURITY_MAX_SID_SIZE, SID, SID_IDENTIFIER_AUTHORITY,
        TOKEN_USER,
    };
    use windows_sys::Win32::System::SystemServices::ACCESS_ALLOWED_ACE_TYPE;

    use super::{
        SecurityDescriptor, acl_ownership, converted_sid, create_private_dir,
        create_private_dir_using, current_user_sid, open_private_dir, ownership, ownership_using,
        sid_text, token_user, too_many_aces, unsupported_ace, validated_sid,
    };

    fn assert_os_error(error: &std::io::Error, code: u32) {
        assert_eq!(error.raw_os_error(), Some(code.cast_signed()));
    }

    #[test]
    fn a_missing_sid_is_rejected_before_conversion() {
        let called = Cell::new(false);
        let error = converted_sid(
            std::ptr::null_mut(),
            |_sid, _output| {
                called.set(true);
                unsafe { SetLastError(ERROR_NOT_ENOUGH_MEMORY) };
                0
            },
            |_output| {},
        )
        .expect_err("missing SID");
        assert_eq!(error.to_string(), "the SID is missing");
        assert!(!called.get());
    }

    #[test]
    fn a_failed_conversion_never_accepts_its_output_buffer() {
        let mut sid = SID {
            Revision: 1,
            SubAuthorityCount: 1,
            IdentifierAuthority: SID_IDENTIFIER_AUTHORITY {
                Value: [0, 0, 0, 0, 0, 5],
            },
            SubAuthority: [18],
        };
        let mut text: Vec<u16> = "S-1-5-18"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let released = Cell::new(false);
        let error = converted_sid(
            (&raw mut sid).cast(),
            |_sid, output| {
                *output = text.as_mut_ptr();
                unsafe { SetLastError(ERROR_NOT_ENOUGH_MEMORY) };
                0
            },
            |_output| released.set(true),
        )
        .expect_err("converter failure");
        assert_eq!(
            error.raw_os_error(),
            Some(ERROR_NOT_ENOUGH_MEMORY.cast_signed())
        );
        assert!(!released.get());
    }

    #[test]
    fn a_failed_token_size_query_stops_before_loading_the_user() {
        let called = Cell::new(0);
        let error = token_user(|_buffer, _length, _needed| {
            called.set(called.get() + 1);
            0
        })
        .expect_err("no required size");
        assert_eq!(
            error.to_string(),
            "the token user is unavailable or too large"
        );
        assert_eq!(called.get(), 1);
    }

    #[test]
    fn a_failed_token_read_never_interprets_its_initialized_buffer() {
        let size =
            size_of::<TOKEN_USER>() + usize::try_from(SECURITY_MAX_SID_SIZE).expect("SID bound");
        let error = token_user(|buffer, length, needed| {
            if let Some(bytes) = buffer {
                assert_eq!(bytes.len(), size);
                assert_eq!(length, u32::try_from(size).expect("token size"));
            } else {
                assert_eq!(length, 0);
                *needed = u32::try_from(size).expect("token size");
            }
            0
        })
        .expect_err("token query failure");
        assert_eq!(error.to_string(), "the token has no user");
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
    fn a_security_query_preserves_the_handle_access_denial() {
        let root = tempfile::tempdir().expect("owned directory");
        let owned = tempfile::NamedTempFile::new_in(root.path()).expect("owned file");
        let file = super::super::raw::options()
            .access_mode(0)
            .open(owned.path())
            .expect("metadata-only handle");
        let error = ownership(&file).expect_err("READ_CONTROL is required");
        assert_eq!(
            error.raw_os_error(),
            Some(ERROR_ACCESS_DENIED.cast_signed())
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
            assert!(!sid.is_null());
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
    fn ace_type_and_header_size_are_independent_rejection_reasons() {
        let size = u16::try_from(size_of::<ACCESS_ALLOWED_ACE>()).expect("ACE size");
        let allowed = u8::try_from(ACCESS_ALLOWED_ACE_TYPE).expect("allowed ACE type");
        let mut header = ACE_HEADER {
            AceType: allowed,
            AceFlags: 0,
            AceSize: size,
        };
        assert!(!unsupported_ace(header));
        header.AceType = allowed + 1;
        assert!(unsupported_ace(header));
        header.AceType = allowed;
        header.AceSize = size - 1;
        assert!(unsupported_ace(header));
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
                sid_text(sid)
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
