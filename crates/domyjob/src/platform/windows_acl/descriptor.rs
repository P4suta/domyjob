#![expect(
    unsafe_code,
    reason = "This leaf owns checked Windows security allocations and their borrowed views"
)]

use std::ffi::c_void;
use std::fs;
use std::io;
use std::marker::PhantomData;
use std::os::windows::fs::OpenOptionsExt as _;
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
use std::path::Path;
use std::ptr::NonNull;

use windows_sys::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, INVALID_HANDLE_VALUE, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo,
    SDDL_REVISION_1, SE_FILE_OBJECT,
};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, ACL_SIZE_INFORMATION, AclSizeInformation,
    DACL_SECURITY_INFORMATION, GetAce, GetAclInformation, GetTokenInformation,
    OWNER_SECURITY_INFORMATION, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateDirectoryW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, READ_CONTROL,
};
use windows_sys::Win32::System::SystemServices::ACCESS_ALLOWED_ACE_TYPE;
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

#[derive(Debug, Clone, Copy)]
pub(super) struct Success(());

fn checked_bool(status: i32) -> io::Result<Success> {
    if status == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(Success(()))
    }
}

fn present<T>(value: *mut T) -> io::Result<NonNull<T>> {
    NonNull::new(value)
        .ok_or_else(|| io::Error::other("Windows returned a missing security output"))
}

#[derive(Debug)]
struct LocalAllocation(NonNull<c_void>);

impl Drop for LocalAllocation {
    fn drop(&mut self) {
        unsafe { LocalFree(self.0.as_ptr()) };
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Sid(String);

impl Sid {
    pub(super) fn as_str(&self) -> &str {
        &self.0
    }
}

pub(super) fn validated_sid(text: String) -> io::Result<Sid> {
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

#[derive(Debug, Clone, Copy)]
pub(super) struct SidRef<'owner> {
    raw: NonNull<c_void>,
    owner: PhantomData<&'owner ()>,
}

impl SidRef<'_> {
    pub(super) fn text(self) -> io::Result<Sid> {
        sid_text(self)
    }
}

pub(super) fn sid_text(sid: SidRef<'_>) -> io::Result<Sid> {
    let mut wide = std::ptr::null_mut();
    checked_bool(unsafe { ConvertSidToStringSidW(sid.raw.as_ptr(), &raw mut wide) })?;
    let allocation = LocalAllocation(present(wide)?.cast());
    let text = allocation.0.cast::<u16>();
    let mut length = 0_usize;
    while length < 256 && unsafe { *text.as_ptr().add(length) } != 0 {
        length = length.saturating_add(1);
    }
    if length == 256 {
        return Err(io::Error::other("the SID is too long"));
    }
    let text = String::from_utf16(unsafe { std::slice::from_raw_parts(text.as_ptr(), length) })
        .map_err(io::Error::other)?;
    validated_sid(text)
}

#[derive(Debug)]
struct TokenUserBuffer(Vec<u8>);

impl TokenUserBuffer {
    fn read(token: &OwnedHandle) -> io::Result<Self> {
        let mut needed = 0;
        let status = unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                TokenUser,
                std::ptr::null_mut(),
                0,
                &raw mut needed,
            )
        };
        let error = io::Error::last_os_error();
        if status == 0 && error.raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER.cast_signed()) {
            return Err(error);
        }
        if needed == 0 || needed > 4096 {
            return Err(io::Error::other(
                "the token user is unavailable or too large",
            ));
        }
        let size = usize::try_from(needed).map_err(io::Error::other)?;
        let mut buffer = vec![0_u8; size];
        checked_bool(unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                TokenUser,
                buffer.as_mut_ptr().cast(),
                needed,
                &raw mut needed,
            )
        })?;
        if buffer.len() < size_of::<TOKEN_USER>() {
            return Err(io::Error::other("the token has no user"));
        }
        Ok(Self(buffer))
    }

    fn user(&self) -> io::Result<SidRef<'_>> {
        let user: TOKEN_USER = unsafe { std::ptr::read_unaligned(self.0.as_ptr().cast()) };
        let raw = present(user.User.Sid)?;
        let start = self.0.as_ptr().addr();
        let end = start.saturating_add(self.0.len());
        if raw.as_ptr().addr() < start || raw.as_ptr().addr() >= end {
            return Err(io::Error::other("the token SID is outside its buffer"));
        }
        Ok(SidRef {
            raw,
            owner: PhantomData,
        })
    }
}

