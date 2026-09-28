#[cfg(unix)]
pub use std::os::unix::net::{UnixListener, UnixStream};

#[cfg(windows)]
pub use uds_windows::{UnixListener, UnixStream};

pub const SOCKET_PATH_LIMIT: usize = if cfg!(windows) { 107 } else { 103 };

use std::path::Path;

use crate::paths::Family;
use crate::snapshot::Mode;

pub const OS: &str = std::env::consts::OS;
pub const ARCH: &str = std::env::consts::ARCH;
pub const EXE_SUFFIX: &str = std::env::consts::EXE_SUFFIX;
pub const FAMILY: Family = if cfg!(windows) {
    Family::Windows
} else {
    Family::Unix
};
pub const LINKS: bool = FAMILY.links();
pub const RUNNING_EXECUTABLE: Option<&str> = if cfg!(target_os = "linux") {
    Some("/proc/self/exe")
} else {
    None
};
pub const MODES: bool = FAMILY.modes();

#[cfg(unix)]
#[must_use]
pub fn numeric_user_id() -> u32 {
    rustix::process::getuid().as_raw()
}

#[cfg(not(unix))]
#[must_use]
pub const fn numeric_user_id() -> u32 {
    0
}

#[must_use]
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn boot_identity() -> Option<String> {
    boot_identity_on_this_system()
}

#[must_use]
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub const fn boot_identity() -> Option<String> {
    boot_identity_on_this_system()
}

#[cfg(target_os = "linux")]
fn boot_identity_on_this_system() -> Option<String> {
    let Ok(identity) = crate::bounded::text_file(
        Path::new("/proc/sys/kernel/random/boot_id"),
        crate::bounded::BOOT_ID,
    ) else {
        return None;
    };
    let identity = identity.trim().to_owned();
    (!identity.is_empty()).then_some(identity)
}

#[cfg(target_os = "macos")]
fn boot_identity_on_this_system() -> Option<String> {
    let mut command = crate::spawn::Invocation::new(
        crate::template::Arg::literal("sysctl"),
        vec![
            crate::template::Arg::literal("-n"),
            crate::template::Arg::literal("kern.bootsessionuuid"),
        ],
    )
    .command();
    let Ok(output) =
        crate::bounded::command_output(&mut command, crate::bounded::Capture::BootIdentity)
    else {
        return None;
    };
    if !output.status.success() {
        return None;
    }
    let Ok(identity) = String::from_utf8(output.stdout) else {
        return None;
    };
    let identity = identity.trim().to_owned();
    (!identity.is_empty()).then_some(identity)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
const fn boot_identity_on_this_system() -> Option<String> {
    None
}

const fn bits(mode: Mode) -> u32 {
    mode.unix_bits()
}

const fn mode_from(bits: u32) -> Mode {
    if bits & 0o111 == 0 {
        Mode::Regular
    } else {
        Mode::Executable
    }
}

pub trait Moded {
    fn mode(&self) -> Mode;
}

impl Moded for std::fs::Metadata {
    fn mode(&self) -> Mode {
        mode_from(std_bits(self))
    }
}

impl Moded for cap_std::fs::Metadata {
    fn mode(&self) -> Mode {
        mode_from(cap_bits(self))
    }
}

#[cfg(unix)]
fn std_bits(meta: &std::fs::Metadata) -> u32 {
    std::os::unix::fs::PermissionsExt::mode(&meta.permissions())
}

#[cfg(not(unix))]
const fn std_bits(_meta: &std::fs::Metadata) -> u32 {
    Mode::Regular.unix_bits()
}

#[cfg(unix)]
fn cap_bits(meta: &cap_std::fs::Metadata) -> u32 {
    cap_std::fs::PermissionsExt::mode(&meta.permissions())
}

#[cfg(not(unix))]
const fn cap_bits(_meta: &cap_std::fs::Metadata) -> u32 {
    Mode::Regular.unix_bits()
}

pub fn create_as(options: &mut cap_std::fs::OpenOptions, mode: Mode) {
    create_with_bits(options, bits(mode));
}

#[cfg(unix)]
fn create_with_bits(options: &mut cap_std::fs::OpenOptions, bits: u32) {
    cap_std::fs::OpenOptionsExt::mode(options, bits);
}

#[cfg(not(unix))]
#[expect(
    clippy::missing_const_for_fn,
    reason = "kept a plain function so create_as has one signature on every system"
)]
fn create_with_bits(_options: &mut cap_std::fs::OpenOptions, _bits: u32) {}

pub fn link(dir: &cap_std::fs::Dir, target: &str, at: &Path) -> std::io::Result<()> {
    link_in(dir, target, at)
}

#[cfg(unix)]
fn link_in(dir: &cap_std::fs::Dir, target: &str, at: &Path) -> std::io::Result<()> {
    dir.symlink_contents(target, at)
}

#[cfg(not(unix))]
fn link_in(_dir: &cap_std::fs::Dir, target: &str, _at: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        format!("a symbolic link to {target} cannot be made on this system"),
    ))
}

#[cfg(test)]
pub fn make_link(target: &str, at: &Path) -> std::io::Result<()> {
    make_link_in(Path::new(target), at, LinkKind::File)
}

#[cfg(test)]
pub fn make_dir_link(target: &Path, at: &Path) -> std::io::Result<()> {
    make_link_in(target, at, LinkKind::Directory)
}

#[cfg(test)]
#[derive(Clone, Copy)]
enum LinkKind {
    File,
    Directory,
}

#[cfg(all(test, unix))]
fn make_link_in(target: &Path, at: &Path, _kind: LinkKind) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, at)
}

#[cfg(all(test, windows))]
fn make_link_in(target: &Path, at: &Path, kind: LinkKind) -> std::io::Result<()> {
    match kind {
        LinkKind::File => std::os::windows::fs::symlink_file(target, at),
        LinkKind::Directory => std::os::windows::fs::symlink_dir(target, at),
    }
}

#[cfg(all(test, not(any(unix, windows))))]
fn make_link_in(target: &Path, _at: &Path, _kind: LinkKind) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        format!(
            "a symbolic link to {} cannot be made on this system",
            target.display()
        ),
    ))
}

