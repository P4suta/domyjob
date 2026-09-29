use std::fs::{self, TryLockError};
use std::io::{self, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

use serde_json::{Value, json};

use crate::{Context as _, Failure};

const HUGE_ANSWER_BYTES: usize = 4_198_400;
const HANG_MILLISECONDS: u64 = 60_000;
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

    const fn resume_flag(self) -> &'static str {
        match self {
            Self::Claude => "--resume",
            Self::Codex => "resume",
            Self::Opencode => "--session",
        }
    }

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
        Some((
            crate::os::parent_of(std::process::id()).context("finding the parent process")?,
            start_sleeper()?,
        ))
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

    fn words(&self) -> impl Iterator<Item = &str> {
        self.prompt
            .split(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
            .filter(|word| !word.is_empty())
    }

    fn says(&self, keyword: &str) -> bool {
        self.words().any(|word| word == keyword)
    }

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

fn delegate(target: &str, question: &str) -> Result<String, Failure> {
    let question = if question.is_empty() {
        "a delegated question"
    } else {
        question
    };
    let output = Command::new("domyjob")
        .args([
            "chat",
            "--json",
            "ask",
            target,
            question,
            "--timeout",
            "120",
        ])
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

fn rendezvous(cwd: &Path) -> Result<bool, Failure> {
    let place = cwd
        .parent()
        .ok_or_else(|| Failure::new("the working directory has no parent"))?
        .join("rendezvous.jsonl");
    crate::append_line(&place, &json!(std::process::id())).context("joining the rendezvous")?;
    for _ in 0..crate::world::POLLS {
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
