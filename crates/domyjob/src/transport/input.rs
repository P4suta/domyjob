use std::io::{self, Write};

#[derive(Clone, Copy, Debug)]
pub(crate) struct Outbound<'a> {
    pub(crate) source: &'a [u8],
    pub(crate) frame: &'a [u8],
    pub(crate) payload: &'a [u8],
}

#[cfg(test)]
#[path = "input/tests.rs"]
mod tests;

impl Outbound<'_> {
    pub(crate) fn send(&self, output: &mut impl Write) -> io::Result<()> {
        output.write_all(self.source)?;
        output.write_all(self.frame)?;
        output.write_all(self.payload)?;
        output.flush()
    }
}
