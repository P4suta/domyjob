#![expect(
    unsafe_code,
    reason = "Toolhelp is how Windows itself reports a process's parent"
)]

use std::io;

use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};

pub(super) fn parent_of(pid: u32) -> io::Result<u32> {
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let size = u32::try_from(size_of::<PROCESSENTRY32W>()).map_err(io::Error::other)?;
    let mut entry = PROCESSENTRY32W {
        dwSize: size,
        ..PROCESSENTRY32W::default()
    };
    let mut parent = None;
    let mut more = unsafe { Process32FirstW(snapshot, &raw mut entry) } != 0;
    while more {
        if entry.th32ProcessID == pid {
            parent = Some(entry.th32ParentProcessID);
            break;
        }
        more = unsafe { Process32NextW(snapshot, &raw mut entry) } != 0;
    }
    unsafe { CloseHandle(snapshot) };
    parent.ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("no process {pid}")))
}
