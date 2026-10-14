use std::ffi::OsString;
use std::io::{self, Write as _};
use std::net::Shutdown;
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::os::unix::fs::FileTypeExt as _;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;

use domyjob_core::domain::JobId;
use domyjob_core::wire::MAX_CONTROL_BYTES;
use serde::{Deserialize, Serialize};

use super::{ProcessError, Tool, run_tool};
use crate::layout::State;
use crate::lock::OsLock;
use crate::platform::{self, clock, user_files};

const IO_TIMEOUT_MILLIS: u64 = 30_000;
const MAX_REPLY_BYTES: usize = 16_385;
const MAX_ERROR_CHARS: usize = 4096;

mod reply;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Handoff {
    program: Vec<u8>,
    state: Vec<u8>,
    job: JobId,
    environment: Vec<(String, Vec<u8>)>,
}

impl Handoff {
    fn current(job: &JobId) -> io::Result<Self> {
        Ok(Self {
            program: std::env::current_exe()?.as_os_str().as_bytes().to_vec(),
            state: std::path::absolute(State::here()?.root())?
                .as_os_str()
                .as_bytes()
                .to_vec(),
            job: job.clone(),
            environment: platform::job_environment_names()
                .filter_map(|name| {
                    std::env::var_os(name)
                        .map(|value| (name.to_owned(), value.as_os_str().as_bytes().to_vec()))
                })
                .collect(),
        })
    }

    fn validate(&self) -> io::Result<()> {
        for bytes in [&self.program, &self.state] {
            if !Path::new(std::ffi::OsStr::from_bytes(bytes)).is_absolute() || bytes.contains(&0) {
                return Err(io::Error::other(
                    "the worker and state paths must be absolute",
                ));
            }
        }
        if self.environment.iter().any(|(name, value)| {
            !platform::job_environment_names().any(|allowed| name == allowed) || value.contains(&0)
        }) {
            return Err(io::Error::other(
                "the job environment violates its allowlist",
            ));
        }
        Ok(())
    }
}

fn refused(cause: impl std::fmt::Display) -> ProcessError {
    ProcessError::Login(cause.to_string())
}

fn logged_in() -> Result<bool, ProcessError> {
    let user = platform::user_id().ok_or_else(|| refused("user UID unavailable"))?;
    let arguments = ["print".to_owned(), format!("gui/{user}")];
    let output = run_tool(&Tool::new("launchctl", &arguments)).map_err(refused)?;
    Ok(output.success)
}

fn hand_off(path: &Path, logged_in: bool) -> Result<UnixStream, ProcessError> {
    if !logged_in {
        return Err(refused("the user is not logged in to a macOS GUI session"));
    }
    UnixStream::connect(path).map_err(|error| {
        refused(format!(
            "the login-session agent is missing or unavailable ({error}); \
             run `domyjob chat service install` inside the user's login session"
        ))
    })
}

fn exchange(mut stream: UnixStream, request: &Handoff) -> Result<(), ProcessError> {
    clock::socket_timeout(&stream, IO_TIMEOUT_MILLIS).map_err(refused)?;
    let bytes = serde_json::to_vec(request).map_err(refused)?;
    if bytes.len() > MAX_CONTROL_BYTES {
        return Err(refused(
            "the login-session hand-off exceeds the 1 MiB limit",
        ));
    }
    stream.write_all(&bytes).map_err(refused)?;
    stream.shutdown(Shutdown::Write).map_err(refused)?;
    let reply = crate::bounded::read(&mut stream, MAX_REPLY_BYTES)
        .map_err(refused)?
        .ok_or_else(|| refused("the login-session agent returned an oversized reply"))?;
    if reply == b"R" {
        Ok(())
    } else {
        Err(refused(format!(
            "the login-session agent did not start the worker: {}",
            String::from_utf8_lossy(&reply)
        )))
    }
}

pub(super) fn launch_worker(job: &JobId) -> Result<(), ProcessError> {
    let path = platform::login_directory().map_err(refused)?.join("socket");
    let stream = hand_off(&path, logged_in()?)?;
    exchange(stream, &Handoff::current(job).map_err(refused)?)
}

