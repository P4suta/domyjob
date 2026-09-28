//! One specimen of everything the chat store keeps, whose digest is the store's format.
//!
//! Every table, every key encoding, every kind of event body, and every stored record appear here,
//! so any change that would leave an existing store unreadable changes the specimen.

use std::collections::BTreeSet;
use std::num::NonZeroU64;

use domyjob_core::chat::card::{Access, Card, MachineCard, Mode, Os, Skills, Tag, Tool};
use domyjob_core::chat::event::{Body, Chain, Event, Intent, Members, Outcome, Sealed};
use domyjob_core::chat::id::{
    AgentId, AgentName, Conversation, EventId, Line, Origin, Paragraph, RoomId, RoomName, Text,
};
use domyjob_core::chat::ledger::{Ending, Resolution, Room};
use domyjob_core::chat::policy::audience;

use super::admin::{Link, LinkState};
use super::tables::{encode, event_key, joined_key, manifest, order_key, room_key, thread_key};
use super::views::LocalAgent;

/// The name of each kind of body; a new kind fails to compile here until it has a name,
/// and the test fails until [`KINDS`] lists it and the specimen holds one.
const fn kind(body: &Body) -> &'static str {
    match body {
        Body::Profile { .. } => "profile",
        Body::Left { .. } => "left",
        Body::Machine { .. } => "machine",
        Body::Room { .. } => "room",
        Body::RoomClosed { .. } => "room_closed",
        Body::Message { .. } => "message",
        Body::TurnStarted { .. } => "turn_started",
        Body::Resolved { .. } => "resolved",
        Body::Omitted {} => "omitted",
    }
}

const KINDS: [&str; 9] = [
    "profile",
    "left",
    "machine",
    "room",
    "room_closed",
    "message",
    "turn_started",
    "resolved",
    "omitted",
];

fn origin(digit: char) -> Origin {
    Origin::try_from(std::iter::repeat_n(digit, 32).collect::<String>()).unwrap()
}

fn name(text: &str) -> AgentName {
    AgentName::try_from(text.to_owned()).unwrap()
}

fn agent(text: &str, digit: char) -> AgentId {
    AgentId::new(name(text), origin(digit))
}

fn id(digit: char, seq: u64) -> EventId {
    EventId::new(origin(digit), NonZeroU64::new(seq).unwrap())
}

fn card() -> Card {
    Card {
        display_name: Line::try_from("Reviewer".to_owned()).unwrap(),
        role: Some(Line::try_from("reviewer".to_owned()).unwrap()),
        description: Some(Paragraph::try_from("Reviews Rust changes.".to_owned()).unwrap()),
        skills: Skills::collect([Tag::try_from("rust".to_owned()).unwrap()]).unwrap(),
        project: Some(Line::try_from("domyjob".to_owned()).unwrap()),
        status: Some(Line::try_from("reviewing".to_owned()).unwrap()),
        tool: Tool::Codex,
        mode: Mode::Managed,
        access: Access::Read,
    }
}

fn room() -> RoomId {
    RoomId::new(
        origin('a'),
        RoomName::try_from("release".to_owned()).unwrap(),
    )
}

/// Bodies that describe agents, machines, and rooms.
fn directory_bodies() -> Vec<(char, Body)> {
    let members = Members::try_from(vec![agent("asker", 'a'), agent("responder", 'b')]).unwrap();
    vec![
        (
            'a',
            Body::Profile {
                agent: name("asker"),
                card: Box::new(card()),
            },
        ),
        (
            'a',
            Body::Left {
                agent: name("gone"),
            },
        ),
        (
            'a',
            Body::Machine {
                card: MachineCard {
                    label: Line::try_from("laptop".to_owned()).unwrap(),
                    os: Os::Linux,
                },
            },
        ),
        (
            'a',
            Body::Room {
                name: room().name().clone(),
                topic: Some(Line::try_from("the release".to_owned()).unwrap()),
                members,
            },
        ),
        (
            'a',
            Body::RoomClosed {
                name: room().name().clone(),
            },
        ),
    ]
}