pub(super) fn current_user_sid() -> io::Result<Sid> {
    let mut raw = std::ptr::null_mut();
    checked_bool(unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut raw) })?;
    let handle = present(raw)?;
    if handle.as_ptr() == INVALID_HANDLE_VALUE {
        return Err(io::Error::other("Windows returned an invalid token handle"));
    }
    let token = unsafe { OwnedHandle::from_raw_handle(handle.as_ptr()) };
    TokenUserBuffer::read(&token)?.user()?.text()
}

#[derive(Debug)]
pub(super) struct SecurityDescriptor {
    allocation: LocalAllocation,
}

impl SecurityDescriptor {
    pub(super) fn for_user(user: &Sid) -> io::Result<Self> {
        let sddl = format!(
            "O:{}D:P(A;OICI;FA;;;{})(A;OICI;FA;;;SY)",
            user.as_str(),
            user.as_str()
        );
        Self::from_sddl(&sddl)
    }

    fn from_sddl(sddl: &str) -> io::Result<Self> {
        let wide: Vec<u16> = sddl.encode_utf16().chain(std::iter::once(0)).collect();
        let mut raw = std::ptr::null_mut();
        checked_bool(unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                wide.as_ptr(),
                SDDL_REVISION_1,
                &raw mut raw,
                std::ptr::null_mut(),
            )
        })?;
        Ok(Self {
            allocation: LocalAllocation(present(raw)?),
        })
    }

    pub(super) fn create_dir(&self, path: &Path) -> io::Result<Success> {
        use std::os::windows::ffi::OsStrExt as _;

        let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
        if wide.contains(&0) {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        wide.push(0);
        let size = u32::try_from(size_of::<SECURITY_ATTRIBUTES>()).map_err(io::Error::other)?;
        let attributes = SECURITY_ATTRIBUTES {
            nLength: size,
            lpSecurityDescriptor: self.allocation.0.as_ptr(),
            bInheritHandle: 0,
        };
        checked_bool(unsafe { CreateDirectoryW(wide.as_ptr(), &raw const attributes) })
    }
}

#[derive(Debug)]
pub(super) struct FileSecurity {
    _descriptor: SecurityDescriptor,
    owner: NonNull<c_void>,
    dacl: Option<NonNull<ACL>>,
}

impl FileSecurity {
    pub(super) fn read(file: &fs::File) -> io::Result<Self> {
        let mut owner = std::ptr::null_mut();
        let mut dacl = std::ptr::null_mut();
        let mut raw = std::ptr::null_mut();
        let status = unsafe {
            GetSecurityInfo(
                file.as_raw_handle(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                &raw mut owner,
                std::ptr::null_mut(),
                &raw mut dacl,
                std::ptr::null_mut(),
                &raw mut raw,
            )
        };
        if status != 0 {
            return Err(io::Error::from_raw_os_error(status.cast_signed()));
        }
        let descriptor = SecurityDescriptor {
            allocation: LocalAllocation(present(raw)?),
        };
        Ok(Self {
            _descriptor: descriptor,
            owner: present(owner)?,
            dacl: NonNull::new(dacl),
        })
    }

    pub(super) const fn owner(&self) -> SidRef<'_> {
        SidRef {
            raw: self.owner,
            owner: PhantomData,
        }
    }

    pub(super) const fn acl(&self) -> Option<Acl<'_>> {
        match self.dacl {
            Some(raw) => Some(Acl {
                raw,
                descriptor: PhantomData,
            }),
            None => None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) struct Acl<'descriptor> {
    raw: NonNull<ACL>,
    descriptor: PhantomData<&'descriptor SecurityDescriptor>,
}

#[derive(Debug, Clone, Copy)]
pub(super) enum Ace<'acl> {
    Unsupported,
    Allowed(SidRef<'acl>),
}

impl Acl<'_> {
    pub(super) fn ace_count(&self) -> io::Result<u32> {
        let mut size = ACL_SIZE_INFORMATION::default();
        let bytes = u32::try_from(size_of::<ACL_SIZE_INFORMATION>()).map_err(io::Error::other)?;
        checked_bool(unsafe {
            GetAclInformation(
                self.raw.as_ptr(),
                (&raw mut size).cast(),
                bytes,
                AclSizeInformation,
            )
        })?;
        Ok(size.AceCount)
    }

    pub(super) fn ace(&self, index: u32) -> io::Result<Ace<'_>> {
        let mut raw = std::ptr::null_mut();
        checked_bool(unsafe { GetAce(self.raw.as_ptr(), index, &raw mut raw) })?;
        let raw = present(raw)?;
        let header: ACE_HEADER = unsafe { std::ptr::read_unaligned(raw.as_ptr().cast()) };
        if unsupported_ace(header) {
            return Ok(Ace::Unsupported);
        }
        let ace: &ACCESS_ALLOWED_ACE = unsafe { &*raw.as_ptr().cast() };
        let sid = present((&raw const ace.SidStart).cast_mut().cast())?;
        Ok(Ace::Allowed(SidRef {
            raw: sid,
            owner: PhantomData,
        }))
    }
}

