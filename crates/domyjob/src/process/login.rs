use std::ffi::OsString;
use std::io;
use std::net::Shutdown;
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::os::unix::fs::FileTypeExt as _;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;

use domyjob_core::domain::JobId;
use domyjob_core::wire::{self, MAX_CONTROL_BYTES};
use domyjob_core::worker_handoff::{HandoffFailure, HandoffPhase};
use serde::{Deserialize, Serialize};

use super::{ProcessError, Tool, run_tool};
use crate::layout::State;
use crate::lock::OsLock;
use crate::platform::{self, user_files};

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

struct PreparedHandoff(Vec<u8>);

impl PreparedHandoff {
    fn new(request: &Handoff) -> Result<Self, ProcessError> {
        let bytes = wire::frame(request).map_err(|error| match error {
            wire::WireError::TooLarge => {
                refused("the login-session hand-off exceeds the 1 MiB limit")
            }
            error @ (wire::WireError::Incomplete
            | wire::WireError::Trailing
            | wire::WireError::Snapshot
            | wire::WireError::Json(_)) => refused(error),
        })?;
        Ok(Self(bytes))
    }
}

fn exchange(
    request: &Handoff,
    connect: impl FnOnce() -> Result<UnixStream, ProcessError>,
) -> Result<(), ProcessError> {
    let prepared = PreparedHandoff::new(request)?;
    let mut stream = connect()?;
    exchange_using(&mut stream, &prepared, |socket| {
        socket.shutdown(Shutdown::Write)
    })
}

fn exchange_using<S: io::Read + io::Write>(
    stream: &mut S,
    request: &PreparedHandoff,
    finish: impl FnOnce(&mut S) -> io::Result<()>,
) -> Result<(), ProcessError> {
    stream
        .write_all(&request.0)
        .map_err(|error| refused(format!("request write: {error}")))?;
    finish(stream).map_err(|error| refused(format!("request finish: {error}")))?;
    let reply = crate::bounded::read(stream, MAX_REPLY_BYTES)
        .map_err(|error| sent_refusal(format!("reply read: {error}"), request.0.len()))?
        .ok_or_else(|| {
            sent_refusal(
                "the login-session agent returned an oversized reply",
                request.0.len(),
            )
        })?;
    if reply == b"R" {
        Ok(())
    } else {
        Err(sent_refusal(
            format!(
                "the login-session agent refused the hand-off: {}",
                String::from_utf8_lossy(&reply)
            ),
            request.0.len(),
        ))
    }
}

fn sent_refusal(cause: impl std::fmt::Display, bytes: usize) -> ProcessError {
    refused(format!(
        "{cause}; sender bytes={bytes}; write_complete=true; write_half_closed=true"
    ))
}

pub(super) fn launch_worker(job: &JobId) -> Result<(), ProcessError> {
    let request = Handoff::current(job).map_err(refused)?;
    let path = platform::login_directory().map_err(refused)?.join("socket");
    exchange(&request, || hand_off(&path, logged_in()?))
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
    let mut reader = RequestReader {
        inner: &mut stream,
        bytes: 0,
        eof: false,
    };
    let started = receive_request(&mut reader, launch);
    let bytes = reader.bytes;
    let eof = reader.eof;
    match started {
        Ok(()) => reply::complete(&mut stream, b"R", |socket| socket.shutdown(Shutdown::Write)),
        Err(error) => {
            let detail = format!(
                "{}; receiver bytes={bytes}; request_eof={eof}",
                refusal_detail(&error)
            );
            reply::complete(&mut stream, detail.as_bytes(), |socket| {
                socket.shutdown(Shutdown::Write)
            })
        }
    }
}

struct RequestReader<R> {
    inner: R,
    bytes: usize,
    eof: bool,
}

impl<R: io::Read> io::Read for RequestReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let count = self.inner.read(buffer)?;
        self.bytes = self.bytes.saturating_add(count);
        if !buffer.is_empty() && count == 0 {
            self.eof = true;
        }
        Ok(count)
    }
}

