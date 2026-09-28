use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::atomic::AtomicBool;

use domyjob_core::chat::card::{Access, Card, Mode, Skills, Tool};
use domyjob_core::chat::event::{Body, Chain, Intent, Members};
use domyjob_core::chat::id::{AgentId, AgentName, Conversation, Line, RoomId, RoomName, Text};
use domyjob_core::chat::policy::{Priority, audience};
use domyjob_core::chat_wire::{ChatReply, ChatRequest};

use super::{Channel, SyncError, serve, sync_with};
use crate::chat::store::{LinkState, LocalAgent, Store};
use crate::platform::clock::Deadline;

/// A channel that serves requests with another store in this process.
struct Local<'a> {
    node: &'a Store,
    offline: bool,
}

impl Channel for Local<'_> {
    fn call(&mut self, request: ChatRequest, _deadline: Deadline) -> Result<ChatReply, SyncError> {
        if self.offline {
            return Err(SyncError::Unexpected);
        }
        serve(self.node, request, &AtomicBool::new(false))
    }
}

struct Net<'a> {
    stores: BTreeMap<&'static str, &'a Store>,
    offline: RefCell<Vec<&'static str>>,
}

impl Net<'_> {
    fn sync(&self, store: &Store) -> super::Report {
        sync_with(
            store,
            None,
            Deadline::after_seconds(30),
            &mut |alias: &str| {
                let node = self
                    .stores
                    .get(alias)
                    .copied()
                    .ok_or(SyncError::UnknownPeer(alias.to_owned()))?;
                Ok(Local {
                    node,
                    offline: self.offline.borrow().contains(&alias),
                })
            },
        )
        .unwrap()
    }
}

fn store(root: &tempfile::TempDir, name: &str) -> Store {
    Store::open_in(&root.path().join(name)).unwrap()
}

fn register(store: &Store, name: &str) -> AgentId {
    let agent = AgentName::try_from(name.to_owned()).unwrap();
    store
        .write(|tx| {
            tx.configure(
                &agent,
                Some(&LocalAgent {
                    cwd: "/work".to_owned(),
                    session: None,
                }),
            )?;
            tx.author(
                Priority::Ordinary,
                Body::Profile {
                    agent: agent.clone(),
                    card: Box::new(Card {
                        display_name: Line::try_from(name.to_owned()).unwrap(),
                        role: None,
                        description: None,
                        skills: Skills::default(),
                        project: None,
                        status: None,
                        tool: Tool::Claude,
                        mode: Mode::Interactive,
                        access: Access::Read,
                    }),
                },
            )
        })
        .unwrap();
    AgentId::new(agent, store.origin().clone())
}

fn states(report: &super::Report) -> Vec<(String, LinkState)> {
    report
        .peers
        .iter()
        .map(|peer| (peer.machine.clone(), peer.state))
        .collect()
}

#[test]
fn offline_peers_are_reported_and_catch_up_after_reconnecting() {
    let root = tempfile::tempdir().unwrap();
    let (a, b) = (store(&root, "a"), store(&root, "b"));
    a.pin("b", b.origin(), false).unwrap();
    let net = Net {
        stores: BTreeMap::from([("b", &b)]),
        offline: RefCell::new(vec!["b"]),
    };
    let (alice, bob) = (register(&a, "alice"), register(&b, "bob"));
    a.write(|tx| {
        tx.author(
            Priority::Ordinary,
            Body::Message {
                conversation: Conversation::direct(&alice, &bob).unwrap(),
                from: alice.name().clone(),
                text: Text::try_from("while offline".to_owned()).unwrap(),
                audience: audience([&alice, &bob]).unwrap(),
                intent: Intent::Send {},
                at: 0,
            },
        )
    })
    .unwrap();
    assert_eq!(states(&net.sync(&a)), [("b".to_owned(), LinkState::Failed)]);
    assert_eq!(
        a.links().unwrap().get("b").map(|link| link.state),
        Some(LinkState::Failed)
    );
    net.offline.borrow_mut().clear();
    assert_eq!(states(&net.sync(&a)), [("b".to_owned(), LinkState::Synced)]);
    assert_eq!(states(&net.sync(&a)), [("b".to_owned(), LinkState::Synced)]);
    let inbox = b
        .read(|read| crate::chat::store::inbox(read, &bob, "", 10))
        .unwrap();
    assert_eq!(inbox.len(), 1);
}

