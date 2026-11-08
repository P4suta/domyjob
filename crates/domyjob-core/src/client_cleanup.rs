use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsonClient {
    Claude,
    OpenCode,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigurationUpdate(String);

impl ConfigurationUpdate {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CleanupError {
    #[error(transparent)]
    Json(#[from] crate::ingress::JsonError),
    #[error(transparent)]
    Jsonc(#[from] crate::jsonc::JsoncError),
    #[error("the client configuration has an invalid {0}")]
    Shape(&'static str),
}

pub fn retire_hooks(document: &str) -> Result<Option<ConfigurationUpdate>, CleanupError> {
    let mut parsed = configuration(document)?;
    let Some(events) = parsed.get_mut("hooks") else {
        return Ok(None);
    };
    let events = events
        .as_object_mut()
        .ok_or(CleanupError::Shape("hooks map"))?;
    let mut edited = String::from(document);
    let mut changed = false;
    for event in ["SessionStart", "PreToolUse", "PostToolUse", "Stop"] {
        let Some(groups) = events.get_mut(event) else {
            continue;
        };
        let groups = groups
            .as_array_mut()
            .ok_or(CleanupError::Shape("event groups"))?;
        let before = groups.clone();
        for group in groups.iter_mut() {
            if !group.is_object() {
                return Err(CleanupError::Shape("matcher group"));
            }
            if let Some(handlers) = group.get_mut("hooks") {
                let handlers = handlers
                    .as_array_mut()
                    .ok_or(CleanupError::Shape("handlers"))?;
                handlers.retain(|handler| !owned_hook(handler));
            }
        }
        if *groups != before {
            groups.retain(|group| {
                !group
                    .get("hooks")
                    .and_then(Value::as_array)
                    .is_some_and(Vec::is_empty)
            });
            edited = crate::jsonc::set_member(
                &edited,
                &["hooks", event],
                &Value::from(groups.clone()).to_string(),
            )?;
            changed = true;
        }
    }
    update(edited, changed)
}

pub fn retire_mcp(
    document: &str,
    client: JsonClient,
) -> Result<Option<ConfigurationUpdate>, CleanupError> {
    let parsed = configuration(document)?;
    let member = match client {
        JsonClient::Claude => "mcpServers",
        JsonClient::OpenCode => "mcp",
    };
    let Some(servers) = parsed.get(member) else {
        return Ok(None);
    };
    let servers = servers
        .as_object()
        .ok_or(CleanupError::Shape("MCP servers"))?;
    let Some(server) = servers.get("domyjob") else {
        return Ok(None);
    };
    let owned = match client {
        JsonClient::Claude => server
            .get("command")
            .and_then(Value::as_str)
            .zip(server.get("args").and_then(strings))
            .is_some_and(|(program, arguments)| owns_words(program, &arguments)),
        JsonClient::OpenCode => server
            .get("command")
            .and_then(strings)
            .is_some_and(|words| {
                words
                    .split_first()
                    .is_some_and(|(program, arguments)| owns_words(program, arguments))
            }),
    };
    if !owned {
        return Ok(None);
    }
    update(
        crate::jsonc::remove_member(document, &[member, "domyjob"])?,
        true,
    )
}

#[must_use]
pub fn owns_mcp(program: &str, arguments: &[String]) -> bool {
    program_name(program) && arguments == ["mcp"]
}

fn check_length(document: &str) -> Result<(), CleanupError> {
    if document.len() > crate::ingress::MAX_FOREIGN_BYTES {
        return Err(crate::ingress::JsonError::TooLarge {
            limit: crate::ingress::MAX_FOREIGN_BYTES,
        }
        .into());
    }
    Ok(())
}

fn configuration(document: &str) -> Result<Value, CleanupError> {
    check_length(document)?;
    let parsed: Value = crate::ingress::foreign_json(&crate::jsonc::strip(document))?;
    if !parsed.is_object() {
        return Err(CleanupError::Shape("root"));
    }
    Ok(parsed)
}

fn update(document: String, changed: bool) -> Result<Option<ConfigurationUpdate>, CleanupError> {
    check_length(&document)?;
    Ok(changed.then_some(ConfigurationUpdate(document)))
}

fn strings(value: &Value) -> Option<Vec<&str>> {
    value.as_array()?.iter().map(Value::as_str).collect()
}

fn program_name(program: &str) -> bool {
    matches!(
        program.rsplit(['/', '\\']).next(),
        Some("domyjob" | "domyjob.exe")
    )
}

fn owns_words(program: &str, arguments: &[&str]) -> bool {
    program_name(program) && arguments == ["mcp"]
}

fn owned_hook(handler: &Value) -> bool {
    if handler.get("type").and_then(Value::as_str) != Some("command") {
        return false;
    }
    let Some(command) = handler.get("command").and_then(Value::as_str) else {
        return false;
    };
    if let Some(arguments) = handler.get("args").and_then(strings)
        && let ["chat", "hook", phase, "--client", client, "--if-bound"] = arguments.as_slice()
    {
        return program_name(command) && hook_phase(phase) && hook_client(client);
    }
    for phase in ["start", "pre", "post", "stop"] {
        for client in ["claude", "codex", "opencode"] {
            let suffix = format!(" chat hook {phase} --client {client} --if-bound");
            if let Some(program) = command.strip_suffix(&suffix) {
                let program = program.trim();
                if let Some(quoted) = program
                    .strip_prefix('\'')
                    .and_then(|value| value.strip_suffix('\''))
                {
                    let decoded = quoted.replace("'\\''", "'");
                    if program_name(&decoded)
                        && (decoded.starts_with('/') || decoded.contains(":\\"))
                    {
                        return true;
                    }
                } else if !program.chars().any(|character| {
                    character.is_whitespace() || ";&|$()<>`\"'".contains(character)
                }) && program_name(program)
                {
                    return true;
                }
            }
        }
    }
    false
}

fn hook_phase(phase: &str) -> bool {
    matches!(phase, "start" | "pre" | "post" | "stop")
}

fn hook_client(client: &str) -> bool {
    matches!(client, "claude" | "codex" | "opencode")
}

#[cfg(test)]
mod tests {
    use alloc::string::{String, ToString as _};
    use serde_json::{Value, json};

    use super::{JsonClient, owns_mcp, retire_hooks, retire_mcp};

    #[test]
    fn cleanup_removes_only_owned_handlers_from_shared_hook_groups() {
        let foreign = json!({"type":"command", "command":"other-tool", "statusMessage":"domyjob: chat lifecycle"});
        let owned = json!({
            "type":"command", "command":"'/owner tools/domyjob' chat hook pre --client codex --if-bound",
            "commandWindows":"powershell.exe -EncodedCommand encoded", "statusMessage":"domyjob: chat lifecycle"
        });
        let input = json!({"keep":{"enabled":true}, "hooks":{"PreToolUse":[
            {"matcher":"Bash", "hooks":[foreign, owned]},
            {"hooks":[{"type":"command", "command":"/bin/domyjob", "args":["chat","hook","pre","--client","claude","--if-bound"]}]}
        ]}});
        let cleaned = retire_hooks(&input.to_string()).unwrap().unwrap();
        let output: Value = crate::ingress::foreign_json(cleaned.as_str()).unwrap();
        assert_eq!(
            output,
            json!({"keep":{"enabled":true}, "hooks":{"PreToolUse":[
                {"matcher":"Bash", "hooks":[foreign]}
            ]}})
        );
        assert!(retire_hooks(cleaned.as_str()).unwrap().is_none());
    }

    #[test]
    fn foreign_hooks_and_configuration_bytes_remain_unchanged() {
        let input = "{\n  // keep this comment\n  \"hooks\": {\"Stop\": [{\"hooks\": [{\"type\":\"command\",\"command\":\"echo /bin/domyjob chat hook stop --client codex --if-bound\",\"statusMessage\":\"domyjob: chat lifecycle\"}]}]}\n}\n";
        assert!(retire_hooks(input).unwrap().is_none());
    }

    #[test]
    fn mcp_cleanup_preserves_other_servers_and_jsonc_comments() {
        let input = "{\n // unrelated settings\n \"theme\":\"system\",\n \"mcp\":{\"other\":{\"url\":\"https://example.invalid\"},\"domyjob\":{\"type\":\"local\",\"command\":[\"C:\\\\tools\\\\domyjob.exe\",\"mcp\"],\"enabled\":false}}\n}\n";
        let cleaned = retire_mcp(input, JsonClient::OpenCode).unwrap().unwrap();
        assert!(cleaned.as_str().contains("// unrelated settings"));
        let output: Value =
            crate::ingress::foreign_json(&crate::jsonc::strip(cleaned.as_str())).unwrap();
        assert_eq!(
            output,
            json!({"theme":"system", "mcp":{"other":{"url":"https://example.invalid"}}})
        );
        assert!(
            retire_mcp(cleaned.as_str(), JsonClient::OpenCode)
                .unwrap()
                .is_none()
        );
        let claude = json!({"mcpServers":{"domyjob":{"command":"/bin/domyjob","args":["mcp"]}},"other":"keep"});
        let claude_cleaned = retire_mcp(&claude.to_string(), JsonClient::Claude)
            .unwrap()
            .unwrap();
        let claude_output: Value = crate::ingress::foreign_json(claude_cleaned.as_str()).unwrap();
        assert_eq!(claude_output, json!({"mcpServers":{},"other":"keep"}));
    }

    #[test]
    fn shared_names_do_not_grant_configuration_ownership() {
        for program in ["other", "/bin/domyjob-helper", "/bin/domyjob.exe.backup"] {
            assert!(!owns_mcp(program, &[String::from("mcp")]));
        }
        assert!(!owns_mcp("/bin/domyjob", &[String::from("other")]));
        let input = json!({"mcpServers":{"domyjob":{"command":"other","args":["mcp"]}}});
        assert!(
            retire_mcp(&input.to_string(), JsonClient::Claude)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn malformed_configuration_is_refused_before_any_edit() {
        for input in [
            "{",
            "[]",
            "{\"hooks\":false}",
            "{\"hooks\":{\"Stop\":false}}",
        ] {
            assert!(retire_hooks(input).is_err(), "{input}");
        }
    }

    #[test]
    fn ambiguous_configuration_cannot_remove_a_foreign_registration() {
        let input = r#"{"mcpServers":{"domyjob":{"command":"foreign","args":["mcp"]},"domyjob":{"command":"/bin/domyjob","args":["mcp"]}}}"#;
        retire_mcp(input, JsonClient::Claude).unwrap_err();
    }
}