fn launch(request: Handoff) -> Result<(), ProcessError> {
    let mut command = super::command(OsString::from_vec(request.program));
    command
        .arg("worker")
        .arg(request.job.as_str())
        .env_clear()
        .envs(
            request
                .environment
                .into_iter()
                .map(|(name, bytes)| (name, OsString::from_vec(bytes))),
        )
        .env("DOMYJOB_STATE", OsString::from_vec(request.state));
    super::unix::launch_worker_command(command, None)
}

fn receive(
    mut stream: UnixStream,
    launch: impl FnOnce(Handoff) -> Result<(), ProcessError>,
) -> io::Result<()> {
    clock::socket_timeout(&stream, IO_TIMEOUT_MILLIS)?;
    let started = (|| {
        let bytes = crate::bounded::read(&mut stream, MAX_CONTROL_BYTES)?.ok_or_else(|| {
            io::Error::other("the login-session hand-off exceeds the 1 MiB limit")
        })?;
        let request: Handoff =
            domyjob_core::ingress::json(&bytes, MAX_CONTROL_BYTES).map_err(io::Error::other)?;
        request.validate()?;
        launch(request).map_err(io::Error::other)
    })();
    match started {
        Ok(()) => reply::complete(&mut stream, b"R", |socket| socket.shutdown(Shutdown::Write)),
        Err(error) => {
            let detail: String = error.to_string().chars().take(MAX_ERROR_CHARS).collect();
            reply::complete(&mut stream, detail.as_bytes(), |socket| {
                socket.shutdown(Shutdown::Write)
            })
        }
    }
}

