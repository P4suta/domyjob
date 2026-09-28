use alloc::collections::BTreeSet;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

pub const MAX_TEXT: usize = 64 * 1024;
pub const BATCH: usize = 2;
pub const MAX_EVENT_BYTES: usize = 400 * 1024;
const MAX_NAME_BYTES: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("a chat event is invalid: {0}")]
pub struct ChatValidationError(pub &'static str);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentTool {
    Claude,
    Codex,
    Opencode,
}

impl AgentTool {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Opencode => "opencode",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct Audience(Vec<String>);

impl TryFrom<Vec<String>> for Audience {
    type Error = ChatValidationError;

    fn try_from(members: Vec<String>) -> Result<Self, Self::Error> {
        if members.is_empty()
            || members.len() > 64
            || members.iter().any(|member| !valid_origin(member))
            || members
                .windows(2)
                .any(|pair| matches!(pair, [a, b] if a >= b))
        {
            return Err(ChatValidationError(
                "audience must have one to 64 sorted, unique machine identities",
            ));
        }
        Ok(Self(members))
    }
}

impl<'de> Deserialize<'de> for Audience {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let members = Vec::<String>::deserialize(deserializer)?;
        Self::try_from(members).map_err(serde::de::Error::custom)
    }
}

impl Audience {
    #[must_use]
    pub fn includes(&self, origin: &str) -> bool {
        self.0
            .binary_search_by(|member| member.as_str().cmp(origin))
            .is_ok()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnFailure {
    Failed,
    Interrupted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "mode",
    content = "details",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum MessageMode {
    Send,
    Ask { responder: String },
    Reply { request: String },
}

impl MessageMode {
    fn valid_for(&self, to: &str, from: &str, audience: &Audience) -> bool {
        match self {
            Self::Send => true,
            Self::Ask { responder } => {
                valid_agent_address(responder)
                    && responder
                        .rsplit_once('@')
                        .is_some_and(|(_, origin)| audience.includes(origin))
                    && (!to.starts_with("dm:") || direct(from, responder) == to)
            }
            Self::Reply { request } => valid_event_id(request),
        }
    }
}

impl TurnFailure {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Failed => "failed",
            Self::Interrupted => "interrupted",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "UncheckedEvent")]
pub struct Event {
    pub origin: String,
    pub seq: u64,
    pub clock: u64,
    pub data: EventData,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UncheckedEvent {
    origin: String,
    seq: u64,
    clock: u64,
    data: EventData,
}

impl TryFrom<UncheckedEvent> for Event {
    type Error = ChatValidationError;

    fn try_from(raw: UncheckedEvent) -> Result<Self, Self::Error> {
        let event = Self {
            origin: raw.origin,
            seq: raw.seq,
            clock: raw.clock,
            data: raw.data,
        };
        event.validate()?;
        Ok(event)
    }
}

#[must_use]
pub fn valid_origin(origin: &str) -> bool {
    !origin.is_empty()
        && origin.len() <= 128
        && origin
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

impl Event {
    pub fn validate(&self) -> Result<(), ChatValidationError> {
        if !valid_origin(&self.origin) || self.seq == 0 || self.clock == 0 || self.clock == u64::MAX
        {
            return Err(ChatValidationError("event origin, sequence, or clock"));
        }
        self.data.validate(&self.origin)
    }

    #[must_use]
    pub fn id(&self) -> String {
        format!("{}:{:016x}", self.origin, self.seq)
    }
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.trim() == name
        && name.len() <= MAX_NAME_BYTES
        && !name.contains('@')
        && !name.chars().any(char::is_control)
}

fn valid_agent_config(cwd: &str, session: Option<&str>) -> bool {
    !cwd.is_empty()
        && cwd.len() <= 8192
        && !cwd.contains('\0')
        && session.is_none_or(|value| {
            !value.is_empty() && value.len() <= 4096 && !value.chars().any(char::is_control)
        })
}

fn valid_agent_address(text: &str) -> bool {
    text.rsplit_once('@')
        .is_some_and(|(name, origin)| valid_name(name) && valid_origin(origin))
}

#[must_use]
pub fn valid_event_id(text: &str) -> bool {
    text.rsplit_once(':').is_some_and(|(origin, sequence)| {
        valid_origin(origin)
            && sequence.len() == 16
            && sequence
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
            && u64::from_str_radix(sequence, 16).is_ok_and(|value| value != 0)
    })
}

fn valid_conversation(text: &str) -> bool {
    if let Some(hash) = text.strip_prefix("dm:") {
        hash.len() == 64
            && hash
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    } else {
        text.strip_prefix("room:")
            .and_then(|value| value.split_once(':'))
            .is_some_and(|(owner, name)| valid_origin(owner) && valid_name(name))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case", tag = "kind")]
pub enum EventData {
    Agent {
        name: String,
        tool: AgentTool,
        cwd: String,
        session: Option<String>,
        managed: bool,
    },
    Room {
        id: String,
        name: String,
        members: Vec<String>,
    },
    Membership {
        room: String,
        member: String,
        present: bool,
    },
    Message {
        to: String,
        from: String,
        text: String,
        audience: Audience,
        mode: MessageMode,
    },
    TurnFailure {
        request: String,
        to: String,
        agent: String,
        audience: Audience,
        failure: TurnFailure,
    },
    Omitted {},
}

impl EventData {
    #[must_use]
    pub fn conversation(&self) -> Option<&str> {
        match self {
            Self::Message { to, .. } | Self::TurnFailure { to, .. } => Some(to),
            Self::Agent { .. } | Self::Room { .. } | Self::Membership { .. } | Self::Omitted {} => {
                None
            }
        }
    }

    pub fn validate(&self, origin: &str) -> Result<(), ChatValidationError> {
        if !valid_origin(origin) {
            return Err(ChatValidationError("machine identity"));
        }
        match self {
            Self::Agent {
                name, cwd, session, ..
            } => {
                if name == "owner"
                    || !valid_name(name)
                    || !valid_agent_config(cwd, session.as_deref())
                {
                    return Err(ChatValidationError("agent name, tool, or directory"));
                }
            }
            Self::Room { id, name, members } => {
                if !valid_name(name)
                    || id != &format!("room:{origin}:{name}")
                    || members.is_empty()
                    || members.len() > 64
                    || members.iter().collect::<BTreeSet<_>>().len() != members.len()
                    || !members.iter().all(|member| valid_agent_address(member))
                    || !members.contains(&format!("owner@{origin}"))
                {
                    return Err(ChatValidationError("room owner, name, or members"));
                }
            }
            Self::Membership {
                room,
                member,
                present,
            } => {
                if !room.starts_with(&format!("room:{origin}:"))
                    || !valid_conversation(room)
                    || !valid_agent_address(member)
                    || (!present && member == &format!("owner@{origin}"))
                {
                    return Err(ChatValidationError(
                        "only the room creator may change members",
                    ));
                }
            }
            Self::Message {
                to,
                from,
                text,
                audience,
                mode,
            } => {
                if !valid_conversation(to)
                    || !valid_agent_address(from)
                    || !from.ends_with(&format!("@{origin}"))
                    || text.is_empty()
                    || text.len() > MAX_TEXT
                    || !audience.includes(origin)
                    || !mode.valid_for(to, from, audience)
                {
                    return Err(ChatValidationError("message address or size"));
                }
            }
            Self::TurnFailure {
                request,
                to,
                agent,
                audience,
                ..
            } => {
                if !valid_event_id(request)
                    || !valid_conversation(to)
                    || !valid_agent_address(agent)
                    || !agent.ends_with(&format!("@{origin}"))
                    || !audience.includes(origin)
                {
                    return Err(ChatValidationError("turn failure address"));
                }
            }
            Self::Omitted {} => {}
        }
        Ok(())
    }
}

#[must_use]
pub fn direct(a: &str, b: &str) -> String {
    let (first, second) = if a < b { (a, b) } else { (b, a) };
    let mut hash = blake3::Hasher::new_derive_key("domyjob chat direct room v1");
    hash.update(first.as_bytes());
    hash.update(&[0]);
    hash.update(second.as_bytes());
    format!("dm:{}", hash.finalize().to_hex())
}
pub fn validate_response(
    parent: &Event,
    response: &EventData,
    resolved: bool,
) -> Result<(), ChatValidationError> {
    let EventData::Message {
        to: parent_to,
        from: parent_from,
        audience: parent_audience,
        mode: parent_mode,
        ..
    } = &parent.data
    else {
        return Err(ChatValidationError("reply target is not a message"));
    };
    match response {
        EventData::Message {
            to,
            from,
            audience,
            mode: MessageMode::Reply { request },
            ..
        } if request == &parent.id()
            && to == parent_to
            && audience == parent_audience
            && (!to.starts_with("dm:") || direct(parent_from, from) == *to)
            && match parent_mode {
                MessageMode::Ask { responder } => resolved && from == responder,
                MessageMode::Send | MessageMode::Reply { .. } => !resolved,
            } => {}
        EventData::TurnFailure {
            request,
            to,
            agent,
            audience,
            ..
        } if resolved
            && request == &parent.id()
            && to == parent_to
            && audience == parent_audience
            && matches!(parent_mode, MessageMode::Ask { responder } if agent == responder) => {}
        EventData::Agent { .. }
        | EventData::Room { .. }
        | EventData::Membership { .. }
        | EventData::Message { .. }
        | EventData::TurnFailure { .. }
        | EventData::Omitted {} => {
            return Err(ChatValidationError("reply does not match its request"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Audience, Event, EventData, MessageMode, TurnFailure, direct};
    use alloc::borrow::ToOwned;
    use alloc::vec;

    #[test]
    #[expect(
        clippy::disallowed_methods,
        reason = "exercise the domain decoder with malformed ingress records"
    )]
    fn decoding_cannot_construct_invalid_events_or_mixed_message_modes() {
        for invalid in [
            r#"{"origin":"bad/origin","seq":1,"clock":1,"data":{"kind":"omitted"}}"#,
            r#"{"origin":"a","seq":0,"clock":1,"data":{"kind":"omitted"}}"#,
            r#"{"origin":"a","seq":1,"clock":0,"data":{"kind":"omitted"}}"#,
            r#"{"origin":"a","seq":1,"clock":1,"data":{"kind":"omitted"},"extra":true}"#,
            r#"{"origin":"a","seq":1,"clock":1,"data":{"kind":"omitted","extra":true}}"#,
        ] {
            serde_json::from_str::<Event>(invalid).unwrap_err();
        }
        for invalid in [
            r#"{"mode":"send","responder":"bob@a"}"#,
            r#"{"mode":"ask"}"#,
            r#"{"mode":"ask","details":{"responder":"bob@a","request":"a:0000000000000001"}}"#,
        ] {
            serde_json::from_str::<MessageMode>(invalid).unwrap_err();
        }
        Audience::try_from(vec!["a".to_owned(), "a".to_owned()]).unwrap_err();
        Audience::try_from(vec!["a/invalid".to_owned()]).unwrap_err();
    }

    #[test]
    fn a_failure_must_reference_a_valid_request() {
        let data = EventData::TurnFailure {
            request: "not-an-event".to_owned(),
            to: direct("alice@a", "bob@a"),
            agent: "bob@a".to_owned(),
            audience: vec!["a".to_owned()].try_into().unwrap(),
            failure: TurnFailure::Interrupted,
        };
        data.validate("a").unwrap_err();
    }
}
