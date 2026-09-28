#![expect(
    unsafe_code,
    reason = "Windows ACL and token APIs require raw handles, pointers, and ownership transfer"
)]

use std::fs;
use std::io;
use std::path::Path;

use windows_sys::Win32::Foundation::{GENERIC_EXECUTE, GENERIC_READ, LocalFree};
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
    CreateDirectoryW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_GENERIC_EXECUTE, FILE_GENERIC_READ, READ_CONTROL,
};
use windows_sys::Win32::System::SystemServices::ACCESS_ALLOWED_ACE_TYPE;
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use super::Ownership;

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
    if sid.is_null() {
        return Err(io::Error::other("the SID is missing"));
    }
    let mut wide: *mut u16 = std::ptr::null_mut();
    if unsafe { ConvertSidToStringSidW(sid, &raw mut wide) } == 0 || wide.is_null() {
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
    unsafe { LocalFree(wide.cast()) };
    result
}

fn current_user_sid() -> io::Result<Sid> {
    use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};

    let mut raw = std::ptr::null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut raw) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let token = unsafe { OwnedHandle::from_raw_handle(raw) };
    let mut needed = 0_u32;
    unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            std::ptr::null_mut(),
            0,
            &raw mut needed,
        )
    };
    if needed == 0 || needed > 4096 {
        return Err(io::Error::other(
            "the token user is unavailable or too large",
        ));
    }
    let size = usize::try_from(needed).map_err(io::Error::other)?;
    let mut buffer = vec![0_u8; size];
    if unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            buffer.as_mut_ptr().cast(),
            needed,
            &raw mut needed,
        )
    } == 0
        || buffer.len() < size_of::<TOKEN_USER>()
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
    let read_only = FILE_GENERIC_READ | FILE_GENERIC_EXECUTE | GENERIC_READ | GENERIC_EXECUTE;
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
        return Ok(Ownership::ExposedAcl);
    }
    let mut size = ACL_SIZE_INFORMATION::default();
    let bytes = u32::try_from(size_of::<ACL_SIZE_INFORMATION>()).map_err(io::Error::other)?;
    if unsafe { GetAclInformation(dacl, (&raw mut size).cast(), bytes, AclSizeInformation) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if size.AceCount > MAX_ACES {
        return Ok(Ownership::ExposedAcl);
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
            return Ok(Ownership::ExposedAcl);
        }
        let ace: &ACCESS_ALLOWED_ACE = unsafe { &*raw.cast() };
        let sid = sid_text((&raw const ace.SidStart).cast_mut().cast())?;
        if sid != user && sid.as_str() != SYSTEM {
            if sid.as_str() != ADMINISTRATORS && ace.Mask & !read_only != 0 {
                return Ok(Ownership::ExposedAcl);
            }
            return Ok(Ownership::ExposedAcl);
        }
    }
    Ok(Ownership::Private)
}

#[expect(
    clippy::disallowed_methods,
    reason = "ACL inspection opens a handle without following a reparse point"
)]
pub(super) fn open_private_dir(path: &Path) -> io::Result<fs::File> {
    use std::os::windows::fs::OpenOptionsExt as _;

    let mut options = fs::OpenOptions::new();
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
    use super::validated_sid;

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
