use std::io::{self, Write};

#[path = "../../domyjob/src/process/login/reply.rs"]
mod reply;

const PAYLOAD: &[u8] = b"done";
const ORIGINAL_ERROR: &str = "original write failure";
const SHUTDOWN_ERROR: &str = "original shutdown failure";

struct Writer {
    bytes: Vec<u8>,
    cutoff: usize,
    interrupted: bool,
    zero: bool,
    flushed: bool,
}

impl Write for Writer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.interrupted {
            self.interrupted = false;
            return Err(io::ErrorKind::Interrupted.into());
        }
        if self.zero {
            return Ok(0);
        }
        if self.bytes.len() == self.cutoff {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, ORIGINAL_ERROR));
        }
        self.bytes
            .push(*bytes.first().ok_or(io::ErrorKind::InvalidInput)?);
        Ok(1)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.flushed = true;
        Err(io::ErrorKind::Unsupported.into())
    }
}

#[test]
fn completion_preserves_partial_writes_and_both_failure_outcomes() {
    for cutoff in 0..=PAYLOAD.len() {
        for shutdown_failure in [false, true] {
            let mut output = Writer {
                bytes: Vec::new(),
                cutoff,
                interrupted: false,
                zero: false,
                flushed: false,
            };
            let mut completed = false;
            let result = reply::complete(&mut output, PAYLOAD, |writer| {
                assert_eq!(writer.bytes, PAYLOAD);
                completed = true;
                if shutdown_failure {
                    Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        SHUTDOWN_ERROR,
                    ))
                } else {
                    Ok(())
                }
            });
            assert_eq!(output.bytes.as_slice(), PAYLOAD.get(..cutoff).unwrap());
            assert_eq!(completed, cutoff == PAYLOAD.len());
            assert!(!output.flushed);
            if cutoff < PAYLOAD.len() {
                let error = result.unwrap_err();
                assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
                assert_eq!(error.to_string(), ORIGINAL_ERROR);
            } else if shutdown_failure {
                let error = result.unwrap_err();
                assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
                assert_eq!(error.to_string(), SHUTDOWN_ERROR);
            } else {
                result.unwrap();
            }
        }
    }
}

#[test]
fn completion_recovers_an_interrupted_write_before_shutdown() {
    let mut output = Writer {
        bytes: Vec::new(),
        cutoff: PAYLOAD.len(),
        interrupted: true,
        zero: false,
        flushed: false,
    };
    reply::complete(&mut output, PAYLOAD, |writer| {
        assert_eq!(writer.bytes, PAYLOAD);
        Ok(())
    })
    .unwrap();
    assert!(!output.flushed);
}

#[test]
fn completion_stops_on_write_zero_without_shutdown() {
    let mut output = Writer {
        bytes: Vec::new(),
        cutoff: PAYLOAD.len(),
        interrupted: false,
        zero: true,
        flushed: false,
    };
    let error = reply::complete(&mut output, PAYLOAD, |_| {
        panic!("an incomplete write cannot finish the socket")
    })
    .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::WriteZero);
    assert_eq!(output.bytes, Vec::<u8>::new());
    assert!(!output.flushed);
}

#[test]
fn completion_finishes_an_empty_reply_without_writing() {
    let mut output = Writer {
        bytes: Vec::new(),
        cutoff: 0,
        interrupted: false,
        zero: true,
        flushed: false,
    };
    let mut completed = false;
    reply::complete(&mut output, &[], |_| {
        completed = true;
        Ok(())
    })
    .unwrap();
    assert!(completed);
    assert_eq!(output.bytes, Vec::<u8>::new());
    assert!(!output.flushed);
}
