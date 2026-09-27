use std::io::{BufRead, ErrorKind, Read, Write};
use std::path::Path;

pub const REQUEST_LINE: u64 = 1 << 20;
pub const REPLY_LINE: u64 = 64 << 20;
pub const BLOB: u64 = 8 << 30;
pub const UPLOAD_COUNT: u64 = 2_000_000;
pub const TAIL_WINDOW: u64 = 4 << 20;
pub const IN_MEMORY_FILE: u64 = 64 << 20;
pub const CAPTURE: u64 = 4 << 20;
pub const CONTROL_LINE: u64 = 1024;
pub const CONFIG_TEXT: u64 = 4 << 20;
pub const BOOT_ID: u64 = 128;
pub const SIGNED_METADATA: u64 = 4 << 20;
pub const SOURCE_ARCHIVE: u64 = 64 << 20;

fn too_long(limit: u64) -> std::io::Error {
    std::io::Error::new(
        ErrorKind::InvalidData,
        format!("input longer than {limit} bytes was refused"),
    )
}

pub fn to_end(reader: &mut dyn Read, limit: u64) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::new();
    reader.take(limit.saturating_add(1)).read_to_end(&mut out)?;
    if crate::domain::len_u64(out.len()) > limit {
        return Err(too_long(limit));
    }
    Ok(out)
}

pub fn file_bytes(path: &Path, limit: u64) -> std::io::Result<Vec<u8>> {
    let mut file = std::fs::File::open(path)?;
    to_end(&mut file, limit)
}

pub fn text_file(path: &Path, limit: u64) -> std::io::Result<String> {
    let bytes = file_bytes(path, limit)?;
    String::from_utf8(bytes).map_err(|error| std::io::Error::new(ErrorKind::InvalidData, error))
}

#[derive(Debug)]
pub struct CappedVec {
    bytes: Vec<u8>,
    limit: u64,
}

impl CappedVec {
    #[must_use]
    pub const fn new(limit: u64) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
        }
    }

    #[must_use]
    pub fn into_vec(self) -> Vec<u8> {
        self.bytes
    }
}

impl Write for CappedVec {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if crate::domain::len_u64(self.bytes.len().saturating_add(bytes.len())) > self.limit {
            return Err(too_long(self.limit));
        }
        self.bytes
            .try_reserve(bytes.len())
            .map_err(std::io::Error::other)?;
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
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

pub fn line_unread_past(reader: &mut dyn Read, limit: u64) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        if reader.read(&mut byte)? == 0 {
            return Ok(out);
        }
        if crate::domain::len_u64(out.len()) >= limit {
            return Err(too_long(limit));
        }
        out.extend_from_slice(&byte);
        if byte == *b"\n" {
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

    proptest::proptest! {
        #[test]
        fn both_line_readers_agree_and_the_unbuffered_one_reads_nothing_past_its_line(
            input in proptest::collection::vec(proptest::prelude::any::<u8>(), 0..64),
            limit in 0u64..40,
        ) {
            let buffered = line(&mut std::io::Cursor::new(&input), limit);
            let mut raw = std::io::Cursor::new(&input);
            let unbuffered = line_unread_past(&mut raw, limit);
            match (&buffered, &unbuffered) {
                (Ok(a), Ok(b)) => {
                    proptest::prop_assert_eq!(a, b);
                    proptest::prop_assert_eq!(raw.position(), crate::domain::len_u64(b.len()));
                }
                (Err(a), Err(b)) => proptest::prop_assert_eq!(a.kind(), b.kind()),
                (Ok(_), Err(_)) | (Err(_), Ok(_)) => {
                    proptest::prop_assert!(false, "{buffered:?} vs {unbuffered:?}");
                }
            }
        }
    }

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

    #[test]
    fn to_end_rejects_a_reader_that_grows_past_its_budget() {
        let mut exact: &[u8] = b"abcd";
        assert_eq!(to_end(&mut exact, 4).unwrap(), b"abcd");
        let mut long: &[u8] = b"abcde";
        assert_eq!(
            to_end(&mut long, 4).unwrap_err().kind(),
            ErrorKind::InvalidData
        );
    }

    #[test]
    fn text_files_refuse_excess_bytes_and_invalid_utf8() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        crate::user_files::write(&path, b"abc").unwrap();
        assert_eq!(text_file(&path, 3).unwrap(), "abc");
        assert_eq!(file_bytes(&path, 3).unwrap(), b"abc");
        assert_eq!(
            file_bytes(&path, 2).unwrap_err().kind(),
            ErrorKind::InvalidData
        );
        assert_eq!(
            text_file(&path, 2).unwrap_err().kind(),
            ErrorKind::InvalidData
        );
        crate::user_files::write(&path, &[0xff]).unwrap();
        assert_eq!(
            text_file(&path, 2).unwrap_err().kind(),
            ErrorKind::InvalidData
        );
        assert_eq!(
            text_file(&dir.path().join("absent"), 2).unwrap_err().kind(),
            ErrorKind::NotFound
        );
    }

    #[test]
    fn capped_writes_refuse_an_archive_before_growing_past_its_budget() {
        let mut buffer = CappedVec::new(3);
        buffer.write_all(b"abc").unwrap();
        assert_eq!(
            buffer.write_all(b"d").unwrap_err().kind(),
            ErrorKind::InvalidData
        );
        assert_eq!(buffer.into_vec(), b"abc");
    }
}