#[cfg(all(test, unix))]
#[expect(
    clippy::disallowed_methods,
    reason = "tests give fixture files the mode a project would have"
)]
pub fn set_mode(path: &Path, mode: Mode) -> std::io::Result<()> {
    std::fs::set_permissions(
        path,
        std::os::unix::fs::PermissionsExt::from_mode(bits(mode)),
    )
}

#[cfg(all(test, not(unix)))]
#[expect(
    clippy::unnecessary_wraps,
    reason = "shares the signature of systems that keep an executable bit"
)]
pub const fn set_mode(_path: &Path, _mode: Mode) -> std::io::Result<()> {
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ownership {
    Private,
    OtherOwner,
    Exposed(u32),
    LegacyAcl,
    ExposedAcl,
}

#[cfg(unix)]
pub fn ownership(file: &std::fs::File) -> std::io::Result<Ownership> {
    use std::os::unix::fs::MetadataExt;
    const GROUP_AND_OTHER_BITS: u32 = 6;
    let meta = file.metadata()?;
    if meta.uid() != rustix::process::geteuid().as_raw() {
        return Ok(Ownership::OtherOwner);
    }
    let mode = MetadataExt::mode(&meta) & 0o777;
    if mode.trailing_zeros() >= GROUP_AND_OTHER_BITS {
        Ok(Ownership::Private)
    } else {
        Ok(Ownership::Exposed(mode))
    }
}

#[cfg(windows)]
#[expect(
    unsafe_code,
    reason = "the ACL must be read from the open handle and each allowed SID inspected"
)]
pub fn ownership(file: &std::fs::File) -> std::io::Result<Ownership> {
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Foundation::{GENERIC_EXECUTE, GENERIC_READ};
    use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_FILE_OBJECT};
    use windows_sys::Win32::Security::{
        ACCESS_ALLOWED_ACE, ACE_HEADER, ACL_SIZE_INFORMATION, AclSizeInformation,
        DACL_SECURITY_INFORMATION, GetAce, GetAclInformation, OWNER_SECURITY_INFORMATION,
    };
    use windows_sys::Win32::Storage::FileSystem::{FILE_GENERIC_EXECUTE, FILE_GENERIC_READ};
    use windows_sys::Win32::System::SystemServices::ACCESS_ALLOWED_ACE_TYPE;

    const SYSTEM: &str = "S-1-5-18";
    const ADMINISTRATORS: &str = "S-1-5-32-544";
    const MAX_PRIVATE_ACES: u32 = 32;
    let read_only = FILE_GENERIC_READ | FILE_GENERIC_EXECUTE | GENERIC_READ | GENERIC_EXECUTE;

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
        return Err(std::io::Error::from_raw_os_error(status.cast_signed()));
    }
    let _descriptor = LocalSecurityDescriptor(raw);
    let user = current_user_sid()?;
    let owner = sid_text(owner)?;
    if owner != user && owner.as_str() != ADMINISTRATORS {
        return Ok(Ownership::OtherOwner);
    }
    if dacl.is_null() {
        return Ok(Ownership::ExposedAcl);
    }
    let mut size = ACL_SIZE_INFORMATION::default();
    let size_bytes =
        u32::try_from(size_of::<ACL_SIZE_INFORMATION>()).map_err(std::io::Error::other)?;
    if unsafe { GetAclInformation(dacl, (&raw mut size).cast(), size_bytes, AclSizeInformation) }
        == 0
    {
        return Err(std::io::Error::last_os_error());
    }
    if size.AceCount > MAX_PRIVATE_ACES {
        return Ok(Ownership::ExposedAcl);
    }
    let mut legacy_acl = false;
    for index in 0..size.AceCount {
        let mut entry = std::ptr::null_mut();
        if unsafe { GetAce(dacl, index, &raw mut entry) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let header: &ACE_HEADER = unsafe { &*entry.cast() };
        if u32::from(header.AceType) != ACCESS_ALLOWED_ACE_TYPE
            || usize::from(header.AceSize) < size_of::<ACCESS_ALLOWED_ACE>()
        {
            return Ok(Ownership::ExposedAcl);
        }
        let ace: &ACCESS_ALLOWED_ACE = unsafe { &*entry.cast() };
        let sid = sid_text((&raw const ace.SidStart).cast_mut().cast())?;
        if sid != user && sid.as_str() != SYSTEM {
            if sid.as_str() != ADMINISTRATORS && ace.Mask & !read_only != 0 {
                return Ok(Ownership::ExposedAcl);
            }
            legacy_acl = true;
        }
    }
    Ok(if legacy_acl {
        Ownership::LegacyAcl
    } else {
        Ownership::Private
    })
}

#[cfg(windows)]
pub fn open_dir_for_ownership(path: &Path) -> std::io::Result<std::fs::File> {
    open_for_acl_inspection(path)
}

#[cfg(windows)]
#[expect(
    clippy::disallowed_methods,
    reason = "ACL inspection and repair open the named object without following a reparse point"
)]
fn open_acl_handle(path: &Path, access: u32) -> std::io::Result<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt as _;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    };

    let mut options = std::fs::OpenOptions::new();
    options.access_mode(access);
    options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS);
    options.open(path)
}

#[cfg(windows)]
pub fn open_for_acl_inspection(path: &Path) -> std::io::Result<std::fs::File> {
    use windows_sys::Win32::Storage::FileSystem::READ_CONTROL;

    open_acl_handle(path, READ_CONTROL)
}

#[cfg(windows)]
pub fn open_for_acl_repair(path: &Path) -> std::io::Result<std::fs::File> {
    use windows_sys::Win32::Storage::FileSystem::{READ_CONTROL, WRITE_DAC};

    open_acl_handle(path, READ_CONTROL | WRITE_DAC)
}

