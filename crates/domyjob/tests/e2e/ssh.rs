//! The fake OpenSSH client.
//!
//! It accepts `-T`, `--`, and any `-o` option, which it ignores, and reaches a machine by running the real `domyjob node` with that machine's state, home, and PATH.
//! Standard input and output pass straight through, so a node sees the client leave as the end of its input, as it would over SSH.
//! Every connection is logged to `ssh.jsonl` in the world's root with the machine that made it.
//! Marker files in the machine directory simulate network trouble: `offline` refuses new connections and cuts open ones, and `drop-reply` lets the node finish but loses its reply.

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, ExitStatus, Stdio};

use serde_json::json;

use crate::{Context as _, Failure};

/// What the client asked the remote side to run.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Remote {
    /// The probe that tells a POSIX shell, which prints `:OS`, from PowerShell.
    ShellProbe,
    /// `domyjob node` with the arguments that follow `node`.
    Node(Vec<String>),
    /// Anything this fake cannot stand in for, such as a remote build.
    Unsupported,
}

impl Remote {
    fn describe(&self) -> String {
        match self {
            Self::ShellProbe => "probe".to_owned(),
            Self::Node(arguments) => std::iter::once("node")
                .chain(arguments.iter().map(String::as_str))
                .collect::<Vec<_>>()
                .join(" "),
            Self::Unsupported => "unsupported".to_owned(),
        }
    }
}

/// One parsed `ssh` command line.
#[derive(Debug)]
struct Invocation {
    options: Vec<String>,
    alias: String,
    command: String,
}

pub(crate) fn main() -> ExitCode {
    match connect() {
        Ok(code) => code,
        Err(failure) => {
            eprintln!("ssh: {failure}");
            ExitCode::from(255)
        }
    }
}

fn connect() -> Result<ExitCode, Failure> {
    let command_line: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect();
    let invocation = parse(&command_line)?;
    let root = PathBuf::from(
        std::env::var_os("E2E_ROOT").ok_or_else(|| Failure::new("E2E_ROOT is not set"))?,
    );
    let machine = root.join("machines").join(&invocation.alias);
    let alias = &invocation.alias;
    let remote = classify(&invocation.command);
    if !valid_alias(alias) || !crate::present(&machine)? {
        log(&root, &invocation, "unknown-host")?;
        return Err(Failure::new(format!(
            "Could not resolve hostname {alias}: Name or service not known"
        )));
    }
    if crate::present(&machine.join("offline"))? {
        log(&root, &invocation, "offline")?;
        return Err(Failure::new(format!(
            "connect to host {alias} port 22: Connection refused"
        )));
    }
    match remote {
        Remote::ShellProbe => {
            log(&root, &invocation, "probe")?;
            println!(":OS");
            Ok(ExitCode::SUCCESS)
        }
        Remote::Node(arguments) => {
            let dropped = crate::present(&machine.join("drop-reply"))?;
            log(&root, &invocation, if dropped { "dropped" } else { "node" })?;
            let status = node(&root, &machine, &arguments, dropped)?;
            if dropped {
                return Err(Failure::new(format!(
                    "Connection to {alias} closed by remote host."
                )));
            }
            Ok(crate::exit_code(status))
        }
        Remote::Unsupported => {
            log(&root, &invocation, "unsupported")?;
            eprintln!(
                "ssh: the fake cannot run this remote command: {}",
                invocation.command
            );
            Ok(ExitCode::from(127))
        }
    }
}

fn parse(arguments: &[String]) -> Result<Invocation, Failure> {
    let mut options = Vec::new();
    let mut rest = arguments.iter();
    let mut next = |missing: &str| {
        rest.next()
            .cloned()
            .ok_or_else(|| Failure::new(missing.to_owned()))
    };
    let alias = loop {
        let argument = next("usage: ssh [-T] [-o option] [--] destination command")?;
        match argument.as_str() {
            "--" => break next("a destination must follow --")?,
            "-T" => {}
            "-o" => options.push(next("option requires an argument -- o")?),
            other => {
                if let Some(option) = other.strip_prefix("-o") {
                    options.push(option.to_owned());
                } else if other.starts_with('-') {
                    return Err(Failure::new(format!(
                        "the fake does not support the option {other}"
                    )));
                } else {
                    break argument;
                }
            }
        }
    };
    let command = rest.map(String::as_str).collect::<Vec<_>>().join(" ");
    ensure!(
        !command.is_empty(),
        "the fake needs a remote command; interactive sessions are not supported"
    );
    Ok(Invocation {
        options,
        alias,
        command,
    })
}

