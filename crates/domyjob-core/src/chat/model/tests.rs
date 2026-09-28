use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::vec::Vec;

use super::Model;
use crate::chat::event::{Body, Chain, Event, Intent, Members, Outcome};
use crate::chat::exchange::respond;
use crate::chat::fixtures::{agent, ask, card, id, origin, text};
use crate::chat::id::{AgentId, Audience, Conversation, EventId, Origin, RoomId, RoomName};
use crate::chat::ledger::{Ending, Failure, Ledger, Rejection, receive};
use crate::chat::policy::audience;
use crate::chat_wire::{Batch, Offer};

fn model(digit: char) -> Model {
    Model::new(origin(digit))
}

fn message(
    from: &AgentId,
    conversation: Conversation,
    members: &[&AgentId],
    intent: Intent,
) -> Body {
    Body::Message {
        conversation,
        from: from.name().clone(),
        text: text("hello"),
        audience: audience(members.iter().copied()).unwrap(),
        intent,
        at: 0,
    }
}

fn direct(from: &AgentId, to: &AgentId, intent: Intent) -> Body {
    message(
        from,
        Conversation::direct(from, to).unwrap(),
        &[from, to],
        intent,
    )
}

fn room_id(owner: char) -> RoomId {
    RoomId::new(
        origin(owner),
        RoomName::try_from("release".to_owned()).unwrap(),
    )
}

fn room(members: &[&AgentId]) -> Body {
    Body::Room {
        name: RoomName::try_from("release".to_owned()).unwrap(),
        topic: None,
        members: Members::try_from(
            members
                .iter()
                .map(|&member| member.clone())
                .collect::<Vec<_>>(),
        )
        .unwrap(),
    }
}

/// Open a room of `members` on `owner` and ask `responder` there as its first member.
fn room_question(owner: &mut Model, members: &[&AgentId], responder: &AgentId) -> Event {
    owner.write(room(members)).unwrap();
    let asker = members.first().copied().unwrap();
    owner
        .write(in_room(
            asker,
            members,
            Intent::Ask {
                responder: responder.clone(),
                chain: Chain::default(),
            },
        ))
        .unwrap()
}

fn in_room(from: &AgentId, members: &[&AgentId], intent: Intent) -> Body {
    message(from, Conversation::Room(room_id('a')), members, intent)
}

fn reply(request: &Event, from: &AgentId) -> Body {
    let (Some(conversation), Some(members)) =
        (request.body().conversation(), request.body().audience())
    else {
        panic!("replies answer messages");
    };
    Body::Message {
        conversation: conversation.clone(),
        from: from.name().clone(),
        text: text("answer"),
        audience: members.clone(),
        intent: Intent::Reply {
            request: request.id().clone(),
        },
        at: 0,
    }
}

fn ending(request: &Event, by: &AgentId, outcome: Outcome) -> Body {
    let (Some(conversation), Some(members)) =
        (request.body().conversation(), request.body().audience())
    else {
        panic!("endings follow messages");
    };
    Body::Resolved {
        request: request.id().clone(),
        conversation: conversation.clone(),
        audience: members.clone(),
        agent: by.name().clone(),
        outcome,
    }
}

fn ids(model: &Model) -> Vec<EventId> {
    model
        .ordered()
        .into_iter()
        .map(|event| event.id().clone())
        .collect()
}

fn sync(first: &mut Model, second: &mut Model) {
    first.sync_with(second, 64).unwrap();
}

#[test]
fn offline_writes_merge_once_into_one_shared_order() {
    let (mut a, mut b) = (model('a'), model('b'));
    let (alice, bob) = (agent("alice", 'a'), agent("bob", 'b'));
    a.write(direct(&alice, &bob, Intent::Send {})).unwrap();
    b.write(direct(&bob, &alice, Intent::Send {})).unwrap();
    b.write(direct(&bob, &alice, Intent::Send {})).unwrap();
    sync(&mut a, &mut b);
    sync(&mut b, &mut a);
    assert_eq!(ids(&a), ids(&b));
    assert_eq!(ids(&a).len(), 3);
    let before = ids(&a);
    sync(&mut a, &mut b);
    assert_eq!(ids(&a), before);
}

