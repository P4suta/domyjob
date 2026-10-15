use std::io;
use std::os::fd::{AsRawFd as _, BorrowedFd};

use domyjob_core::terminal_input;
use rustix::event::{FdSetElement, Timespec};

#[expect(
    unsafe_code,
    reason = "The select set contains only this borrowed live FD for the duration of the bounded call"
)]
pub(super) fn ready(handle: BorrowedFd<'_>, timeout: &Timespec) -> io::Result<bool> {
    let fd = handle.as_raw_fd();
    let span = terminal_input::descriptor_span(fd).ok_or(io::ErrorKind::InvalidInput)?;
    let maximum = terminal_input::poll_nanos(terminal_input::MAX_POLL_MILLIS)
        .ok_or(io::ErrorKind::InvalidInput)?;
    if timeout.tv_sec != 0 || !(0..=maximum).contains(&timeout.tv_nsec) {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    let mut descriptors =
        vec![FdSetElement::default(); rustix::event::fd_set_num_elements(1, span)];
    rustix::event::fd_set_insert(&mut descriptors, fd);
    match unsafe { rustix::event::select(span, Some(&mut descriptors), None, None, Some(timeout)) }
    {
        Ok(count) => Ok(count != 0),
        Err(error) => Err(error.into()),
    }
}