fn valid_alias(alias: &str) -> bool {
    alias
        .as_bytes()
        .first()
        .is_some_and(u8::is_ascii_alphanumeric)
        && alias
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

/// The machine whose client started this connection, read from the state directory it passed down.
fn caller(root: &Path) -> Option<String> {
    let state = PathBuf::from(std::env::var_os("DOMYJOB_STATE")?);
    let machine = state.parent()?;
    (machine.parent()? == root.join("machines"))
        .then(|| machine.file_name()?.to_str().map(str::to_owned))
        .flatten()
}

fn log(root: &Path, invocation: &Invocation, outcome: &str) -> Result<(), Failure> {
    let record = json!({
        "from": caller(root),
        "alias": invocation.alias,
        "options": invocation.options,
        "command": invocation.command,
        "outcome": outcome,
    });
    crate::append_line(&root.join("ssh.jsonl"), &record).context("logging the connection")?;
    Ok(())
}

fn classify(command: &str) -> Remote {
    if command.contains("echo $env:OS") {
        return Remote::ShellProbe;
    }
    node_arguments(command).map_or(Remote::Unsupported, Remote::Node)
}

/// Finds `.../domyjob node [ARGUMENTS]` inside whatever shell wrapper carries it.
///
/// Words are split on whitespace and stripped of quotes and statement punctuation.
/// The program is a word whose last path segment is `domyjob`, or a shell variable when the command also names such a path, as in `p=".../domyjob"; exec "$p" node`.
/// The arguments end with the statement that holds them.
fn node_arguments(command: &str) -> Option<Vec<String>> {
    let words: Vec<&str> = command.split_whitespace().collect();
    let names_domyjob = words.iter().any(|word| is_domyjob(assigned(word)));
    let start = words.windows(2).position(|pair| {
        matches!(pair, [program, verb] if bare(verb) == "node"
            && (is_domyjob(bare(program)) || (names_domyjob && is_variable(bare(program)))))
    })?;
    let mut arguments = Vec::new();
    if words
        .get(start.saturating_add(1))
        .is_some_and(|verb| ends_statement(verb))
    {
        return Some(arguments);
    }
    for word in words.iter().skip(start.saturating_add(2)) {
        if matches!(*word, ";" | "&&" | "||" | "|") {
            break;
        }
        let argument = bare(word);
        if !argument.is_empty() {
            arguments.push(argument.to_owned());
        }
        if ends_statement(word) {
            break;
        }
    }
    Some(arguments)
}

fn bare(word: &str) -> &str {
    word.trim_matches(|character| {
        matches!(character, '\'' | '"' | ';' | '(' | ')' | '{' | '}' | '&')
    })
}

fn ends_statement(word: &str) -> bool {
    word.ends_with([';', '\'', '"', ')', '}'])
}

fn is_domyjob(word: &str) -> bool {
    word.rsplit(['/', '\\'])
        .next()
        .is_some_and(|name| name == "domyjob" || name.eq_ignore_ascii_case("domyjob.exe"))
}

/// The value of a `name=value` word, or the bare word itself.
fn assigned(word: &str) -> &str {
    let word = bare(word);
    bare(word.split_once('=').map_or(word, |(_name, value)| value))
}

/// Whether a word reads a shell variable, such as `$p` or `${p}`.
fn is_variable(word: &str) -> bool {
    word.strip_prefix('$').is_some_and(|name| {
        let name = name.trim_start_matches('{').trim_end_matches('}');
        !name.is_empty()
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    })
}

/// Runs the real node as the remote machine would, with its state, home, and PATH.
fn node(
    root: &Path,
    machine: &Path,
    arguments: &[String],
    drop_reply: bool,
) -> Result<ExitStatus, Failure> {
    let domyjob =
        std::env::var_os("E2E_DOMYJOB").ok_or_else(|| Failure::new("E2E_DOMYJOB is not set"))?;
    let home = machine.join("home");
    let mut command = Command::new(domyjob);
    command
        .arg("node")
        .args(arguments)
        .current_dir(&home)
        .env("DOMYJOB_STATE", machine.join("state"))
        .env("HOME", &home)
        .env("PATH", crate::search_path(&root.join("bin"))?)
        .env("DOMYJOB_REFRESH", "never")
        .env_remove("DOMYJOB_CHAT_AGENT");
    if cfg!(windows) {
        command.env("USERPROFILE", &home);
    }
    if drop_reply {
        let mut child = command
            .stdout(Stdio::piped())
            .spawn()
            .context("starting the remote node")?;
        if let Some(mut reply) = child.stdout.take() {
            io::copy(&mut reply, &mut io::sink()).context("discarding the node's reply")?;
        }
        return child.wait().context("waiting for the remote node");
    }
    let mut child = command.spawn().context("starting the remote node")?;
    let offline = machine.join("offline");
    loop {
        if let Some(status) = child.try_wait().context("waiting for the remote node")? {
            return Ok(status);
        }
        if crate::present(&offline)? {
            // The machine went offline mid-connection, the way a network outage cuts a live session.
            let _killed = child.kill();
            let _reaped = child.wait();
            return Err(Failure::new("Connection reset by peer"));
        }
        crate::pause(10);
    }
}

/// Checks the remote-command recognizer against the wrappers domyjob sends or is likely to send.
///
/// The first two are the wrappers `transport::node_command` builds for a POSIX shell and for PowerShell.
pub(crate) fn check_recognizer() -> Result<(), Failure> {
    let cases = [
        (
            r#"sh -c 'p="$HOME/.cargo/domyjob/versions/0123456789abcdef/bin/domyjob"; [ -x "$p" ] || { echo "domyjob: node 0123456789abcdef missing" >&2; exit 97; }; exec "$p" node'"#,
            "node",
        ),
        (
            r"$p = Join-Path $env:USERPROFILE '.cargo\domyjob\versions\0123456789abcdef\bin\domyjob.exe'; if (-not (Test-Path -LiteralPath $p)) { [Console]::Error.WriteLine('domyjob: node 0123456789abcdef missing'); exit 97 }; & $p node; exit $LASTEXITCODE",
            "node",
        ),
        (
            "~/.cargo/domyjob/versions/0123456789abcdef/bin/domyjob node",
            "node",
        ),
        (
            "~/.cargo/domyjob/versions/0123456789abcdef/bin/domyjob node --reap 7",
            "node --reap 7",
        ),
        (
            r#"sh -c 'exec "$HOME/.cargo/domyjob/versions/0123456789abcdef/bin/domyjob" node'"#,
            "node",
        ),
        (
            "bash -lc 'exec ~/.cargo/domyjob/versions/0123456789abcdef/bin/domyjob node --reap 7'",
            "node --reap 7",
        ),
        (
            r#"powershell.exe -NoProfile -Command "& '$env:USERPROFILE\.cargo\domyjob\versions\0123456789abcdef\bin\domyjob.exe' node; exit $LASTEXITCODE""#,
            "node",
        ),
        ("echo $env:OS", "probe"),
        (
            "bash -lc 'set -eu; mise x -- cargo install --path crates/domyjob --bin domyjob'",
            "unsupported",
        ),
        ("~/bin/domyjob nodes", "unsupported"),
        ("~/bin/not-domyjob node", "unsupported"),
        (
            r#"echo "domyjob: node 0123456789abcdef missing""#,
            "unsupported",
        ),
        (r#"p="/opt/other"; exec "$p" node"#, "unsupported"),
    ];
    for (command, expected) in cases {
        let found = classify(command).describe();
        ensure!(
            found == expected,
            "the fake SSH read `{command}` as `{found}` instead of `{expected}`"
        );
    }
    Ok(())
}
