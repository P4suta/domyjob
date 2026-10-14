use std::fs;
use std::io::{self, Read as _, Write as _};
use std::path::Path;
use std::process::{ExitCode, Stdio};

use domyjob_core::domain::JobId;
use domyjob_core::wire::{self, Input, Reply, Request};
use serde_json::json;

use crate::world::{Run, World};
use crate::{Context as _, Failure};

const SNAPSHOT_FILE: &str = "original-binary.bin";
const SNAPSHOT_BYTES: &[u8] = b"\0\xff\x80original request\n";
pub(crate) const PRIMARY_EXIT: u8 = 7;
const INSTALL_NOISE: &str = "fixture installer stdout must remain diagnostic";
const INSTALL_BUILD_JOBS: &str = "2";
const SUBMISSION: &str = "0123456789abcdef0123456789abcdef";
const WINDOWS_SCRIPT_PREFIX: &str =
    "& ([ScriptBlock]::Create([Text.Encoding]::Unicode.GetString([Convert]::FromBase64String('";
const WINDOWS_SCRIPT_SUFFIX: &str = "'))))";
pub(crate) const INPUT_REMAINDER: &[u8] = b"\0\xff\x80\n";

pub(crate) fn input_remainder() -> ExitCode {
    match check_remainder() {
        Ok(()) => ExitCode::from(PRIMARY_EXIT),
        Err(failure) => {
            eprintln!("bootstrap fixture: {failure}");
            ExitCode::FAILURE
        }
    }
}

fn check_remainder() -> Result<(), Failure> {
    const EOF_PROBE_BYTES: u64 = 1;
    let bound = u64::try_from(INPUT_REMAINDER.len())
        .context("bounding fixture bytes")?
        .checked_add(EOF_PROBE_BYTES)
        .ok_or_else(|| Failure::new("fixture remainder bound overflowed"))?;
    let mut received = Vec::new();
    io::stdin()
        .take(bound)
        .read_to_end(&mut received)
        .context("reading inherited binary stdin")?;
    ensure!(
        received == INPUT_REMAINDER,
        "inherited stdin changed: {received:?}"
    );
    io::stdout()
        .write_all(&received)
        .context("writing the inherited binary bytes")?;
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Fault {
    None,
    Cwd,
    Cleanup,
    Truncated,
}

impl Fault {
    const fn label(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Cwd => "cwd",
            Self::Cleanup => "cleanup",
            Self::Truncated => "truncated",
        }
    }
}

pub(crate) fn installer() -> ExitCode {
    result(install())
}

fn install() -> Result<(), Failure> {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let destination = arguments
        .windows(2)
        .find_map(|pair| match pair {
            [option, root] if option == "--root" => Some(Path::new(root).to_path_buf()),
            _ => None,
        })
        .ok_or_else(|| Failure::new("fake mise requires an install root"))?;
    ensure!(
        arguments.starts_with(&["x".into(), "--".into(), "cargo".into(), "install".into()]),
        "unexpected installer arguments: {arguments:?}"
    );
    ensure!(
        arguments.iter().filter(|argument| *argument == "--jobs").count() == 1
            && arguments.windows(2).any(
                |pair| matches!(pair, [option, value] if option == "--jobs" && value == INSTALL_BUILD_JOBS)
            ),
        "the installer must receive one explicit build-job cap: {arguments:?}"
    );
    let mut extra = [0_u8; 1];
    ensure!(
        io::stdin()
            .read(&mut extra)
            .context("checking isolated installer stdin")?
            == 0,
        "the installer received an unexpected byte on its isolated stdin: {extra:?}"
    );
    let source = std::env::current_exe().context("locating the fixture installer")?;
    let bin = destination.join("bin");
    fs::create_dir_all(&bin).context("creating the fixture installation")?;
    fs::copy(
        source,
        bin.join(format!("domyjob{}", std::env::consts::EXE_SUFFIX)),
    )
    .context("installing the fixture candidate")?;
    println!("{INSTALL_NOISE}");
    Ok(())
}

pub(crate) fn published() -> ExitCode {
    match first_request() {
        Ok(()) => {
            if std::env::var_os("E2E_BOOTSTRAP_EXIT").is_some() {
                ExitCode::from(PRIMARY_EXIT)
            } else {
                ExitCode::SUCCESS
            }
        }
        Err(failure) => {
            eprintln!("bootstrap fixture: {failure}");
            ExitCode::FAILURE
        }
    }
}

