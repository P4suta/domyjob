use std::io::{self, Write};

pub(super) fn complete<W: Write>(
    stream: &mut W,
    bytes: &[u8],
    finish: impl FnOnce(&mut W) -> io::Result<()>,
) -> io::Result<()> {
    stream.write_all(bytes)?;
    finish(stream)
}
