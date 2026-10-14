use std::io;
use std::os::windows::io::{AsHandle as _, AsRawHandle as _};

use windows_spawn::Job;
use windows_sys::Win32::System::JobObjects::{
    JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JobObjectBasicAccountingInformation,
    QueryInformationJobObject,
};

#[expect(
    unsafe_code,
    reason = "the retained validated Job borrows its handle while Win32 fills the exact initialized accounting buffer"
)]
pub(super) fn active(job: &Job) -> io::Result<u32> {
    let mut information = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
    let size = u32::try_from(size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>())
        .map_err(io::Error::other)?;
    let mut returned = 0;
    let success = unsafe {
        QueryInformationJobObject(
            job.as_handle().as_raw_handle(),
            JobObjectBasicAccountingInformation,
            (&raw mut information).cast(),
            size,
            &raw mut returned,
        )
    };
    if success == 0 {
        return Err(io::Error::last_os_error());
    }
    if returned != size {
        return Err(io::Error::other(
            "Job accounting returned an unexpected buffer length",
        ));
    }
    Ok(information.ActiveProcesses)
}
