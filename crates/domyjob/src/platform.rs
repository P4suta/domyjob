#[cfg(unix)]
pub use std::os::unix::net::{UnixListener, UnixStream};

#[cfg(windows)]
pub use uds_windows::{UnixListener, UnixStream};

pub const SOCKET_PATH_LIMIT: usize = if cfg!(windows) { 107 } else { 103 };
