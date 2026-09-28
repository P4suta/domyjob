//! Fake AI CLIs, and the `sleeper` they start as a child.
//!
//! A fake AI CLI reads the whole prompt from standard input, appends a record of the call to `calls.jsonl` in its working directory, and answers in its tool's structured format.
//! The record holds the arguments, the prompt, and the agent and turn from `DOMYJOB_CHAT_AGENT` and `DOMYJOB_CHAT_TURN`, and `overlap: true` when another fake still runs in the same directory.
//! A resumed call keeps the session it was given; a new call gets `session-TOOL-1`, or a fixed UUID for Codex, whose sessions are UUIDs.
//! Invocations that are not turns, such as `claude auth status`, print nothing and succeed.
//! Words in the prompt, matched whole, change the behavior:
//!
//! - `INCOMPLETE` omits the event that completes the answer.
//! - `NONZERO` answers completely and then exits with 3.
//! - `HUGE` answers with more than the four mebibytes domyjob accepts.
//! - `CHANGE_SESSION` reports a different session ID.
//! - `DELAY` answers after 300 milliseconds.
//! - `RENDEZVOUS` waits up to ten seconds for another fake whose working directory has the same parent, and says whether they met.
//! - `DELEGATE_TO_name` asks the agent `name` with `domyjob chat ask` during the turn and quotes the outcome; the next `DELEGATE_TO_` words of the prompt become the delegated question.
//! - `HANG` appends its PID to `hang.pid` and sleeps for a minute unless the harness kills it first.
//! - `KILL_PARENT` starts a `sleeper` child, then kills the process that started the fake, the chat worker.

use std::fs::{self, TryLockError};
use std::io::{self, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

use serde_json::{Value, json};

use crate::{Context as _, Failure};

/// Longer than the four mebibytes domyjob accepts from an AI CLI.
const HUGE_ANSWER_BYTES: usize = 4_198_400;
/// How long `HANG` and a `KILL_PARENT` child sleep, so that a process the harness lost track of still ends.
const HANG_MILLISECONDS: u64 = 60_000;
/// The session a new Codex call reports, and the one `CHANGE_SESSION` reports instead of a resumed one.
pub(crate) const CODEX_SESSION: &str = "0e2e0000-0000-4000-8000-000000000001";
const CODEX_OTHER_SESSION: &str = "0e2e0000-0000-4000-8000-0000000000ff";
const DELEGATE: &str = "DELEGATE_TO_";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tool {
    Claude,
    Codex,
    Opencode,
}

impl Tool {
    const fn name(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Opencode => "opencode",
        }
    }

    /// The word before a resumed session ID on the tool's command line.
    const fn resume_flag(self) -> &'static str {
        match self {
            Self::Claude => "--resume",
            Self::Codex => "resume",
            Self::Opencode => "--session",
        }
    }

    /// Whether the arguments start a turn rather than a command such as `auth status`.
    fn is_turn(self, arguments: &[String]) -> bool {
        match self {
            Self::Claude => arguments.iter().any(|argument| argument == "-p"),
            Self::Codex => arguments.first().is_some_and(|first| first == "exec"),
            Self::Opencode => arguments.first().is_some_and(|first| first == "run"),
        }
    }

    fn new_session(self) -> String {
        match self {
            Self::Codex => CODEX_SESSION.to_owned(),
            Self::Claude | Self::Opencode => format!("session-{}-1", self.name()),
        }
    }

    fn other_session(self, session: &str) -> String {
        match self {
            Self::Codex => CODEX_OTHER_SESSION.to_owned(),
            Self::Claude | Self::Opencode => format!("{session}-changed"),
        }
    }
}

pub(crate) fn main(tool: Tool) -> ExitCode {
    match respond(tool) {
        Ok(code) => code,
        Err(failure) => {
            eprintln!("fake {}: {failure}", tool.name());
            ExitCode::from(70)
        }
    }
}

fn respond(tool: Tool) -> Result<ExitCode, Failure> {
    let call = Call::read(tool)?;
    if !tool.is_turn(&call.arguments) {
        return Ok(ExitCode::SUCCESS);
    }
    let running = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(call.cwd.join("provider.lock"))
        .context("opening provider.lock")?;
    let overlap = match running.try_lock() {
        Ok(()) => false,
        Err(TryLockError::WouldBlock) => true,
        Err(TryLockError::Error(error)) => {
            return Err(Failure::new(format!("locking provider.lock: {error}")));
        }
    };
    let family = if call.says("KILL_PARENT") {
        Some((parent_of(std::process::id())?, start_sleeper()?))
    } else {
        None
    };
    let index = crate::append_line(&call.cwd.join("calls.jsonl"), &call.record(overlap, family))
        .context("recording the call")?;
    if call.says("DELAY") {
        crate::pause(300);
    }
    if call.says("HANG") {
        hang(&call.cwd)?;
        return Ok(ExitCode::from(4));
    }
    if let Some((parent, _child)) = family {
        crate::kill_process(parent).context("killing the parent process")?;
        return Ok(ExitCode::from(9));
    }
    let text = call.answer(index)?;
    let session = call.session();
    let reported = if call.says("CHANGE_SESSION") {
        tool.other_session(&session)
    } else {
        session
    };
    emit(tool, &reported, &text, !call.says("INCOMPLETE")).context("writing the answer")?;
    drop(running);
    Ok(if call.says("NONZERO") {
        ExitCode::from(3)
    } else {
        ExitCode::SUCCESS
    })
}