#[cfg(windows)]
#[expect(
    unsafe_code,
    reason = "the same open handle receives a protected owner-and-SYSTEM DACL after its current ACL was classified"
)]
pub fn tighten_legacy_acl(file: &std::fs::File) -> std::io::Result<()> {
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Security::Authorization::{SE_FILE_OBJECT, SetSecurityInfo};
    use windows_sys::Win32::Security::{
        DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl, PROTECTED_DACL_SECURITY_INFORMATION,
    };

    match ownership(file)? {
        Ownership::Private => return Ok(()),
        Ownership::LegacyAcl => {}
        Ownership::OtherOwner | Ownership::Exposed(_) | Ownership::ExposedAcl => {
            return Err(std::io::ErrorKind::PermissionDenied.into());
        }
    }
    let descriptor = LocalSecurityDescriptor::for_current_user()?;
    let mut present = 0;
    let mut defaulted = 0;
    let mut dacl = std::ptr::null_mut();
    if unsafe {
        GetSecurityDescriptorDacl(
            descriptor.0,
            &raw mut present,
            &raw mut dacl,
            &raw mut defaulted,
        )
    } == 0
        || present == 0
        || dacl.is_null()
    {
        return Err(std::io::Error::other("the private DACL is unavailable"));
    }
    let status = unsafe {
        SetSecurityInfo(
            file.as_raw_handle(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            dacl,
            std::ptr::null_mut(),
        )
    };
    if status != 0 {
        return Err(std::io::Error::from_raw_os_error(status.cast_signed()));
    }
    match ownership(file)? {
        Ownership::Private => Ok(()),
        Ownership::OtherOwner
        | Ownership::Exposed(_)
        | Ownership::LegacyAcl
        | Ownership::ExposedAcl => Err(std::io::Error::other("the ACL remained exposed")),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LegacyAclError {
    #[error(transparent)]
    Io(#[from] crate::failure::IoFailure),
    #[error("{path} is not a regular state directory")]
    NotDirectory { path: std::path::PathBuf },
    #[error("{path} belongs to another user; refusing to migrate it")]
    Foreign { path: std::path::PathBuf },
    #[error("{path} cannot be retained during Windows state ACL migration")]
    UnsafeEntry { path: std::path::PathBuf },
    #[error(
        "the Windows state inventory exceeds its {entries}-entry or {path_bytes}-byte path budget at {path}"
    )]
    InventoryLimit {
        path: std::path::PathBuf,
        entries: usize,
        path_bytes: usize,
    },
    #[error(
        "{path} still has {remaining} legacy ACLs after migration; rerun `domyjob self secure-state`"
    )]
    Remaining {
        path: std::path::PathBuf,
        remaining: usize,
    },
    #[error("state ACL migration is available only on Windows")]
    Unsupported,
}

#[derive(Debug, Clone, Copy)]
pub struct LegacyAclReport {
    pub scanned: usize,
    pub candidates: usize,
}

#[cfg(not(windows))]
pub const fn secure_legacy_state(
    _root: &Path,
    _apply: bool,
) -> Result<LegacyAclReport, LegacyAclError> {
    Err(LegacyAclError::Unsupported)
}

#[cfg(windows)]
pub fn secure_legacy_state(root: &Path, apply: bool) -> Result<LegacyAclReport, LegacyAclError> {
    let plan = plan_legacy_acl(root)?;
    let report = LegacyAclReport {
        scanned: plan.scanned(),
        candidates: plan.candidates(),
    };
    if apply {
        plan.apply()?;
    }
    Ok(report)
}

#[cfg(windows)]
const LEGACY_INVENTORY_ENTRIES: usize = 150_000;
#[cfg(windows)]
const LEGACY_INVENTORY_PATH_BYTES: usize = 32 << 20;

#[cfg(windows)]
#[derive(Debug)]
pub struct LegacyAclPlan {
    root: std::path::PathBuf,
    candidates: Vec<std::path::PathBuf>,
    scanned: usize,
}

#[cfg(windows)]
impl LegacyAclPlan {
    #[must_use]
    pub const fn scanned(&self) -> usize {
        self.scanned
    }

    #[must_use]
    pub const fn candidates(&self) -> usize {
        self.candidates.len()
    }

    pub fn apply(self) -> Result<(), LegacyAclError> {
        for path in &self.candidates {
            let file = open_for_acl_repair(path).map_err(crate::failure::io("opening", path))?;
            let meta = file
                .metadata()
                .map_err(crate::failure::io("checking", path))?;
            if is_reparse_point(&meta) || !(meta.is_dir() || meta.is_file()) {
                return Err(LegacyAclError::UnsafeEntry { path: path.clone() });
            }
            tighten_legacy_acl(&file).map_err(crate::failure::io("securing", path))?;
        }
        if self.scanned == 0 {
            create_private_dir(&self.root).map_err(crate::failure::io("creating", &self.root))?;
        }
        let remaining = plan_legacy_acl(&self.root)?.candidates();
        if remaining != 0 {
            return Err(LegacyAclError::Remaining {
                path: self.root,
                remaining,
            });
        }
        Ok(())
    }
}

#[cfg(windows)]
fn legacy_inventory_limit(path: &Path) -> LegacyAclError {
    LegacyAclError::InventoryLimit {
        path: path.to_path_buf(),
        entries: LEGACY_INVENTORY_ENTRIES,
        path_bytes: LEGACY_INVENTORY_PATH_BYTES,
    }
}

