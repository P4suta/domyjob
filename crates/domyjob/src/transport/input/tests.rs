use std::io::{self, Write};

use super::Outbound;

const SOURCE: &[u8] = b"ab";
const FRAME: &[u8] = b"cde";
const PAYLOAD: &[u8] = b"fg";
const EXPECTED: &[u8] = b"abcdefg";
const EMPTY: [u8; 0] = [];

#[derive(Clone, Copy)]
enum WriteFault {
    Error,
    Zero,
}

#[derive(Clone, Copy)]
enum FlushFault {
    None,
    Error,
}

struct Fixture {
    bytes: Vec<u8>,
    remaining: usize,
    fault: WriteFault,
    flush: FlushFault,
    interrupted: bool,
    flushes: usize,
}

impl Fixture {
    fn new(remaining: usize, fault: WriteFault, flush: FlushFault) -> Self {
        Self {
            bytes: Vec::new(),
            remaining,
            fault,
            flush,
            interrupted: false,
            flushes: 0,
        }
    }
}

impl Write for Fixture {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.interrupted {
            self.interrupted = false;
            return Err(io::ErrorKind::Interrupted.into());
        }
        if self.remaining == 0 {
            return match self.fault {
                WriteFault::Error => Err(io::ErrorKind::BrokenPipe.into()),
                WriteFault::Zero => Ok(0),
            };
        }
        if let Some(first) = bytes.first() {
            self.bytes.push(*first);
            self.remaining = self.remaining.saturating_sub(1);
            Ok(1)
        } else {
            Ok(0)
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.flushes = self.flushes.saturating_add(1);
        match self.flush {
            FlushFault::None => Ok(()),
            FlushFault::Error => Err(io::ErrorKind::Other.into()),
        }
    }
}

fn outgoing() -> Outbound<'static> {
    Outbound {
        source: SOURCE,
        frame: FRAME,
        payload: PAYLOAD,
    }
}

#[test]
fn short_writes_and_interrupted_retries_preserve_all_segment_bytes() {
    let mut output = Fixture::new(usize::MAX, WriteFault::Error, FlushFault::None);
    output.interrupted = true;
    outgoing().send(&mut output).unwrap();
    assert_eq!(output.bytes, EXPECTED);
    assert_eq!(output.flushes, 1);
}

#[test]
fn every_write_failure_stops_at_the_exact_prefix_without_flushing() {
    for accepted in 0..EXPECTED.len() {
        for fault in [WriteFault::Error, WriteFault::Zero] {
            let mut output = Fixture::new(accepted, fault, FlushFault::None);
            let error = outgoing().send(&mut output).unwrap_err();
            let kind = match fault {
                WriteFault::Error => io::ErrorKind::BrokenPipe,
                WriteFault::Zero => io::ErrorKind::WriteZero,
            };
            assert_eq!(error.kind(), kind);
            assert_eq!(output.bytes, EXPECTED.get(..accepted).unwrap());
            assert_eq!(output.flushes, 0);
        }
    }
}

#[test]
fn flush_failure_is_returned_after_all_segments_without_a_second_flush() {
    let mut output = Fixture::new(usize::MAX, WriteFault::Error, FlushFault::Error);
    assert_eq!(
        outgoing().send(&mut output).unwrap_err().kind(),
        io::ErrorKind::Other
    );
    assert_eq!(output.bytes, EXPECTED);
    assert_eq!(output.flushes, 1);
}

#[test]
fn empty_segments_still_flush_once_and_do_not_write() {
    let mut output = Fixture::new(0, WriteFault::Error, FlushFault::None);
    Outbound {
        source: &[],
        frame: &[],
        payload: &[],
    }
    .send(&mut output)
    .unwrap();
    assert_eq!(output.bytes, EMPTY);
    assert_eq!(output.flushes, 1);
}