/// One run of a fake AI CLI, as it was started.
struct Call {
    tool: Tool,
    arguments: Vec<String>,
    cwd: PathBuf,
    prompt: String,
}

impl Call {
    fn read(tool: Tool) -> Result<Self, Failure> {
        let mut prompt = String::new();
        io::stdin()
            .read_to_string(&mut prompt)
            .context("reading the prompt")?;
        Ok(Self {
            tool,
            arguments: std::env::args_os()
                .skip(1)
                .map(|argument| argument.to_string_lossy().into_owned())
                .collect(),
            cwd: std::env::current_dir().context("finding the working directory")?,
            prompt,
        })
    }

    /// The prompt's words, split on everything but letters, digits, and underscores.
    fn words(&self) -> impl Iterator<Item = &str> {
        self.prompt
            .split(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
            .filter(|word| !word.is_empty())
    }

    /// Whether the prompt holds `keyword` as a whole word, so that `CHANGE_SESSION` never also means `HANG`.
    fn says(&self, keyword: &str) -> bool {
        self.words().any(|word| word == keyword)
    }

    /// The session this call resumes, or the one a new call starts.
    fn session(&self) -> String {
        self.arguments
            .windows(2)
            .find_map(|pair| match pair {
                [flag, session] if flag == self.tool.resume_flag() && session != "-" => {
                    Some(session.clone())
                }
                _ => None,
            })
            .unwrap_or_else(|| self.tool.new_session())
    }

    fn record(&self, overlap: bool, family: Option<(u32, u32)>) -> Value {
        let variable =
            |name: &str| std::env::var_os(name).map(|value| value.to_string_lossy().into_owned());
        let mut record = json!({
            "tool": self.tool.name(),
            "args": self.arguments,
            "cwd": self.cwd.to_string_lossy(),
            "prompt": self.prompt,
            "agent": variable("DOMYJOB_CHAT_AGENT"),
            "turn": variable("DOMYJOB_CHAT_TURN"),
            "pid": std::process::id(),
        });
        if let Some(fields) = record.as_object_mut() {
            if overlap {
                fields.insert("overlap".to_owned(), Value::Bool(true));
            }
            if let Some((parent, child)) = family {
                fields.insert("parent".to_owned(), json!(parent));
                fields.insert("child".to_owned(), json!(child));
            }
        }
        record
    }

    /// The answer text: the call's number, then what a rendezvous or a delegated ask found.
    fn answer(&self, index: usize) -> Result<String, Failure> {
        if self.says("HUGE") {
            return Ok("x".repeat(HUGE_ANSWER_BYTES));
        }
        let mut text = format!("fake {} answer {index}", self.tool.name());
        if self.says("RENDEZVOUS") {
            let met = rendezvous(&self.cwd)?;
            text.push_str(if met { "; met" } else { "; alone" });
        }
        let delegated: Vec<&str> = self
            .words()
            .filter_map(|word| word.strip_prefix(DELEGATE))
            .collect();
        if let Some((target, rest)) = delegated.split_first() {
            let question: Vec<String> = rest
                .iter()
                .map(|name| format!("{DELEGATE}{name}"))
                .collect();
            let outcome = delegate(target, &question.join(" "))?;
            text = format!("{text}; {outcome}");
        }
        Ok(text)
    }
}

/// Asks `target` from inside this turn, with the agent and turn domyjob gave this process.
fn delegate(target: &str, question: &str) -> Result<String, Failure> {
    let question = if question.is_empty() {
        "a delegated question"
    } else {
        question
    };
    let output = Command::new("domyjob")
        .args(["chat", "--json", "ask", target, question, "--timeout", "30"])
        .env("DOMYJOB_REFRESH", "never")
        .stdin(Stdio::null())
        .output()
        .context("running the delegated ask")?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let code = output
        .status
        .code()
        .map_or_else(|| "a signal".to_owned(), |code| code.to_string());
    Ok(format!(
        "asked {target}, exit {code}: {} {}",
        stdout.trim(),
        stderr.trim()
    ))
}

/// Waits for another fake in a sibling working directory; true when both ran at once.
fn rendezvous(cwd: &Path) -> Result<bool, Failure> {
    let place = cwd
        .parent()
        .ok_or_else(|| Failure::new("the working directory has no parent"))?
        .join("rendezvous.jsonl");
    crate::append_line(&place, &json!(std::process::id())).context("joining the rendezvous")?;
    for _ in 0..200 {
        if crate::read_lines(&place)?.len() >= 2 {
            return Ok(true);
        }
        crate::pause(50);
    }
    Ok(false)
}

fn hang(cwd: &Path) -> Result<(), Failure> {
    let mut pids = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(cwd.join("hang.pid"))
        .context("opening hang.pid")?;
    writeln!(pids, "{}", std::process::id()).context("writing hang.pid")?;
    drop(pids);
    crate::pause(HANG_MILLISECONDS);
    Ok(())
}

/// Starts a long-sleeping child, so killing the fake's parent leaves a process tree behind to clean up.
fn start_sleeper() -> Result<u32, Failure> {
    let program = std::env::current_exe()
        .context("locating the fake")?
        .with_file_name(format!("sleeper{}", std::env::consts::EXE_SUFFIX));
    let child = Command::new(program)
        .arg(HANG_MILLISECONDS.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("starting the sleeper")?;
    Ok(child.id())
}

/// Asks the operating system for the parent of `pid`; the standard library has no portable call for it.
fn parent_of(pid: u32) -> Result<u32, Failure> {
    let mut command = if cfg!(windows) {
        let mut command = Command::new("powershell.exe");
        command.args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &format!("(Get-CimInstance Win32_Process -Filter 'ProcessId={pid}').ParentProcessId"),
        ]);
        command
    } else {
        let mut command = Command::new("ps");
        command.args(["-o", "ppid=", "-p", &pid.to_string()]);
        command
    };
    let output = command
        .stdin(Stdio::null())
        .output()
        .context("asking for the parent process")?;
    let text = String::from_utf8_lossy(&output.stdout);
    text.trim()
        .parse()
        .context(&format!("reading a parent PID from {text:?}"))
}