#[test]
fn gaps_conflicts_and_clock_regressions_keep_the_admitted_prefix() {
    let mut a = model('a');
    let event = |seq: u64, clock: u64, body: Body| Event::new(id('b', seq), clock, body).unwrap();
    let (bob, alice) = (agent("bob", 'b'), agent("alice", 'a'));
    let hello = direct(&bob, &alice, Intent::Send {});
    let Ok(gap) = receive(&mut a, &origin('b'), &[event(2, 2, hello.clone())]);
    assert!(matches!(
        gap.rejected,
        Some(Rejection::Gap { expected: 1, .. })
    ));
    assert_eq!(gap.seen, 0);
    let first = event(1, 5, hello.clone());
    let regressed = event(2, 5, hello.clone());
    let Ok(outcome) = receive(&mut a, &origin('b'), &[first.clone(), regressed]);
    assert_eq!(outcome.seen, 1);
    assert!(matches!(
        outcome.rejected,
        Some(Rejection::ClockRegression { .. })
    ));
    let Ok(duplicate) = receive(&mut a, &origin('b'), core::slice::from_ref(&first));
    assert_eq!(
        (duplicate.seen, duplicate.appended, duplicate.rejected),
        (1, 0, None)
    );
    let altered = event(1, 5, ask(&bob, &alice));
    let Ok(conflict) = receive(&mut a, &origin('b'), &[altered]);
    assert!(matches!(
        conflict.rejected,
        Some(Rejection::Conflict { .. })
    ));
    let Ok(forged) = receive(&mut a, &origin('c'), &[event(2, 6, hello)]);
    assert!(matches!(
        forged.rejected,
        Some(Rejection::Unauthorized { .. })
    ));
}

#[test]
fn private_content_reaches_only_its_audience_while_ids_stay_aligned() {
    let (mut a, mut b, mut c) = (model('a'), model('b'), model('c'));
    let (alice, bob) = (agent("alice", 'a'), agent("bob", 'b'));
    let secret = a.write(direct(&alice, &bob, Intent::Send {})).unwrap();
    sync(&mut a, &mut b);
    sync(&mut a, &mut c);
    assert_eq!(ids(&b), ids(&c));
    let stored = |model: &Model| {
        model
            .ordered()
            .into_iter()
            .find(|event| event.id() == secret.id())
            .map(|event| event.body().clone())
    };
    assert_eq!(stored(&b), Some(secret.body().clone()));
    assert_eq!(stored(&c), Some(Body::Omitted {}));
}

#[test]
fn an_answer_before_its_question_is_deferred_and_then_converges() {
    let [mut a, mut b, mut c] = [model('a'), model('b'), model('c')];
    let (alice, bob) = (agent("alice", 'a'), agent("bob", 'b'));
    let question = room_question(&mut a, &[&alice, &bob, &agent("carol", 'c')], &bob);
    sync(&mut a, &mut b);
    b.write(Body::Profile {
        agent: bob.name().clone(),
        card: Box::new(card("Bob", "builder", &[])),
    })
    .unwrap();
    b.write(reply(&question, &bob)).unwrap();
    let deferred = c.sync_with(&mut b, 4).unwrap();
    assert!(matches!(
        deferred.received.rejected,
        Some(Rejection::MissingDependency { .. })
    ));
    assert_eq!(deferred.received.seen, 1);
    assert_eq!(c.profiles().len(), 1);
    sync(&mut c, &mut a);
    sync(&mut c, &mut b);
    sync(&mut a, &mut b);
    assert_eq!(ids(&a), ids(&c));
    assert_eq!(a.resolutions(), c.resolutions());
    assert_eq!(
        c.resolutions()
            .get(question.id())
            .map(|resolution| resolution.ending),
        Some(Ending::Answered)
    );
    assert!(c.open_asks().is_empty());
}

#[test]
fn a_racing_answer_and_withdrawal_select_the_same_winner_everywhere() {
    let [mut a, mut b, mut c] = [model('a'), model('b'), model('c')];
    let (alice, bob) = (agent("alice", 'a'), agent("bob", 'b'));
    let question = room_question(&mut a, &[&alice, &bob, &agent("carol", 'c')], &bob);
    sync(&mut a, &mut b);
    sync(&mut a, &mut c);
    let withdrawal = a
        .write(ending(&question, &alice, Outcome::Withdrawn))
        .unwrap();
    let answer = b.write(reply(&question, &bob)).unwrap();
    sync(&mut c, &mut b);
    sync(&mut c, &mut a);
    sync(&mut a, &mut b);
    let winner = if withdrawal.order() < answer.order() {
        withdrawal.id()
    } else {
        answer.id()
    };
    for model in [&a, &b, &c] {
        assert_eq!(
            model
                .resolutions()
                .get(question.id())
                .map(|resolution| &resolution.event),
            Some(winner)
        );
        assert!(model.open_asks().is_empty());
    }
}