#[cfg(windows)]
pub fn plan_legacy_acl(root: &Path) -> Result<LegacyAclPlan, LegacyAclError> {
    if let Err(error) = std::fs::symlink_metadata(root) {
        if error.kind() == std::io::ErrorKind::NotFound {
            return Ok(LegacyAclPlan {
                root: root.to_path_buf(),
                candidates: Vec::new(),
                scanned: 0,
            });
        }
        return Err(crate::failure::io("checking", root)(error).into());
    }
    let mut pending = vec![(root.to_path_buf(), false)];
    let mut entries = 1usize;
    let mut path_bytes = root.as_os_str().as_encoded_bytes().len();
    let mut candidates = Vec::new();
    let mut scanned = 0usize;
    while let Some((path, exposed_parent)) = pending.pop() {
        let meta =
            std::fs::symlink_metadata(&path).map_err(crate::failure::io("checking", &path))?;
        let reparse = is_reparse_point(&meta);
        if path == root && (!meta.is_dir() || reparse) {
            return Err(LegacyAclError::NotDirectory { path });
        }
        let file = open_for_acl_inspection(&path).map_err(crate::failure::io("opening", &path))?;
        let opened = file
            .metadata()
            .map_err(crate::failure::io("checking", &path))?;
        if reparse != is_reparse_point(&opened)
            || (!reparse
                && (meta.is_dir() != opened.is_dir() || meta.is_file() != opened.is_file()))
            || !(meta.is_dir() || meta.is_file() || reparse)
        {
            return Err(LegacyAclError::UnsafeEntry { path });
        }
        let owner = ownership(&file).map_err(crate::failure::io("checking", &path))?;
        let exposed = match owner {
            Ownership::Private => false,
            Ownership::LegacyAcl if !reparse => {
                candidates.push(path.clone());
                true
            }
            Ownership::OtherOwner => return Err(LegacyAclError::Foreign { path }),
            Ownership::LegacyAcl | Ownership::ExposedAcl | Ownership::Exposed(_) => {
                return Err(LegacyAclError::UnsafeEntry { path });
            }
        };
        scanned = scanned
            .checked_add(1)
            .ok_or_else(|| legacy_inventory_limit(&path))?;
        if reparse {
            if exposed_parent {
                return Err(LegacyAclError::UnsafeEntry { path });
            }
            continue;
        }
        if meta.is_dir() {
            for entry in std::fs::read_dir(&path).map_err(crate::failure::io("reading", &path))? {
                let entry = entry.map_err(crate::failure::io("reading", &path))?;
                let child = entry.path();
                entries = entries
                    .checked_add(1)
                    .ok_or_else(|| legacy_inventory_limit(&child))?;
                path_bytes = path_bytes
                    .checked_add(child.as_os_str().as_encoded_bytes().len())
                    .ok_or_else(|| legacy_inventory_limit(&child))?;
                if entries > LEGACY_INVENTORY_ENTRIES || path_bytes > LEGACY_INVENTORY_PATH_BYTES {
                    return Err(legacy_inventory_limit(&child));
                }
                pending.push((child, exposed));
            }
        }
    }
    Ok(LegacyAclPlan {
        root: root.to_path_buf(),
        candidates,
        scanned,
    })
}

#[cfg(unix)]
#[expect(
    clippy::disallowed_methods,
    reason = "the ownership check opens a directory handle without following a symlink"
)]
pub fn open_dir_for_ownership(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    no_follow(&mut options);
    options.open(path)
}

#[cfg(unix)]
#[must_use]
pub fn is_reparse_point(meta: &std::fs::Metadata) -> bool {
    meta.file_type().is_symlink()
}

#[cfg(windows)]
#[must_use]
pub fn is_reparse_point(meta: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt as _;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
    meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

pub fn create_private_dir(path: &Path) -> std::io::Result<()> {
    create_private_dir_in(path)
}

#[cfg(unix)]
fn create_private_dir_in(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
}

#[cfg(windows)]
#[expect(
    unsafe_code,
    reason = "Windows renders a binary SID from the ACL or process token"
)]
fn sid_text(sid: windows_sys::Win32::Security::PSID) -> std::io::Result<crate::domain::WindowsSid> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    let refuse = |detail: &str| std::io::Error::other(detail.to_owned());
    if sid.is_null() {
        return Err(refuse("the SID is missing"));
    }
    let mut wide: *mut u16 = std::ptr::null_mut();
    if unsafe { ConvertSidToStringSidW(sid, &raw mut wide) } == 0 || wide.is_null() {
        return Err(refuse("the SID cannot be rendered"));
    }
    let mut len = 0usize;
    while unsafe { *wide.add(len) } != 0 {
        len = len.saturating_add(1);
    }
    let text = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(wide, len) });
    unsafe { LocalFree(wide.cast()) };
    crate::domain::WindowsSid::try_from(text).map_err(|e| refuse(&e.to_string()))
}

#[cfg(windows)]
#[expect(
    unsafe_code,
    reason = "reading the current user's SID from the process token needs the token API"
)]
fn current_user_sid() -> std::io::Result<crate::domain::WindowsSid> {
    use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    let refuse = |detail: &str| std::io::Error::other(detail.to_owned());
    let mut token: HANDLE = std::ptr::null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token) } == 0 {
        return Err(refuse("the process token cannot be opened"));
    }
    let token = unsafe { OwnedHandle::from_raw_handle(token) };
    let mut needed = 0u32;
    unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            std::ptr::null_mut(),
            0,
            &raw mut needed,
        )
    };
    let Ok(capacity) = usize::try_from(needed) else {
        return Err(refuse("the token is too large"));
    };
    let mut buffer = vec![0u8; capacity];
    let asked = unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            buffer.as_mut_ptr().cast(),
            needed,
            &raw mut needed,
        )
    };
    if asked == 0 || buffer.len() < size_of::<TOKEN_USER>() {
        return Err(refuse("the token has no user"));
    }
    let user: TOKEN_USER = unsafe { std::ptr::read_unaligned(buffer.as_ptr().cast()) };
    sid_text(user.User.Sid)
}

#[cfg(windows)]
struct LocalSecurityDescriptor(windows_sys::Win32::Security::PSECURITY_DESCRIPTOR);

#[cfg(windows)]
impl LocalSecurityDescriptor {
    fn for_current_user() -> std::io::Result<Self> {
        let sid = current_user_sid()?;
        Self::from_sddl(&format!(
            "O:{}D:P(A;OICI;FA;;;{})(A;OICI;FA;;;SY)",
            sid.as_str(),
            sid.as_str()
        ))
    }

