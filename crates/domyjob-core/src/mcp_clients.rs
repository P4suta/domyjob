//! Commands and checks that register `<program> mcp` as the stdio MCP server `domyjob` in AI clients.
//!
//! Each builder returns the arguments that follow the client's program name, for a process started without a shell.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use serde::Deserialize;

use crate::ingress;

/// The name under which every client knows the server.
pub const SERVER: &str = "domyjob";

/// The program of Claude Code.
pub const CLAUDE: &str = "claude";

/// The program of Codex.
pub const CODEX: &str = "codex";

/// The member path of the server in an `OpenCode` configuration, for [`set_member`](crate::jsonc::set_member) and [`remove_member`](crate::jsonc::remove_member).
pub const OPENCODE_MEMBER: [&str; 2] = ["mcp", SERVER];

/// Returns the Claude Code arguments that describe the registered server.
#[must_use]
pub fn claude_get() -> Vec<String> {
    ["mcp", "get", SERVER].map(String::from).into()
}

/// Returns the Claude Code arguments that remove the server from the user's configuration.
#[must_use]
pub fn claude_remove() -> Vec<String> {
    ["mcp", "remove", "--scope", "user", SERVER]
        .map(String::from)
        .into()
}

/// Returns the Claude Code arguments that register `program mcp` for all of the user's projects.
#[must_use]
pub fn claude_add(program: &str) -> Vec<String> {
    [
        "mcp",
        "add",
        "--scope",
        "user",
        "--transport",
        "stdio",
        SERVER,
        "--",
        program,
        "mcp",
    ]
    .map(String::from)
    .into()
}

/// Whether the output of the arguments from [`claude_get`] shows a user-scoped stdio server that runs exactly `program mcp`.
#[must_use]
pub fn claude_registration_matches(get_output: &str, program: &str) -> bool {
    let lines: Vec<&str> = get_output.lines().map(str::trim).collect();
    let command = format!("Command: {program}");
    lines
        .iter()
        .any(|line| line.starts_with("Scope: User config"))
        && ["Type: stdio", command.as_str(), "Args: mcp"]
            .iter()
            .all(|wanted| lines.contains(wanted))
}

/// Returns the Codex arguments that describe the registered server as JSON.
#[must_use]
pub fn codex_get() -> Vec<String> {
    ["mcp", "get", SERVER, "--json"].map(String::from).into()
}

/// Returns the Codex arguments that remove the server.
#[must_use]
pub fn codex_remove() -> Vec<String> {
    ["mcp", "remove", SERVER].map(String::from).into()
}

/// Returns the Codex arguments that register `program mcp`.
#[must_use]
pub fn codex_add(program: &str) -> Vec<String> {
    ["mcp", "add", SERVER, "--", program, "mcp"]
        .map(String::from)
        .into()
}

/// The part of `codex mcp get --json` that decides whether a registration is current.
#[derive(Deserialize)]
struct CodexServer {
    enabled: bool,
    transport: CodexTransport,
}

#[derive(Deserialize)]
struct CodexTransport {
    #[serde(rename = "type")]
    kind: String,
    command: String,
    args: Vec<String>,
}

/// Whether the output of the arguments from [`codex_get`] shows an enabled stdio server that runs exactly `program mcp`.
#[must_use]
pub fn codex_registration_matches(get_json: &str, program: &str) -> bool {
    match ingress::foreign_json::<CodexServer>(get_json) {
        Ok(server) => {
            server.enabled
                && server.transport.kind == "stdio"
                && server.transport.command == program
                && server.transport.args == ["mcp"]
        }
        Err(_unregistered) => false,
    }
}

/// Returns the JSON value that runs `program mcp` as a local `OpenCode` server, for the member at [`OPENCODE_MEMBER`].
#[must_use]
pub fn opencode_mcp_value(program: &str) -> String {
    format!(
        r#"{{"type":"local","command":[{},"mcp"],"enabled":true}}"#,
        serde_json::Value::from(program)
    )
}

#[cfg(test)]
mod tests {
    use super::{
        OPENCODE_MEMBER, claude_add, claude_get, claude_registration_matches, claude_remove,
        codex_add, codex_get, codex_registration_matches, codex_remove, opencode_mcp_value,
    };
    use crate::{ingress, jsonc};
    use serde_json::{Value, json};

    const CLAUDE_OUTPUT: &str = "domyjob:\n  Scope: User config (available in all your projects)\n  Status: ✓ Connected\n  Type: stdio\n  Command: /bin/domyjob\n  Args: mcp\n  Environment:\n\nTo remove this server, run: claude mcp remove \"domyjob\" -s user\n";
    const CODEX_OUTPUT: &str = r#"{"name":"domyjob","enabled":true,"transport":{"type":"stdio","command":"/bin/domyjob","args":["mcp"],"env":null,"env_vars":[],"cwd":null},"startup_timeout_sec":null}"#;

