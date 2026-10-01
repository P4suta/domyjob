use std::io::{self, Read};

mod raw {
    #![expect(
        clippy::disallowed_methods,
        reason = "`bounded` is the one reader that reads to the end, always through a limit"
    )]

    use std::io::{self, Read};

    pub(super) fn read_limited(
        reader: impl Read,
        limit: u64,
        into: &mut Vec<u8>,
    ) -> io::Result<usize> {
        reader.take(limit).read_to_end(into)
    }
}

pub(crate) fn read(reader: impl Read, limit: usize) -> io::Result<Option<Vec<u8>>> {
    let beyond = u64::try_from(limit)
        .map_err(io::Error::other)?
        .saturating_add(1);
    let mut bytes = Vec::new();
    raw::read_limited(reader, beyond, &mut bytes)?;
    Ok((bytes.len() <= limit).then_some(bytes))
}

pub(crate) fn prefix(reader: impl Read, limit: usize) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    raw::read_limited(
        reader,
        u64::try_from(limit).map_err(io::Error::other)?,
        &mut bytes,
    )?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use std::io::{self, Read};

    use super::{prefix, read};

    struct FailingReader;

    impl Read for FailingReader {
        fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "reader refused",
            ))
        }
    }

    #[test]
    fn a_limit_is_never_exceeded() {
        assert_eq!(read(&b"four"[..], 4).unwrap(), Some(b"four".to_vec()));
        assert_eq!(read(&b"five!"[..], 4).unwrap(), None);
        assert_eq!(prefix(&b"five!"[..], 4).unwrap(), b"five".to_vec());
    }

    #[test]
    fn a_reader_error_after_partial_data_is_preserved() {
        for error in [
            read((&b"part"[..]).chain(FailingReader), 8).unwrap_err(),
            prefix((&b"part"[..]).chain(FailingReader), 8).unwrap_err(),
        ] {
            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
            assert_eq!(error.to_string(), "reader refused");
        }
    }
}
