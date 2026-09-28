#![expect(
    clippy::disallowed_methods,
    reason = "this adapter owns fixed AI CLI invocation"
)]

use std::io::{self, Read, Seek, Write};
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};

use domyjob_core::chat::card::{Access, Tool};
use domyjob_core::chat::id::{AgentId, EventId};

use super::{Group, ProcessError};

const MAX_OUTPUT: usize = 4 * 1024 * 1024;
const MAX_DIAGNOSTIC: usize = 4096;

#[derive(Debug, thiserror::Error)]
pub(crate) enum ChatProcessError {
    #[error(transparent)]
    Process(#[from] ProcessError),
    #[error("running the AI CLI: {0}")]
    Io(#[from] io::Error),
    #[error("{0} is not installed or not on PATH")]
    Missing(&'static str),
    #[error("the AI CLI exited unsuccessfully ({status}): {detail}")]
    Failed { status: ExitStatus, detail: String },
    #[error("the AI CLI exceeded the 4 MiB output limit")]
    TooLarge,
    #[error("the AI session ID is invalid")]
    Session,
}

/// Everything one managed turn passes to its AI client.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Invocation<'a> {
    pub(crate) tool: Tool,
    pub(crate) access: Access,
    pub(crate) cwd: &'a Path,
    pub(crate) session: Option<&'a str>,
    pub(crate) prompt: &'a str,
    pub(crate) agent: &'a AgentId,
    pub(crate) turn: &'a EventId,
}

/// The `domyjob mcp` arguments that bind the client's chat tools to this agent and turn.
fn bound_mcp(invocation: &Invocation<'_>) -> Vec<String> {
    vec![
        "mcp".to_owned(),
        "--as".to_owned(),
        invocation.agent.to_string(),
        "--turn".to_owned(),
        invocation.turn.to_string(),
    ]
}

fn json_text(value: &serde_json::Value) -> String {
    value.to_string()
}

/// Whether `session` is a session ID this client resumes exactly.
///
/// Codex treats anything but a UUID as a thread name and silently starts a new thread when none matches.
pub(crate) fn resumable(tool: Tool, session: &str) -> bool {
    let uuid = || {
        let groups: Vec<&str> = session.split('-').collect();
        groups.iter().map(|group| group.len()).eq([8, 4, 4, 4, 12])
            && groups
                .iter()
                .all(|group| group.bytes().all(|byte| byte.is_ascii_hexdigit()))
    };
    crate::chat::provider::valid_session(session)
        && match tool {
            Tool::Codex => uuid(),
            Tool::Claude | Tool::Opencode => true,
        }
}

/// The read-only agent injected into OpenCode, whose own permissions follow the user's.
const OPENCODE_READ_AGENT: &str = "domyjob-read";

/// The argument vector after the program, with the prompt read from standard input.
fn arguments(invocation: &Invocation<'_>, domyjob: &str) -> Result<Vec<String>, ChatProcessError> {
    if invocation
        .session
        .is_some_and(|session| !resumable(invocation.tool, session))
    {
        return Err(ChatProcessError::Session);
    }
    let mcp = bound_mcp(invocation);
    let owned = |words: &[&str]| {
        words
            .iter()
            .map(|word| (*word).to_owned())
            .collect::<Vec<_>>()
    };
    let mut words = match invocation.tool {
        Tool::Claude => {
            let config = serde_json::json!({
                "mcpServers": {"domyjob": {"type": "stdio", "command": domyjob, "args": mcp}}
            });
            let mut words = owned(&["-p", "--output-format", "json", "--strict-mcp-config"]);
            words.push(format!("--mcp-config={}", json_text(&config)));
            words.push("--allowedTools=mcp__domyjob".to_owned());
            words.extend(match invocation.access {
                Access::Read => owned(&["--permission-mode", "dontAsk", "--tools=Read,Grep,Glob"]),
                Access::Write => {
                    owned(&["--permission-mode", "auto", "--permission-prompts", "none"])
                }
            });
            words
        }
        Tool::Codex => {
            let sandbox = match invocation.access {
                Access::Read => "read-only",
                Access::Write => "workspace-write",
            };
            let mut words = owned(&[
                "exec",
                "--json",
                "--skip-git-repo-check",
                "--sandbox",
                sandbox,
            ]);
            for setting in [
                format!(
                    "mcp_servers.domyjob.command={}",
                    json_text(&serde_json::json!(domyjob))
                ),
                format!(
                    "mcp_servers.domyjob.args={}",
                    json_text(&serde_json::json!(mcp))
                ),
                "mcp_servers.domyjob.default_tools_approval_mode=\"approve\"".to_owned(),
            ] {
                words.extend(["-c".to_owned(), setting]);
            }
            words
        }
        Tool::Opencode => {
            let mut words = owned(&["run", "--format", "json"]);
            if invocation.access == Access::Read {
                words.extend(["--agent".to_owned(), OPENCODE_READ_AGENT.to_owned()]);
            }
            words
        }
    };
    if let Some(session) = invocation.session {
        let flag = match invocation.tool {
            Tool::Claude => "--resume",
            Tool::Codex => "resume",
            Tool::Opencode => "--session",
        };
        words.extend([flag.to_owned(), session.to_owned()]);
    }
    if invocation.tool == Tool::Codex {
        words.push("-".to_owned());
    }
    Ok(words)
}

/// Configuration a client reads from its environment instead of its arguments.
fn environment(invocation: &Invocation<'_>, domyjob: &str) -> Vec<(&'static str, String)> {
    match invocation.tool {
        Tool::Claude | Tool::Codex => Vec::new(),
        Tool::Opencode => {
            let mut command = vec![domyjob.to_owned()];
            command.extend(bound_mcp(invocation));
            let deny = serde_json::json!({"edit": "deny", "bash": "deny", "task": "deny", "webfetch": "deny"});
            let config = serde_json::json!({
                "mcp": {"domyjob": {"type": "local", "command": command, "enabled": true}},
                "agent": {OPENCODE_READ_AGENT: {"mode": "primary", "permission": deny}},
            });
            vec![("OPENCODE_CONFIG_CONTENT", json_text(&config))]
        }
    }
}

fn command(invocation: &Invocation<'_>) -> Result<Command, ChatProcessError> {
    let program = crate::platform::find_program(invocation.tool.as_str())
        .ok_or(ChatProcessError::Missing(invocation.tool.as_str()))?;
    let domyjob = std::env::current_exe()?;
    let domyjob = domyjob
        .to_str()
        .ok_or_else(|| io::Error::other("the domyjob path is not UTF-8"))?;
    let mut command = Command::new(program);
    command.args(arguments(invocation, domyjob)?);
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
    for (name, value) in environment(invocation, domyjob) {
        command.env(name, value);
    }
    command
        .current_dir(invocation.cwd)
        .env("DOMYJOB_CHAT_AGENT", invocation.agent.to_string())
        .env("DOMYJOB_CHAT_TURN", invocation.turn.to_string());
    Ok(command)
}

/// The end of a client's diagnostic output as printable text.
fn recent(bytes: &[u8]) -> String {
    let start = bytes.len().saturating_sub(MAX_DIAGNOSTIC);
    domyjob_core::domain::terminal_text(
        String::from_utf8_lossy(bytes.get(start..).unwrap_or_default()).trim(),
    )
}

fn tail(file: &mut std::fs::File) -> io::Result<String> {
    let limit = u64::try_from(MAX_DIAGNOSTIC).map_err(io::Error::other)?;
    let length = file.seek(io::SeekFrom::End(0))?;
    file.seek(io::SeekFrom::Start(length.saturating_sub(limit)))?;
    let mut bytes = Vec::new();
    file.take(limit).read_to_end(&mut bytes)?;
    Ok(recent(&bytes))
}

/// Run one turn and return the client's complete standard output.
pub(crate) fn run(invocation: &Invocation<'_>) -> Result<Vec<u8>, ChatProcessError> {
    let command = command(invocation)?;
    let mut input = tempfile::tempfile()?;
    input.write_all(invocation.prompt.as_bytes())?;
    input.rewind()?;
    let mut errors = tempfile::tempfile()?;
    let (reader, writer) = io::pipe()?;
    let group = Group::spawn_io(
        command,
        Stdio::from(input),
        Stdio::from(writer),
        Stdio::from(errors.try_clone()?),
    )?;
    let limit = u64::try_from(MAX_OUTPUT.saturating_add(1)).map_err(io::Error::other)?;
    let (status, bytes) = std::thread::scope(|scope| {
        let capture = scope.spawn(|| -> Result<Vec<u8>, ChatProcessError> {
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
            .map_err(|_panic| io::Error::other("the AI output reader panicked"));
        (status, bytes)
    });
    let bytes = bytes??;
    let status = status?;
    if !status.success() {
        let mut detail = tail(&mut errors)?;
        if detail.is_empty() {
            // Clients in JSON mode may report their failure on standard output instead.
            detail = recent(&bytes);
        }
        return Err(ChatProcessError::Failed { status, detail });
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use domyjob_core::chat::card::{Access, Tool};
    use domyjob_core::chat::id::{AgentId, EventId};

    use super::{Invocation, arguments};

    fn invocation(tool: Tool, access: Access, session: Option<&str>) -> Vec<String> {
        let agent = AgentId::try_from(format!("reviewer@{}", "a".repeat(32))).unwrap();
        let turn = EventId::try_from(format!("{}:{:016x}", "b".repeat(32), 3)).unwrap();
        arguments(
            &Invocation {
                tool,
                access,
                cwd: Path::new("/work"),
                session,
                prompt: "hello",
                agent: &agent,
                turn: &turn,
            },
            "/bin/domyjob",
        )
        .unwrap()
    }

    #[test]
    fn prompts_stay_off_the_command_line_and_access_maps_to_each_client() {
        let session = "0199a2b3-c4d5-7e6f-8a9b-0c1d2e3f4a5b";
        let codex = invocation(Tool::Codex, Access::Read, Some(session));
        assert!(
            codex
                .windows(2)
                .any(|pair| pair == ["--sandbox", "read-only"])
        );
        assert_eq!(
            codex.iter().rev().take(3).collect::<Vec<_>>(),
            ["-", session, "resume"]
        );
        let resume = codex.iter().position(|word| word == "resume").unwrap();
        let sandbox = codex.iter().position(|word| word == "--sandbox").unwrap();
        assert!(
            sandbox < resume,
            "codex accepts --sandbox only before resume"
        );
        assert!(
            codex.iter().any(
                |word| word.starts_with("mcp_servers.domyjob.args=") && word.contains("--turn")
            )
        );
        let write = invocation(Tool::Codex, Access::Write, None);
        assert!(
            write
                .windows(2)
                .any(|pair| pair == ["--sandbox", "workspace-write"])
        );
        let claude = invocation(Tool::Claude, Access::Write, Some("s1"));
        assert!(
            claude
                .windows(2)
                .any(|pair| pair == ["--permission-mode", "auto"])
        );
        assert!(claude.windows(2).any(|pair| pair == ["--resume", "s1"]));
        assert!(claude.iter().any(|word| word == "--strict-mcp-config"));
        let read = invocation(Tool::Claude, Access::Read, None);
        assert!(
            read.windows(2)
                .any(|pair| pair == ["--permission-mode", "dontAsk"])
        );
        assert!(read.iter().any(|word| word == "--tools=Read,Grep,Glob"));
        let opencode = invocation(Tool::Opencode, Access::Read, None);
        assert!(
            opencode
                .windows(2)
                .any(|pair| pair == ["--agent", "domyjob-read"])
        );
        for words in [codex, claude, opencode] {
            assert!(!words.iter().any(|word| word == "hello"));
        }
    }

    #[test]
    fn invalid_sessions_never_reach_the_command_line() {
        let agent = AgentId::try_from(format!("reviewer@{}", "a".repeat(32))).unwrap();
        let turn = EventId::try_from(format!("{}:{:016x}", "b".repeat(32), 3)).unwrap();
        for (tool, session) in [
            (Tool::Claude, "--last"),
            (Tool::Claude, "a b"),
            (Tool::Claude, ""),
            (Tool::Codex, "named-thread"),
        ] {
            arguments(
                &Invocation {
                    tool,
                    access: Access::Read,
                    cwd: Path::new("/work"),
                    session: Some(session),
                    prompt: "hello",
                    agent: &agent,
                    turn: &turn,
                },
                "/bin/domyjob",
            )
            .unwrap_err();
        }
    }
}