fn unsupported_ace(header: ACE_HEADER) -> bool {
    u32::from(header.AceType) != ACCESS_ALLOWED_ACE_TYPE
        || usize::from(header.AceSize) < size_of::<ACCESS_ALLOWED_ACE>()
}

pub(super) fn no_follow(options: &mut fs::OpenOptions) {
    options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
}

pub(super) fn open_private_dir(path: &Path, options: &mut fs::OpenOptions) -> io::Result<fs::File> {
    options.access_mode(READ_CONTROL);
    options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS);
    options.open(path)
}

#[cfg(test)]
mod tests {
    use std::os::windows::fs::OpenOptionsExt as _;
    use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};

    use windows_sys::Win32::Security::{
        ACCESS_ALLOWED_ACE, ACE_HEADER, GetSecurityDescriptorDacl, TOKEN_QUERY,
    };
    use windows_sys::Win32::System::SystemServices::ACCESS_ALLOWED_ACE_TYPE;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, OpenProcessToken, PROCESS_SYNCHRONIZE,
    };

    use super::{
        FileSecurity, SecurityDescriptor, checked_bool, current_user_sid, present, unsupported_ace,
    };

    #[test]
    fn token_outputs_require_success_and_a_present_owned_buffer() {
        let process = present(unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, std::process::id()) })
            .expect("wait-only process handle");
        let process = unsafe { OwnedHandle::from_raw_handle(process.as_ptr()) };
        let mut token = std::ptr::null_mut();
        let error = checked_bool(unsafe {
            OpenProcessToken(process.as_raw_handle(), TOKEN_QUERY, &raw mut token)
        })
        .expect_err("query rights are required");
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(token.is_null());
        let missing = present::<std::ffi::c_void>(std::ptr::null_mut())
            .expect_err("successful outputs still require presence");
        assert_eq!(
            missing.to_string(),
            "Windows returned a missing security output"
        );
        let sid = current_user_sid().expect("owned token user and allocated SID text");
        assert!(sid.as_str().starts_with("S-1-"));
    }

    #[test]
    fn direct_status_and_nullable_acl_keep_their_distinct_contracts() {
        let root = tempfile::tempdir().expect("owned directory");
        let file = tempfile::NamedTempFile::new_in(root.path()).expect("owned file");
        let mut options = crate::platform::raw::options();
        let denied = options
            .access_mode(0)
            .open(file.path())
            .expect("metadata-only file handle");
        let error = FileSecurity::read(&denied).expect_err("READ_CONTROL is required");
        assert_eq!(error.raw_os_error(), Some(5));
        let user = current_user_sid().expect("current user");
        let descriptor = SecurityDescriptor::for_user(&user).expect("owned descriptor");
        let path = root.path().join("private");
        descriptor.create_dir(&path).expect("private directory");
        let directory = super::open_private_dir(&path, &mut crate::platform::raw::options())
            .expect("security-readable directory");
        let security = FileSecurity::read(&directory).expect("owned file security");
        assert_eq!(security.owner().text().expect("borrowed owner SID"), user);
        let acl = security.acl().expect("private DACL");
        assert_eq!(acl.ace_count().expect("ACL count"), 2);
        for index in 0..2 {
            let super::Ace::Allowed(sid) = acl.ace(index).expect("borrowed ACE") else {
                panic!("a private descriptor only contains allowed ACEs");
            };
            sid.text().expect("borrowed ACE SID");
        }
        let unrestricted = SecurityDescriptor::from_sddl("O:S-1-5-18D:NO_ACCESS_CONTROL")
            .expect("owned descriptor with a NULL DACL");
        let mut present_acl = 0;
        let mut dacl = std::ptr::null_mut();
        let mut defaulted = 0;
        checked_bool(unsafe {
            GetSecurityDescriptorDacl(
                unrestricted.allocation.0.as_ptr(),
                &raw mut present_acl,
                &raw mut dacl,
                &raw mut defaulted,
            )
        })
        .expect("valid nullable DACL query");
        assert_ne!(present_acl, 0);
        assert!(dacl.is_null());
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
}