fn first_request() -> Result<(), Failure> {
    let mut input = io::stdin().lock();
    let frame = original_frame(&mut input)?;
    let request =
        domyjob_core::ingress::request(&frame).context("decoding the original first request")?;
    let Request::Run {
        submission,
        command,
        input: Input::Snapshot(snapshot),
    } = request
    else {
        return Err(Failure::new(
            "bootstrap did not forward the original snapshot Run first",
        ));
    };
    let mut payload = Vec::new();
    input
        .by_ref()
        .take(snapshot.bytes())
        .read_to_end(&mut payload)
        .context("reading the original binary payload")?;
    ensure!(
        u64::try_from(payload.len()).context("bounding the original payload")? == snapshot.bytes(),
        "truncated original payload"
    );
    ensure!(
        blake3::hash(&payload).to_hex().as_str() == snapshot.digest(),
        "original payload digest changed"
    );
    let mut extra = [0_u8; 1];
    ensure!(
        input
            .read(&mut extra)
            .context("checking original payload EOF")?
            == 0,
        "bytes followed the original payload"
    );
    drop(input);
    let mut found = false;
    for entry in tar::Archive::new(payload.as_slice())
        .entries()
        .context("reading the original snapshot")?
    {
        let mut entry = entry.context("reading an original snapshot entry")?;
        if entry.path().context("reading the original snapshot path")? == Path::new(SNAPSHOT_FILE) {
            let mut bytes = Vec::new();
            entry
                .read_to_end(&mut bytes)
                .context("reading the binary snapshot file")?;
            ensure!(bytes == SNAPSHOT_BYTES, "the original binary file changed");
            found = true;
        }
    }
    ensure!(found, "the original binary file is missing");
    let state =
        std::env::var_os("DOMYJOB_STATE").ok_or_else(|| Failure::new("fixture state missing"))?;
    fs::create_dir_all(&state).context("creating the fixture request log directory")?;
    crate::append_line(
        &Path::new(&state).join("first-request.jsonl"),
        &json!({"submission":submission,"command":command,"bytes":snapshot.bytes(),"eof":true}),
    )
    .context("recording the original first request")?;
    io::stdout()
        .write_all(
            &wire::frame(&Reply::Accepted {
                job: JobId::from(&submission),
            })
            .context("framing the original reply")?,
        )
        .context("writing the original reply")?;
    Ok(())
}

fn original_frame(input: &mut impl io::Read) -> Result<Vec<u8>, Failure> {
    let mut header = [0_u8; wire::CONTROL_HEADER_BYTES];
    input
        .read_exact(&mut header)
        .context("reading the original first header")?;
    let length =
        wire::ControlLength::try_from(header).context("decoding the original first header")?;
    let mut frame = Vec::from(header);
    let mut body = vec![0_u8; length.bytes()];
    input
        .read_exact(&mut body)
        .context("reading the original first frame")?;
    frame.extend(body);
    Ok(frame)
}

fn result(outcome: Result<(), Failure>) -> ExitCode {
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(failure) => {
            eprintln!("bootstrap fixture: {failure}");
            ExitCode::FAILURE
        }
    }
}

pub(crate) fn connect(
    root: &Path,
    machine: &Path,
    remote: &str,
    mut log: impl FnMut(&str) -> Result<(), Failure>,
) -> Result<ExitCode, Failure> {
    if remote == "echo $env:OS" {
        log("probe")?;
        println!("{}", crate::os::bootstrap_os());
        return Ok(ExitCode::SUCCESS);
    }
    let installing = remote.starts_with(WINDOWS_SCRIPT_PREFIX) || remote.starts_with("bash -c ");
    log(if installing { "bootstrap" } else { "missing" })?;
    let home = machine.join("home");
    let fault = fs::read_to_string(machine.join("bootstrap-fault"))
        .context("reading the controlled bootstrap fault")?;
    let injected = if installing {
        inject_effect_failure(remote, &fault)?
    } else {
        remote.to_owned()
    };
    let mut shell = crate::os::bootstrap_shell(&injected);
    shell
        .current_dir(&home)
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("LOCALAPPDATA", home.join("AppData/Local"))
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env("DOMYJOB_STATE", machine.join("state"))
        .env("PATH", crate::search_path(&root.join("bin"))?)
        .env("E2E_BOOTSTRAP_PATH", crate::search_path(&root.join("bin"))?)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    if crate::present(&machine.join("bootstrap-exit"))? {
        shell.env("E2E_BOOTSTRAP_EXIT", "1");
    }
    let status = if installing && fault == Fault::Truncated.label() {
        let mut child = shell
            .stdin(Stdio::piped())
            .spawn()
            .context("starting the truncated native bootstrap")?;
        let mut pipe = child
            .stdin
            .take()
            .ok_or_else(|| Failure::new("bootstrap stdin missing"))?;
        io::copy(&mut io::stdin().lock().take(1), &mut pipe)
            .context("injecting truncated source EOF")?;
        drop(pipe);
        io::copy(&mut io::stdin().lock(), &mut io::sink())
            .context("draining the rejected bootstrap upload")?;
        child
            .wait()
            .context("collecting the rejected native bootstrap")?
    } else {
        shell
            .status()
            .context("executing the exact native bootstrap command")?
    };
    log(&format!("exit {:?}", status.code()))?;
    Ok(crate::exit_code(status))
}

