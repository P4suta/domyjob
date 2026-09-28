//! This process's standard output, as a handle that only `main` creates.
//!
//! The MCP server and a node's wire protocol frame their messages on standard output,
//! so only commands that print receive the handle, and no other code can write there:
//! a stray line inside shared chat code would otherwise corrupt the MCP stream.

use std::fmt::Display;
use std::io::{self, Write as _};

mod raw {
    #![expect(
        clippy::disallowed_methods,
        reason = "`Output::of_process` alone takes this process's standard output"
    )]

    pub(super) fn stdout() -> std::io::Stdout {
        std::io::stdout()
    }
}

/// The standard output of a command that prints or frames its results.
#[derive(Debug)]
pub(crate) struct Output(io::Stdout);

impl Output {
    /// Take this process's standard output; only `main` does this, once per command.
    pub(crate) fn of_process() -> Self {
        Self(raw::stdout())
    }

    /// Print one line.
    pub(crate) fn line(&self, text: impl Display) -> io::Result<()> {
        writeln!(self.0.lock(), "{text}")
    }

    /// Write bytes exactly and flush them.
    pub(crate) fn write(&self, bytes: &[u8]) -> io::Result<()> {
        let mut locked = self.0.lock();
        locked.write_all(bytes)?;
        locked.flush()
    }

    /// The underlying stream, for a writer that frames its own messages.
    pub(crate) const fn into_stream(self) -> io::Stdout {
        self.0
    }
}
