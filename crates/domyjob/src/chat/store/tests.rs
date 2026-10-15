use domyjob_core::chat::card::Mode;
use domyjob_core::chat::event::{Body, Chain, Event, Intent, Members, Outcome};
use domyjob_core::chat::id::{AgentId, AgentName, Audience, Conversation, RoomName, Text};
use domyjob_core::chat::ledger::{Ending, Rejection};
use domyjob_core::chat::policy::{Priority, audience};

use super::{Completion, Store, StoreError};

use super::fixtures::{register, store};

fn direct(from: &AgentId, to: &AgentId, intent: Intent) -> Body {
    Body::Message {
        conversation: Conversation::direct(from, to).unwrap(),
        from: from.name().clone(),
        text: Text::try_from("hello".to_owned()).unwrap(),
        audience: audience([from, to]).unwrap(),
        intent,
        at: domyjob_core::chat::event::Stamp::from_unix_millis(0),
        notice: None,
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

fn local_pair(root: &tempfile::TempDir) -> (Store, AgentId, AgentId) {
    let local = store(root, "local");
    let alice = register(&local, "alice", Mode::Interactive);
    let bob = register(&local, "bob", Mode::Interactive);
    (local, alice, bob)
}

fn substituted_event_store(root: &tempfile::TempDir) -> (Store, Event) {
    let (local, alice, bob) = local_pair(root);
    let requested = write(&local, direct(&alice, &bob, Intent::Send {}));
    let replacement = write(&local, direct(&alice, &bob, Intent::Send {}));
    assert_ne!(requested.id(), replacement.id());
    let encoded = serde_json::to_string(&replacement).unwrap();
    local
        .write(|tx| {
            tx.transaction().open_table(super::tables::EVENTS)?.insert(
                super::tables::event_key(requested.id()).as_str(),
                encoded.as_str(),
            )?;
            Ok(())
        })
        .unwrap();
    (local, requested)
}

#[test]
fn stored_events_reject_a_valid_payload_under_a_different_event_key() {
    let root = tempfile::tempdir().unwrap();
    let (local, requested) = substituted_event_store(&root);
    let observed = local.read(|read| super::views::stored(read, requested.id()));
    assert!(
        matches!(observed, Err(StoreError::Corrupt("event identity"))),
        "a different event's valid payload was returned: {observed:?}"
    );
}

#[test]
fn transaction_parent_rejects_a_valid_payload_under_a_different_event_key() {
    use domyjob_core::chat::ledger::Ledger;
    let root = tempfile::tempdir().unwrap();
    let (local, requested) = substituted_event_store(&root);
    let observed = local.write(|tx| tx.parent(requested.id()));
    assert!(
        matches!(&observed, Err(StoreError::Corrupt("event identity"))),
        "the parent port returned a different event: {observed:?}"
    );
}

fn exchange(client: &Store, node: &Store) {
    const MAX_EXCHANGE_ROUNDS: usize = 16;
    for _ in 0..MAX_EXCHANGE_ROUNDS {
        let offer = client.offer(node.origin()).unwrap().unwrap();
        let answer = node.respond(&offer).unwrap().unwrap();
        let progress = client.accept(&offer, answer).unwrap().unwrap();
        if !progress.more {
            return;
        }
    }
    panic!("the exchange did not settle");
}

#[test]
fn recipients_sync_delivery_read_and_ack_without_advancing_each_other() {
    use domyjob_core::chat::notification::Delivery;
    let root = tempfile::tempdir().unwrap();
    let (a, b) = (store(&root, "a"), store(&root, "b"));
    let alice = register(&a, "alice", Mode::Interactive);
    let bob = register(&b, "bob", Mode::Interactive);
    let message = write(&a, direct(&alice, &bob, Intent::Send {}));
    exchange(&a, &b);
    for stage in [Delivery::Delivered, Delivery::Read, Delivery::Acknowledged] {
        b.write(|tx| tx.receipt(&bob, &message, stage)).unwrap();
        exchange(&b, &a);
        assert_eq!(
            a.read(|read| super::delivery(read, &bob, message.id()))
                .unwrap(),
            stage
        );
        assert_eq!(
            a.read(|read| super::delivery(read, &alice, message.id()))
                .unwrap(),
            Delivery::Pending
        );
    }
    let seen = b.local_seen().unwrap();
    b.write(|tx| tx.receipt(&bob, &message, Delivery::Delivered))
        .unwrap();
    assert_eq!(b.local_seen().unwrap(), seen);
}

#[test]
fn an_unanswered_question_escalates_once_at_its_deadline() {
    use domyjob_core::chat::event::Stamp;
    use domyjob_core::chat::id::Line;
    use domyjob_core::chat::notification::{Notice, Question};
    let root = tempfile::tempdir().unwrap();
    let (local, alice, bob) = local_pair(&root);
    let fallback = register(&local, "fallback", Mode::Interactive);
    let mut question = ask(&alice, &bob);
    if let Body::Message { notice, .. } = &mut question {
        *notice = Some(Notice {
            recipients: Members::try_from(vec![bob]).unwrap(),
            urgent: false,
            subject: None,
            question: Some(
                Question::new(
                    Line::try_from("Approve or explain".to_owned()).unwrap(),
                    1,
                    fallback.clone(),
                )
                .unwrap(),
            ),
        });
    }
    let question = write(&local, question);
    assert_eq!(local.escalate(Stamp::from_unix_millis(999)).unwrap(), 0);
    assert_eq!(local.escalate(Stamp::from_unix_millis(1000)).unwrap(), 1);
    assert_eq!(local.escalate(Stamp::from_unix_millis(2000)).unwrap(), 0);
    let forwarded = local
        .read(|read| super::escalation(read, question.id()))
        .unwrap()
        .unwrap();
    let forwarded = local
        .read(|read| super::event(read, &forwarded))
        .unwrap()
        .unwrap();
    assert!(matches!(forwarded.body(), Body::Message {
        intent: Intent::Ask { responder, .. }, notice: Some(notice), ..
    } if *responder == fallback && notice.urgent));
    assert_eq!(
        local.read(|read| super::urgent(read, &fallback)).unwrap(),
        [forwarded.id().clone()]
    );
}

#[test]
fn a_reply_acknowledges_an_urgent_message_and_releases_stop() {
    use domyjob_core::chat::notification::{Delivery, Notice};
    let root = tempfile::tempdir().unwrap();
    let (local, alice, bob) = local_pair(&root);
    let mut message = direct(&alice, &bob, Intent::Send {});
    if let Body::Message { notice, .. } = &mut message {
        *notice = Some(Notice {
            recipients: Members::try_from(vec![bob.clone()]).unwrap(),
            urgent: true,
            subject: None,
            question: None,
        });
    }
    let message = write(&local, message);
    write(
        &local,
        direct(
            &bob,
            &alice,
            Intent::Reply {
                request: message.id().clone(),
            },
        ),
    );
    assert_eq!(
        local
            .read(|read| super::delivery(read, &bob, message.id()))
            .unwrap(),
        Delivery::Acknowledged
    );
    assert_eq!(
        local.read(|read| super::urgent(read, &bob)).unwrap(),
        Vec::new()
    );
}

#[test]
fn room_receipts_advance_only_the_named_recipient() {
    use domyjob_core::chat::notification::{Delivery, Notice};
    let root = tempfile::tempdir().unwrap();
    let local = store(&root, "local");
    let sender = register(&local, "sender", Mode::Interactive);
    let first = register(&local, "first", Mode::Interactive);
    let second = register(&local, "second", Mode::Interactive);
    let name = RoomName::try_from("attention".to_owned()).unwrap();
    write(
        &local,
        Body::Room {
            name: name.clone(),
            topic: None,
            members: Members::try_from(vec![sender.clone(), first.clone(), second.clone()])
                .unwrap(),
        },
    );
    let message = write(
        &local,
        Body::Message {
            conversation: Conversation::Room(domyjob_core::chat::id::RoomId::new(
                local.origin().clone(),
                name,
            )),
            from: sender.name().clone(),
            text: Text::try_from("review".to_owned()).unwrap(),
            audience: audience([&sender, &first, &second]).unwrap(),
            intent: Intent::Send {},
            at: domyjob_core::chat::event::Stamp::from_unix_millis(0),
            notice: Some(Notice {
                recipients: Members::try_from(vec![first.clone(), second.clone()]).unwrap(),
                urgent: true,
                subject: None,
                question: None,
            }),
        },
    );
    local
        .write(|tx| tx.receipt(&first, &message, Delivery::Acknowledged))
        .unwrap();
    assert_eq!(
        local
            .read(|read| super::delivery(read, &first, message.id()))
            .unwrap(),
        Delivery::Acknowledged
    );
    assert_eq!(
        local
            .read(|read| super::delivery(read, &second, message.id()))
            .unwrap(),
        Delivery::Pending
    );
    assert_eq!(
        local.read(|read| super::urgent(read, &first)).unwrap(),
        Vec::new()
    );
    assert_eq!(
        local.read(|read| super::urgent(read, &second)).unwrap(),
        [message.id().clone()]
    );
    let error = local
        .clean(message.body().conversation().unwrap())
        .unwrap_err();
    assert!(matches!(error, StoreError::Invalid(_)));
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
fn managed_claim_finish_and_recovery_publish_actual_activity_boundaries() {
    use domyjob_core::chat::notification::ActivityState;
    let root = tempfile::tempdir().unwrap();
    let local = store(&root, "local");
    let asker = register(&local, "asker", Mode::Interactive);
    let worker = register(&local, "worker", Mode::Managed);
    write(&local, ask(&asker, &worker));
    let activity = || {
        local
            .read(|read| super::activity(read, &worker))
            .unwrap()
            .unwrap()
    };
    let turn = local.claim(&worker).unwrap().unwrap();
    assert_eq!(activity().state, ActivityState::Working);
    local.finish(&turn, Completion::Failed).unwrap();
    assert_eq!(activity().state, ActivityState::Waiting);
    write(&local, ask(&asker, &worker));
    local.claim(&worker).unwrap().unwrap();
    assert_eq!(activity().state, ActivityState::Working);
    assert_eq!(local.recover(&worker).unwrap(), 1);
    assert_eq!(activity().state, ActivityState::Waiting);
    let seen = local.local_seen().unwrap();
    assert_eq!(local.recover(&worker).unwrap(), 0);
    assert_eq!(local.local_seen().unwrap(), seen);
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
    assert_eq!(a.dispatchable().unwrap().len(), 0);
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
    assert_eq!(a.dispatchable().unwrap().len(), 0);
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
            at: domyjob_core::chat::event::Stamp::from_unix_millis(0),
            notice: None,
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
    assert_eq!(
        a.read(|read| super::views::inbox(read, &member, &cursor, 50))
            .unwrap()
            .len(),
        0
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
    assert_eq!(
        a.read(|read| super::views::thread(read, &conversation, 50, None))
            .unwrap()
            .len(),
        0
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

#[test]
fn additive_notifications_upgrade_the_previous_format_without_losing_history() {
    let root = tempfile::tempdir().unwrap();
    let original = store(&root, "previous");
    let alice = register(&original, "alice", Mode::Interactive);
    let bob = register(&original, "bob", Mode::Interactive);
    let message = write(&original, direct(&alice, &bob, Intent::Send {}));
    {
        let database = super::open_database(original.paths()).unwrap();
        let write = database.begin_write().unwrap();
        write
            .open_table(super::tables::META)
            .unwrap()
            .insert("format", crate::formats::previous_chat().as_str())
            .unwrap();
        for table in [
            super::tables::DELIVERY,
            super::tables::ACTIVITY,
            super::tables::ESCALATIONS,
            super::tables::URGENT,
        ] {
            write.delete_table(table).unwrap();
        }
        write.commit().unwrap();
    }
    let upgraded = store(&root, "previous");
    assert_eq!(upgraded.origin(), original.origin());
    assert_eq!(
        upgraded
            .read(|read| super::event(read, message.id()))
            .unwrap(),
        Some(message)
    );
    assert_eq!(
        upgraded.read(|read| super::urgent(read, &bob)).unwrap(),
        Vec::new()
    );
}
