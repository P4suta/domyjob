use std::collections::BTreeSet;
use std::num::NonZeroU64;

use domyjob_core::chat::card::{Access, Card, MachineCard, Mode, Os, Skills, Tag, Tool};
use domyjob_core::chat::event::{Body, Chain, Event, Intent, Members, Outcome, Sealed, Stamp};
use domyjob_core::chat::handoff::{Descriptor, Metadata, Project, ReadLimit, Scope, Target};
use domyjob_core::chat::id::{
    AgentId, AgentName, Conversation, EventId, Line, Origin, Paragraph, RoomId, RoomName, Text,
};
use domyjob_core::chat::ledger::{Ending, Resolution, Room};
use domyjob_core::chat::notification::{Activity, ActivityState, Delivery, Notice, Question};
use domyjob_core::chat::policy::audience;

use super::admin::{Link, LinkState};
use super::tables::{encode, event_key, joined_key, manifest, order_key, room_key, thread_key};
use super::views::LocalAgent;

const SPECIMEN_MILLIS: u64 = 1_700_000_000_000;
const RECEIPT_REQUEST_SEQUENCE: u64 = 7;
const SPECIMEN_FALLBACK_SECONDS: u64 = 120;
const ORIGIN_HEX_BYTES: usize = 32;
const RESOLUTION_SEQUENCE: u64 = 9;
const RESOLUTION_CLOCK: u64 = 9;
const SPECIMEN_CWD: &str = "/home/user/project";

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
        Body::Receipt { .. } => "receipt",
        Body::Activity { .. } => "activity",
        Body::Handoff { .. } => "handoff",
        Body::HandoffRead { .. } => "handoff_read",
    }
}

const KINDS: [&str; 13] = [
    "profile",
    "left",
    "machine",
    "room",
    "room_closed",
    "message",
    "turn_started",
    "resolved",
    "omitted",
    "receipt",
    "activity",
    "handoff",
    "handoff_read",
];

fn origin(digit: char) -> Origin {
    Origin::try_from(std::iter::repeat_n(digit, ORIGIN_HEX_BYTES).collect::<String>()).unwrap()
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
        at: Stamp::from_unix_millis(SPECIMEN_MILLIS),
        notice: None,
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
                request: id('a', RECEIPT_REQUEST_SEQUENCE),
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
                    request: id('a', RECEIPT_REQUEST_SEQUENCE),
                },
            ),
        ),
        ('b', Body::Omitted {}),
    ];
    bodies.extend(notification_bodies());
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
                request: id('a', RECEIPT_REQUEST_SEQUENCE),
                conversation: direct.clone(),
                audience: pair.clone(),
                agent: by.name().clone(),
                outcome,
            },
        ));
    }
    bodies
}

fn notification_bodies() -> Vec<(char, Body)> {
    let (asker, responder) = (agent("asker", 'a'), agent("responder", 'b'));
    let direct = Conversation::direct(&asker, &responder).unwrap();
    let pair = audience([&asker, &responder]).unwrap();
    let mut bodies = vec![(
        'b',
        Body::Activity {
            agent: responder.name().clone(),
            activity: Activity {
                state: ActivityState::Working,
                at: Stamp::from_unix_millis(SPECIMEN_MILLIS),
                status: Some(Line::try_from("reviewing".to_owned()).unwrap()),
            },
        },
    )];
    for delivery in [Delivery::Delivered, Delivery::Read, Delivery::Acknowledged] {
        bodies.push((
            'b',
            Body::Receipt {
                request: id('a', RECEIPT_REQUEST_SEQUENCE),
                conversation: direct.clone(),
                audience: pair.clone(),
                agent: responder.name().clone(),
                delivery,
            },
        ));
    }
    bodies.push((
        'a',
        Body::Message {
            conversation: Conversation::Room(room()),
            from: asker.name().clone(),
            text: Text::try_from("text".to_owned()).unwrap(),
            audience: pair,
            intent: Intent::Ask {
                responder: responder.clone(),
                chain: Chain::default(),
            },
            at: Stamp::from_unix_millis(SPECIMEN_MILLIS),
            notice: Some(Notice {
                recipients: Members::try_from(vec![responder.clone()]).unwrap(),
                urgent: true,
                subject: Some(Line::try_from("Review".to_owned()).unwrap()),
                question: Some(
                    Question::new(
                        Line::try_from("Approve or explain".to_owned()).unwrap(),
                        SPECIMEN_FALLBACK_SECONDS,
                        responder,
                    )
                    .unwrap(),
                ),
            }),
        },
    ));
    bodies
}

fn events() -> Vec<Event> {
    directory_bodies()
        .into_iter()
        .chain(thread_bodies())
        .chain(handoff_bodies())
        .zip(1_u64..)
        .map(|((digit, body), seq)| Event::new(id(digit, seq), seq, body).unwrap())
        .collect()
}

fn handoff_bodies() -> Vec<(char, Body)> {
    let reader = agent("continuing", 'c');
    let metadata = Metadata::new(
        agent("asker", 'a'),
        Target::Agent {
            agent: agent("responder", 'b'),
        },
        (
            Project::try_from("owner/project".to_owned()).unwrap(),
            Scope::try_from("issue-47".to_owned()).unwrap(),
        ),
        Members::try_from(vec![agent("responder", 'b'), reader.clone()]).unwrap(),
    )
    .unwrap();
    let text = Text::try_from("scoped handoff".to_owned()).unwrap();
    let descriptor = Descriptor::new(id('a', 1), metadata.clone(), &text).unwrap();
    let observation = descriptor
        .authorize(&reader, &descriptor, ReadLimit::MAX)
        .unwrap()
        .observation();
    vec![
        (
            'a',
            Body::Handoff {
                metadata: metadata.clone(),
                text,
                at: Stamp::from_unix_millis(SPECIMEN_MILLIS),
            },
        ),
        (
            'c',
            Body::HandoffRead {
                observation,
                audience: metadata.audience().clone(),
            },
        ),
    ]
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
                event: id('b', RESOLUTION_SEQUENCE),
                clock: RESOLUTION_CLOCK,
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
            cwd: SPECIMEN_CWD.to_owned(),
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
                at: Stamp::from_unix_millis(SPECIMEN_MILLIS),
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