    #[expect(
        unsafe_code,
        reason = "Windows converts SDDL into a security descriptor owned by LocalFree"
    )]
    fn from_sddl(sddl: &str) -> std::io::Result<Self> {
        use windows_sys::Win32::Security::Authorization::{
            ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
        };

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
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self(descriptor))
    }

    #[expect(
        unsafe_code,
        reason = "CreateDirectoryW applies the owner-only ACL as the directory is created"
    )]
    fn create(&self, path: &Path) -> std::io::Result<()> {
        use std::os::windows::ffi::OsStrExt as _;
        use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
        use windows_sys::Win32::Storage::FileSystem::CreateDirectoryW;

        let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
        if wide.contains(&0) {
            return Err(std::io::ErrorKind::InvalidInput.into());
        }
        wide.push(0);
        let size =
            u32::try_from(size_of::<SECURITY_ATTRIBUTES>()).map_err(std::io::Error::other)?;
        let attributes = SECURITY_ATTRIBUTES {
            nLength: size,
            lpSecurityDescriptor: self.0,
            bInheritHandle: 0,
        };
        if unsafe { CreateDirectoryW(wide.as_ptr(), &raw const attributes) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(windows)]
impl Drop for LocalSecurityDescriptor {
    #[expect(
        unsafe_code,
        reason = "LocalFree releases the descriptor returned by the SDDL conversion API"
    )]
    fn drop(&mut self) {
        unsafe { windows_sys::Win32::Foundation::LocalFree(self.0) };
    }
}

#[cfg(windows)]
fn create_private_dir_in(path: &Path) -> std::io::Result<()> {
    let descriptor = LocalSecurityDescriptor::for_current_user()?;
    for ancestor in path.ancestors().collect::<Vec<_>>().into_iter().rev() {
        if ancestor.as_os_str().is_empty() {
            continue;
        }
        match std::fs::symlink_metadata(ancestor) {
            Ok(meta) if meta.is_dir() => continue,
            Ok(_) => return Err(std::io::ErrorKind::AlreadyExists.into()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        match descriptor.create(ancestor) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                if !std::fs::symlink_metadata(ancestor)?.is_dir() {
                    return Err(error);
                }
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[expect(
    clippy::disallowed_methods,
    reason = "the options every private state file is opened with"
)]
#[must_use]
pub fn private_options() -> std::fs::OpenOptions {
    let mut options = std::fs::OpenOptions::new();
    owner_only(&mut options);
    options
}

#[cfg(unix)]
pub fn no_follow(options: &mut std::fs::OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt as _;
    let flags = rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK;
    options.custom_flags(flags.bits().cast_signed());
}

#[cfg(windows)]
pub fn no_follow(options: &mut std::fs::OpenOptions) {
    use std::os::windows::fs::OpenOptionsExt as _;
    options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
}

#[cfg(unix)]
fn owner_only(options: &mut std::fs::OpenOptions) {
    std::os::unix::fs::OpenOptionsExt::mode(options, 0o600);
}

#[cfg(not(unix))]
const fn owner_only(_options: &mut std::fs::OpenOptions) {}

#[cfg(unix)]
pub fn sync_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::File::open(dir).and_then(|handle| handle.sync_all())
}

#[cfg(not(unix))]
#[expect(
    clippy::unnecessary_wraps,
    reason = "shares the signature of systems that can sync a directory"
)]
pub const fn sync_dir(_dir: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(unix)]
pub fn let_owner_change(perms: &mut std::fs::Permissions) {
    std::os::unix::fs::PermissionsExt::set_mode(perms, 0o700);
}

#[cfg(not(unix))]
pub fn let_owner_change(perms: &mut std::fs::Permissions) {
    perms.set_readonly(false);
}

#[cfg(test)]
#[expect(
    clippy::disallowed_methods,
    reason = "tests lock a path down the way a careless job would"
)]
pub fn lock_down(path: &Path) -> std::io::Result<()> {
    let mut perms = std::fs::symlink_metadata(path)?.permissions();
    forbid(&mut perms);
    std::fs::set_permissions(path, perms)
}

#[cfg(all(test, unix))]
fn forbid(perms: &mut std::fs::Permissions) {
    std::os::unix::fs::PermissionsExt::set_mode(perms, 0o500);
}

#[cfg(all(test, not(unix)))]
fn forbid(perms: &mut std::fs::Permissions) {
    perms.set_readonly(true);
}

#[cfg(all(test, unix))]
#[expect(
    clippy::disallowed_methods,
    reason = "tests widen a state file the way a careless user would"
)]
pub fn expose(path: &Path) -> std::io::Result<bool> {
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o644))?;
    Ok(true)
}

#[cfg(all(test, windows))]
pub fn expose(path: &Path) -> std::io::Result<bool> {
    expose_with_rights(path, "GR")
}

#[cfg(all(test, windows))]
#[expect(
    unsafe_code,
    reason = "the test creates a file with an Everyone ACE to verify the production ACL rejection"
)]
#[expect(
    clippy::disallowed_methods,
    reason = "the test replaces its private fixture file with one created under an exposed ACL"
)]
fn expose_with_rights(path: &Path, rights: &str) -> std::io::Result<bool> {
    use std::os::windows::ffi::OsStrExt as _;
    use std::os::windows::io::FromRawHandle as _;
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
    use windows_sys::Win32::Storage::FileSystem::{
        CREATE_NEW, CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE,
    };

    let sid = current_user_sid()?;
    let descriptor = LocalSecurityDescriptor::from_sddl(&format!(
        "O:{}D:P(A;OICI;FA;;;{})(A;OICI;FA;;;SY)(A;OICI;{rights};;;WD)",
        sid.as_str(),
        sid.as_str()
    ))?;
    if std::fs::symlink_metadata(path)?.is_dir() {
        std::fs::remove_dir(path)?;
        descriptor.create(path)?;
        return Ok(true);
    }
    std::fs::remove_file(path)?;
    let mut name: Vec<u16> = path.as_os_str().encode_wide().collect();
    name.push(0);
    let attributes = SECURITY_ATTRIBUTES {
        nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>()).map_err(std::io::Error::other)?,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: 0,
    };
    let handle = unsafe {
        CreateFileW(
            name.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            &raw const attributes,
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }
    drop(unsafe { std::fs::File::from_raw_handle(handle) });
    Ok(true)
}

#[cfg(test)]
pub(crate) struct ReadOnly(std::path::PathBuf);