/// Bodies of conversations: every intent, every outcome, and both kinds of conversation.
fn thread_bodies() -> Vec<(char, Body)> {
    let (asker, responder) = (agent("asker", 'a'), agent("responder", 'b'));
    let direct = Conversation::direct(&asker, &responder).unwrap();
    let pair = audience([&asker, &responder]).unwrap();
    let message = |conversation: &Conversation, from: &AgentId, intent| Body::Message {
        conversation: conversation.clone(),
        from: from.name().clone(),
        text: Text::try_from("text".to_owned()).unwrap(),
        audience: pair.clone(),
        intent,
        at: 1_700_000_000_000,
    };
    let chain = Chain::try_from(vec![agent("waiting", 'c')]).unwrap();
    let mut bodies = vec![
        ('a', message(&direct, &asker, Intent::Send {})),
        (
            'a',
            message(
                &direct,
                &asker,
                Intent::Ask {
                    responder: responder.clone(),
                    chain,
                },
            ),
        ),
        (
            'a',
            message(&Conversation::Room(room()), &asker, Intent::Send {}),
        ),
        (
            'b',
            Body::TurnStarted {
                request: id('a', 7),
                conversation: direct.clone(),
                audience: pair.clone(),
                agent: responder.name().clone(),
            },
        ),
        (
            'b',
            message(
                &direct,
                &responder,
                Intent::Reply {
                    request: id('a', 7),
                },
            ),
        ),
        ('b', Body::Omitted {}),
    ];
    for outcome in [
        Outcome::Failed,
        Outcome::Interrupted,
        Outcome::Unavailable,
        Outcome::Withdrawn,
    ] {
        let (digit, by) = if outcome == Outcome::Withdrawn {
            ('a', &asker)
        } else {
            ('b', &responder)
        };
        bodies.push((
            digit,
            Body::Resolved {
                request: id('a', 7),
                conversation: direct.clone(),
                audience: pair.clone(),
                agent: by.name().clone(),
                outcome,
            },
        ));
    }
    bodies
}

/// Events of every kind, each with the next sequence and clock.
fn events() -> Vec<Event> {
    directory_bodies()
        .into_iter()
        .chain(thread_bodies())
        .zip(1_u64..)
        .map(|((digit, body), seq)| Event::new(id(digit, seq), seq, body).unwrap())
        .collect()
}

fn specimen() -> String {
    let mut lines: Vec<String> = manifest()
        .into_iter()
        .map(|table| format!("table {table}"))
        .collect();
    let mut kinds = BTreeSet::new();
    for event in events() {
        kinds.insert(kind(event.body()));
        let order = order_key(&event);
        lines.push(format!("key event {}", event_key(event.id())));
        lines.push(format!("key order {order}"));
        if let Some((conversation, _)) = event.body().thread() {
            lines.push(format!("key thread {:?}", thread_key(conversation, &order)));
            lines.push(format!(
                "key joined {:?}",
                joined_key(&agent("asker", 'a'), conversation)
            ));
        }
        lines.push(format!("event {}", Sealed::new(event).unwrap().encoded()));
    }
    assert_eq!(
        kinds,
        KINDS.into_iter().collect(),
        "the specimen holds one event of every kind of body"
    );
    lines.push(format!("key room {}", room_key(&room())));
    lines.push(format!("record card {}", encode(&card()).unwrap()));
    lines.push(format!(
        "record room {}",
        encode(&Room {
            topic: Some(Line::try_from("the release".to_owned()).unwrap()),
            members: Members::try_from(vec![agent("asker", 'a')]).unwrap(),
        })
        .unwrap()
    ));
    for ending in [Ending::Answered, Ending::Ended(Outcome::Withdrawn)] {
        lines.push(format!(
            "record resolution {}",
            encode(&Resolution {
                event: id('b', 9),
                clock: 9,
                ending,
                asker: agent("asker", 'a'),
                responder: agent("responder", 'b'),
            })
            .unwrap()
        ));
    }
    lines.push(format!(
        "record local_agent {}",
        encode(&LocalAgent {
            cwd: "/home/user/project".to_owned(),
            session: Some("0199aa6e-3f1c-7b9a-9d7e-2c4b5a6f7e80".to_owned()),
        })
        .unwrap()
    ));
    for state in [LinkState::Synced, LinkState::Deferred, LinkState::Failed] {
        lines.push(format!(
            "record link {}",
            encode(&Link {
                state,
                detail: Some("detail".to_owned()),
                at: 1_700_000_000_000,
            })
            .unwrap()
        ));
    }
    lines.join("\n")
}

#[test]
fn the_chat_format_is_its_specimen() {
    crate::formats::check("chat", &specimen());
}