/// Open a room on `owner` for `members` and ask `responder` there as `asker`.
fn room_ask(
    owner: &Store,
    members: Members,
    (asker, responder): (&AgentId, &AgentId),
) -> domyjob_core::chat::event::Event {
    let release = RoomName::try_from("release".to_owned()).unwrap();
    let everyone = audience(members.agents()).unwrap();
    owner
        .write(|tx| {
            tx.author(
                Priority::Ordinary,
                Body::Room {
                    name: release.clone(),
                    topic: None,
                    members,
                },
            )?;
            tx.author(
                Priority::Ordinary,
                Body::Message {
                    conversation: Conversation::Room(RoomId::new(owner.origin().clone(), release)),
                    from: asker.name().clone(),
                    text: Text::try_from("question".to_owned()).unwrap(),
                    audience: everyone,
                    intent: Intent::Ask {
                        responder: responder.clone(),
                        chain: Chain::default(),
                    },
                    at: 0,
                },
            )
        })
        .unwrap()
}

fn answer(store: &Store, responder: &AgentId, question: &domyjob_core::chat::event::Event) {
    let (conversation, audience) = question.body().thread().unwrap();
    store
        .write(|tx| {
            tx.author(
                Priority::Ending,
                Body::Message {
                    conversation: conversation.clone(),
                    from: responder.name().clone(),
                    text: Text::try_from("answer".to_owned()).unwrap(),
                    audience: audience.clone(),
                    intent: Intent::Reply {
                        request: question.id().clone(),
                    },
                    at: 0,
                },
            )
        })
        .unwrap();
}

#[test]
fn an_answer_that_arrives_before_its_question_is_retried_in_the_same_sync() {
    let root = tempfile::tempdir().unwrap();
    let (a, b, c) = (store(&root, "a"), store(&root, "b"), store(&root, "c"));
    let (asker, responder) = (register(&a, "alice"), register(&b, "bob"));
    let mut agents = vec![asker.clone(), responder.clone(), register(&c, "carol")];
    agents.sort();
    let question = room_ask(&a, Members::try_from(agents).unwrap(), (&asker, &responder));
    b.pin("a", a.origin(), false).unwrap();
    Net {
        stores: BTreeMap::from([("a", &a)]),
        offline: RefCell::new(Vec::new()),
    }
    .sync(&b);
    answer(&b, &responder, &question);
    c.pin("a-bob", b.origin(), false).unwrap();
    c.pin("z-alice", a.origin(), false).unwrap();
    let from_c = Net {
        stores: BTreeMap::from([("a-bob", &b), ("z-alice", &a)]),
        offline: RefCell::new(Vec::new()),
    };
    let report = from_c.sync(&c);
    assert_eq!(
        states(&report),
        [
            ("a-bob".to_owned(), LinkState::Synced),
            ("z-alice".to_owned(), LinkState::Synced)
        ]
    );
    let answered = c
        .read(|read| crate::chat::store::resolution(read, question.id()))
        .unwrap();
    assert!(answered.is_some());
}

#[test]
fn a_reset_peer_is_refused_until_its_new_identity_is_confirmed() {
    let root = tempfile::tempdir().unwrap();
    let (a, b) = (store(&root, "a"), store(&root, "b"));
    a.pin("b", b.origin(), false).unwrap();
    let replaced = Store::reset(&root.path().join("b")).unwrap();
    let net = Net {
        stores: BTreeMap::from([("b", &replaced)]),
        offline: RefCell::new(Vec::new()),
    };
    let report = net.sync(&a);
    assert_eq!(states(&report), [("b".to_owned(), LinkState::Failed)]);
    assert!(
        report
            .peers
            .first()
            .and_then(|peer| peer.detail.as_deref())
            .is_some_and(|detail| detail.contains("peer replace"))
    );
    a.pin("b", replaced.origin(), true).unwrap();
    assert_eq!(states(&net.sync(&a)), [("b".to_owned(), LinkState::Synced)]);
}

#[test]
fn a_wait_answers_at_once_when_the_node_has_newer_events() {
    let root = tempfile::tempdir().unwrap();
    let (a, b) = (store(&root, "a"), store(&root, "b"));
    register(&b, "bob");
    let seen = 0;
    let reply = serve(
        &b,
        ChatRequest::Wait {
            from: a.origin().clone(),
            to: b.origin().clone(),
            seen,
        },
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(reply, ChatReply::Changed {});
    let abandoned = AtomicBool::new(true);
    let current = b.local_seen().unwrap();
    let heartbeat = serve(
        &b,
        ChatRequest::Wait {
            from: a.origin().clone(),
            to: b.origin().clone(),
            seen: current,
        },
        &abandoned,
    )
    .unwrap();
    assert_eq!(heartbeat, ChatReply::Heartbeat {});
}