fn refusal_detail(error: &HandoffFailure<io::Error>) -> String {
    let phase = match error.phase {
        HandoffPhase::Read => "request read",
        HandoffPhase::Decode => "request decode",
        HandoffPhase::Validate => "request validation",
        HandoffPhase::Launch => "worker launch",
    };
    format!("{phase}: {}", error.cause)
        .chars()
        .take(MAX_ERROR_CHARS)
        .collect()
}

fn receive_request(
    reader: impl io::Read,
    launch: impl FnOnce(Handoff) -> Result<(), ProcessError>,
) -> Result<(), HandoffFailure<io::Error>> {
    domyjob_core::worker_handoff::start(
        || read_request_frame(reader),
        |bytes| domyjob_core::ingress::json(&bytes, MAX_CONTROL_BYTES).map_err(io::Error::other),
        Handoff::validate,
        |request| launch(request).map_err(io::Error::other),
    )
}

fn read_request_frame(mut reader: impl io::Read) -> io::Result<Vec<u8>> {
    let mut header = [0; wire::CONTROL_HEADER_BYTES];
    reader.read_exact(&mut header)?;
    let length = wire::ControlLength::try_from(header).map_err(io::Error::other)?;
    let mut bytes = vec![0; length.bytes()];
    reader.read_exact(&mut bytes)?;
    Ok(bytes)
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
    use domyjob_core::domain::{IDENTIFIER_HEX_BYTES, JobId};
    use domyjob_core::wire;
    use std::os::unix::net::UnixStream;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum TransportFault {
        Write,
        Finish,
        Read,
    }

    struct FaultyTransport {
        fault: TransportFault,
        sent: Vec<u8>,
        finished: bool,
        read: bool,
        flushed: bool,
    }

    fn transport_fault() -> std::io::Error {
        std::io::Error::new(std::io::ErrorKind::WouldBlock, "injected transport fault")
    }

    impl std::io::Write for FaultyTransport {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if self.finished || self.read {
                return Err(std::io::Error::other(
                    "write occurred after request completion",
                ));
            }
            if self.fault == TransportFault::Write {
                return Err(transport_fault());
            }
            self.sent.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            self.flushed = true;
            Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "flushing cannot replace the request half-close",
            ))
        }
    }

    impl std::io::Read for FaultyTransport {
        fn read(&mut self, _bytes: &mut [u8]) -> std::io::Result<usize> {
            self.read = true;
            if !self.finished || self.fault != TransportFault::Read {
                return Err(std::io::Error::other(
                    "reply read preceded request completion",
                ));
            }
            Err(transport_fault())
        }
    }

    #[test]
    fn client_transport_faults_preserve_the_phase_and_stop_later_effects() {
        for (fault, phase) in [
            (TransportFault::Write, "request write"),
            (TransportFault::Finish, "request finish"),
            (TransportFault::Read, "reply read"),
        ] {
            let packet = request();
            let bytes = wire::frame(&packet).unwrap();
            let mut stream = FaultyTransport {
                fault,
                sent: Vec::new(),
                finished: false,
                read: false,
                flushed: false,
            };
            let prepared = super::PreparedHandoff::new(&packet).unwrap();
            let result = super::exchange_using(&mut stream, &prepared, |stream| {
                assert_eq!(stream.sent, bytes);
                assert!(!stream.read);
                stream.finished = true;
                if stream.fault == TransportFault::Finish {
                    Err(transport_fault())
                } else {
                    Ok(())
                }
            });
            let detail =
                format!("macOS login-session hand-off refused: {phase}: injected transport fault");
            let detail = if fault == TransportFault::Read {
                format!(
                    "{detail}; sender bytes={}; write_complete=true; write_half_closed=true",
                    bytes.len()
                )
            } else {
                detail
            };
            assert_eq!(result.unwrap_err().to_string(), detail);
            assert_eq!(stream.finished, fault != TransportFault::Write);
            assert_eq!(stream.read, fault == TransportFault::Read);
            assert!(!stream.flushed);
        }
    }

    #[test]
    fn request_read_fault_keeps_phase_cause_and_never_launches() {
        use std::io::Read as _;

        const MACOS_WOULD_BLOCK: i32 = 35;

        struct WouldBlock;

        impl std::io::Read for WouldBlock {
            fn read(&mut self, _buffer: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::from_raw_os_error(MACOS_WOULD_BLOCK))
            }
        }

        let mut launches = 0_u8;
        let header = 1_u32.to_be_bytes();
        let error = super::receive_request(header.as_slice().chain(WouldBlock), |_| {
            launches += 1;
            Ok(())
        })
        .unwrap_err();
        assert_eq!(launches, 0);
        assert_eq!(error.phase, super::HandoffPhase::Read);
        assert_eq!(error.cause.kind(), std::io::ErrorKind::WouldBlock);
        assert_eq!(error.cause.raw_os_error(), Some(MACOS_WOULD_BLOCK));
        let original = error.cause.to_string();
        let detail = super::refusal_detail(&error);
        assert_eq!(detail, format!("request read: {original}"));
        let mut output = Vec::new();
        let mut shutdowns = 0_u8;
        super::reply::complete(&mut output, detail.as_bytes(), |bytes| {
            assert_eq!(bytes.as_slice(), detail.as_bytes());
            shutdowns += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(output, detail.as_bytes());
        assert_eq!(shutdowns, 1);
    }

    fn request() -> Handoff {
        Handoff {
            program: b"/bin/domyjob".to_vec(),
            state: b"/tmp/state".to_vec(),
            job: JobId::try_from("1".repeat(IDENTIFIER_HEX_BYTES)).unwrap(),
            environment: vec![("PATH".to_owned(), b"/usr/bin".to_vec())],
        }
    }

    #[test]
    fn an_oversized_request_is_refused_before_connecting() {
        let mut packet = request();
        packet
            .environment
            .push(("PATH".to_owned(), vec![b'x'; wire::MAX_CONTROL_BYTES]));
        let connected = std::cell::Cell::new(false);
        let error = exchange(&packet, || {
            connected.set(true);
            Err(ProcessError::NotStarted("unexpected connection".to_owned()))
        })
        .unwrap_err();
        assert!(!connected.get());
        assert!(error.to_string().contains("1 MiB"));
    }

    #[test]
    fn request_observation_distinguishes_partial_data_from_eof_without_payloads() {
        use std::io::Read as _;

        struct Refuses;

        impl std::io::Read for Refuses {
            fn read(&mut self, _buffer: &mut [u8]) -> std::io::Result<usize> {
                Err(transport_fault())
            }
        }

        let mut complete = super::RequestReader {
            inner: &b"private payload"[..],
            bytes: 0,
            eof: false,
        };
        assert_eq!(complete.read(&mut []).unwrap(), 0);
        assert!(!complete.eof);
        crate::bounded::read(&mut complete, super::MAX_CONTROL_BYTES)
            .unwrap()
            .unwrap();
        assert_eq!(complete.bytes, b"private payload".len());
        assert!(complete.eof);

        let mut partial = super::RequestReader {
            inner: (&b"private payload"[..]).chain(Refuses),
            bytes: 0,
            eof: false,
        };
        let error = crate::bounded::read(&mut partial, super::MAX_CONTROL_BYTES).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
        assert_eq!(partial.bytes, b"private payload".len());
        assert!(!partial.eof);
    }

    #[test]
    fn actual_decode_and_validation_refuse_before_launch() {
        let malformed = super::receive_request(wire::frame(&"{").unwrap().as_slice(), |_| {
            panic!("malformed JSON cannot reach launch")
        })
        .unwrap_err();
        assert_eq!(malformed.phase, super::HandoffPhase::Decode);
        assert!(super::refusal_detail(&malformed).starts_with("request decode: "));
        for invalid_path in [false, true] {
            let mut packet = request();
            let expected = if invalid_path {
                packet.program = b"relative-worker".to_vec();
                "absolute"
            } else {
                packet
                    .environment
                    .push(("GH_TOKEN".to_owned(), b"private".to_vec()));
                "allowlist"
            };
            let bytes = wire::frame(&packet).unwrap();
            let error = super::receive_request(bytes.as_slice(), |_| {
                panic!("invalid request cannot reach launch")
            })
            .unwrap_err();
            assert_eq!(error.phase, super::HandoffPhase::Validate);
            let detail = super::refusal_detail(&error);
            assert!(detail.starts_with("request validation: "));
            assert!(detail.contains(expected));
        }
    }

    #[test]
    fn launch_refusal_retains_its_owned_process_error() {
        const MACOS_WOULD_BLOCK: i32 = 35;
        let bytes = wire::frame(&request()).unwrap();
        let mut launches = 0_u8;
        let error = super::receive_request(bytes.as_slice(), |_| {
            launches += 1;
            Err(ProcessError::Spawn {
                what: "the worker",
                source: std::io::Error::from_raw_os_error(MACOS_WOULD_BLOCK),
            })
        })
        .unwrap_err();
        assert_eq!(launches, 1);
        assert_eq!(error.phase, super::HandoffPhase::Launch);
        if let ProcessError::Spawn { what, source } = error
            .cause
            .get_ref()
            .unwrap()
            .downcast_ref::<ProcessError>()
            .unwrap()
        {
            assert_eq!(*what, "the worker");
            assert_eq!(source.kind(), std::io::ErrorKind::WouldBlock);
            assert_eq!(source.raw_os_error(), Some(MACOS_WOULD_BLOCK));
        } else {
            panic!("the original spawn error must survive");
        }
        assert!(super::refusal_detail(&error).starts_with("worker launch: starting the worker: "));
    }

    #[test]
    fn legacy_refusal_text_remains_a_client_refusal() {
        let (client, mut server) = UnixStream::pair().unwrap();
        let receiver = std::thread::spawn(move || {
            crate::bounded::read(&mut server, super::MAX_CONTROL_BYTES)
                .unwrap()
                .unwrap();
            super::reply::complete(&mut server, b"legacy refusal", |stream| {
                stream.shutdown(std::net::Shutdown::Write)
            })
        });
        let packet = request();
        let sent = wire::frame(&packet).unwrap().len();
        let observed = exchange(&packet, || Ok(client));
        let completed = receiver.join();
        completed.unwrap().unwrap();
        assert_eq!(
            observed.unwrap_err().to_string(),
            format!(
                "macOS login-session hand-off refused: the login-session agent refused the hand-off: legacy refusal; sender bytes={sent}; write_complete=true; write_half_closed=true"
            )
        );
    }

    #[test]
    fn partial_request_eof_is_named_as_read_failure_and_never_launches() {
        let (mut client, server) = UnixStream::pair().unwrap();
        let receiver = std::thread::spawn(move || {
            let mut launches = 0_u8;
            let completed = receive(server, |_| {
                launches += 1;
                Ok(())
            });
            (completed, launches)
        });
        let partial = 1_u32.to_be_bytes();
        let sent = std::io::Write::write_all(&mut client, &partial);
        client.shutdown(std::net::Shutdown::Write).unwrap();
        let observed = crate::bounded::read(&mut client, super::MAX_REPLY_BYTES);
        drop(client);
        let joined = receiver.join();
        sent.unwrap();
        let (completed, launches) = joined.unwrap();
        completed.unwrap();
        assert_eq!(launches, 0);
        let detail = String::from_utf8(observed.unwrap().unwrap()).unwrap();
        assert!(detail.starts_with("request read: "));
        assert!(detail.contains(&format!(
            "receiver bytes={}; request_eof=true",
            partial.len()
        )));
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
                assert_eq!(received.job.as_str(), "1".repeat(IDENTIFIER_HEX_BYTES));
                assert_eq!(received.environment, request().environment);
                Ok(())
            })
            .unwrap();
        });
        exchange(&request(), || Ok(client)).unwrap();
        receiver.join().unwrap();
    }

    #[test]
    fn handoff_waits_for_the_peer_without_socket_deadlines() {
        let (client, server) = UnixStream::pair().unwrap();
        let client_observer = client.try_clone().unwrap();
        let server_observer = server.try_clone().unwrap();
        let receiver = std::thread::spawn(move || receive(server, |_| Ok(())));
        exchange(&request(), || Ok(client)).unwrap();
        receiver.join().unwrap().unwrap();
        for socket in [client_observer, server_observer] {
            assert_eq!(socket.read_timeout().unwrap(), None);
            assert_eq!(socket.write_timeout().unwrap(), None);
        }
    }

    #[test]
    fn a_complete_request_frame_does_not_wait_for_transport_eof() {
        use std::io::Read as _;

        struct NoEof;

        impl std::io::Read for NoEof {
            fn read(&mut self, _buffer: &mut [u8]) -> std::io::Result<usize> {
                Err(transport_fault())
            }
        }

        let frame = wire::frame(&request()).unwrap();
        let mut launched = false;
        super::receive_request(frame.as_slice().chain(NoEof), |_| {
            launched = true;
            Ok(())
        })
        .unwrap();
        assert!(launched);
    }

    #[test]
    fn readiness_can_answer_before_the_sender_closes_its_write_side() {
        use std::io::Write as _;

        let (mut client, server) = UnixStream::pair().unwrap();
        let receiver = std::thread::spawn(move || receive(server, |_| Ok(())));
        client.write_all(&wire::frame(&request()).unwrap()).unwrap();
        let reply = crate::bounded::read(&mut client, super::MAX_REPLY_BYTES).unwrap();
        drop(client);
        receiver.join().unwrap().unwrap();
        assert_eq!(reply, Some(b"R".to_vec()));
    }

    #[test]
    fn oversized_and_truncated_request_frames_cannot_launch() {
        let oversized = u32::try_from(super::MAX_CONTROL_BYTES + 1)
            .unwrap()
            .to_be_bytes();
        let truncated = 1_u32.to_be_bytes();
        for bytes in [oversized.as_slice(), truncated.as_slice(), &[]] {
            let error = super::receive_request(bytes, |_| panic!("invalid frame cannot launch"))
                .unwrap_err();
            assert_eq!(error.phase, super::HandoffPhase::Read);
        }
    }

    fn reply_with_held_receiver(
        launch: impl FnOnce(Handoff) -> Result<(), ProcessError> + Send + 'static,
    ) -> std::io::Result<Option<Vec<u8>>> {
        let (mut client, server) = UnixStream::pair()?;
        let held = server.try_clone()?;
        let receiver = std::thread::spawn(move || receive(server, launch));
        let bytes = wire::frame(&request()).map_err(std::io::Error::other)?;
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
            let bytes = wire::frame(&packet).unwrap().len();
            let error = exchange(&packet, || Ok(client)).unwrap_err();
            let cause = if unapproved {
                "allowlist"
            } else {
                "worker refused"
            };
            let detail = error.to_string();
            assert!(detail.contains(cause));
            assert!(detail.contains(&format!("receiver bytes={bytes}; request_eof=false")));
            assert!(detail.contains(&format!(
                "sender bytes={bytes}; write_complete=true; write_half_closed=true"
            )));
            assert!(!detail.contains("never-forward"));
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
        packet
            .environment
            .push(("PATH".to_owned(), vec![b'x'; wire::MAX_CONTROL_BYTES]));
        assert!(
            exchange(&packet, || Ok(client))
                .unwrap_err()
                .to_string()
                .contains("1 MiB")
        );
    }
}