#[test]
fn only_the_responder_ends_an_ask_and_only_the_asker_withdraws_it() {
    let (mut a, mut b) = (model('a'), model('b'));
    let (alice, bob, other) = (agent("alice", 'a'), agent("bob", 'b'), agent("other", 'b'));
    let members = [&alice, &bob, &other];
    let question = room_question(&mut a, &members, &bob);
    sync(&mut a, &mut b);
    b.write(reply(&question, &other)).unwrap();
    assert!(b.resolutions().is_empty());
    assert!(matches!(
        b.write(ending(&question, &other, Outcome::Failed)),
        Err(Failure::Rejected(Rejection::Mismatch { .. }))
    ));
    assert!(matches!(
        b.write(ending(&question, &bob, Outcome::Withdrawn)),
        Err(Failure::Rejected(Rejection::Mismatch { .. }))
    ));
    b.write(ending(&question, &bob, Outcome::Failed)).unwrap();
    sync(&mut a, &mut b);
    assert_eq!(
        a.resolutions()
            .get(question.id())
            .map(|resolution| resolution.ending),
        Some(Ending::Ended(Outcome::Failed))
    );
}

#[test]
fn turn_starts_come_only_from_the_responder_after_the_question() {
    let (mut a, mut b) = (model('a'), model('b'));
    let (alice, bob) = (agent("alice", 'a'), agent("bob", 'b'));
    let question = a.write(ask(&alice, &bob)).unwrap();
    let started = |by: &AgentId| {
        let (Some(conversation), Some(members)) =
            (question.body().conversation(), question.body().audience())
        else {
            panic!("the question is a message");
        };
        Body::TurnStarted {
            request: question.id().clone(),
            conversation: conversation.clone(),
            audience: members.clone(),
            agent: by.name().clone(),
        }
    };
    assert!(matches!(
        b.write(started(&bob)),
        Err(Failure::Rejected(Rejection::MissingDependency { .. }))
    ));
    sync(&mut a, &mut b);
    assert!(matches!(
        b.write(started(&agent("mallory", 'b'))),
        Err(Failure::Invalid(_))
    ));
    b.write(started(&bob)).unwrap();
    sync(&mut a, &mut b);
    assert_eq!(a.started().get(question.id()), Some(&bob));
    assert_eq!(a.open_asks().len(), 1);
}

#[test]
fn profiles_and_rooms_follow_their_owner_in_sequence() {
    let (mut a, mut b) = (model('a'), model('b'));
    let (alice, bob) = (agent("alice", 'a'), agent("bob", 'b'));
    for role in ["reviewer", "maintainer"] {
        a.write(Body::Profile {
            agent: alice.name().clone(),
            card: Box::new(card("Alice", role, &["rust"])),
        })
        .unwrap();
    }
    a.write(room(&[&alice])).unwrap();
    a.write(room(&[&alice, &bob])).unwrap();
    sync(&mut a, &mut b);
    assert_eq!(
        b.profiles()
            .get(&alice)
            .and_then(|card| card.role.as_ref())
            .map(crate::chat::id::Line::as_str),
        Some("maintainer")
    );
    assert_eq!(
        b.rooms()
            .get(&room_id('a'))
            .map(|room| room.members.agents().len()),
        Some(2)
    );
    a.write(Body::RoomClosed {
        name: RoomName::try_from("release".to_owned()).unwrap(),
    })
    .unwrap();
    a.write(Body::Left {
        agent: alice.name().clone(),
    })
    .unwrap();
    sync(&mut a, &mut b);
    assert!(b.rooms().is_empty());
    assert!(b.profiles().is_empty());
}

#[test]
fn a_node_refuses_offers_for_another_machine_or_from_the_future() {
    let mut b = model('b');
    let offer = |to: Origin, seen: u64| Offer {
        from: origin('a'),
        to,
        after: 0,
        seen,
        batch: Batch::default(),
    };
    let Ok(wrong) = respond(&mut b, &offer(origin('c'), 0));
    assert_eq!(wrong, Err(Rejection::WrongPeer));
    let Ok(ahead) = respond(&mut b, &offer(origin('b'), 1));
    assert_eq!(ahead, Err(Rejection::AheadOfHistory));
    let Ok(empty) = respond(&mut b, &offer(origin('b'), 0));
    empty.unwrap();
}

#[test]
fn long_histories_move_in_bounded_rounds() {
    let (mut a, mut b) = (model('a'), model('b'));
    let (alice, bob) = (agent("alice", 'a'), agent("bob", 'b'));
    for _ in 0..300 {
        a.write(direct(&alice, &bob, Intent::Send {})).unwrap();
    }
    let first = a.sync_with(&mut b, 1).unwrap();
    assert!(first.more);
    assert_eq!(b.cursor(&origin('a')).map(|cursor| cursor.seen), Ok(256));
    let rest = a.sync_with(&mut b, 8).unwrap();
    assert!(!rest.more);
    assert_eq!(ids(&a), ids(&b));
    let audience_of = |event: &&Event| {
        event
            .body()
            .audience()
            .map(Audience::members)
            .map(<[Origin]>::len)
    };
    assert!(
        b.ordered()
            .iter()
            .all(|event| audience_of(event) == Some(2))
    );
}
