use std::io;
use std::os::fd::{AsFd as _, OwnedFd};

use domyjob_core::terminal_input::{self, Decoded, Decoder, KeyEvent};
use rustix::event::Timespec;
#[cfg(not(target_os = "macos"))]
use rustix::event::{PollFd, PollFlags};
use rustix::termios::{OptionalActions, Termios};

use crate::platform::clock;

const INPUT_BATCH: usize = 64;
#[cfg(target_os = "macos")]
mod select;

#[derive(Debug)]
pub(crate) struct Input {
    handle: OwnedFd,
    original: Termios,
    bytes: [u8; INPUT_BATCH],
    position: usize,
    used: usize,
    decoder: Decoder,
}

impl Input {
    pub(super) fn new(input: &io::Stdin) -> io::Result<Self> {
        Self::capture(rustix::io::fcntl_dupfd_cloexec(input.as_fd(), 0)?)
    }

    fn capture(handle: OwnedFd) -> io::Result<Self> {
        let original = rustix::termios::tcgetattr(&handle)?;
        Ok(Self {
            handle,
            original,
            bytes: [0; INPUT_BATCH],
            position: 0,
            used: 0,
            decoder: Decoder::default(),
        })
    }

    pub(super) fn setup(&self) -> io::Result<()> {
        self.setup_using(|handle, raw| {
            rustix::termios::tcsetattr(handle, OptionalActions::Now, raw)?;
            Ok(())
        })
    }

    fn setup_using(
        &self,
        set: impl FnOnce(&OwnedFd, &Termios) -> io::Result<()>,
    ) -> io::Result<()> {
        let mut raw = self.original.clone();
        raw.make_raw();
        set(&self.handle, &raw)
    }

    pub(super) fn restore(&self) -> io::Result<()> {
        rustix::termios::tcsetattr(&self.handle, OptionalActions::Now, &self.original)?;
        #[cfg(target_os = "macos")]
        if self
            .original
            .local_modes
            .contains(rustix::termios::LocalModes::ICANON)
            && !self
                .original
                .local_modes
                .contains(rustix::termios::LocalModes::PENDIN)
        {
            rustix::io::ioctl_fionread(&self.handle)?;
        }
        Ok(())
    }

    pub(crate) fn next(&mut self, millis: u64) -> io::Result<Option<KeyEvent>> {
        if terminal_input::poll_nanos(millis).is_none() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the terminal poll exceeds its deadline budget",
            ));
        }
        let deadline = clock::Deadline::after_millis(millis);
        loop {
            if let Some(key) = self.buffered() {
                return Ok(Some(key));
            }
            match ready(&self.handle, deadline.left_millis()) {
                Ok(false) => return Ok(key(self.decoder.quiet())),
                Ok(true) => {}
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                    if deadline.expired() {
                        return Ok(None);
                    }
                    continue;
                }
                Err(error) => return Err(error),
            }
            match rustix::io::read(&self.handle, &mut self.bytes) {
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(count) if count <= self.bytes.len() => {
                    self.used = count;
                    self.position = 0;
                }
                Ok(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "the terminal returned an oversized input batch",
                    ));
                }
                Err(rustix::io::Errno::INTR) => {}
                Err(error) => return Err(error.into()),
            }
            if deadline.expired() {
                return Ok(self.buffered());
            }
        }
    }

    fn buffered(&mut self) -> Option<KeyEvent> {
        while self.position < self.used {
            let byte = *self.bytes.get(self.position)?;
            self.position = self.position.saturating_add(1);
            if let Some(key) = key(self.decoder.decode(byte)) {
                return Some(key);
            }
        }
        None
    }
}

const fn key(decoded: Decoded) -> Option<KeyEvent> {
    match decoded {
        Decoded::Key(key) => Some(key),
        Decoded::Pending | Decoded::Ignored => None,
    }
}

fn ready(handle: &OwnedFd, millis: u64) -> io::Result<bool> {
    let nanos = terminal_input::poll_nanos(millis).ok_or(io::ErrorKind::InvalidInput)?;
    let timeout = Timespec {
        tv_sec: 0,
        tv_nsec: nanos,
    };
    wait(handle, &timeout)
}

#[cfg(target_os = "macos")]
fn wait(handle: &OwnedFd, timeout: &Timespec) -> io::Result<bool> {
    select::ready(handle.as_fd(), timeout)
}

#[cfg(not(target_os = "macos"))]
fn wait(handle: &OwnedFd, timeout: &Timespec) -> io::Result<bool> {
    let mut descriptor = [PollFd::new(handle, PollFlags::IN)];
    match rustix::event::poll(&mut descriptor, Some(timeout)) {
        Ok(count) => Ok(count != 0),
        Err(error) => Err(error.into()),
    }
}

#[derive(Debug)]
pub(crate) struct Display {
    handle: OwnedFd,
}

impl Display {
    pub(super) fn new(output: &io::Stdout) -> io::Result<Self> {
        Ok(Self {
            handle: rustix::io::fcntl_dupfd_cloexec(output.as_fd(), 0)?,
        })
    }

    pub(crate) fn frame_size(&self) -> io::Result<(u16, u16)> {
        let size = rustix::termios::tcgetwinsize(&self.handle)?;
        Ok((size.ws_col, size.ws_row))
    }
}

impl super::DisplayModes for Display {
    fn setup(&self) -> io::Result<()> {
        Ok(())
    }
    fn restore(&self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests;
