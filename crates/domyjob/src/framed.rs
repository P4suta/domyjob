use std::io::{BufRead, ErrorKind, Write};

use serde::{Deserialize, Serialize};

use crate::protocol::Refusal;

pub const MAX_CHUNK: usize = 1 << 20;
const ENDING_LINE: u64 = 64 << 10;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case", tag = "ending")]
pub enum Ending {
    Complete { bytes: u64, blake3: String },
    Failed { refusal: Refusal },
}

impl crate::ingress::Ingress for Ending {}

pub struct Framed<'a> {
    out: &'a mut dyn Write,
    hasher: blake3::Hasher,
    bytes: u64,
}

impl std::fmt::Debug for Framed<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Framed")
            .field("bytes", &self.bytes)
            .finish_non_exhaustive()
    }
}

impl<'a> Framed<'a> {
    pub fn new(out: &'a mut dyn Write) -> Self {
        Self {
            out,
            hasher: blake3::Hasher::new(),
            bytes: 0,
        }
    }

    pub fn finish(self, outcome: Result<(), Refusal>) -> std::io::Result<()> {
        let ending = match outcome {
            Ok(()) => Ending::Complete {
                bytes: self.bytes,
                blake3: self.hasher.finalize().to_hex().to_string(),
            },
            Err(refusal) => Ending::Failed { refusal },
        };
        let mut closing = 0u32.to_be_bytes().to_vec();
        closing.extend(serde_json::to_vec(&ending).map_err(std::io::Error::other)?);
        closing.push(b'\n');
        self.out.write_all(&closing)?;
        self.out.flush()
    }
}

