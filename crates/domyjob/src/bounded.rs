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
    use super::{prefix, read};

    #[test]
    fn a_limit_is_never_exceeded() {
        assert_eq!(read(&b"four"[..], 4).unwrap(), Some(b"four".to_vec()));
        assert_eq!(read(&b"five!"[..], 4).unwrap(), None);
        assert_eq!(prefix(&b"five!"[..], 4).unwrap(), b"five".to_vec());
    }
}
