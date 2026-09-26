#![cfg(feature = "failpoints")]

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;
use std::path::Path;
use std::process::{Output, Stdio};

use domyjob::authz::Submitter;
use domyjob::clock::Timestamp;
use domyjob::domain::{Concurrency, JobId};
use domyjob::paths::{Dirs, Family};
use domyjob::protocol::{Change, Command, Location, Outcome, Phase, Request, Spec};
use domyjob::spawn::Invocation;
use domyjob::store::{LaunchEnv, Store};
use domyjob::template::Arg;
use notify::Watcher as _;

fn dirs(root: &Path) -> Dirs {
    Dirs::isolated_for_test(root)
}

fn command(dirs: &Dirs, args: Vec<Arg>) -> std::process::Command {
    let executable = Path::new(env!("CARGO_BIN_EXE_domyjob"));
    let mut command = Invocation::new(Arg::path(executable), args).command();
    command
        .env("HOME", &dirs.home)
        .env("DOMYJOB_STATE", &dirs.state)
        .env("DOMYJOB_CONFIG", &dirs.config)
        .env("DOMYJOB_CACHE", &dirs.cache);
    command
}

#[expect(
    clippy::unwrap_used,
    reason = "the process harness cannot continue after its fixture setup fails"
)]
fn ask(dirs: &Dirs, request: &Request, abort_at: Option<&str>) -> Output {
    let mut command = command(dirs, vec![Arg::literal("node")]);
    if let Some(site) = abort_at {
        command.env("DOMYJOB_ABORT_AT", site);
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = serde_json::to_vec(request).unwrap();
    input.push(b'\n');
    child.stdin.take().unwrap().write_all(&input).unwrap();
    child.wait_with_output().unwrap()
}

fn recover(dirs: &Dirs) -> Output {
    ask(
        dirs,
        &Request::Configure {
            change: Change::default(),
        },
        None,
    )
}

#[test]
fn a_real_node_recovers_after_aborting_at_each_atomic_write_step() {
    for site in [
        "durable::name",
        "durable::create",
        "durable::write",
        "durable::sync",
        "durable::measure",
        "durable::replace",
        "durable::sync_dir",
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = dirs(tmp.path());
        let interrupted = ask(
            &dirs,
            &Request::Configure {
                change: Change {
                    paused: Some(true),
                    max_jobs: Some(Concurrency::try_from(2).unwrap()),
                },
            },
            Some(site),
        );
        assert!(!interrupted.status.success(), "{site} did not abort");
        let recovered = recover(&dirs);
        assert!(
            recovered.status.success(),
            "{site}: {}",
            String::from_utf8_lossy(&recovered.stderr)
        );
        assert!(
            String::from_utf8_lossy(&recovered.stdout).contains("\"reply\":\"report\""),
            "{site}: {}",
            String::from_utf8_lossy(&recovered.stdout)
        );
    }
}

#[test]
fn a_real_supervisor_killed_after_starting_is_recovered_without_rerunning() {
    let tmp = tempfile::tempdir().unwrap();
    let dirs = dirs(tmp.path());
    let store = Store::open(&dirs).unwrap();
    let id: JobId = "0BBBBBBBBBBBBBBB".parse().unwrap();
    let script = match domyjob::platform::FAMILY {
        Family::Unix => "yes",
        Family::Windows => "while ($true) { Write-Output x }",
    };
    let spec = Spec {
        id: id.clone(),
        name: None,
        command: Command::Script(script.to_owned()),
        location: Location::Home,
        env_names: BTreeSet::new(),
        shell: None,
        concurrency: Concurrency::DEFAULT,
        sequence: 1,
        submitted_by: Submitter::Owner,
        submitted_at: Timestamp::observe(),
    };
    store
        .stage(&spec, (&BTreeMap::new(), &LaunchEnv::default()))
        .unwrap();
    let (change_tx, change_rx) = std::sync::mpsc::channel();
    let mut watcher =
        notify::recommended_watcher(move |_| if change_tx.send(()).is_err() {}).unwrap();
    watcher
        .watch(&store.area(""), notify::RecursiveMode::Recursive)
        .unwrap();
    let mut supervisor = command(
        &dirs,
        vec![
            Arg::literal("node"),
            Arg::literal("--supervise"),
            Arg::word(&id),
            Arg::literal("--state-dir"),
            Arg::path(&dirs.state),
            Arg::literal("--home-dir"),
            Arg::path(&dirs.home),
        ],
    )
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();
    loop {
        change_rx.recv().unwrap();
        match store.phase(&id) {
            Ok(Phase::Starting { .. } | Phase::Running { .. }) => break,
            Ok(Phase::Finished { .. }) => {
                panic!("the job finished before the supervisor was killed")
            }
            Ok(Phase::Queued | Phase::Preparing { .. }) | Err(_) => {}
        }
    }
    supervisor.kill().unwrap();
    let stopped = supervisor.wait_with_output().unwrap();
    assert!(!stopped.status.success());
    let recovered = recover(&dirs);
    assert!(
        recovered.status.success(),
        "{}",
        String::from_utf8_lossy(&recovered.stderr)
    );
    assert!(matches!(
        store.phase(&id).unwrap(),
        Phase::Finished {
            outcome: Outcome::Errored { .. },
            ..
        }
    ));
}
