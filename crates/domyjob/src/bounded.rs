use std::io::{BufRead, ErrorKind, Read};

pub const REQUEST_LINE: u64 = 1 << 20;
pub const REPLY_LINE: u64 = 64 << 20;
pub const BLOB: u64 = 8 << 30;
pub const UPLOAD_COUNT: u64 = 2_000_000;
pub const TAIL_WINDOW: u64 = 4 << 20;
pub const CONTROL_LINE: u64 = 1024;

fn too_long(limit: u64) -> std::io::Error {
    std::io::Error::new(
        ErrorKind::InvalidData,
        format!("a line longer than {limit} bytes was refused"),
    )
}

pub fn line(reader: &mut dyn BufRead, limit: u64) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(out);
        }
        let (take, done) = match available.iter().position(|b| *b == b'\n') {
            Some(at) => (at.saturating_add(1), true),
            None => (available.len(), false),
        };
        let chunk = available.get(..take).unwrap_or(&[]);
        if crate::domain::len_u64(out.len().saturating_add(chunk.len())) > limit {
            return Err(too_long(limit));
        }
        out.extend_from_slice(chunk);
        reader.consume(take);
        if done {
            return Ok(out);
        }
    }
}

pub fn exactly(
    reader: &mut dyn Read,
    size: u64,
    sink: &mut dyn FnMut(&[u8]) -> std::io::Result<()>,
) -> std::io::Result<()> {
    let mut left = size;
    let mut buffer = vec![0u8; 64 * 1024];
    while left > 0 {
        let want = match usize::try_from(left) {
            Ok(fits) => fits.min(buffer.len()),
            Err(_beyond_usize) => buffer.len(),
        };
        let slot = buffer.get_mut(..want).unwrap_or(&mut []);
        let read = reader.read(slot)?;
        if read == 0 {
            return Err(std::io::Error::new(
                ErrorKind::UnexpectedEof,
                "the peer stopped before sending everything it announced",
            ));
        }
        sink(slot.get(..read).unwrap_or(&[]))?;
        left = left.saturating_sub(crate::domain::len_u64(read));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_stop_at_the_limit() {
        let mut input: &[u8] = b"short\nthis one is far too long\n";
        assert_eq!(line(&mut input, 10).unwrap(), b"short\n");
        assert_eq!(
            line(&mut input, 10).unwrap_err().kind(),
            ErrorKind::InvalidData
        );
        let mut exact: &[u8] = b"abcdef";
        let mut seen = Vec::new();
        exactly(&mut exact, 4, &mut |chunk| {
            seen.extend_from_slice(chunk);
            Ok(())
        })
        .unwrap();
        assert_eq!(seen, b"abcd");
        let mut short: &[u8] = b"ab";
        assert_eq!(
            exactly(&mut short, 4, &mut |_| Ok(())).unwrap_err().kind(),
            ErrorKind::UnexpectedEof
        );
    }
}
