use domyjob_core::chat::card::{Access, Card, Mode, Skills, Tool};
use domyjob_core::chat::event::{Body, Chain, Event, Intent, Members, Outcome};
use domyjob_core::chat::id::{AgentId, AgentName, Audience, Conversation, Line, RoomName, Text};
use domyjob_core::chat::ledger::{Ending, Rejection};
use domyjob_core::chat::policy::{Priority, audience};

use super::views::LocalAgent;
use super::{Completion, Store, StoreError};

fn store(root: &tempfile::TempDir, name: &str) -> Store {
    Store::open_in(&crate::layout::State::at(&root.path().join(name))).unwrap()
}

fn card(tool: Tool, mode: Mode) -> Box<Card> {
    Box::new(Card {
        display_name: Line::try_from("Agent".to_owned()).unwrap(),
        role: None,
        description: None,
        skills: Skills::default(),
        project: None,
        status: None,
        tool,
        mode,
        access: Access::Read,
    })
}

fn register(store: &Store, name: &str, mode: Mode) -> AgentId {
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
                    card: card(Tool::Codex, mode),
                },
            )
        })
        .unwrap();
    AgentId::new(agent, store.origin().clone())
}

fn direct(from: &AgentId, to: &AgentId, intent: Intent) -> Body {
    Body::Message {
        conversation: Conversation::direct(from, to).unwrap(),
        from: from.name().clone(),
        text: Text::try_from("hello".to_owned()).unwrap(),
        audience: audience([from, to]).unwrap(),
        intent,
        at: 0,
    }
}

fn ask(from: &AgentId, to: &AgentId) -> Body {
    direct(
        from,
        to,
        Intent::Ask {
            responder: to.clone(),
            chain: Chain::default(),
        },
    )
}

fn write(store: &Store, body: Body) -> Event {
    store
        .write(|tx| tx.author(Priority::Ordinary, body))
        .unwrap()
}

/// Exchange rounds between two stores the way a client and a node do.
fn exchange(client: &Store, node: &Store) {
    for _ in 0..16 {
        let offer = client.offer(node.origin()).unwrap().unwrap();
        let answer = node.respond(&offer).unwrap().unwrap();
        let progress = client.accept(&offer, answer).unwrap().unwrap();
        if !progress.more {
            return;
        }
    }
    panic!("the exchange did not settle");
}

fn ids(store: &Store) -> Vec<String> {
    store
        .read(|read| {
            let mut ids = Vec::new();
            for entry in redb::ReadableTable::iter(&read.open_table(super::tables::ORDER)?)? {
                ids.push(entry?.1.value().to_owned());
            }
            Ok(ids)
        })
        .unwrap()
}

#[test]
fn a_store_keeps_its_identity_and_refuses_writes_after_a_reset() {
    let root = tempfile::tempdir().unwrap();
    let first = store(&root, "a");
    let again = store(&root, "a");
    assert_eq!(first.origin(), again.origin());
    let replaced = Store::reset(&crate::layout::State::at(&root.path().join("a"))).unwrap();
    assert_ne!(replaced.origin(), first.origin());
    assert!(matches!(
        first.write(|tx| tx.author(Priority::Ordinary, Body::Omitted {})),
        Err(StoreError::IdentityChanged | StoreError::Invalid(_))
    ));
}

#[test]
fn two_stores_converge_through_offers_and_answers() {
    let root = tempfile::tempdir().unwrap();
    let (a, b) = (store(&root, "a"), store(&root, "b"));
    let alice = register(&a, "alice", Mode::Interactive);
    let bob = register(&b, "bob", Mode::Interactive);
    for _ in 0..300 {
        write(&a, direct(&alice, &bob, Intent::Send {}));
    }
    write(&b, direct(&bob, &alice, Intent::Send {}));
    exchange(&a, &b);
    assert_eq!(ids(&a), ids(&b));
    assert_eq!(ids(&a).len(), 303);
    let before = ids(&a);
    exchange(&b, &a);
    assert_eq!(ids(&a), before);
}

#[test]
fn a_node_refuses_an_offer_meant_for_another_identity() {
    let root = tempfile::tempdir().unwrap();
    let (a, b, c) = (store(&root, "a"), store(&root, "b"), store(&root, "c"));
    let offer = a.offer(c.origin()).unwrap().unwrap();
    assert_eq!(b.respond(&offer).unwrap(), Err(Rejection::WrongPeer));
}

#[test]
fn claims_finish_atomically_and_abandoned_claims_become_interrupted() {
    let root = tempfile::tempdir().unwrap();
    let a = store(&root, "a");
    let asker = register(&a, "asker", Mode::Interactive);
    let worker = register(&a, "worker", Mode::Managed);
    let first = write(&a, ask(&asker, &worker));
    let second = write(&a, ask(&asker, &worker));
    assert_eq!(a.dispatchable().unwrap(), vec![worker.clone()]);
    let turn = a.claim(&worker).unwrap().unwrap();
    assert_eq!(turn.request.id(), first.id());
    a.finish(
        &turn,
        Completion::Answered {
            text: Text::try_from("done".to_owned()).unwrap(),
            session: "0199a2b3-c4d5-7e6f-8a9b-0c1d2e3f4a5b".to_owned(),
        },
    )
    .unwrap();
    let abandoned = a.claim(&worker).unwrap().unwrap();
    assert_eq!(abandoned.request.id(), second.id());
    assert_eq!(
        abandoned.config.session.as_deref(),
        Some("0199a2b3-c4d5-7e6f-8a9b-0c1d2e3f4a5b")
    );
    assert_eq!(a.recover(&worker).unwrap(), 1);
    assert_eq!(a.recover(&worker).unwrap(), 0);
    let endings = a
        .read(|read| {
            Ok([first.id(), second.id()].map(|id| {
                super::views::resolution(read, id)
                    .unwrap()
                    .map(|resolution| resolution.ending)
            }))
        })
        .unwrap();
    assert_eq!(
        endings,
        [
            Some(Ending::Answered),
            Some(Ending::Ended(Outcome::Interrupted))
        ]
    );
    assert!(a.claim(&worker).unwrap().is_none());
    assert!(a.dispatchable().unwrap().is_empty());
}

