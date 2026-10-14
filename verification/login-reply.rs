use std::io::{self, Write};

#[path = "../crates/domyjob/src/process/login/reply.rs"]
mod reply;

const PAYLOAD_BYTES: usize = 4;

struct Writer {
    bytes: [u8; PAYLOAD_BYTES],
    used: usize,
    cutoff: usize,
    completed: bool,
    flushed: bool,
}

impl Write for Writer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.used == self.cutoff {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        let byte = bytes.first().ok_or(io::ErrorKind::InvalidInput)?;
        let slot = self
            .bytes
            .get_mut(self.used)
            .ok_or(io::ErrorKind::InvalidInput)?;
        *slot = *byte;
        self.used += 1;
        Ok(1)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.flushed = true;
        Err(io::ErrorKind::Unsupported.into())
    }
}

#[kani::proof]
#[kani::unwind(8)]
fn complete_replies_finish_once_after_every_admitted_byte() {
    let bytes: [u8; PAYLOAD_BYTES] = kani::any();
    let length: u8 = kani::any();
    let length = usize::from(length);
    kani::assume(length <= PAYLOAD_BYTES);
    let cutoff: u8 = kani::any();
    let cutoff = usize::from(cutoff);
    kani::assume(cutoff <= length);
    let shutdown_failure: bool = kani::any();
    let mut output = Writer {
        bytes: [0; PAYLOAD_BYTES],
        used: 0,
        cutoff,
        completed: false,
        flushed: false,
    };
    let result = reply::complete(&mut output, &bytes[..length], |writer| {
        assert_eq!(writer.used, length);
        assert!(!writer.completed);
        writer.completed = true;
        if shutdown_failure {
            Err(io::ErrorKind::PermissionDenied.into())
        } else {
            Ok(())
        }
    });
    assert_eq!(output.used, cutoff);
    assert_eq!(output.bytes[..cutoff], bytes[..cutoff]);
    assert_eq!(output.completed, cutoff == length);
    assert!(!output.flushed);
    let expected = if cutoff < length {
        Some(io::ErrorKind::BrokenPipe)
    } else if shutdown_failure {
        Some(io::ErrorKind::PermissionDenied)
    } else {
        None
    };
    assert_eq!(result.as_ref().err().map(io::Error::kind), expected);
    kani::cover!(length == 0);
    kani::cover!(length > 0 && result.is_ok());
    kani::cover!(cutoff < length);
    kani::cover!(cutoff == length && shutdown_failure);
}

#[kani::proof]
#[kani::unwind(8)]
fn rejects_shutdown_after_an_incomplete_reply() {
    let mut output = Writer {
        bytes: [0; PAYLOAD_BYTES],
        used: 0,
        cutoff: 0,
        completed: false,
        flushed: false,
    };
    let _result = reply::complete(&mut output, b"R", |writer| {
        writer.completed = true;
        Ok(())
    });
    kani::assert(output.completed, "NEGATIVE_LOGIN_REPLY_ORDER");
}

#[kani::proof]
#[kani::unwind(8)]
fn rejects_success_after_reply_shutdown_failure() {
    let mut output = Vec::new();
    let result = reply::complete(&mut output, b"R", |_| {
        Err(io::ErrorKind::PermissionDenied.into())
    });
    kani::assert(result.is_ok(), "NEGATIVE_LOGIN_REPLY_SHUTDOWN");
}