fn bind(directory: &Path) -> io::Result<(UnixListener, OsLock)> {
    crate::state_io::private_dir(directory).map_err(io::Error::other)?;
    let guard = OsLock::try_exclusive(&directory.join("serve.lock"))
        .map_err(io::Error::other)?
        .ok_or_else(|| io::Error::other("the login-session agent is already running"))?;
    let path = directory.join("socket");
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_socket() => {
            user_files::remove(&path).map_err(io::Error::other)?;
        }
        Ok(_) => {
            return Err(io::Error::other(
                "the login-session socket path is not a socket",
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    Ok((UnixListener::bind(path)?, guard))
}

pub(crate) fn start() -> io::Result<()> {
    let arguments = ["managername".to_owned()];
    let manager = run_tool(&Tool::new("launchctl", &arguments)).map_err(io::Error::other)?;
    if !manager.success || manager.stdout.trim() != "Aqua" {
        return Ok(());
    }
    let (listener, guard) = bind(&platform::login_directory()?)?;
    std::thread::spawn(move || {
        let _running = guard;
        for connection in listener.incoming() {
            match connection {
                Ok(stream) => {
                    std::thread::spawn(move || {
                        if let Err(error) = receive(stream, launch) {
                            eprintln!("domyjob: login-session hand-off failed: {error}");
                        }
                    });
                }
                Err(error) => {
                    eprintln!("domyjob: accepting a login-session hand-off failed: {error}");
                }
            }
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Handoff, bind, exchange, hand_off, receive};
    use crate::process::ProcessError;
    use domyjob_core::domain::JobId;
    use std::os::unix::net::UnixStream;

    const JOB_ID_HEX_CHARS: usize = 32;

    fn request() -> Handoff {
        Handoff {
            program: b"/bin/domyjob".to_vec(),
            state: b"/tmp/state".to_vec(),
            job: JobId::try_from("1".repeat(JOB_ID_HEX_CHARS)).unwrap(),
            environment: vec![("PATH".to_owned(), b"/usr/bin".to_vec())],
        }
    }

    #[test]
    fn a_missing_login_session_is_named_and_never_falls_back() {
        let error = hand_off(std::path::Path::new("unused"), false).unwrap_err();
        assert!(error.to_string().contains("not logged in"));
    }

    #[test]
    fn a_missing_login_agent_is_named_and_never_falls_back() {
        let root = tempfile::tempdir_in("/private/tmp").unwrap();
        let error = hand_off(&root.path().join("absent.sock"), true).unwrap_err();
        assert!(error.to_string().contains("login-session agent"));
    }

    #[test]
    fn handoff_preserves_worker_job_state_and_allowed_environment_before_readiness() {
        let (client, server) = UnixStream::pair().unwrap();
        let receiver = std::thread::spawn(move || {
            receive(server, |received| {
                assert_eq!(received.program, b"/bin/domyjob");
                assert_eq!(received.state, b"/tmp/state");
                assert_eq!(received.job.as_str(), "1".repeat(JOB_ID_HEX_CHARS));
                assert_eq!(received.environment, request().environment);
                Ok(())
            })
            .unwrap();
        });
        exchange(client, &request()).unwrap();
        receiver.join().unwrap();
    }

    fn reply_with_held_receiver(
        launch: impl FnOnce(Handoff) -> Result<(), ProcessError> + Send + 'static,
    ) -> std::io::Result<Option<Vec<u8>>> {
        const OBSERVATION_TIMEOUT_MILLIS: u64 = 100;
        let (mut client, server) = UnixStream::pair()?;
        super::clock::socket_timeout(&client, OBSERVATION_TIMEOUT_MILLIS)?;
        let held = server.try_clone()?;
        let receiver = std::thread::spawn(move || receive(server, launch));
        let bytes = serde_json::to_vec(&request()).map_err(std::io::Error::other)?;
        std::io::Write::write_all(&mut client, &bytes)?;
        client.shutdown(std::net::Shutdown::Write)?;
        receiver
            .join()
            .map_err(|_panic| std::io::Error::other("the receiver fixture panicked"))??;
        let observed = crate::bounded::read(&mut client, super::MAX_REPLY_BYTES);
        drop(held);
        observed
    }

    #[test]
    fn readiness_reply_finishes_while_another_receiver_handle_remains_open() {
        assert_eq!(
            reply_with_held_receiver(|_| Ok(())).unwrap(),
            Some(b"R".to_vec())
        );
    }

    #[test]
    fn refusal_reply_finishes_while_another_receiver_handle_remains_open() {
        let reply = reply_with_held_receiver(|_| {
            Err(ProcessError::NotStarted("worker refused".to_owned()))
        })
        .unwrap()
        .unwrap();
        assert!(String::from_utf8(reply).unwrap().contains("worker refused"));
    }

    #[test]
    fn a_refused_worker_or_unapproved_environment_cannot_announce_readiness() {
        for unapproved in [false, true] {
            let (client, server) = UnixStream::pair().unwrap();
            let receiver = std::thread::spawn(move || {
                receive(server, |_| {
                    assert!(!unapproved, "unapproved input must fail before launch");
                    Err(ProcessError::NotStarted("worker refused".to_owned()))
                })
                .unwrap();
            });
            let mut packet = request();
            if unapproved {
                packet
                    .environment
                    .push(("GH_TOKEN".to_owned(), b"never-forward".to_vec()));
            }
            let error = exchange(client, &packet).unwrap_err();
            let cause = if unapproved {
                "allowlist"
            } else {
                "worker refused"
            };
            assert!(error.to_string().contains(cause));
            receiver.join().unwrap();
        }
    }

    #[test]
    fn the_socket_is_private_single_owner_and_recovers_after_agent_exit() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = tempfile::tempdir_in("/private/tmp").unwrap();
        let directory = root.path().join("agent");
        let (listener, guard) = bind(&directory).unwrap();
        assert_eq!(
            std::fs::metadata(&directory).unwrap().permissions().mode() & 0o077,
            0
        );
        assert!(
            bind(&directory)
                .unwrap_err()
                .to_string()
                .contains("already running")
        );
        drop(listener);
        drop(guard);
        let (_listener, _guard) = bind(&directory).unwrap();
    }

    #[test]
    fn invalid_paths_and_oversized_requests_are_refused_before_launch() {
        for path in [b"relative".to_vec(), b"/tmp/nul\0".to_vec()] {
            let mut packet = request();
            packet.program = path;
            assert!(
                packet
                    .validate()
                    .unwrap_err()
                    .to_string()
                    .contains("absolute")
            );
        }
        let (client, _server) = UnixStream::pair().unwrap();
        let mut packet = request();
        packet.environment.push((
            "PATH".to_owned(),
            vec![b'x'; domyjob_core::wire::MAX_CONTROL_BYTES],
        ));
        assert!(
            exchange(client, &packet)
                .unwrap_err()
                .to_string()
                .contains("1 MiB")
        );
    }
}