#[cfg(test)]
#[expect(
    clippy::disallowed_methods,
    reason = "tests make a directory read-only and must give it back even when they panic"
)]
impl ReadOnly {
    pub(crate) fn make(dir: &Path) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        lock_down(dir)?;
        Ok(Self(dir.to_path_buf()))
    }
}

#[cfg(test)]
#[expect(
    clippy::disallowed_methods,
    reason = "tests make a directory read-only and must give it back even when they panic"
)]
impl Drop for ReadOnly {
    fn drop(&mut self) {
        let Ok(meta) = std::fs::symlink_metadata(&self.0) else {
            return;
        };
        let mut perms = meta.permissions();
        let_owner_change(&mut perms);
        match std::fs::set_permissions(&self.0, perms) {
            Ok(()) | Err(_) => {}
        }
    }
}

#[cfg(unix)]
#[must_use]
pub fn elevated() -> bool {
    rustix::process::geteuid().is_root()
}

#[cfg(windows)]
#[expect(
    unsafe_code,
    reason = "asking Windows whether this process token is elevated needs the token API"
)]
#[must_use]
pub fn elevated() -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Security::{
        GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    let mut token: HANDLE = std::ptr::null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token) } == 0 {
        return true;
    }
    let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
    let mut returned = 0u32;
    let Ok(size) = u32::try_from(size_of::<TOKEN_ELEVATION>()) else {
        return true;
    };
    let asked = unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            (&raw mut elevation).cast(),
            size,
            &raw mut returned,
        )
    };
    unsafe { CloseHandle(token) };
    asked == 0 || elevation.TokenIsElevated != 0
}

#[cfg(windows)]
pub fn push_arg(command: &mut std::process::Command, word: &str, cmd: bool) {
    use std::os::windows::process::CommandExt;
    if cmd {
        command.raw_arg(word);
    } else {
        command.arg(word);
    }
}

#[cfg(not(windows))]
pub fn push_arg(command: &mut std::process::Command, word: &str, _cmd: bool) {
    command.arg(word);
}

#[cfg(all(test, windows))]
mod windows_private_dir_tests {
    use std::os::windows::ffi::OsStrExt as _;

    use super::*;

