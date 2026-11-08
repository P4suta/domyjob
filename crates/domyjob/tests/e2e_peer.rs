#[path = "e2e/peer.rs"]
mod peer;

use std::io::{Read as _, Write as _};
use std::process::Stdio;

use peer::Peer;

mod raw {
    #![expect(
        clippy::disallowed_methods,
        reason = "the fixture starts an owned child and writes its actual protocol stream"
    )]

    pub(super) fn command(program: std::path::PathBuf) -> std::process::Command {
        std::process::Command::new(program)
    }

    pub(super) fn stdout() -> std::io::Stdout {
        std::io::stdout()
    }
}

const FIXTURE: &str = "DOMYJOB_E2E_PEER_FIXTURE";
const PARTIAL_REPLY: &str = "reply without a newline";

fn spawn_fixture(mode: &str) -> std::io::Result<Peer> {
    let mut command = raw::command(std::env::current_exe()?);
    command
        .args(["--exact", "peer_fixture", "--ignored", "--nocapture"])
        .env(FIXTURE, mode)
        .stdin(Stdio::piped());
    Peer::spawn(command, "controlled reply peer".to_owned(), None)
}

#[test]
fn a_live_peer_keeps_partial_bytes_until_the_requested_reply() {
    let mut peer = spawn_fixture("reply").unwrap();
    let mut input = peer.input().unwrap();
    let request = "request 7: finish the reply";
    let mut pending = peer.waiting(request);
    while !String::from_utf8_lossy(peer.stdout()).contains(PARTIAL_REPLY) {
        assert!(peer.poll().unwrap().is_none());
        pending.pause(peer.diagnostic(request));
    }
    assert!(peer.poll().unwrap().is_none());
    let diagnostic = peer
        .failure(request, "waiting for a complete frame")
        .to_string();
    for expected in ["controlled reply peer", "pid", request, PARTIAL_REPLY] {
        assert!(diagnostic.contains(expected), "{diagnostic}");
    }
    input.write_all(b"R").unwrap();
    drop(input);
    while peer.poll().unwrap().is_none() {
        pending.pause(peer.diagnostic(request));
    }
    let output = peer.wait(request).unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("reply without a newline completed"));
    assert!(String::from_utf8_lossy(&output.stderr).contains("peer stderr"));
}

#[test]
fn optional_sampling_waits_for_its_owned_child_completion() {
    let mut child = raw::command(std::env::current_exe().unwrap())
        .args(["--exact", "peer_fixture", "--ignored", "--nocapture"])
        .env(FIXTURE, "exit")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let status = peer::wait_sampler(&mut child).unwrap();
    assert!(status.success());
    assert_eq!(child.try_wait().unwrap(), Some(status));
}

#[test]
fn exit_without_a_reply_retains_the_process_request_and_last_bytes() {
    let mut peer = spawn_fixture("exit").unwrap();
    drop(peer.input());
    let request = "request 8: missing reply";
    let mut pending = peer.waiting(request);
    let status = loop {
        if let Some(status) = peer.poll().unwrap() {
            break status;
        }
        pending.pause(peer.diagnostic(request));
    };
    let failure = peer.failure(request, status).to_string();
    for expected in [
        "controlled reply peer",
        "pid",
        request,
        PARTIAL_REPLY,
        "peer stderr",
    ] {
        assert!(failure.contains(expected), "{failure}");
    }
}

#[test]
fn a_hung_peer_is_diagnosable_before_the_outer_runner_stops_it() {
    let mut peer = spawn_fixture("reply").unwrap();
    let _held_input = peer.input().unwrap();
    let request = "request 9: peer never answers";
    let mut pending = peer.waiting(request);
    loop {
        peer.read().unwrap();
        if String::from_utf8_lossy(peer.stdout()).contains(PARTIAL_REPLY) {
            break;
        }
        pending.pause(peer.diagnostic(request));
    }
    assert!(peer.poll().unwrap().is_none());
    let diagnostic = peer.diagnostic(request);
    assert!(diagnostic.contains(request));
    assert!(diagnostic.contains(PARTIAL_REPLY));
    drop(peer);
}

#[test]
fn diagnostics_escape_invalid_output_without_replacing_raw_bytes() {
    let mut peer = spawn_fixture("invalid-utf8").unwrap();
    drop(peer.input());
    let mut pending = peer.waiting("binary output");
    while peer.poll().unwrap().is_none() {
        pending.pause(peer.diagnostic("binary output"));
    }
    assert!(peer.stdout().windows(2).any(|bytes| bytes == b"\xff\n"));
    assert!(peer.diagnostic("binary output").contains(r"\xff"));
}

#[test]
#[ignore = "spawned as an owned reply peer by the adapter tests"]
fn peer_fixture() {
    raw::stdout().write_all(PARTIAL_REPLY.as_bytes()).unwrap();
    raw::stdout().flush().unwrap();
    eprint!("peer stderr");
    std::io::stderr().flush().unwrap();
    let mode = std::env::var(FIXTURE).unwrap();
    if mode == "invalid-utf8" {
        raw::stdout().write_all(b"\xff\n").unwrap();
    } else if mode == "reply" {
        let mut byte = [0];
        std::io::stdin().read_exact(&mut byte).unwrap();
        assert_eq!(byte, *b"R");
        raw::stdout().write_all(b" completed\n").unwrap();
    }
}