impl Write for Framed<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let chunk = bytes.get(..bytes.len().min(MAX_CHUNK)).unwrap_or(&[]);
        if chunk.is_empty() {
            return Ok(0);
        }
        let length = u32::try_from(chunk.len()).map_err(std::io::Error::other)?;
        let mut framed = Vec::with_capacity(chunk.len().saturating_add(4));
        framed.extend_from_slice(&length.to_be_bytes());
        framed.extend_from_slice(chunk);
        self.out.write_all(&framed)?;
        self.hasher.update(chunk);
        self.bytes = self
            .bytes
            .saturating_add(crate::domain::len_u64(chunk.len()));
        Ok(chunk.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.out.flush()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Unframed {
    #[error("the stream was cut before it ended")]
    Truncated,
    #[error("the stream was malformed: {0}")]
    Malformed(&'static str),
    #[error("the stream arrived damaged: {0}")]
    Damaged(&'static str),
    #[error("reading the stream failed: {0}")]
    Reading(std::io::Error),
    #[error("writing what arrived failed: {0}")]
    Writing(std::io::Error),
    #[error("the sender failed partway: {}", .0.detail)]
    Failed(Refusal),
}

fn reading(error: std::io::Error) -> Unframed {
    if error.kind() == ErrorKind::UnexpectedEof {
        Unframed::Truncated
    } else {
        Unframed::Reading(error)
    }
}

pub fn unframe(reader: &mut dyn BufRead, sink: &mut dyn Write) -> Result<u64, Unframed> {
    let mut hasher = blake3::Hasher::new();
    let mut bytes = 0u64;
    let mut buffer = vec![0u8; MAX_CHUNK];
    loop {
        let mut length = [0u8; 4];
        reader.read_exact(&mut length).map_err(reading)?;
        let length = u32::from_be_bytes(length);
        if length == crate::liveness::STREAM_BEAT {
            continue;
        }
        let length = usize::try_from(length)
            .map_err(|_beyond_usize| Unframed::Malformed("a chunk longer than memory"))?;
        if length == 0 {
            break;
        }
        let chunk = buffer
            .get_mut(..length)
            .ok_or(Unframed::Malformed("a chunk longer than the limit"))?;
        reader.read_exact(chunk).map_err(reading)?;
        hasher.update(chunk);
        bytes = bytes.saturating_add(crate::domain::len_u64(length));
        sink.write_all(chunk).map_err(Unframed::Writing)?;
        sink.flush().map_err(Unframed::Writing)?;
    }
    let raw = crate::bounded::line(reader, ENDING_LINE).map_err(reading)?;
    if raw.last() != Some(&b'\n') {
        return Err(Unframed::Truncated);
    }
    let ending: Ending = crate::ingress::json(&raw)
        .map_err(|_malformed| Unframed::Malformed("an ending that is not one"))?;
    match ending {
        Ending::Failed { refusal } => Err(Unframed::Failed(refusal)),
        Ending::Complete {
            bytes: announced,
            blake3,
        } => {
            if announced != bytes {
                return Err(Unframed::Damaged("the byte count does not match"));
            }
            if blake3 != hasher.finalize().to_hex().as_str() {
                return Err(Unframed::Damaged("the checksum does not match"));
            }
            Ok(bytes)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::RefusalCode;
    use crate::terminal::RemoteText;

    fn framed(pieces: &[&[u8]], outcome: Result<(), Refusal>) -> Vec<u8> {
        let mut wire = Vec::new();
        let mut framed = Framed::new(&mut wire);
        for piece in pieces {
            framed.write_all(piece).unwrap();
        }
        framed.finish(outcome).unwrap();
        wire
    }

    fn refusal() -> Refusal {
        Refusal {
            code: RefusalCode::Storage,
            detail: RemoteText::new("the disk filled up".to_owned()),
        }
    }

    #[test]
    fn heartbeats_between_chunks_are_not_part_of_the_stream() {
        let mut wire = Vec::new();
        wire.extend_from_slice(&crate::liveness::STREAM_BEAT.to_be_bytes());
        let body = framed(&[b"real"], Ok(()));
        let (first, rest) = body.split_at(8);
        wire.extend_from_slice(first);
        wire.extend_from_slice(&crate::liveness::STREAM_BEAT.to_be_bytes());
        wire.extend_from_slice(rest);
        let mut out = Vec::new();
        assert_eq!(unframe(&mut wire.as_slice(), &mut out).unwrap(), 4);
        assert_eq!(out, b"real");
    }

    #[test]
    fn a_whole_stream_arrives_exactly_and_nothing_else() {
        let wire = framed(&[b"first ", b"", b"second"], Ok(()));
        let mut out = Vec::new();
        assert_eq!(unframe(&mut wire.as_slice(), &mut out).unwrap(), 12);
        assert_eq!(out, b"first second");
    }

    #[test]
    fn every_cut_short_of_the_end_is_an_error() {
        let wire = framed(&[b"some output", b"more output"], Ok(()));
        for cut in 0..wire.len() {
            let mut out = Vec::new();
            let result = unframe(&mut wire.get(..cut).unwrap(), &mut out);
            assert!(
                matches!(result, Err(Unframed::Truncated)),
                "a stream cut at {cut} of {} was {result:?}",
                wire.len()
            );
        }
    }

    #[test]
    fn a_failure_partway_is_reported_as_the_senders_refusal() {
        let wire = framed(&[b"partial"], Err(refusal()));
        let mut out = Vec::new();
        let Err(Unframed::Failed(got)) = unframe(&mut wire.as_slice(), &mut out) else {
            panic!("a failed stream was taken as complete");
        };
        assert_eq!(got, refusal());
        assert_eq!(out, b"partial");
    }

    #[test]
    fn a_flipped_byte_or_a_wrong_count_is_damage() {
        let mut flipped = framed(&[b"exact bytes"], Ok(()));
        *flipped.get_mut(5).unwrap() ^= 1;
        assert!(matches!(
            unframe(&mut flipped.as_slice(), &mut Vec::new()),
            Err(Unframed::Damaged(_))
        ));
        let recounted = String::from_utf8_lossy(&framed(&[b"exact bytes"], Ok(())))
            .replace("\"bytes\":11", "\"bytes\":12")
            .into_bytes();
        assert!(matches!(
            unframe(&mut recounted.as_slice(), &mut Vec::new()),
            Err(Unframed::Damaged(_))
        ));
    }

    #[test]
    fn large_writes_are_split_into_bounded_chunks() {
        let big = vec![7u8; MAX_CHUNK * 2 + 3];
        let wire = framed(&[&big], Ok(()));
        let mut out = Vec::new();
        assert_eq!(
            unframe(&mut wire.as_slice(), &mut out).unwrap(),
            crate::domain::len_u64(big.len())
        );
        assert_eq!(out, big);
    }

    #[derive(Debug, Default)]
    struct Refusing {
        flushed: bool,
    }

    impl Write for Refusing {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("refused"))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            self.flushed = true;
            Err(std::io::Error::other("refused"))
        }
    }

    #[test]
    fn a_failing_destination_is_an_error_at_every_step() {
        let mut out = Refusing::default();
        let mut framed = Framed::new(&mut out);
        framed.write_all(b"x").unwrap_err();
        framed.flush().unwrap_err();
        framed.finish(Ok(())).unwrap_err();
        assert!(out.flushed);
        let mut wire = Vec::new();
        let mut empty = Framed::new(&mut wire);
        assert_eq!(empty.write(b"").unwrap(), 0);
        assert!(wire.is_empty());
    }

    #[derive(Debug)]
    struct Broken;

    impl std::io::Read for Broken {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("the cable is out"))
        }
    }

    #[test]
    fn a_broken_source_or_sink_is_told_apart_from_a_cut() {
        let mut broken = std::io::BufReader::new(Broken);
        assert!(matches!(
            unframe(&mut broken, &mut Vec::new()),
            Err(Unframed::Reading(_))
        ));
        let wire = framed(&[b"data"], Ok(()));
        assert!(matches!(
            unframe(&mut wire.as_slice(), &mut Refusing::default()),
            Err(Unframed::Writing(_))
        ));
        let mut no_ending = wire.get(..wire.len().saturating_sub(2)).unwrap().to_vec();
        no_ending.push(b'\n');
        assert!(matches!(
            unframe(&mut no_ending.as_slice(), &mut Vec::new()),
            Err(Unframed::Malformed(_))
        ));
    }

    proptest::proptest! {
        #[test]
        fn any_payload_arrives_whole_or_not_at_all(
            pieces in proptest::collection::vec(proptest::collection::vec(proptest::num::u8::ANY, 0..64), 0..6),
            cut in proptest::num::usize::ANY,
        ) {
            let slices: Vec<&[u8]> = pieces.iter().map(Vec::as_slice).collect();
            let wire = framed(&slices, Ok(()));
            let whole: Vec<u8> = pieces.concat();
            let mut out = Vec::new();
            proptest::prop_assert_eq!(unframe(&mut wire.as_slice(), &mut out).unwrap(), crate::domain::len_u64(whole.len()));
            proptest::prop_assert_eq!(&out, &whole);
            let at = cut.checked_rem(wire.len()).unwrap();
            let result = unframe(&mut wire.get(..at).unwrap(), &mut Vec::new());
            proptest::prop_assert!(matches!(result, Err(Unframed::Truncated)));
        }
    }
}
