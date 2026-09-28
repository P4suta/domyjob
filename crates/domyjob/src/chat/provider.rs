//! Structured results of the Claude Code, Codex, and OpenCode command-line clients.
//!
//! A turn counts only when the client reports one complete, successful answer and one session.

use domyjob_core::chat::card::Tool;
use serde_json::Value;

use crate::process::chat::MAX_OUTPUT;

/// The complete answer of one managed turn and the session that produced it.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Answer {
    pub(crate) text: String,
    pub(crate) session: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the AI CLI did not return one complete, valid answer")]
pub(crate) struct Incomplete;

/// A session identifier safe to pass back as one command-line argument.
#[must_use]
pub(crate) fn valid_session(session: &str) -> bool {
    (1..=256).contains(&session.len())
        && session
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        && session
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn json(bytes: &[u8]) -> Result<Value, Incomplete> {
    domyjob_core::ingress::json(bytes, MAX_OUTPUT).map_err(|_invalid| Incomplete)
}

fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str, Incomplete> {
    value.get(key).and_then(Value::as_str).ok_or(Incomplete)
}

fn answer(text: String, session: String) -> Result<Answer, Incomplete> {
    if text.trim().is_empty() || !valid_session(&session) {
        return Err(Incomplete);
    }
    Ok(Answer { text, session })
}

fn claude(output: &[u8]) -> Result<Answer, Incomplete> {
    let value = json(output)?;
    let result = match &value {
        Value::Object(_) => &value,
        Value::Array(events) => {
            let mut results = events
                .iter()
                .filter(|event| event.get("type").and_then(Value::as_str) == Some("result"));
            let result = results.next().ok_or(Incomplete)?;
            if results.next().is_some() {
                return Err(Incomplete);
            }
            result
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {
            return Err(Incomplete);
        }
    };
    if text(result, "type")? != "result" || result.get("is_error") != Some(&Value::Bool(false)) {
        return Err(Incomplete);
    }
    answer(
        text(result, "result")?.to_owned(),
        text(result, "session_id")?.to_owned(),
    )
}

#[derive(Default)]
struct Stream {
    session: Option<String>,
    answer: String,
    complete: bool,
}

impl Stream {
    fn session(&mut self, session: &str) -> Result<(), Incomplete> {
        if !valid_session(session)
            || self
                .session
                .as_deref()
                .is_some_and(|previous| previous != session)
        {
            return Err(Incomplete);
        }
        self.session = Some(session.to_owned());
        Ok(())
    }

    fn codex(&mut self, event: &Value) -> Result<(), Incomplete> {
        match text(event, "type")? {
            "thread.started" => self.session(text(event, "thread_id")?)?,
            "turn.started" => {
                self.complete = false;
                self.answer.clear();
            }
            "item.completed" => {
                let item = event.get("item").ok_or(Incomplete)?;
                if text(item, "type")? == "agent_message" {
                    text(item, "text")?.clone_into(&mut self.answer);
                }
            }
            "turn.completed" => self.complete = true,
            "turn.failed" | "error" => return Err(Incomplete),
            _ => {}
        }
        Ok(())
    }

    fn opencode(&mut self, event: &Value) -> Result<(), Incomplete> {
        self.session(text(event, "sessionID")?)?;
        match text(event, "type")? {
            "step_start" => {
                self.complete = false;
                self.answer.clear();
            }
            "text" => {
                let part = event.get("part").ok_or(Incomplete)?;
                self.answer.push_str(text(part, "text")?);
            }
            "step_finish" => {
                let part = event.get("part").ok_or(Incomplete)?;
                self.complete = text(part, "reason")? == "stop";
            }
            "error" => return Err(Incomplete),
            _ => {}
        }
        Ok(())
    }
}

/// Parse a client's complete output into exactly one answer.
pub(crate) fn parse(tool: Tool, output: &[u8]) -> Result<Answer, Incomplete> {
    if tool == Tool::Claude {
        return claude(output);
    }
    let mut stream = Stream::default();
    for line in output
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.iter().all(u8::is_ascii_whitespace))
    {
        let event = json(line)?;
        match tool {
            Tool::Codex => stream.codex(&event)?,
            Tool::Opencode => stream.opencode(&event)?,
            Tool::Claude => return Err(Incomplete),
        }
    }
    if !stream.complete {
        return Err(Incomplete);
    }
    answer(stream.answer, stream.session.ok_or(Incomplete)?)
}

#[cfg(test)]
mod tests {
    use domyjob_core::chat::card::Tool;

    use super::{parse, valid_session};

    #[test]
    fn complete_results_bind_one_answer_and_one_session() {
        let fixtures = [
            (
                Tool::Claude,
                r#"{"type":"result","is_error":false,"result":"answer","session_id":"s1"}"#,
            ),
            (
                Tool::Claude,
                r#"[{"type":"system"},{"type":"result","is_error":false,"result":"answer","session_id":"s1"}]"#,
            ),
            (
                Tool::Codex,
                "{\"type\":\"thread.started\",\"thread_id\":\"s1\"}\n{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"answer\"}}\n{\"type\":\"turn.completed\"}\n",
            ),
            (
                Tool::Opencode,
                "{\"type\":\"step_start\",\"sessionID\":\"s1\"}\n{\"type\":\"step_finish\",\"sessionID\":\"s1\",\"part\":{\"reason\":\"tool-calls\"}}\n{\"type\":\"step_start\",\"sessionID\":\"s1\"}\n{\"type\":\"text\",\"sessionID\":\"s1\",\"part\":{\"text\":\"answer\"}}\n{\"type\":\"step_finish\",\"sessionID\":\"s1\",\"part\":{\"reason\":\"stop\"}}\n",
            ),
        ];
        for (tool, fixture) in fixtures {
            let answer = parse(tool, fixture.as_bytes()).expect("complete structured result");
            assert_eq!(
                (answer.text.as_str(), answer.session.as_str()),
                ("answer", "s1")
            );
        }
    }

    #[test]
    fn incomplete_failed_or_malformed_output_never_becomes_an_answer() {
        for fixture in [
            "{\"type\":\"thread.started\",\"thread_id\":\"s1\"}\n{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"partial\"}}\n",
            "{\"type\":\"thread.started\",\"thread_id\":\"s1\"}\n{\"type\":\"turn.completed\"}\n",
            "{\"type\":\"turn.failed\"}\n",
            "debug text\n{\"type\":\"turn.completed\"}\n",
            "{\"type\":\"thread.started\",\"thread_id\":\"--last\"}\n",
            "{\"type\":\"thread.started\",\"thread_id\":\"s1\"}\n{\"type\":\"thread.started\",\"thread_id\":\"s2\"}\n",
        ] {
            parse(Tool::Codex, fixture.as_bytes()).expect_err("incomplete Codex output");
        }
        for fixture in [
            r#"{"type":"result","is_error":true,"result":"partial","session_id":"s1"}"#,
            r#"[{"type":"result","is_error":false,"result":"a","session_id":"s1"},{"type":"result","is_error":false,"result":"b","session_id":"s1"}]"#,
            r#"{"type":"result","is_error":false,"result":"   ","session_id":"s1"}"#,
        ] {
            parse(Tool::Claude, fixture.as_bytes()).expect_err("unusable Claude output");
        }
        parse(
            Tool::Opencode,
            br#"{"type":"text","sessionID":"s1","part":{"text":"partial"}}"#,
        )
        .expect_err("incomplete OpenCode output");
        for invalid in ["", "--last", "a\nb", "a b", "a/../../b"] {
            assert!(!valid_session(invalid));
        }
    }
}
