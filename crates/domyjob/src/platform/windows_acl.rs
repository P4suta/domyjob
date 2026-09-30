#![expect(
    unsafe_code,
    reason = "Windows ACL and token APIs require raw handles, pointers, and ownership transfer"
)]

use std::fs;
use std::io;
use std::path::Path;

use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo,
    SDDL_REVISION_1, SE_FILE_OBJECT,
};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACE_HEADER, ACL_SIZE_INFORMATION, AclSizeInformation,
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
    use std::os::windows::io::AsRawHandle as _;

    const SYSTEM: &str = "S-1-5-18";
    const ADMINISTRATORS: &str = "S-1-5-32-544";
    const MAX_ACES: u32 = 32;
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
    let user = current_user_sid()?;
    let owner = sid_text(owner)?;
    if owner != user && owner.as_str() != ADMINISTRATORS {
        return Ok(Ownership::Foreign);
    }
    if dacl.is_null() {
        return Ok(Ownership::Exposed(Exposure));
    }
    let mut size = ACL_SIZE_INFORMATION::default();
    let bytes = u32::try_from(size_of::<ACL_SIZE_INFORMATION>()).map_err(io::Error::other)?;
    if unsafe { GetAclInformation(dacl, (&raw mut size).cast(), bytes, AclSizeInformation) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if size.AceCount > MAX_ACES {
        return Ok(Ownership::Exposed(Exposure));
    }
    for index in 0..size.AceCount {
        let mut raw = std::ptr::null_mut();
        if unsafe { GetAce(dacl, index, &raw mut raw) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let header: &ACE_HEADER = unsafe { &*raw.cast() };
        if u32::from(header.AceType) != ACCESS_ALLOWED_ACE_TYPE
            || usize::from(header.AceSize) < size_of::<ACCESS_ALLOWED_ACE>()
        {
            return Ok(Ownership::Exposed(Exposure));
        }
        let ace: &ACCESS_ALLOWED_ACE = unsafe { &*raw.cast() };
        let sid = sid_text((&raw const ace.SidStart).cast_mut().cast())?;
        if sid != user && sid.as_str() != SYSTEM {
            return Ok(Ownership::Exposed(Exposure));
        }
    }
    Ok(Ownership::Private)
}

pub(super) fn open_private_dir(path: &Path) -> io::Result<fs::File> {
    use std::os::windows::fs::OpenOptionsExt as _;

    let mut options = super::raw::options();
    options.access_mode(READ_CONTROL);
    options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS);
    options.open(path)
}

pub(super) fn create_private_dir(path: &Path) -> io::Result<()> {
    let user = current_user_sid()?;
    let descriptor = SecurityDescriptor::for_user(&user)?;
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
        match descriptor.create_dir(ancestor) {
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

    use windows_sys::Win32::Foundation::{ERROR_NOT_ENOUGH_MEMORY, SetLastError};
    use windows_sys::Win32::Security::{
        SECURITY_MAX_SID_SIZE, SID, SID_IDENTIFIER_AUTHORITY, TOKEN_USER,
    };

    use super::{converted_sid, token_user, validated_sid};

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
