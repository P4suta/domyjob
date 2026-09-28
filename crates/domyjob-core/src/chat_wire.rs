//! Chat messages carried inside the framed SSH protocol.

use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

use crate::chat::event::Event;
use crate::chat::id::{Invalid, Origin};
use crate::chat::ledger::Rejection;

/// The most events one batch may carry; the frame limit bounds its bytes.
pub const MAX_BATCH: usize = 256;

/// Consecutive events of one origin with advancing clocks, and whether more follow.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawBatch", into = "RawBatch")]
pub struct Batch {
    events: Vec<Event>,
    more: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBatch {
    events: Vec<Event>,
    more: bool,
}

impl TryFrom<RawBatch> for Batch {
    type Error = Invalid;

    fn try_from(raw: RawBatch) -> Result<Self, Self::Error> {
        Self::new(raw.events, raw.more)
    }
}

impl From<Batch> for RawBatch {
    fn from(batch: Batch) -> Self {
        Self {
            events: batch.events,
            more: batch.more,
        }
    }
}

impl Batch {
    pub fn new(events: Vec<Event>, more: bool) -> Result<Self, Invalid> {
        if events.len() > MAX_BATCH
            || events.windows(2).any(|pair| {
                matches!(pair, [first, second]
                    if first.origin() != second.origin()
                        || first.id().seq().checked_add(1) != Some(second.id().seq())
                        || first.clock() >= second.clock())
            })
        {
            return Err(Invalid(
                "chat batch must hold consecutive events of one origin",
            ));
        }
        Ok(Self { events, more })
    }

    #[must_use]
    pub fn events(&self) -> &[Event] {
        &self.events
    }

    #[must_use]
    pub const fn more(&self) -> bool {
        self.more
    }

    #[must_use]
    pub fn into_events(self) -> Vec<Event> {
        self.events
    }
}

/// The sender's events after `after`, its cursor of the receiver, and the receiver it expects.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Offer {
    pub from: Origin,
    pub to: Origin,
    pub after: u64,
    pub seen: u64,
    pub batch: Batch,
}

/// The receiver's cursor of the sender after storing the offer, and its own events after the offer's cursor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Answer {
    pub from: Origin,
    pub seen: u64,
    pub batch: Batch,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rejected: Option<Rejection>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, tag = "operation", rename_all = "snake_case")]
pub enum ChatRequest {
    Identity {},
    Exchange(Offer),
    /// Wait until the receiver has events after `seen`, or until its heartbeat.
    Wait {
        from: Origin,
        to: Origin,
        seen: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, tag = "result", rename_all = "snake_case")]
pub enum ChatReply {
    Identity { origin: Origin },
    Exchanged(Answer),
    Changed {},
    Heartbeat {},
    Rejected { rejection: Rejection },
}

#[cfg(test)]
mod tests {
    use alloc::borrow::ToOwned;
    use alloc::format;
    use alloc::vec;

    use super::{Batch, ChatReply, ChatRequest, MAX_BATCH, Offer};
    use crate::chat::event::{Body, Event};
    use crate::chat::fixtures::id;
    use crate::chat::fixtures::origin;
    use crate::chat::ledger::Rejection;

    fn event(seq: u64) -> Event {
        Event::new(id('a', seq), seq, Body::Omitted {}).unwrap()
    }

    #[test]
    fn batches_hold_bounded_consecutive_advancing_events_of_one_origin() {
        Batch::new(vec![event(1); MAX_BATCH.saturating_add(1)], false).unwrap_err();
        Batch::new(vec![event(1), event(1)], false).unwrap_err();
        Batch::new(vec![event(1), event(3)], false).unwrap_err();
        let foreign = Event::new(id('b', 2), 2, Body::Omitted {}).unwrap();
        Batch::new(vec![event(1), foreign], false).unwrap_err();
        let regressed = Event::new(id('a', 2), 1, Body::Omitted {}).unwrap();
        Batch::new(vec![event(1), regressed], false).unwrap_err();
        Batch::new(vec![event(1), event(2)], true).unwrap();
    }

    #[expect(
        clippy::disallowed_methods,
        reason = "exercise the chat message decoders directly until the wire carries them"
    )]
    fn decode<T: serde::de::DeserializeOwned>(text: &str) -> serde_json::Result<T> {
        serde_json::from_str(text)
    }

    #[test]
    fn chat_messages_round_trip_through_their_strict_encoding() {
        let requests = [
            ChatRequest::Identity {},
            ChatRequest::Exchange(Offer {
                from: origin('a'),
                to: origin('b'),
                after: 0,
                seen: 0,
                batch: Batch::new(vec![event(1), event(2)], false).unwrap(),
            }),
            ChatRequest::Wait {
                from: origin('a'),
                to: origin('b'),
                seen: 3,
            },
        ];
        for request in requests {
            let text = serde_json::to_string(&request).unwrap();
            assert_eq!(decode::<ChatRequest>(&text).unwrap(), request);
        }
        for reply in [
            ChatReply::Identity {
                origin: origin('b'),
            },
            ChatReply::Heartbeat {},
            ChatReply::Rejected {
                rejection: Rejection::Gap {
                    origin: origin('a'),
                    expected: 4,
                },
            },
        ] {
            let text = serde_json::to_string(&reply).unwrap();
            assert_eq!(decode::<ChatReply>(&text).unwrap(), reply);
        }
    }

    #[test]
    fn chat_requests_reject_unknown_fields_before_any_effect() {
        for invalid in [
            r#"{"operation":"identity","extra":true}"#.to_owned(),
            format!(
                r#"{{"operation":"wait","from":"{}","to":"{}","seen":0,"extra":1}}"#,
                origin('a'),
                origin('b')
            ),
            format!(
                r#"{{"operation":"exchange","from":"{}","to":"bad","after":0,"seen":0,"batch":{{"events":[],"more":false}}}}"#,
                origin('a')
            ),
            format!(
                r#"{{"operation":"exchange","from":"{}","to":"{}","after":0,"seen":0,"batch":{{"events":[],"more":false,"x":1}}}}"#,
                origin('a'),
                origin('b')
            ),
        ] {
            decode::<ChatRequest>(&invalid).unwrap_err();
        }
    }
}
