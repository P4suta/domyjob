use std::io::{ErrorKind, Read, Write};
use std::path::Path;

#[derive(Debug)]
pub struct Listener(platform::Listener);

#[derive(Debug)]
pub struct Stream(platform::Stream);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reach<T> {
    Reached(T),
    NobodyListening,
}

pub const PATH_LIMIT: usize = if cfg!(windows) { 107 } else { 103 };

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
        platform::Listener::bind(path).map(Self)
    }

    pub fn accept(&self) -> std::io::Result<Stream> {
        self.0.accept().map(Stream)
    }
}

impl Stream {
    pub fn connect(path: &Path) -> std::io::Result<Reach<Self>> {
        fits(path)?;
        match platform::Stream::connect(path) {
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
        self.0.close_both()
    }

    pub fn close_sending(&self) -> std::io::Result<()> {
        self.0.close_sending()
    }
}

impl Read for &Stream {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        (&self.0).read(buffer)
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

#[cfg(unix)]
mod platform {
    use std::io::{Read, Write};
    use std::net::Shutdown;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::Path;

    #[derive(Debug)]
    pub(super) struct Listener(UnixListener);

    #[derive(Debug)]
    pub(super) struct Stream(UnixStream);

    impl Listener {
        pub(super) fn bind(path: &Path) -> std::io::Result<Self> {
            UnixListener::bind(path).map(Self)
        }

        pub(super) fn accept(&self) -> std::io::Result<Stream> {
            self.0.accept().map(|(stream, _)| Stream(stream))
        }
    }

    impl Stream {
        pub(super) fn connect(path: &Path) -> std::io::Result<Self> {
            UnixStream::connect(path).map(Self)
        }

        pub(super) fn close_both(&self) -> std::io::Result<()> {
            ignore_disconnected(self.0.shutdown(Shutdown::Both))
        }

        pub(super) fn close_sending(&self) -> std::io::Result<()> {
            ignore_disconnected(self.0.shutdown(Shutdown::Write))
        }
    }

    fn ignore_disconnected(result: std::io::Result<()>) -> std::io::Result<()> {
        match result {
            Err(error) if error.kind() == std::io::ErrorKind::NotConnected => Ok(()),
            Ok(()) | Err(_) => result,
        }
    }

    impl Read for &Stream {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            (&self.0).read(buffer)
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
}

#[cfg(windows)]
#[expect(
    unsafe_code,
    reason = "Rust's standard library has no AF_UNIX sockets on Windows; this wraps the Winsock calls"
)]
mod platform {
    use std::io::{Read, Write};
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    use std::sync::OnceLock;

    use windows_sys::Win32::Networking::WinSock::{
        ADDRESS_FAMILY, AF_UNIX, INVALID_SOCKET, SD_BOTH, SD_SEND, SOCK_STREAM, SOCKADDR,
        SOCKADDR_UN, SOCKET, SOCKET_ERROR, WSADATA, WSAGetLastError, WSAStartup, accept, bind,
        closesocket, connect, listen, recv, send, shutdown, socket,
    };

    #[derive(Debug)]
    struct Socket(SOCKET);

    impl Drop for Socket {
        fn drop(&mut self) {
            unsafe { closesocket(self.0) };
        }
    }

    #[derive(Debug)]
    pub(super) struct Listener(Socket);

    #[derive(Debug)]
    pub(super) struct Stream(Socket);

    fn last_error() -> std::io::Error {
        std::io::Error::from_raw_os_error(unsafe { WSAGetLastError() })
    }

    fn started() -> std::io::Result<()> {
        static STARTED: OnceLock<i32> = OnceLock::new();
        let code = *STARTED.get_or_init(|| {
            let mut data = WSADATA::default();
            unsafe { WSAStartup(0x0202, &raw mut data) }
        });
        match code {
            0 => Ok(()),
            code => Err(std::io::Error::from_raw_os_error(code)),
        }
    }

    fn open() -> std::io::Result<Socket> {
        started()?;
        let raw = unsafe { socket(i32::from(AF_UNIX), SOCK_STREAM, 0) };
        if raw == INVALID_SOCKET {
            Err(last_error())
        } else {
            Ok(Socket(raw))
        }
    }

    fn address(path: &Path) -> std::io::Result<(SOCKADDR_UN, i32)> {
        let mut address = SOCKADDR_UN {
            sun_family: ADDRESS_FAMILY::from(AF_UNIX),
            sun_path: [0; 108],
        };
        let wide: Vec<u16> = path.as_os_str().encode_wide().collect();
        let text = String::from_utf16(&wide).map_err(|_unpaired| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "the socket path is not UTF-16",
            )
        })?;
        let bytes = text.as_bytes();
        if bytes.len() >= address.sun_path.len() || bytes.contains(&0) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "the socket path is too long",
            ));
        }
        for (slot, byte) in address.sun_path.iter_mut().zip(bytes) {
            *slot = i8::from_ne_bytes([*byte]);
        }
        let length = i32::try_from(size_of::<SOCKADDR_UN>()).map_err(|_huge| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "the address is too large")
        })?;
        Ok((address, length))
    }

    impl Listener {
        pub(super) fn bind(path: &Path) -> std::io::Result<Self> {
            let socket = open()?;
            let (address, length) = address(path)?;
            if unsafe { bind(socket.0, (&raw const address).cast::<SOCKADDR>(), length) }
                == SOCKET_ERROR
            {
                return Err(last_error());
            }
            if unsafe { listen(socket.0, 64) } == SOCKET_ERROR {
                return Err(last_error());
            }
            Ok(Self(socket))
        }

        pub(super) fn accept(&self) -> std::io::Result<Stream> {
            let raw = unsafe { accept(self.0.0, std::ptr::null_mut(), std::ptr::null_mut()) };
            if raw == INVALID_SOCKET {
                Err(last_error())
            } else {
                Ok(Stream(Socket(raw)))
            }
        }
    }

    impl Stream {
        pub(super) fn connect(path: &Path) -> std::io::Result<Self> {
            let socket = open()?;
            let (address, length) = address(path)?;
            if unsafe { connect(socket.0, (&raw const address).cast::<SOCKADDR>(), length) }
                == SOCKET_ERROR
            {
                return Err(last_error());
            }
            Ok(Self(socket))
        }

        pub(super) fn close_both(&self) -> std::io::Result<()> {
            self.shut(SD_BOTH)
        }

        pub(super) fn close_sending(&self) -> std::io::Result<()> {
            self.shut(SD_SEND)
        }

        fn shut(&self, how: i32) -> std::io::Result<()> {
            if unsafe { shutdown(self.0.0, how) } == SOCKET_ERROR {
                let error = last_error();
                if error.kind() == std::io::ErrorKind::NotConnected {
                    return Ok(());
                }
                return Err(error);
            }
            Ok(())
        }
    }

    fn chunk(len: usize) -> i32 {
        match i32::try_from(len) {
            Ok(fits) => fits,
            Err(_huge) => i32::MAX,
        }
    }

    impl Read for &Stream {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            let got = unsafe { recv(self.0.0, buffer.as_mut_ptr(), chunk(buffer.len()), 0) };
            match usize::try_from(got) {
                Ok(read) => Ok(read),
                Err(_negative) => {
                    let error = last_error();
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::ConnectionAborted | std::io::ErrorKind::ConnectionReset
                    ) {
                        Ok(0)
                    } else {
                        Err(error)
                    }
                }
            }
        }
    }

    impl Write for &Stream {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            let sent = unsafe { send(self.0.0, bytes.as_ptr(), chunk(bytes.len()), 0) };
            usize::try_from(sent).map_err(|_negative| last_error())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
}
