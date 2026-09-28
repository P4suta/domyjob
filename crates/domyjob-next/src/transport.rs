#![expect(
    clippy::disallowed_methods,
    reason = "this module owns the single OpenSSH process boundary"
)]
#![expect(
    clippy::redundant_pub_crate,
    reason = "the composition root needs these names but the binary has no public API"
)]

use std::io::{Read, Write};
use std::process::{Child, Command, ExitStatus, Stdio};

use domyjob_core::domain::{Invalid, MachineName};
use domyjob_core::ingress;
use domyjob_core::wire::{self, Reply, Request, WireError};
use thiserror::Error;

#[derive(Debug, Error)]
pub(crate) enum TransportError {
    #[error(transparent)]
    Invalid(#[from] Invalid),
    #[error(transparent)]
    Wire(#[from] WireError),
    #[error("SSH or node I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("remote node did not exit successfully: {0}")]
    Remote(ExitStatus),
    #[error("remote node sent a reply other than hello")]
    UnexpectedReply,
}

struct SshChild {
    child: Child,
    finished: bool,
}

impl Drop for SshChild {
    fn drop(&mut self) {
        if !self.finished {
            let _killed = self.child.kill();
            let _reaped = self.child.wait();
        }
    }
}

impl SshChild {
    fn start(machine: &MachineName) -> Result<Self, TransportError> {
        let child = Command::new("ssh")
            .args([
                "-T",
                "-o",
                "BatchMode=yes",
                "--",
                machine.as_str(),
                "domyjob-next node",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()?;
        Ok(Self {
            child,
            finished: false,
        })
    }

    fn wait(&mut self) -> Result<ExitStatus, TransportError> {
        let status = self.child.wait()?;
        self.finished = true;
        Ok(status)
    }
}

fn read_frame(input: &mut impl Read) -> Result<Vec<u8>, TransportError> {
    let mut header = [0_u8; 4];
    input.read_exact(&mut header)?;
    let length = wire::ControlLength::try_from(header)?;
    let mut body = vec![0_u8; length.bytes()];
    input.read_exact(&mut body)?;
    let mut frame = Vec::with_capacity(length.bytes().saturating_add(4));
    frame.extend_from_slice(&header);
    frame.extend_from_slice(&body);
    Ok(frame)
}

fn require_end(input: &mut impl Read) -> Result<(), TransportError> {
    let mut extra = [0_u8; 1];
    if input.read(&mut extra)? == 0 {
        Ok(())
    } else {
        Err(WireError::Trailing.into())
    }
}

pub(crate) fn doctor(machine: &MachineName) -> Result<(), TransportError> {
    let mut child = SshChild::start(machine)?;
    let mut stdin = child
        .child
        .stdin
        .take()
        .ok_or_else(|| std::io::Error::other("SSH standard input was not piped"))?;
    stdin.write_all(&wire::frame(&Request::Hello)?)?;
    drop(stdin);
    let mut stdout = child
        .child
        .stdout
        .take()
        .ok_or_else(|| std::io::Error::other("SSH standard output was not piped"))?;
    let reply = ingress::reply(&read_frame(&mut stdout)?)?;
    require_end(&mut stdout)?;
    drop(stdout);
    let status = child.wait()?;
    if !status.success() {
        return Err(TransportError::Remote(status));
    }
    match reply {
        Reply::Hello => {
            println!("{}: ready (wire {})", machine.as_str(), wire::VERSION);
            Ok(())
        }
        Reply::Accepted { .. }
        | Reply::Jobs { .. }
        | Reply::Status { .. }
        | Reply::Logs { .. }
        | Reply::Stopped
        | Reply::Error { .. } => Err(TransportError::UnexpectedReply),
    }
}

pub(crate) fn node() -> Result<(), TransportError> {
    let mut input = std::io::stdin().lock();
    let request = ingress::request(&read_frame(&mut input)?)?;
    if request == Request::Hello {
        require_end(&mut input)?;
    }
    drop(input);
    let reply = match request {
        Request::Hello => Reply::Hello,
        Request::Submit { .. }
        | Request::List
        | Request::Status { .. }
        | Request::Logs { .. }
        | Request::Wait { .. }
        | Request::Kill { .. } => Reply::Error {
            code: wire::ErrorCode::InvalidRequest,
        },
    };
    let mut output = std::io::stdout().lock();
    output.write_all(&wire::frame(&reply)?)?;
    output.flush()?;
    Ok(())
}