#[test]
fn asks_to_unmanaged_or_unknown_local_agents_end_as_unavailable() {
    let root = tempfile::tempdir().unwrap();
    let a = store(&root, "a");
    let asker = register(&a, "asker", Mode::Interactive);
    let ghost = AgentId::new(
        AgentName::try_from("ghost".to_owned()).unwrap(),
        a.origin().clone(),
    );
    let question = write(&a, ask(&asker, &ghost));
    assert!(a.dispatchable().unwrap().is_empty());
    let ending = a
        .read(|read| super::views::resolution(read, question.id()))
        .unwrap()
        .map(|resolution| resolution.ending);
    assert_eq!(ending, Some(Ending::Ended(Outcome::Unavailable)));
}

#[test]
fn inboxes_follow_direct_conversations_and_room_membership() {
    let root = tempfile::tempdir().unwrap();
    let a = store(&root, "a");
    let sender = register(&a, "alice", Mode::Interactive);
    let member = register(&a, "bob", Mode::Interactive);
    let outsider = register(&a, "carol", Mode::Interactive);
    write(&a, direct(&sender, &member, Intent::Send {}));
    write(&a, direct(&sender, &outsider, Intent::Send {}));
    let release = RoomName::try_from("release".to_owned()).unwrap();
    let members = Members::try_from(vec![sender.clone(), member.clone()]).unwrap();
    write(
        &a,
        Body::Room {
            name: release.clone(),
            topic: None,
            members,
        },
    );
    let room_id = domyjob_core::chat::id::RoomId::new(a.origin().clone(), release);
    write(
        &a,
        Body::Message {
            conversation: Conversation::Room(room_id),
            from: sender.name().clone(),
            text: Text::try_from("room note".to_owned()).unwrap(),
            audience: Audience::try_from(vec![a.origin().clone()]).unwrap(),
            intent: Intent::Send {},
            at: 0,
        },
    );
    let inbox = |agent: &AgentId| {
        a.read(|read| super::views::inbox(read, agent, "", 50))
            .unwrap()
            .len()
    };
    assert_eq!(
        (inbox(&member), inbox(&outsider), inbox(&sender)),
        (2, 1, 0)
    );
    let events = a
        .read(|read| super::views::inbox(read, &member, "", 50))
        .unwrap();
    a.write(|tx| tx.mark_read(&member, events.last().unwrap()))
        .unwrap();
    let cursor = a.write(|tx| tx.read_cursor(&member)).unwrap();
    assert!(
        a.read(|read| super::views::inbox(read, &member, &cursor, 50))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn cleaning_waits_for_acknowledgments_and_keeps_later_sync_continuous() {
    let root = tempfile::tempdir().unwrap();
    let (a, b) = (store(&root, "a"), store(&root, "b"));
    let alice = register(&a, "alice", Mode::Interactive);
    let bob = register(&b, "bob", Mode::Interactive);
    write(&a, direct(&alice, &bob, Intent::Send {}));
    let conversation = Conversation::direct(&alice, &bob).unwrap();
    assert!(matches!(
        a.clean(&conversation),
        Err(StoreError::Refused(_))
    ));
    exchange(&a, &b);
    assert_eq!(a.clean(&conversation).unwrap(), 1);
    assert!(
        a.read(|read| super::views::thread(read, &conversation, 50, None))
            .unwrap()
            .is_empty()
    );
    let late = store(&root, "c");
    a.pin("c", late.origin(), false).unwrap();
    exchange(&late, &a);
    assert_eq!(ids(&late).len(), ids(&a).len());
    write(&a, direct(&alice, &bob, Intent::Send {}));
    exchange(&a, &b);
    assert_eq!(
        b.read(|read| super::views::thread(read, &conversation, 50, None))
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn a_changed_peer_identity_is_replaced_only_on_request() {
    let root = tempfile::tempdir().unwrap();
    let (a, b, c) = (store(&root, "a"), store(&root, "b"), store(&root, "c"));
    a.pin("linux", b.origin(), false).unwrap();
    a.pin("linux", b.origin(), false).unwrap();
    assert!(a.pin("linux", c.origin(), false).is_err());
    a.pin("linux", c.origin(), true).unwrap();
    assert_eq!(a.peers().unwrap().get("linux"), Some(c.origin()));
    assert!(a.pin("self", a.origin(), false).is_err());
    assert!(a.unpin("linux").unwrap());
    assert!(!a.unpin("linux").unwrap());
}

#[test]
fn a_store_in_another_or_unrecorded_format_is_refused() {
    let root = tempfile::tempdir().unwrap();
    let first = store(&root, "a");
    for (format, expected) in [
        (Some("0123456789abcdef"), "0123456789abcdef"),
        (None, "unrecorded"),
    ] {
        let database = super::open_database(first.paths()).unwrap();
        let write = database.begin_write().unwrap();
        {
            let mut meta = write.open_table(super::tables::META).unwrap();
            match format {
                Some(format) => {
                    meta.insert("format", format).unwrap();
                }
                None => {
                    meta.remove("format").unwrap();
                }
            }
        }
        write.commit().unwrap();
        drop(database);
        assert!(matches!(
            Store::open_in(&crate::layout::State::at(&root.path().join("a"))),
            Err(StoreError::Format(found)) if found == expected
        ));
    }
}
