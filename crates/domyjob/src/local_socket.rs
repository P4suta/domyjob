use std::io::{ErrorKind, Read, Write};
use std::net::Shutdown;
use std::path::Path;

use crate::platform::{UnixListener, UnixStream};

#[derive(Debug)]
pub struct Listener(UnixListener);

#[derive(Debug)]
pub struct Stream(UnixStream);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reach<T> {
    Reached(T),
    NobodyListening,
}

pub const PATH_LIMIT: usize = crate::platform::SOCKET_PATH_LIMIT;

fn fits(path: &Path) -> std::io::Result<()> {
    let length = path.as_os_str().as_encoded_bytes().len();
    if length > PATH_LIMIT {
        return Err(std::io::Error::new(
            ErrorKind::InvalidInput,
            format!(
                "{} is {length} bytes long, and a local socket path may be at most {PATH_LIMIT}; set DOMYJOB_STATE to a shorter directory",
                path.display()
            ),
        ));
    }
    Ok(())
}

impl Listener {
    pub fn bind(path: &Path) -> std::io::Result<Self> {
        fits(path)?;
        UnixListener::bind(path).map(Self)
    }

    pub fn accept(&self) -> std::io::Result<Stream> {
        self.0.accept().map(|(stream, _)| Stream(stream))
    }
}

impl Stream {
    pub fn connect(path: &Path) -> std::io::Result<Reach<Self>> {
        fits(path)?;
        match UnixStream::connect(path) {
            Ok(stream) => Ok(Reach::Reached(Self(stream))),
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::NotFound | ErrorKind::ConnectionRefused
                ) =>
            {
                Ok(Reach::NobodyListening)
            }
            Err(error) => Err(error),
        }
    }

    pub fn close_both(&self) -> std::io::Result<()> {
        ignore_disconnected(self.0.shutdown(Shutdown::Both))
    }

    pub fn close_sending(&self) -> std::io::Result<()> {
        ignore_disconnected(self.0.shutdown(Shutdown::Write))
    }
}

fn ignore_disconnected(result: std::io::Result<()>) -> std::io::Result<()> {
    match result {
        Err(error) if error.kind() == ErrorKind::NotConnected => Ok(()),
        Ok(()) | Err(_) => result,
    }
}

impl Read for &Stream {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        match (&self.0).read(buffer) {
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::ConnectionAborted | ErrorKind::ConnectionReset
                ) =>
            {
                Ok(0)
            }
            Ok(read) => Ok(read),
            Err(error) => Err(error),
        }
    }
}

impl Write for &Stream {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        (&self.0).write(bytes)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        (&self.0).flush()
    }
}
