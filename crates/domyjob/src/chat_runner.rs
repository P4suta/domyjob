#![expect(
    clippy::redundant_pub_crate,
    reason = "the binary composition root uses this private module"
)]

use std::collections::BTreeSet;
use std::path::Path;

use serde_json::Value;

use crate::chat::{
    AgentTool, ChatError, Event, EventData, MAX_TEXT, MessageMode, Store, TurnFailure,
};
use crate::process::chat::{self as process, ChatProcessError, Invocation};

#[derive(Debug, thiserror::Error)]
pub(crate) enum RunnerError {
    #[error(transparent)]
    Chat(#[from] ChatError),
    #[error(transparent)]
    Process(#[from] ChatProcessError),
    #[error("the AI CLI did not return one complete, valid answer")]
    InvalidOutput,
}

#[derive(Debug, PartialEq, Eq)]
struct Turn {
    answer: String,
    session: String,
}

impl Turn {
    fn new(answer: String, session: String) -> Result<Self, RunnerError> {
        if answer.trim().is_empty() || answer.len() > MAX_TEXT || !process::valid_session(&session)
        {
            return Err(RunnerError::InvalidOutput);
        }
        Ok(Self { answer, session })
    }
}

#[expect(
    clippy::disallowed_methods,
    reason = "this bounded foreign-protocol ingress validates structured AI CLI output"
)]
fn json(bytes: &[u8]) -> Result<Value, RunnerError> {
    serde_json::from_slice(bytes).map_err(|_invalid| RunnerError::InvalidOutput)
}

fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str, RunnerError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or(RunnerError::InvalidOutput)
}

