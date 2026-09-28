use alloc::string::String;
use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

use crate::chat_v0::Event;

pub use crate::chat_v0::BATCH;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Origin(String);

impl TryFrom<String> for Origin {
    type Error = &'static str;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        if text.len() != 32
            || !text
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        {
            return Err("invalid chat origin");
        }
        Ok(Self(text))
    }
}

impl From<Origin> for String {
    fn from(origin: Origin) -> Self {
        origin.0
    }
}

impl Origin {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Vec<Event>", into = "Vec<Event>")]
pub struct Batch(Vec<Event>);

impl TryFrom<Vec<Event>> for Batch {
    type Error = &'static str;

    fn try_from(events: Vec<Event>) -> Result<Self, Self::Error> {
        if events.len() > BATCH {
            return Err("chat batch exceeds its event limit");
        }
        for event in &events {
            event.validate().map_err(|error| error.0)?;
        }
        if events.windows(2).any(|pair| {
            matches!(pair, [first, second]
            if first.origin != second.origin || first.seq.checked_add(1) != Some(second.seq)
                || first.clock >= second.clock)
        }) {
            return Err("chat batch must have one origin and consecutive, advancing events");
        }
        Ok(Self(events))
    }
}

impl From<Batch> for Vec<Event> {
    fn from(batch: Batch) -> Self {
        batch.0
    }
}

impl Batch {
    #[must_use]
    pub fn events(&self) -> &[Event] {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, tag = "operation", rename_all = "snake_case")]
pub enum ChatRequest {
    Identity {},
    Exchange {
        origin: Origin,
        after: u64,
        seen: u64,
        events: Batch,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, tag = "result", rename_all = "snake_case")]
pub enum ChatReply {
    Identity {
        origin: Origin,
    },
    Exchanged {
        origin: Origin,
        seen: u64,
        events: Batch,
    },
}

#[cfg(test)]
mod tests {
    use super::{BATCH, Batch, ChatReply, ChatRequest, Origin};
    use crate::chat_v0::{Event, EventData};
    use crate::{ingress, wire};
    use alloc::format;
    use alloc::vec;

    fn event(sequence: u64) -> Event {
        Event {
            origin: "a".repeat(32),
            seq: sequence,
            clock: sequence,
            data: EventData::Omitted {},
        }
    }

    #[test]
    fn batch_construction_enforces_size_order_and_valid_events() {
        Batch::try_from(vec![event(1); BATCH.saturating_add(1)]).unwrap_err();
        Batch::try_from(vec![event(0)]).unwrap_err();
        Batch::try_from(vec![event(1), event(1)]).unwrap_err();
        Batch::try_from(vec![event(1), event(3)]).unwrap_err();
        let foreign = Event {
            origin: "b".repeat(32),
            ..event(2)
        };
        Batch::try_from(vec![event(1), foreign]).unwrap_err();
        let regressed = Event {
            clock: 1,
            ..event(2)
        };
        Batch::try_from(vec![event(1), regressed]).unwrap_err();
        Batch::try_from(vec![event(1), event(2)]).unwrap();
    }

    #[test]
    fn wire_round_trips_identity_and_exchange_through_the_shared_ingress() {
        let origin = Origin::try_from("a".repeat(32)).unwrap();
        for request in [
            ChatRequest::Identity {},
            ChatRequest::Exchange {
                origin: origin.clone(),
                after: 0,
                seen: 0,
                events: Batch::try_from(vec![event(1), event(2)]).unwrap(),
            },
        ] {
            let wrapped = wire::Request::Chat(request);
            assert_eq!(
                ingress::request(&wire::frame(&wrapped).unwrap()).unwrap(),
                wrapped
            );
        }
        let wrapped = wire::Reply::Chat(ChatReply::Exchanged {
            origin,
            seen: 2,
            events: Batch::try_from(vec![event(1)]).unwrap(),
        });
        assert_eq!(
            ingress::reply(&wire::frame(&wrapped).unwrap()).unwrap(),
            wrapped
        );
    }

    #[test]
    fn chat_wire_rejects_unknown_fields_and_invalid_identities_before_effects() {
        for invalid in ["", "a", &"A".repeat(32), &"g".repeat(32), &"a".repeat(33)] {
            Origin::try_from(alloc::string::String::from(invalid)).unwrap_err();
        }
        for invalid in [
            r#"{"operation":"identity","extra":true}"#.into(),
            r#"{"operation":"exchange","origin":"bad","after":0,"seen":0,"events":[]}"#.into(),
            format!(
                r#"{{"operation":"exchange","origin":"{}","after":0,"seen":0,"events":[{{"origin":"{}","seq":0,"clock":1,"data":{{"kind":"omitted"}}}}]}}"#,
                "a".repeat(32),
                "a".repeat(32)
            ),
        ] {
            let payload =
                format!(r#"{{"version":1,"message":{{"request":"chat","body":{invalid}}}}}"#);
            ingress::stored_request(payload.as_bytes()).unwrap_err();
        }
    }
}
