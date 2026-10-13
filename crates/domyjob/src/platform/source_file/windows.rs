#![expect(
    unsafe_code,
    reason = "this SDK leaf opens one validated filename relative to a borrowed live directory; all UTF-16 and result buffers remain live through NtCreateFile, a successful handle transfers once to File, and reparse and non-disk handles are rejected before reads"
)]

use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::os::windows::ffi::OsStrExt as _;
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _};
use std::ptr;

use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows_sys::Wdk::Storage::FileSystem::{
    FILE_DIRECTORY_FILE, FILE_NON_DIRECTORY_FILE, FILE_OPEN, FILE_OPEN_REPARSE_POINT,
    FILE_SYNCHRONOUS_IO_NONALERT, NtCreateFile,
};
use windows_sys::Win32::Foundation::{OBJ_CASE_INSENSITIVE, RtlNtStatusToDosError, UNICODE_STRING};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_GENERIC_READ, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_TYPE_DISK,
    GetFileType,
};
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

pub(super) fn open(parent: &File, name: &OsStr) -> io::Result<File> {
    create(parent, name, FILE_NON_DIRECTORY_FILE)
}

pub(super) fn directory(parent: &File, name: &OsStr) -> io::Result<File> {
    create(parent, name, FILE_DIRECTORY_FILE)
}

#[cfg(test)]
pub(super) fn rename_blocked(error: &io::Error) -> bool {
    use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_SHARING_VIOLATION};
    let Some(code) = error.raw_os_error() else {
        return false;
    };
    let Ok(code) = u32::try_from(code) else {
        return false;
    };
    matches!(code, ERROR_ACCESS_DENIED | ERROR_SHARING_VIOLATION)
}

fn create(parent: &File, name: &OsStr, kind: u32) -> io::Result<File> {
    let mut wide: Vec<u16> = name.encode_wide().collect();
    if wide.is_empty() || wide.contains(&0) {
        return Err(io::Error::other(
            "the source filename is empty or contains NUL",
        ));
    }
    let byte_count = wide
        .len()
        .checked_mul(size_of::<u16>())
        .ok_or_else(|| io::Error::other("the source filename exceeds the SDK limit"))?;
    let bytes = u16::try_from(byte_count)
        .map_err(|_overflow| io::Error::other("the source filename exceeds the SDK limit"))?;
    let unicode = UNICODE_STRING {
        Length: bytes,
        MaximumLength: bytes,
        Buffer: wide.as_mut_ptr(),
    };
    let attributes = OBJECT_ATTRIBUTES {
        Length: u32::try_from(size_of::<OBJECT_ATTRIBUTES>())
            .map_err(|_overflow| io::Error::other("the SDK attributes exceed their size field"))?,
        RootDirectory: parent.as_raw_handle(),
        ObjectName: &raw const unicode,
        Attributes: OBJ_CASE_INSENSITIVE,
        SecurityDescriptor: ptr::null(),
        SecurityQualityOfService: ptr::null(),
    };
    let mut handle = ptr::null_mut();
    let mut status = IO_STATUS_BLOCK::default();
    let result = unsafe {
        NtCreateFile(
            &raw mut handle,
            FILE_GENERIC_READ,
            &raw const attributes,
            &raw mut status,
            ptr::null(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            FILE_OPEN,
            kind | FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
            ptr::null(),
            0,
        )
    };
    if result < 0 {
        let code = unsafe { RtlNtStatusToDosError(result) };
        return Err(io::Error::from_raw_os_error(i32::try_from(code).map_err(
            |_overflow| io::Error::other("the SDK error code is out of range"),
        )?));
    }
    if handle.is_null() {
        return Err(io::Error::other("the SDK returned an empty source handle"));
    }
    let file = unsafe { File::from_raw_handle(handle) };
    if unsafe { GetFileType(file.as_raw_handle()) } != FILE_TYPE_DISK {
        return Err(io::Error::other("the source handle is not a disk file"));
    }
    Ok(file)
}
