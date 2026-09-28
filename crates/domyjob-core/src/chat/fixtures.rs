//! Values shared by the chat test modules.

use alloc::borrow::ToOwned;
use alloc::string::String;
use alloc::vec::Vec;
use core::num::NonZeroU64;

use super::card::{Access, Card, Mode, Skills, Tag, Tool};
use super::event::{Body, Chain, Intent};
use super::id::{AgentId, AgentName, Conversation, EventId, Line, Origin, Paragraph, Text};
use super::policy::audience;

pub(crate) fn origin(digit: char) -> Origin {
    Origin::try_from(core::iter::repeat_n(digit, 32).collect::<String>()).unwrap()
}

pub(crate) fn agent(name: &str, digit: char) -> AgentId {
    AgentId::new(AgentName::try_from(name.to_owned()).unwrap(), origin(digit))
}

pub(crate) fn id(digit: char, seq: u64) -> EventId {
    EventId::new(origin(digit), NonZeroU64::new(seq).unwrap())
}

pub(crate) fn tags(names: &[&str]) -> Vec<Tag> {
    names
        .iter()
        .map(|name| Tag::try_from((*name).to_owned()).unwrap())
        .collect()
}

pub(crate) fn card(display: &str, role: &str, skills: &[&str]) -> Card {
    Card {
        display_name: Line::try_from(display.to_owned()).unwrap(),
        role: Some(Line::try_from(role.to_owned()).unwrap()),
        description: Some(Paragraph::try_from("Reviews Rust changes.".to_owned()).unwrap()),
        skills: Skills::collect(tags(skills)).unwrap(),
        project: None,
        status: None,
        tool: Tool::Codex,
        mode: Mode::Managed,
        access: Access::Read,
    }
}

pub(crate) fn text(value: &str) -> Text {
    Text::try_from(value.to_owned()).unwrap()
}

/// A direct ask from `from` to `to`.
pub(crate) fn ask(from: &AgentId, to: &AgentId) -> Body {
    Body::Message {
        conversation: Conversation::direct(from, to).unwrap(),
        from: from.name().clone(),
        text: text("question"),
        audience: audience([from, to]).unwrap(),
        intent: Intent::Ask {
            responder: to.clone(),
            chain: Chain::default(),
        },
        at: 1,
    }
}