fn inject_effect_failure(command: &str, fault: &str) -> Result<String, Failure> {
    let windows = command
        .strip_prefix(WINDOWS_SCRIPT_PREFIX)
        .and_then(|script| script.strip_suffix(WINDOWS_SCRIPT_SUFFIX));
    let script = if let Some(encoded) = windows {
        let bytes = data_encoding::BASE64
            .decode(encoded.as_bytes())
            .context("decoding the maintained Windows script")?;
        let (pairs, remainder) = bytes.as_chunks::<2>();
        ensure!(remainder.is_empty(), "malformed Windows script bytes");
        String::from_utf16(
            &pairs
                .iter()
                .map(|pair| u16::from_le_bytes(*pair))
                .collect::<Vec<_>>(),
        )
        .context("decoding the maintained Windows script text")?
    } else {
        command.to_owned()
    };
    let changed = match (windows.is_some(), fault) {
        (true, "cwd") => script.replace(
            "Set-Location $env:USERPROFILE",
            "throw 'injected cwd recovery failure'",
        ),
        (true, "cleanup") => script.replace(
            "Remove-Item -LiteralPath $work -Recurse -Force",
            "throw 'injected cleanup failure'",
        ),
        (false, "cwd") => script.replace("cd \"$HOME\"", "cd /dev/null"),
        (false, "cleanup") => script.replace("rm -rf -- \"$work\"", "false"),
        (_, _) => script,
    };
    if windows.is_some() {
        let bytes: Vec<_> = changed.encode_utf16().flat_map(u16::to_le_bytes).collect();
        Ok(format!(
            "{WINDOWS_SCRIPT_PREFIX}{}{WINDOWS_SCRIPT_SUFFIX}",
            data_encoding::BASE64.encode(&bytes)
        ))
    } else {
        Ok(changed)
    }
}

pub(crate) fn scenario(world: &World) -> Result<(), Failure> {
    for check in crate::os::bootstrap_input_contracts() {
        check(world.root())?;
    }
    let alpha = world.machine("alpha");
    let beta = world.machine("beta");
    let home = beta.home();
    let machine = home
        .parent()
        .ok_or_else(|| Failure::new("fixture home has no parent"))?;
    fs::write(machine.join("bootstrap-exec"), b"").context("enabling exact bootstrap execution")?;
    fs::write(home.join(".bash_profile"), "export PATH=\"$E2E_BOOTSTRAP_PATH\"\nprintf 'fixture profile stdout must remain diagnostic\\n'\n").context("preparing the private fixture login profile")?;
    fs::write(alpha.work().join(SNAPSHOT_FILE), SNAPSHOT_BYTES)
        .context("preparing the binary original snapshot")?;
    for (fault, primary) in [
        (Fault::None, false),
        (Fault::Cwd, true),
        (Fault::Cleanup, true),
        (Fault::Cleanup, false),
        (Fault::Truncated, false),
    ] {
        fs::write(machine.join("bootstrap-fault"), fault.label())
            .context("selecting the controlled bootstrap fault")?;
        let exit = machine.join("bootstrap-exit");
        if primary {
            fs::write(&exit, b"").context("selecting the primary first-node exit")?;
        } else if crate::present(&exit)? {
            fs::remove_file(&exit).context("restoring primary first-node success")?;
        }
        check_request(world, fault, primary)?;
    }
    Ok(())
}