    #[expect(
        clippy::unwrap_used,
        reason = "the shared fixture must fail its boundary test if legacy ACL setup fails"
    )]
    fn legacy_state(root: &Path) {
        crate::state_file::private_dir(root).unwrap();
        assert!(expose(root).unwrap());
    }

    #[test]
    #[expect(
        unsafe_code,
        reason = "the boundary test reads the ACL Windows assigned to a newly created directory"
    )]
    fn nested_private_directories_have_only_the_owner_and_system_in_their_acl() {
        use windows_sys::Win32::Foundation::LocalFree;
        use windows_sys::Win32::Security::Authorization::{
            ConvertSecurityDescriptorToStringSecurityDescriptorW, GetNamedSecurityInfoW,
            SDDL_REVISION_1, SE_FILE_OBJECT,
        };
        use windows_sys::Win32::Security::{DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION};

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("first path").join("実行").join("private");
        create_private_dir(&path).unwrap();
        crate::state_file::write_bytes(&path.join("file"), b"kept").unwrap();
        let information = OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION;
        let sid = current_user_sid().unwrap();
        for directory in path.ancestors().take(3) {
            let mut name: Vec<u16> = directory.as_os_str().encode_wide().collect();
            name.push(0);
            let mut raw = std::ptr::null_mut();
            let status = unsafe {
                GetNamedSecurityInfoW(
                    name.as_ptr(),
                    SE_FILE_OBJECT,
                    information,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &raw mut raw,
                )
            };
            assert_eq!(status, 0);
            let descriptor = LocalSecurityDescriptor(raw);
            let mut shown = std::ptr::null_mut();
            assert_ne!(
                unsafe {
                    ConvertSecurityDescriptorToStringSecurityDescriptorW(
                        descriptor.0,
                        SDDL_REVISION_1,
                        information,
                        &raw mut shown,
                        std::ptr::null_mut(),
                    )
                },
                0
            );
            let mut length = 0usize;
            while unsafe { *shown.add(length) } != 0 {
                length = length.saturating_add(1);
            }
            let acl =
                String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(shown, length) });
            unsafe { LocalFree(shown.cast()) };
            assert!(acl.contains(&format!("O:{}", sid.as_str())), "{acl}");
            assert!(acl.contains("D:P"), "{acl}");
            assert_eq!(acl.matches("(A;").count(), 2, "{acl}");
            assert!(acl.contains(sid.as_str()) && acl.contains(";;;SY"), "{acl}");
        }
    }

    #[test]
    fn existing_directory_with_everyone_acl_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("state");
        legacy_state(&dir);
        let handle = open_dir_for_ownership(&dir).unwrap();
        assert_eq!(ownership(&handle).unwrap(), Ownership::LegacyAcl);
        assert!(matches!(
            crate::state_file::private_dir(&dir),
            Err(crate::state_file::StateError::ExposedAcl { .. })
        ));
    }

    #[test]
    fn writable_everyone_acl_is_not_a_legacy_migration_candidate() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("state");
        crate::state_file::private_dir(&dir).unwrap();
        assert!(expose_with_rights(&dir, "GW").unwrap());
        let handle = open_dir_for_ownership(&dir).unwrap();
        assert_eq!(ownership(&handle).unwrap(), Ownership::ExposedAcl);
        let repair = open_for_acl_repair(&dir).unwrap();
        assert_eq!(
            tighten_legacy_acl(&repair).unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );
    }

    #[test]
    #[expect(
        clippy::disallowed_methods,
        reason = "the fixture must create legacy children that inherit the exposed ACL"
    )]
    fn readonly_legacy_acl_is_tightened_on_the_same_handle_and_propagates() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("state");
        legacy_state(&root);
        let child = root.join("jobs");
        std::fs::create_dir_all(&child).unwrap();
        let record = child.join("record");
        std::fs::write(&record, b"kept").unwrap();
        let root_handle = open_for_acl_repair(&root).unwrap();
        assert_eq!(ownership(&root_handle).unwrap(), Ownership::LegacyAcl);
        tighten_legacy_acl(&root_handle).unwrap();
        for path in [&root, &child, &record] {
            let handle = if path.is_dir() {
                open_dir_for_ownership(path).unwrap()
            } else {
                std::fs::File::open(path).unwrap()
            };
            assert_eq!(ownership(&handle).unwrap(), Ownership::Private);
        }
        assert_eq!(std::fs::read(record).unwrap(), b"kept");
    }

    #[test]
    #[expect(
        clippy::disallowed_methods,
        reason = "the fixture creates preexisting state files under an inherited legacy ACL"
    )]
    fn legacy_state_is_preflighted_and_secured_without_changing_data() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("state");
        legacy_state(&root);
        let jobs = root.join("jobs");
        std::fs::create_dir_all(&jobs).unwrap();
        let record = jobs.join("record");
        std::fs::write(&record, b"kept").unwrap();
        let private = root.join("v3");
        create_private_dir(&private).unwrap();
        crate::state_file::write_bytes(&private.join("private"), b"protected").unwrap();

        let plan = plan_legacy_acl(&root).unwrap();
        assert_eq!(plan.scanned(), 5);
        assert_eq!(plan.candidates(), 3);
        assert_eq!(
            ownership(&open_dir_for_ownership(&root).unwrap()).unwrap(),
            Ownership::LegacyAcl
        );
        plan.apply().unwrap();
        assert_eq!(plan_legacy_acl(&root).unwrap().candidates(), 0);
        assert_eq!(std::fs::read(record).unwrap(), b"kept");
        assert_eq!(
            std::fs::read(private.join("private")).unwrap(),
            b"protected"
        );
        crate::state_file::private_dir(&root).unwrap();
    }

    #[test]
    fn legacy_acl_repair_resumes_after_the_root_was_secured() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("state");
        legacy_state(&root);
        let root_handle = open_for_acl_repair(&root).unwrap();
        tighten_legacy_acl(&root_handle).unwrap();
        let child = root.join("jobs");
        crate::state_file::private_dir(&child).unwrap();
        assert!(expose(&child).unwrap());
        assert_eq!(
            ownership(&open_dir_for_ownership(&root).unwrap()).unwrap(),
            Ownership::Private
        );
        assert_eq!(
            ownership(&open_dir_for_ownership(&child).unwrap()).unwrap(),
            Ownership::LegacyAcl
        );
        let plan = plan_legacy_acl(&root).unwrap();
        assert_eq!(plan.candidates(), 1);
        plan.apply().unwrap();
        assert_eq!(plan_legacy_acl(&root).unwrap().candidates(), 0);
    }

    #[test]
    #[expect(
        clippy::disallowed_methods,
        reason = "the fixture creates a writable foreign ACE in preexisting state"
    )]
    fn legacy_state_preflight_rejects_writable_aces_before_any_change() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("state");
        legacy_state(&root);
        let record = root.join("record");
        std::fs::write(&record, b"kept").unwrap();
        assert!(expose_with_rights(&record, "GW").unwrap());
        std::fs::write(&record, b"kept").unwrap();
        assert!(matches!(
            plan_legacy_acl(&root),
            Err(LegacyAclError::UnsafeEntry { .. })
        ));
        assert_eq!(
            ownership(&open_dir_for_ownership(&root).unwrap()).unwrap(),
            Ownership::LegacyAcl
        );
        assert_eq!(std::fs::read(record).unwrap(), b"kept");
    }

    #[test]
    fn legacy_state_preflight_rejects_a_reparse_point_under_exposed_parent() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("state");
        legacy_state(&root);
        let outside = tmp.path().join("outside");
        crate::state_file::private_dir(&outside).unwrap();
        if make_dir_link(&outside, &root.join("link")).is_err() {
            return;
        }
        assert!(matches!(
            plan_legacy_acl(&root),
            Err(LegacyAclError::UnsafeEntry { .. })
        ));
        assert_eq!(
            ownership(&open_dir_for_ownership(&root).unwrap()).unwrap(),
            Ownership::LegacyAcl
        );
    }
}

#[cfg(all(test, unix))]
mod unix_source_stamp_tests {
    #[test]
    fn source_stamp_rejects_product_links_but_ignores_build_outputs() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("crates/domyjob/src/source.rs");
        crate::user_files::write(&source, b"fn main() {}\n").unwrap();
        let link = tmp.path().join("crates/domyjob/src/link.rs");
        std::os::unix::fs::symlink(&source, &link).unwrap();
        let error = crate::build_stamp::digest(tmp.path()).unwrap_err();
        assert!(error.to_string().contains("symbolic link"));

        crate::user_files::remove(&link).unwrap();
        std::os::unix::fs::symlink(&source, tmp.path().join("target")).unwrap();
        crate::build_stamp::digest(tmp.path()).unwrap();
    }
}

#[cfg(all(test, windows))]
mod windows_source_build_tests {
    use std::io::Write as _;
    use std::path::{Path, PathBuf};
    use std::process::{Output, Stdio};

    use crate::template::Arg;

