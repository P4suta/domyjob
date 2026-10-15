use std::collections::VecDeque;
use std::io;
use std::os::windows::io::{AsHandle as _, BorrowedHandle, OwnedHandle};

use domyjob_core::terminal_input::{self, KeyEvent, Utf16Decoder};

mod kernel;

const INPUT_BATCH: usize = 32;
#[cfg(test)]
const MAX_POLL_MILLIS: u64 = terminal_input::MAX_POLL_MILLIS;

#[derive(Debug)]
struct Mode {
    handle: OwnedHandle,
    original: u32,
}

impl Mode {
    fn capture(handle: OwnedHandle) -> io::Result<Self> {
        let original = kernel::mode(handle.as_handle())?;
        Ok(Self { handle, original })
    }

    fn restore(&self) -> io::Result<()> {
        self.restore_using(kernel::set_mode)
    }

    fn setup_using(
        &self,
        wanted: u32,
        set: impl FnOnce(BorrowedHandle<'_>, u32) -> io::Result<()>,
    ) -> io::Result<()> {
        set(self.handle.as_handle(), wanted)
    }

    fn restore_using(
        &self,
        set: impl FnOnce(BorrowedHandle<'_>, u32) -> io::Result<()>,
    ) -> io::Result<()> {
        set(self.handle.as_handle(), self.original)
    }
}

#[derive(Debug)]
pub(crate) struct Input {
    mode: Mode,
    pending: VecDeque<KeyEvent>,
    decoder: Utf16Decoder,
}

impl Input {
    pub(super) fn new(input: &io::Stdin) -> io::Result<Self> {
        Ok(Self {
            mode: Mode::capture(input.as_handle().try_clone_to_owned()?)?,
            pending: VecDeque::new(),
            decoder: Utf16Decoder::default(),
        })
    }

    pub(super) fn setup(&self) -> io::Result<()> {
        self.mode.setup_using(
            terminal_input::windows_input_mode(self.mode.original),
            kernel::set_mode,
        )
    }

    pub(super) fn restore(&self) -> io::Result<()> {
        self.mode.restore()
    }

    pub(crate) fn next(&mut self, millis: u64) -> io::Result<Option<KeyEvent>> {
        self.next_using(millis, kernel::records)
    }

    fn next_using(
        &mut self,
        millis: u64,
        read: impl FnOnce(BorrowedHandle<'_>, u32) -> io::Result<Vec<kernel::Record>>,
    ) -> io::Result<Option<KeyEvent>> {
        self.next_observed(millis, read, |_record| {})
    }

    fn next_observed(
        &mut self,
        millis: u64,
        read: impl FnOnce(BorrowedHandle<'_>, u32) -> io::Result<Vec<kernel::Record>>,
        mut observed: impl FnMut(&kernel::Record),
    ) -> io::Result<Option<KeyEvent>> {
        if terminal_input::poll_nanos(millis).is_none() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the console poll exceeds its deadline budget",
            ));
        }
        if let Some(key) = self.pending.pop_front() {
            return Ok(Some(key));
        }
        let millis = u32::try_from(millis).map_err(io::Error::other)?;
        let records = read(self.mode.handle.as_handle(), millis)?;
        if records.len() > INPUT_BATCH {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "the console returned an oversized input batch",
            ));
        }
        for record in records {
            observed(&record);
            match record {
                kernel::Record::Key(key) => {
                    if let Some(key) = self.decoder.windows_key(key) {
                        self.pending.push_back(key);
                    }
                }
                kernel::Record::Resize | kernel::Record::Other => {}
            }
        }
        Ok(self.pending.pop_front())
    }
}

#[derive(Debug)]
pub(crate) struct Display {
    mode: Mode,
}

impl Display {
    pub(super) fn new(output: &io::Stdout) -> io::Result<Self> {
        Ok(Self {
            mode: Mode::capture(output.as_handle().try_clone_to_owned()?)?,
        })
    }

    pub(crate) fn frame_size(&self) -> io::Result<(u16, u16)> {
        let [left, top, right, bottom] = kernel::window(self.mode.handle.as_handle())?;
        terminal_input::console_viewport(left, top, right, bottom)
            .map(|viewport| (viewport.width, viewport.height))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "the console viewport is malformed",
                )
            })
    }
}

impl super::DisplayModes for Display {
    fn setup(&self) -> io::Result<()> {
        self.mode.setup_using(
            terminal_input::windows_output_mode(self.mode.original),
            kernel::set_mode,
        )
    }

    fn restore(&self) -> io::Result<()> {
        self.mode.restore()
    }
}

#[cfg(test)]
mod fixture;
#[cfg(test)]
mod tests;
