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

#[derive(Debug)]
pub(crate) struct Output(io::Stdout);

impl Output {
    pub(crate) fn of_process() -> Self {
        Self(raw::stdout())
    }

    pub(crate) fn line(&self, text: impl Display) -> io::Result<()> {
        writeln!(self.0.lock(), "{text}")
    }

    pub(crate) fn write(&self, bytes: &[u8]) -> io::Result<()> {
        let mut locked = self.0.lock();
        locked.write_all(bytes)?;
        locked.flush()
    }

    pub(crate) const fn into_stream(self) -> io::Stdout {
        self.0
    }
}