fn emit(tool: Tool, session: &str, text: &str, complete: bool) -> io::Result<()> {
    let events = match tool {
        Tool::Claude => vec![claude(session, text, complete)],
        Tool::Codex => codex(session, text, complete),
        Tool::Opencode => opencode(session, text, complete),
    };
    let mut output = io::stdout().lock();
    for event in events {
        writeln!(output, "{event}")?;
    }
    output.flush()
}

/// Claude Code prints one result object, or with verbose output an array of events that ends with it.
fn claude(session: &str, text: &str, complete: bool) -> Value {
    if complete {
        json!({
            "type": "result",
            "subtype": "success",
            "is_error": false,
            "result": text,
            "session_id": session,
        })
    } else {
        json!([
            { "type": "system", "subtype": "init", "session_id": session },
            {
                "type": "assistant",
                "session_id": session,
                "message": { "content": [{ "type": "text", "text": text }] },
            },
        ])
    }
}

/// Codex prints JSON lines, and `turn.completed` completes the answer.
fn codex(session: &str, text: &str, complete: bool) -> Vec<Value> {
    let mut events = vec![
        json!({ "type": "thread.started", "thread_id": session }),
        json!({ "type": "turn.started" }),
        json!({
            "type": "item.completed",
            "item": { "id": "item_0", "type": "agent_message", "text": text },
        }),
    ];
    if complete {
        events.push(json!({
            "type": "turn.completed",
            "usage": { "input_tokens": 1, "cached_input_tokens": 0, "output_tokens": 1 },
        }));
    }
    events
}

/// `OpenCode` prints JSON lines that all carry the session, and a `step_finish` that stops completes the answer.
fn opencode(session: &str, text: &str, complete: bool) -> Vec<Value> {
    let mut events = vec![
        json!({ "type": "step_start", "sessionID": session, "part": { "type": "step-start" } }),
        json!({ "type": "text", "sessionID": session, "part": { "type": "text", "text": text } }),
    ];
    if complete {
        events.push(json!({
            "type": "step_finish",
            "sessionID": session,
            "part": { "type": "step-finish", "reason": "stop" },
        }));
    }
    events
}

/// The `sleeper`: sleeps for its first argument in milliseconds, at most two minutes, says so, and exits with its second argument.
pub(crate) fn sleeper() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let number = |index: usize| {
        arguments
            .get(index)
            .and_then(|text| text.parse::<u64>().ok())
    };
    let milliseconds = number(0).unwrap_or(0).min(120_000);
    crate::pause(milliseconds);
    println!("sleeper slept {milliseconds}ms");
    number(1)
        .and_then(|code| u8::try_from(code).ok())
        .map_or(ExitCode::SUCCESS, ExitCode::from)
}
