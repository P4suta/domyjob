#![expect(
    clippy::disallowed_methods,
    reason = "this adapter owns fixed AI CLI invocation and detached chat worker creation"
)]

use std::io::{self, Read, Seek, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use crate::chat::AgentTool;

use super::{Group, ProcessError};

const MAX_OUTPUT: usize = 4 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub(crate) enum ChatProcessError {
    #[error(transparent)]
    Process(#[from] ProcessError),
    #[error("running the AI CLI: {0}")]
    Io(#[from] io::Error),
    #[error("the AI CLI exited unsuccessfully: {0}")]
    Failed(std::process::ExitStatus),
    #[error("the AI CLI exceeded the 4 MiB output limit")]
    TooLarge,
    #[error("the AI session ID is invalid")]
    Session,
}

pub(crate) fn valid_session(session: &str) -> bool {
    !session.is_empty()
        && session.len() <= 256
        && session
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        && session
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn command(tool: AgentTool, session: Option<&str>) -> Result<Command, ChatProcessError> {
    if session.is_some_and(|id| !valid_session(id)) {
        return Err(ChatProcessError::Session);
    }
    let mut command = Command::new(tool.as_str());
    match tool {
        AgentTool::Claude => {
            command.args(["-p", "--output-format", "json"]);
            if let Some(session) = session {
                command.args(["--resume", session]);
            }
        }
        AgentTool::Codex => {
            command.args([
                "exec",
                "--sandbox",
                "read-only",
                "--json",
                "--skip-git-repo-check",
                "-c",
                "approval_policy=\"never\"",
            ]);
            if let Some(session) = session {
                command.args(["resume", session]);
            }
            command.arg("-");
        }
        AgentTool::Opencode => {
            command.args(["run", "--format", "json"]);
            if let Some(session) = session {
                command.args(["--session", session]);
            }
        }
    }
    crate::platform::prepare_job_environment(&mut command);
    for name in [
        "CODEX_HOME",
        "CODEX_API_KEY",
        "ANTHROPIC_API_KEY",
        "OPENAI_API_KEY",
        "OPENAI_BASE_URL",
        "OPENCODE_CONFIG",
        "OPENCODE_CONFIG_DIR",
        "CLAUDE_CONFIG_DIR",
        "DOMYJOB_STATE",
    ] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    Ok(command)
}

#[derive(Clone, Copy)]
pub(crate) struct Invocation<'a> {
    pub(crate) tool: AgentTool,
    pub(crate) cwd: &'a Path,
    pub(crate) session: Option<&'a str>,
    pub(crate) prompt: &'a str,
    pub(crate) agent: &'a str,
}

pub(crate) fn run(invocation: Invocation<'_>) -> Result<Vec<u8>, ChatProcessError> {
    let mut command = command(invocation.tool, invocation.session)?;
    command
        .current_dir(invocation.cwd)
        .env("DOMYJOB_CHAT_AGENT", invocation.agent);
    let mut input = tempfile::tempfile()?;
    input.write_all(invocation.prompt.as_bytes())?;
    input.rewind()?;
    let (reader, writer) = io::pipe()?;
    let group = Group::spawn_io(
        command,
        Stdio::from(input),
        Stdio::from(writer),
        Stdio::null(),
    )?;
    let limit = u64::try_from(MAX_OUTPUT.saturating_add(1)).map_err(io::Error::other)?;
    std::thread::scope(|scope| {
        let capture = scope.spawn(|| {
            let mut bytes = Vec::new();
            let read = reader.take(limit).read_to_end(&mut bytes);
            if read.is_err() || bytes.len() > MAX_OUTPUT {
                group.kill()?;
            }
            read?;
            if bytes.len() > MAX_OUTPUT {
                return Err(ChatProcessError::TooLarge);
            }
            Ok(bytes)
        });
        let status = group.wait();
        let bytes = capture
            .join()
            .map_err(|_panic| io::Error::other("AI output reader panicked"))??;
        let status = status?;
        if !status.success() {
            return Err(ChatProcessError::Failed(status));
        }
        Ok(bytes)
    })
}

pub(crate) fn spawn_chat_worker(agent: &str) -> Result<(), ChatProcessError> {
    Ok(super::launch_chat_worker(agent)?)
}

pub(crate) fn notify_message(id: &str) -> Result<(), ChatProcessError> {
    let message = format!("New chat message {id}");
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = Command::new("osascript");
        command.args([
            "-e",
            "on run argv\ndisplay notification (item 1 of argv) with title \"domyjob\"\nend run",
            "--",
            &message,
        ]);
        command
    };
    #[cfg(target_os = "linux")]
    let mut command = {
        let mut command = Command::new("notify-send");
        command.args(["--", "domyjob", &message]);
        command
    };
    #[cfg(windows)]
    let mut command = {
        let username = std::env::var_os("USERNAME")
            .ok_or_else(|| io::Error::other("the notification recipient is unavailable"))?;
        let mut command = Command::new("msg.exe");
        command.arg(username).arg(&message);
        command
    };
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    std::thread::spawn(move || match child.wait() {
        Ok(status) if status.success() => {}
        Ok(status) => eprintln!("domyjob: chat notification failed: {status}"),
        Err(error) => eprintln!("domyjob: waiting for chat notification: {error}"),
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{command, valid_session};
    use crate::chat::AgentTool;

    #[test]
    fn managed_commands_keep_prompts_off_the_command_line_and_preserve_sandboxing() {
        let command = command(AgentTool::Codex, Some("session-1")).expect("command");
        let args: Vec<_> = command
            .get_args()
            .map(|arg| arg.to_string_lossy())
            .collect();
        assert_eq!(
            args,
            [
                "exec",
                "--sandbox",
                "read-only",
                "--json",
                "--skip-git-repo-check",
                "-c",
                "approval_policy=\"never\"",
                "resume",
                "session-1",
                "-"
            ]
        );
        for invalid in ["", "--last", "a\nb", "a b", "a/../../b"] {
            assert!(!valid_session(invalid));
            super::command(AgentTool::Claude, Some(invalid)).expect_err("invalid session");
        }
    }
}