    #[test]
    fn commands_register_the_program_as_a_stdio_server() {
        let program = "/opt/my tools/domyjob";
        assert_eq!(claude_get(), ["mcp", "get", "domyjob"]);
        assert_eq!(
            claude_remove(),
            ["mcp", "remove", "--scope", "user", "domyjob"]
        );
        assert_eq!(
            claude_add(program),
            [
                "mcp",
                "add",
                "--scope",
                "user",
                "--transport",
                "stdio",
                "domyjob",
                "--",
                program,
                "mcp"
            ]
        );
        assert_eq!(codex_get(), ["mcp", "get", "domyjob", "--json"]);
        assert_eq!(codex_remove(), ["mcp", "remove", "domyjob"]);
        assert_eq!(
            codex_add(program),
            ["mcp", "add", "domyjob", "--", program, "mcp"]
        );
    }

    #[test]
    fn claude_registration_needs_user_scope_stdio_and_the_exact_command() {
        assert!(claude_registration_matches(CLAUDE_OUTPUT, "/bin/domyjob"));
        assert!(claude_registration_matches(
            &CLAUDE_OUTPUT.replace('\n', "\r\n"),
            "/bin/domyjob"
        ));
        assert!(!claude_registration_matches(CLAUDE_OUTPUT, "/bin/old"));
        assert!(!claude_registration_matches(CLAUDE_OUTPUT, "/bin/dom"));
        for (from, to) in [
            ("User config", "Local config"),
            ("Type: stdio", "Type: sse"),
            ("Args: mcp", "Args: mcp --verbose"),
            ("  Args: mcp\n", ""),
        ] {
            let changed = CLAUDE_OUTPUT.replace(from, to);
            assert!(
                !claude_registration_matches(&changed, "/bin/domyjob"),
                "{changed}"
            );
        }
        let windows = CLAUDE_OUTPUT.replace("/bin/domyjob", r"C:\Program Files\domyjob.exe");
        assert!(claude_registration_matches(
            &windows,
            r"C:\Program Files\domyjob.exe"
        ));
        assert!(!claude_registration_matches("", "/bin/domyjob"));
    }

    #[test]
    fn codex_registration_needs_an_enabled_stdio_server_with_the_exact_command() {
        assert!(codex_registration_matches(CODEX_OUTPUT, "/bin/domyjob"));
        assert!(!codex_registration_matches(CODEX_OUTPUT, "/bin/old"));
        for (from, to) in [
            (r#""enabled":true"#, r#""enabled":false"#),
            (r#""type":"stdio""#, r#""type":"streamable_http""#),
            (r#"["mcp"]"#, r#"["mcp","--verbose"]"#),
            (r#"["mcp"]"#, "[]"),
            (r#""command":"/bin/domyjob","#, ""),
        ] {
            let changed = CODEX_OUTPUT.replace(from, to);
            assert!(
                !codex_registration_matches(&changed, "/bin/domyjob"),
                "{changed}"
            );
        }
        let pretty = "{\n  \"enabled\": true,\n  \"transport\": {\n    \"type\": \"stdio\",\n    \"command\": \"C:\\\\Tools\\\\domyjob.exe\",\n    \"args\": [\n      \"mcp\"\n    ]\n  }\n}\n";
        assert!(codex_registration_matches(pretty, r"C:\Tools\domyjob.exe"));
        for output in ["", "No MCP server named 'domyjob' found.", "[]"] {
            assert!(!codex_registration_matches(output, "/bin/domyjob"));
        }
    }

    #[test]
    fn opencode_value_runs_the_program_and_fits_the_configuration() {
        let program = r#"C:\Program Files\"domyjob".exe"#;
        let value = opencode_mcp_value(program);
        assert_eq!(
            value,
            r#"{"type":"local","command":["C:\\Program Files\\\"domyjob\".exe","mcp"],"enabled":true}"#
        );
        let document = jsonc::set_member("", &OPENCODE_MEMBER, &value).unwrap();
        let parsed: Value = ingress::foreign_json(&jsonc::strip(&document)).unwrap();
        assert_eq!(
            parsed,
            json!({"mcp": {"domyjob": {"type": "local", "command": [program, "mcp"], "enabled": true}}})
        );
        assert_eq!(
            jsonc::remove_member(&document, &OPENCODE_MEMBER).unwrap(),
            "{\n  \"mcp\": {\n  }\n}\n"
        );
    }
}