fn claude(output: &[u8]) -> Result<Turn, RunnerError> {
    let value = json(output)?;
    let result = match &value {
        Value::Object(_) => &value,
        Value::Array(events) => {
            let mut results = events
                .iter()
                .filter(|event| event.get("type").and_then(Value::as_str) == Some("result"));
            let result = results.next().ok_or(RunnerError::InvalidOutput)?;
            if results.next().is_some() {
                return Err(RunnerError::InvalidOutput);
            }
            result
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {
            return Err(RunnerError::InvalidOutput);
        }
    };
    if text(result, "type")? != "result" || result.get("is_error") != Some(&Value::Bool(false)) {
        return Err(RunnerError::InvalidOutput);
    }
    Turn::new(
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
    fn session(&mut self, session: &str) -> Result<(), RunnerError> {
        if !process::valid_session(session)
            || self
                .session
                .as_deref()
                .is_some_and(|previous| previous != session)
        {
            return Err(RunnerError::InvalidOutput);
        }
        self.session = Some(session.to_owned());
        Ok(())
    }

    fn codex(&mut self, event: &Value) -> Result<(), RunnerError> {
        match text(event, "type")? {
            "thread.started" => self.session(text(event, "thread_id")?)?,
            "turn.started" => {
                self.complete = false;
                self.answer.clear();
            }
            "item.completed" => {
                let item = event.get("item").ok_or(RunnerError::InvalidOutput)?;
                if text(item, "type")? == "agent_message" {
                    text(item, "text")?.clone_into(&mut self.answer);
                }
            }
            "turn.completed" => self.complete = true,
            "turn.failed" | "error" => return Err(RunnerError::InvalidOutput),
            _ => {}
        }
        Ok(())
    }

    fn opencode(&mut self, event: &Value) -> Result<(), RunnerError> {
        self.session(text(event, "sessionID")?)?;
        match text(event, "type")? {
            "step_start" => {
                self.complete = false;
                self.answer.clear();
            }
            "text" => {
                let part = event.get("part").ok_or(RunnerError::InvalidOutput)?;
                self.answer.push_str(text(part, "text")?);
            }
            "step_finish" => {
                let part = event.get("part").ok_or(RunnerError::InvalidOutput)?;
                self.complete = text(part, "reason")? == "stop";
            }
            "error" => return Err(RunnerError::InvalidOutput),
            _ => {}
        }
        Ok(())
    }
}

fn parse_answer(tool: AgentTool, output: &[u8]) -> Result<Turn, RunnerError> {
    if tool == AgentTool::Claude {
        return claude(output);
    }
    let mut stream = Stream::default();
    for line in output
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.iter().all(u8::is_ascii_whitespace))
    {
        let value = json(line)?;
        match tool {
            AgentTool::Codex => stream.codex(&value)?,
            AgentTool::Opencode => stream.opencode(&value)?,
            AgentTool::Claude => return Err(RunnerError::InvalidOutput),
        }
    }
    if !stream.complete {
        return Err(RunnerError::InvalidOutput);
    }
    Turn::new(
        stream.answer,
        stream.session.ok_or(RunnerError::InvalidOutput)?,
    )
}

fn resolutions(events: &[Event]) -> BTreeSet<&str> {
    events
        .iter()
        .filter_map(|event| match &event.data {
            EventData::Message {
                mode: MessageMode::Reply { request },
                ..
            }
            | EventData::TurnFailure { request, .. } => Some(request.as_str()),
            EventData::Message {
                mode: MessageMode::Send | MessageMode::Ask { .. },
                ..
            }
            | EventData::Agent { .. }
            | EventData::Room { .. }
            | EventData::Membership { .. }
            | EventData::Omitted {} => None,
        })
        .collect()
}

fn asked_agent(event: &Event) -> Option<&str> {
    match &event.data {
        EventData::Message {
            mode: MessageMode::Ask { responder },
            ..
        } => Some(responder),
        EventData::Message {
            mode: MessageMode::Send | MessageMode::Reply { .. },
            ..
        }
        | EventData::Agent { .. }
        | EventData::Room { .. }
        | EventData::Membership { .. }
        | EventData::TurnFailure { .. }
        | EventData::Omitted {} => None,
    }
}

pub(crate) fn dispatch(store: &Store) -> Result<(), RunnerError> {
    let events = store.events()?;
    let answered = resolutions(&events);
    let agents = store.agents()?;
    notify_attached(store, &events, &agents)?;
    let pending: BTreeSet<_> = events
        .iter()
        .filter(|event| !answered.contains(event.id().as_str()))
        .filter_map(asked_agent)
        .collect();
    for agent in pending {
        if !agent.ends_with(&format!("@{}", store.origin())) {
            continue;
        }
        if agents
            .get(agent)
            .is_some_and(|event| matches!(event.data, EventData::Agent { managed: true, .. }))
        {
            let _launch = store.agent_launch_lock(agent)?;
            if let Some(free) = store.try_lock_agent(agent)? {
                drop(free);
                process::spawn_chat_worker(agent)?;
            }
        }
    }
    Ok(())
}

fn notify_attached(
    store: &Store,
    events: &[Event],
    agents: &std::collections::BTreeMap<String, Event>,
) -> Result<(), RunnerError> {
    let local: BTreeSet<_> = agents
        .iter()
        .filter_map(|(id, registration)| {
            (id.ends_with(&format!("@{}", store.origin()))
                && matches!(registration.data, EventData::Agent { managed: false, .. }))
            .then_some(id.as_str())
        })
        .collect();
    let rooms = store.rooms()?;
    let owner = format!("owner@{}", store.origin());
    let mut newest = None;
    for event in events {
        let EventData::Message {
            from, to, audience, ..
        } = &event.data
        else {
            continue;
        };
        if !audience.includes(store.origin()) {
            continue;
        }
        let attached = local
            .iter()
            .any(|agent| *agent != from && crate::chat::direct(from, agent) == *to)
            || rooms.get(to).is_some_and(|room| {
                room.members
                    .iter()
                    .any(|member| member != from && local.contains(member.as_str()))
            });
        let to_owner = from != &owner && crate::chat::direct(from, &owner) == *to;
        if (attached || to_owner) && store.mark_notification(&event.id())? {
            newest = Some(event.id());
        }
    }
    if let Some(id) = newest
        && let Err(error) = process::notify_message(&id)
    {
        eprintln!("domyjob: chat notification unavailable: {error}");
    }
    Ok(())
}

fn record_failure(
    store: &Store,
    event: &Event,
    agent: &str,
    failure: TurnFailure,
) -> Result<(), RunnerError> {
    let EventData::Message { to, audience, .. } = &event.data else {
        return Err(ChatError::Invalid("a managed turn must be a message").into());
    };
    match store.append_resolution(EventData::TurnFailure {
        request: event.id(),
        to: to.clone(),
        agent: agent.to_owned(),
        audience: audience.clone(),
        failure,
    }) {
        Ok(_) | Err(ChatError::AlreadyResolved(_)) => {}
        Err(error) => return Err(error.into()),
    }
    store.finish_turn(&event.id())?;
    Ok(())
}

fn recover(store: &Store, agent: &str) -> Result<(), RunnerError> {
    for id in store.pending_turns()? {
        let event = store
            .event(&id)?
            .ok_or_else(|| ChatError::Unknown(id.clone()))?;
        if asked_agent(&event) == Some(agent) {
            record_failure(store, &event, agent, TurnFailure::Interrupted)?;
        }
    }
    Ok(())
}

fn process_turn(store: &Store, event: &Event, agent: &str) -> Result<(), RunnerError> {
    let registration = store
        .agents()?
        .remove(agent)
        .ok_or_else(|| ChatError::Unknown(agent.to_owned()))?;
    let EventData::Agent {
        name,
        tool,
        cwd,
        session,
        managed: true,
    } = registration.data
    else {
        return Err(ChatError::Invalid("the requested agent is not managed").into());
    };
    let EventData::Message {
        to,
        text,
        audience,
        mode: MessageMode::Ask { .. },
        ..
    } = &event.data
    else {
        return Err(ChatError::Invalid("the managed turn is not an ask").into());
    };
    let prompt = format!(
        "A domyjob chat message asks you to answer once. Message ID: {}.\n\n{text}\n\nGive your answer as the final response. Do not call chat_reply; domyjob will save the answer for this exact message ID.",
        event.id()
    );
    let output = process::run(Invocation {
        tool,
        cwd: Path::new(&cwd),
        session: session.as_deref(),
        prompt: &prompt,
        agent,
    })?;
    let turn = parse_answer(tool, &output)?;
    if session
        .as_deref()
        .is_some_and(|previous| previous != turn.session)
    {
        return Err(RunnerError::InvalidOutput);
    }
    if session.as_deref() != Some(&turn.session) {
        store.append(EventData::Agent {
            name,
            tool,
            cwd,
            session: Some(turn.session),
            managed: true,
        })?;
    }
    match store.append_resolution(EventData::Message {
        to: to.clone(),
        from: agent.to_owned(),
        text: turn.answer,
        audience: audience.clone(),
        mode: MessageMode::Reply {
            request: event.id(),
        },
    }) {
        Ok(_) | Err(ChatError::AlreadyResolved(_)) => {}
        Err(error) => return Err(error.into()),
    }
    store.finish_turn(&event.id())?;
    Ok(())
}

pub(crate) fn worker(
    agent: &str,
    ready: Option<&crate::process::ReadyToken>,
) -> Result<(), RunnerError> {
    let store = Store::open()?;
    if !agent.ends_with(&format!("@{}", store.origin())) {
        return Err(ChatError::Invalid("the managed agent must be local").into());
    }
    let guard = store.try_lock_agent(agent)?;
    crate::process::announce_ready(ready).map_err(ChatProcessError::from)?;
    let Some(guard) = guard else {
        return Ok(());
    };
    recover(&store, agent)?;
    loop {
        let launch = store.agent_launch_lock(agent)?;
        let events = store.events()?;
        let answered = resolutions(&events);
        let next = events.iter().find(|event| {
            asked_agent(event) == Some(agent) && !answered.contains(event.id().as_str())
        });
        let Some(event) = next else {
            drop(guard);
            return Ok(());
        };
        drop(launch);
        if !store.claim_turn(&event.id())? {
            return Err(ChatError::Invalid("the managed turn was already claimed").into());
        }
        if let Err(error) = process_turn(&store, event, agent) {
            record_failure(&store, event, agent, TurnFailure::Failed)?;
            eprintln!("domyjob: chat turn failed: {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_answer, recover};
    use crate::chat::{AgentTool, Audience, EventData, MessageMode, Store, TurnFailure, direct};

    #[test]
    fn an_abandoned_claim_is_resolved_once_without_rerunning_the_provider() {
        let root = tempfile::tempdir().expect("temporary chat state");
        let store = Store::at(root.path(), "a".to_owned());
        let from = "owner@a".to_owned();
        let agent = "reviewer@a";
        let event = store
            .append(EventData::Message {
                to: direct(&from, agent),
                from,
                text: "question".to_owned(),
                audience: Audience::try_from(vec!["a".to_owned()]).expect("audience"),
                mode: MessageMode::Ask {
                    responder: agent.to_owned(),
                },
            })
            .expect("question");
        assert!(store.claim_turn(&event.id()).expect("claim"));
        let _guard = store
            .try_lock_agent(agent)
            .expect("agent lock")
            .expect("free lock");
        recover(&store, agent).expect("recover abandoned turn");
        recover(&store, agent).expect("idempotent recovery");
        let events = store.events().expect("history");
        assert_eq!(events.len(), 2);
        assert!(events.iter().any(|stored| matches!(
            stored.data,
            EventData::TurnFailure {
                failure: TurnFailure::Interrupted,
                ..
            }
        )));
        assert!(!store.claim_turn(&event.id()).expect("resolved turn"));
        assert!(store.pending_turns().expect("pending turns").is_empty());
    }

    #[test]
    fn complete_provider_results_bind_answer_and_session() {
        let fixtures = [
            (
                AgentTool::Claude,
                "{\"type\":\"result\",\"is_error\":false,\"result\":\"answer\",\"session_id\":\"s1\"}",
            ),
            (
                AgentTool::Codex,
                "{\"type\":\"thread.started\",\"thread_id\":\"s1\"}\n{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"answer\"}}\n{\"type\":\"turn.completed\"}\n",
            ),
            (
                AgentTool::Opencode,
                "{\"type\":\"step_start\",\"sessionID\":\"s1\"}\n{\"type\":\"text\",\"sessionID\":\"s1\",\"part\":{\"text\":\"answer\"}}\n{\"type\":\"step_finish\",\"sessionID\":\"s1\",\"part\":{\"reason\":\"stop\"}}\n",
            ),
        ];
        for (tool, fixture) in fixtures {
            let turn = parse_answer(tool, fixture.as_bytes()).expect("complete structured result");
            assert_eq!(turn.answer, "answer");
            assert_eq!(turn.session, "s1");
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
        ] {
            parse_answer(AgentTool::Codex, fixture.as_bytes()).expect_err("incomplete output");
        }
        parse_answer(
            AgentTool::Claude,
            br#"{"type":"result","is_error":true,"result":"partial","session_id":"s1"}"#,
        )
        .expect_err("failed Claude output");
        parse_answer(
            AgentTool::Opencode,
            br#"{"type":"text","sessionID":"s1","part":{"text":"partial"}}"#,
        )
        .expect_err("incomplete OpenCode output");
    }
}
