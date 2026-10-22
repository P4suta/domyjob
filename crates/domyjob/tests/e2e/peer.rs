use std::fmt;
use std::io::{self, Read as _};
use std::process::{Child, ChildStdin, Command, ExitStatus, Output, Stdio};

use tempfile::NamedTempFile;

const REPORT_INTERVAL_MILLIS: u128 = 10_000;
const DIAGNOSTIC_BYTES: usize = 4096;

mod raw {
    #![expect(
        clippy::disallowed_methods,
        clippy::disallowed_types,
        reason = "peer clocks only pace diagnostics; output reads observe the owned capture files"
    )]

    use std::io::Seek as _;
    use std::sync::OnceLock;
    use std::time::{Duration, Instant};

    const POLL_INTERVAL: Duration = Duration::from_millis(50);
    static START: OnceLock<Instant> = OnceLock::new();

    pub(super) fn now() -> u128 {
        START.get_or_init(Instant::now).elapsed().as_millis()
    }

    pub(super) fn pause() {
        std::thread::sleep(POLL_INTERVAL);
    }

    pub(super) fn read(
        input: &mut impl std::io::Read,
        bytes: &mut Vec<u8>,
    ) -> std::io::Result<usize> {
        input.read_to_end(bytes)
    }

    pub(super) fn available(input: &mut std::fs::File) -> std::io::Result<u64> {
        Ok(input
            .metadata()?
            .len()
            .saturating_sub(input.stream_position()?))
    }
}

#[derive(Debug)]
pub(crate) struct Wait {
    process: String,
    request: String,
    reported: u128,
}

impl Wait {
    pub(crate) fn new(process: impl Into<String>, request: impl Into<String>) -> Self {
        Self {
            process: process.into(),
            request: request.into(),
            reported: raw::now(),
        }
    }

    pub(crate) fn pause(&mut self, received: impl fmt::Display) {
        if raw::now().saturating_sub(self.reported) >= REPORT_INTERVAL_MILLIS {
            eprintln!(
                "e2e waiting for {}: {}; last bytes received: {received}",
                self.process, self.request
            );
            self.reported = raw::now();
        }
        raw::pause();
    }
}

#[derive(Debug)]
pub(crate) struct Peer {
    child: Child,
    output: NamedTempFile,
    errors: NamedTempFile,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    label: String,
}

impl Peer {
    pub(crate) fn spawn(mut command: Command, label: String) -> io::Result<Self> {
        let output = NamedTempFile::new()?;
        let errors = NamedTempFile::new()?;
        let child = command
            .stdout(Stdio::from(output.reopen()?))
            .stderr(Stdio::from(errors.reopen()?))
            .spawn()?;
        Ok(Self {
            child,
            output,
            errors,
            stdout: Vec::new(),
            stderr: Vec::new(),
            label,
        })
    }

    pub(crate) const fn input(&mut self) -> Option<ChildStdin> {
        self.child.stdin.take()
    }

    pub(crate) fn waiting(&self, request: &str) -> Wait {
        Wait::new(format!("{} (pid {})", self.label, self.child.id()), request)
    }

    pub(crate) fn read(&mut self) -> io::Result<()> {
        let output = raw::available(self.output.as_file_mut())?;
        let errors = raw::available(self.errors.as_file_mut())?;
        raw::read(
            &mut self.output.as_file_mut().take(output),
            &mut self.stdout,
        )?;
        raw::read(
            &mut self.errors.as_file_mut().take(errors),
            &mut self.stderr,
        )?;
        Ok(())
    }

    pub(crate) fn stdout(&self) -> &[u8] {
        &self.stdout
    }

    pub(crate) fn read_line(&self, offset: &mut usize) -> io::Result<Option<String>> {
        let bytes = self.stdout.get(*offset..).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "output offset exceeds the captured bytes",
            )
        })?;
        let Some(end) = bytes.iter().position(|byte| *byte == b'\n') else {
            return Ok(None);
        };
        let line = std::str::from_utf8(bytes.get(..end).unwrap_or_default())
            .map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    self.failure("output line", error),
                )
            })?
            .to_owned();
        *offset = offset.saturating_add(end).saturating_add(1);
        Ok(Some(line))
    }

    pub(crate) fn poll(&mut self) -> io::Result<Option<ExitStatus>> {
        self.read()?;
        let exited = self.child.try_wait()?;
        if exited.is_some() {
            self.read()?;
        }
        Ok(exited)
    }

    pub(crate) fn diagnostic(&self, request: &str) -> String {
        format!(
            "{} (pid {}), request {request}; stdout tail: {:?}; stderr tail: {:?}",
            self.label,
            self.child.id(),
            tail(&self.stdout),
            tail(&self.stderr)
        )
    }

    pub(crate) fn failure(&self, request: &str, cause: impl fmt::Display) -> io::Error {
        io::Error::other(format!("{cause}; {}", self.diagnostic(request)))
    }

    pub(crate) fn wait(&mut self, request: &str) -> io::Result<Output> {
        let mut pending = self.waiting(request);
        loop {
            if let Some(status) = self.poll()? {
                return Ok(Output {
                    status,
                    stdout: std::mem::take(&mut self.stdout),
                    stderr: std::mem::take(&mut self.stderr),
                });
            }
            pending.pause(self.diagnostic(request));
        }
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        match self.child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => {}
            Err(error) => eprintln!("e2e inspecting {} during cleanup: {error}", self.label),
        }
        if let Err(error) = self.child.kill() {
            eprintln!("e2e stopping {} during cleanup: {error}", self.label);
        }
        if let Err(error) = self.child.wait() {
            eprintln!("e2e reaping {} during cleanup: {error}", self.label);
        }
    }
}

fn tail(bytes: &[u8]) -> String {
    let bytes = bytes
        .get(bytes.len().saturating_sub(DIAGNOSTIC_BYTES)..)
        .unwrap_or_default();
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_owned(),
        Err(_invalid_utf8) => bytes.escape_ascii().to_string(),
    }
}