    fn describe(output: &Output) -> String {
        format!(
            "status: {}; stdout: {}; stderr: {}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    }

    fn absent(path: &Path) -> bool {
        matches!(std::fs::metadata(path), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
    }

    #[expect(
        clippy::unwrap_used,
        reason = "unreadable fixture directories fail the test"
    )]
    fn source_is_clean(cache: &Path) -> bool {
        let transient_sources_absent = std::fs::read_dir(cache).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("source-")
        });
        transient_sources_absent && absent(&cache.join("build").join("source"))
    }

    #[expect(
        clippy::unwrap_used,
        reason = "fixture setup failures should fail the test immediately"
    )]
    fn run_build(
        main: &str,
    ) -> (
        tempfile::TempDir,
        PathBuf,
        crate::remote::TransferId,
        Output,
    ) {
        let temp = tempfile::tempdir().unwrap();
        let cache = temp.path().join(".cache").join("domyjob");
        let (transfer, output) = run_build_in_cache(&cache, main);
        (temp, cache, transfer, output)
    }

    #[expect(
        clippy::unwrap_used,
        reason = "fixture setup failures should fail the test immediately"
    )]
    fn run_build_in_cache(cache: &Path, main: &str) -> (crate::remote::TransferId, Output) {
        let mut archive = tar::Builder::new(Vec::new());
        for (name, contents) in [
            (
                "Cargo.toml",
                "[package]\nname = \"domyjob\"\nversion = \"0.0.0\"\nedition = \"2024\"\n[profile.remote]\ninherits = \"dev\"\n",
            ),
            (
                "Cargo.lock",
                "version = 4\n\n[[package]]\nname = \"domyjob\"\nversion = \"0.0.0\"\n",
            ),
            ("src/main.rs", main),
        ] {
            let mut header = tar::Header::new_gnu();
            header.set_size(contents.len().try_into().unwrap());
            header.set_mode(0o644);
            header.set_cksum();
            archive
                .append_data(&mut header, name, contents.as_bytes())
                .unwrap();
        }
        let archive = archive.into_inner().unwrap();
        let (script, transfer, payload) =
            crate::remote::windows_source_build_for_test(cache, &archive);
        let artifact = cache
            .join("build")
            .join("shared")
            .join("remote/domyjob.exe");
        crate::state_file::private_dir(artifact.parent().unwrap()).unwrap();
        crate::state_file::write_bytes(
            &cache.join("build").join("shared").join("CACHEDIR.TAG"),
            b"Signature: 8a477f597d28d172789f06886806bc55\n",
        )
        .unwrap();
        crate::state_file::write_bytes(&artifact, b"stale executable").unwrap();
        let mut child =
            crate::spawn::Invocation::new(Arg::literal("cmd"), vec![Arg::literal("/c"), script])
                .command()
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
        child.stdin.take().unwrap().write_all(&payload).unwrap();
        let output = child.wait_with_output().unwrap();
        (transfer, output)
    }

    #[expect(
        clippy::unwrap_used,
        reason = "fixture setup and cleanup failures should fail the test immediately"
    )]
    fn discard_staged_for_test(
        profile: &Path,
        staged: &Path,
        transfer: &crate::remote::TransferId,
    ) {
        crate::state_file::write_bytes(staged, b"abandoned stage").unwrap();
        let upload = profile.join(format!(
            "domyjob-{}-{}.b64",
            crate::protocol::build_key(),
            transfer.as_str()
        ));
        let source = profile.join(format!("domyjob-source-{}.b64", transfer.as_str()));
        crate::state_file::write_bytes(&upload, b"abandoned upload").unwrap();
        crate::state_file::write_bytes(&source, b"abandoned source").unwrap();
        let mut discard =
            crate::spawn::Invocation::from_words(crate::remote::discard_windows_argv(transfer))
                .unwrap()
                .command();
        discard.env("USERPROFILE", profile).env("TEMP", profile);
        let discarded = discard.output().unwrap();
        assert!(discarded.status.success(), "{}", describe(&discarded));
        assert!(absent(staged) && absent(&upload) && absent(&source));
    }

    #[test]
    fn failed_source_build_cannot_stage_an_old_executable() {
        let (_temp, cache, transfer, output) = run_build("compile_error!(\"build must fail\");\n");
        let errors = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{errors}");
        assert!(errors.contains("build must fail"), "{errors}");
        assert!(absent(&cache.join("bin").join(format!(
            "domyjob-{}.incoming-{}.exe",
            crate::protocol::build_key(),
            transfer.as_str()
        ))));
        assert!(source_is_clean(&cache));
    }

    #[test]
    fn successful_source_build_replaces_the_old_executable_and_cleans_source() {
        let (_temp, cache, transfer, output) = run_build("fn main() {}\n");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let staged = cache.join("bin").join(format!(
            "domyjob-{}.incoming-{}.exe",
            crate::protocol::build_key(),
            transfer.as_str()
        ));
        assert!(std::fs::metadata(staged).unwrap().len() > 1000);
        assert!(source_is_clean(&cache));
    }

    #[test]
    fn concurrent_source_builds_in_one_cache_stage_their_own_executables() {
        let temp = tempfile::tempdir().unwrap();
        let profile = temp.path().join("profile");
        crate::state_file::private_dir(&profile).unwrap();
        let cache = profile.join(".cache").join("domyjob");
        let ((first, first_output), (second, second_output)) = std::thread::scope(|scope| {
            let first =
                scope.spawn(|| run_build_in_cache(&cache, "fn main() { println!(\"first\"); }\n"));
            let second =
                scope.spawn(|| run_build_in_cache(&cache, "fn main() { println!(\"second\"); }\n"));
            (first.join().unwrap(), second.join().unwrap())
        });
        assert!(first_output.status.success(), "{}", describe(&first_output));
        assert!(
            second_output.status.success(),
            "{}",
            describe(&second_output)
        );
        assert_ne!(first, second);
        let run = |path: &Path| {
            let output =
                crate::spawn::Invocation::new(Arg::for_test(path.display().to_string()), vec![])
                    .command()
                    .output()
                    .unwrap();
            assert!(output.status.success());
            String::from_utf8(output.stdout).unwrap()
        };
        for (transfer, expected) in [(first, "first\n"), (second, "second\n")] {
            let staged = cache.join("bin").join(format!(
                "domyjob-{}.incoming-{}.exe",
                crate::protocol::build_key(),
                transfer.as_str()
            ));
            assert_eq!(run(&staged), expected);
            let mut promote = crate::spawn::Invocation::new(
                Arg::literal("cmd"),
                vec![
                    Arg::literal("/c"),
                    crate::remote::promote_windows_script(&transfer),
                ],
            )
            .command();
            promote.env("USERPROFILE", &profile);
            let result = promote.output().unwrap();
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            assert!(absent(&staged));
            let current = cache
                .join("bin")
                .join(format!("domyjob-{}.exe", crate::protocol::build_key()));
            assert_eq!(run(&current), expected);
            discard_staged_for_test(&profile, &staged, &transfer);
            assert_eq!(run(&current), expected);
        }
    }
}
