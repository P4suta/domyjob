use std::io::{self, Write};

#[path = "../crates/domyjob/src/transport/input.rs"]
mod input;

const SEGMENT_BYTES: usize = 2;
const TOTAL_BYTES: usize = SEGMENT_BYTES * 3;

struct PartialWriter {
    bytes: [u8; TOTAL_BYTES],
    used: usize,
    accepted: usize,
    flush_error: bool,
    flushed: bool,
    writes: usize,
}

impl Write for PartialWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.writes += 1;
        if self.used == self.accepted {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        self.bytes[self.used] = bytes[0];
        self.used += 1;
        Ok(1)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.flushed = true;
        if self.flush_error {
            Err(io::ErrorKind::Other.into())
        } else {
            Ok(())
        }
    }
}

#[kani::proof]
#[kani::unwind(8)]
fn ordered_segments_stop_at_the_first_failed_write() {
    let source: [u8; SEGMENT_BYTES] = kani::any();
    let frame: [u8; SEGMENT_BYTES] = kani::any();
    let payload: [u8; SEGMENT_BYTES] = kani::any();
    let accepted: u8 = kani::any();
    let accepted = usize::from(accepted);
    kani::assume(accepted <= TOTAL_BYTES);
    let flush_error: bool = kani::any();
    let mut output = PartialWriter {
        bytes: [0; TOTAL_BYTES],
        used: 0,
        accepted,
        flush_error,
        flushed: false,
        writes: 0,
    };
    let result = input::Outbound {
        source: &source,
        frame: &frame,
        payload: &payload,
    }
    .send(&mut output);
    let expected = [
        source[0], source[1], frame[0], frame[1], payload[0], payload[1],
    ];
    assert_eq!(output.used, accepted);
    for index in 0..output.used {
        assert_eq!(output.bytes[index], expected[index]);
    }
    assert_eq!(output.flushed, accepted == TOTAL_BYTES);
    assert_eq!(
        output.writes,
        accepted + usize::from(accepted < TOTAL_BYTES)
    );
    assert_eq!(result.is_ok(), accepted == TOTAL_BYTES && !flush_error);
    kani::cover!(result.is_ok());
    kani::cover!(accepted < SEGMENT_BYTES);
    kani::cover!(accepted >= SEGMENT_BYTES && accepted < SEGMENT_BYTES * 2);
    kani::cover!(accepted >= SEGMENT_BYTES * 2 && accepted < TOTAL_BYTES);
    kani::cover!(accepted == TOTAL_BYTES && flush_error);
}

#[kani::proof]
#[kani::unwind(8)]
fn rejects_success_after_a_partial_segment_failure() {
    let mut output = PartialWriter {
        bytes: [0; TOTAL_BYTES],
        used: 0,
        accepted: 1,
        flush_error: false,
        flushed: false,
        writes: 0,
    };
    let result = input::Outbound {
        source: b"ab",
        frame: b"cd",
        payload: b"ef",
    }
    .send(&mut output);
    kani::assert(result.is_ok(), "NEGATIVE_OUTBOUND_CONTROL");
}