fn check_request(world: &World, fault: Fault, primary: bool) -> Result<(), Failure> {
    let alpha = world.machine("alpha");
    let beta = world.machine("beta");
    let before = crate::read_lines(&beta.state().join("first-request.jsonl"))?.len();
    let run = alpha.run(&[
        "run",
        "beta",
        "--submission",
        SUBMISSION,
        "--",
        "sleeper",
        "0",
        "7",
    ])?;
    let calls = world.ssh_log()?;
    let native_exit = calls
        .last()
        .and_then(|call| call.get("outcome"))
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| Failure::new("native bootstrap exit record missing"))?;
    check_outcome(&run, fault, primary, native_exit)?;
    let requests = crate::read_lines(&beta.state().join("first-request.jsonl"))?;
    let count = usize::from(fault != Fault::Truncated);
    ensure!(
        requests.len() == before.saturating_add(count),
        "the first request must execute exactly once after installation: {requests:?}"
    );
    if count != 0 {
        let request = requests
            .get(before)
            .ok_or_else(|| Failure::new("first request log missing"))?;
        ensure!(
            request["submission"] == SUBMISSION
                && request["command"] == json!(["sleeper", "0", "7"]),
            "the original request identity and command must survive bootstrap: {request}"
        );
    }
    check_cleanup(world, fault)
}

fn check_outcome(run: &Run, fault: Fault, primary: bool, native_exit: &str) -> Result<(), Failure> {
    const SUCCESS_STATUS: i32 = 0;
    const FAILURE_STATUS: i32 = 1;
    let expected_status = if primary {
        i32::from(PRIMARY_EXIT)
    } else if fault == Fault::None {
        SUCCESS_STATUS
    } else {
        FAILURE_STATUS
    };
    ensure!(
        native_exit == format!("exit Some({expected_status})"),
        "bootstrap must preserve exact native status {expected_status}: {native_exit}; {run}"
    );
    if fault == Fault::None {
        ensure!(
            run.code == Some(0),
            "the original first request must be accepted: {run}"
        );
    } else {
        let expected = if fault == Fault::Truncated {
            "truncated bootstrap source"
        } else {
            "remote node did not exit successfully:"
        };
        ensure!(
            run.code == Some(1) && run.stderr.contains(expected),
            "bootstrap must preserve its primary outcome for {}: {run}",
            fault.label()
        );
        if fault == Fault::Cwd {
            ensure!(
                run.stderr.contains("cwd recovery failed"),
                "missing cwd failure: {run}"
            );
        }
        if fault == Fault::Cleanup {
            ensure!(
                run.stderr.contains("cleanup failed"),
                "missing cleanup failure: {run}"
            );
        }
    }
    if fault != Fault::Truncated {
        ensure!(
            run.stderr.contains(INSTALL_NOISE),
            "installer stdout must be redirected to diagnostics: {run}"
        );
    }
    if fault == Fault::None {
        ensure!(
            run.stdout.trim().ends_with(SUBMISSION),
            "original submission identity changed: {run}"
        );
    }
    Ok(())
}

fn check_cleanup(world: &World, fault: Fault) -> Result<(), Failure> {
    let beta = world.machine("beta");
    let cache = crate::os::bootstrap_cache(&beta.home()).join("domyjob/bootstrap");
    let stages = fs::read_dir(&cache)
        .context("checking bootstrap cleanup")?
        .collect::<Result<Vec<_>, _>>()
        .context("reading remaining bootstrap stages")?;
    if fault == Fault::Cleanup {
        ensure!(
            stages.len() == 1,
            "only the failed owned cleanup may remain"
        );
        let stage = stages
            .first()
            .ok_or_else(|| Failure::new("injected cleanup stage missing"))?;
        fs::remove_dir_all(stage.path())
            .context("removing the fixture's injected cleanup failure")?;
    } else {
        ensure!(
            stages.is_empty(),
            "completed bootstrap stages must be removed even after cwd failure"
        );
    }
    let calls = world.ssh_log()?;
    ensure!(
        calls
            .iter()
            .rev()
            .take(2)
            .any(|call| call["outcome"] == "bootstrap"),
        "install and first request must share one SSH exchange: {calls:?}"
    );
    Ok(())
}
