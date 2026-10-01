use super::{BATCH_BYTES, Outgoing, accept, offer};
use crate::chat::event::{Body, Event, Intent, Sealed, Stamp};
use crate::chat::fixtures::{agent, id, origin, text};
use crate::chat::id::Conversation;
use crate::chat::ledger::{Ledger as _, Rejection};
use crate::chat::model::Model;
use crate::chat::policy::audience;
use crate::chat_wire::{Answer, Batch};

fn sized_event(seq: u64, encoded_bytes: usize) -> Sealed {
    let (alice, bob) = (agent("alice", 'a'), agent("bob", 'b'));
    let body = |value: &str| Body::Message {
        conversation: Conversation::direct(&alice, &bob).unwrap(),
        from: alice.name().clone(),
        text: text(value),
        audience: audience([&alice, &bob]).unwrap(),
        intent: Intent::Send {},
        at: Stamp::from_unix_millis(0),
    };
    let base = Sealed::new(Event::new(id('a', seq), seq, body("x")).unwrap()).unwrap();
    let payload = encoded_bytes
        .checked_sub(base.encoded().len().checked_sub(1).unwrap())
        .unwrap();
    let mut value = "\u{1}".repeat(payload / 6);
    value.push_str(&"x".repeat(payload % 6));
    let sealed = Sealed::new(Event::new(id('a', seq), seq, body(&value)).unwrap()).unwrap();
    assert_eq!(sealed.encoded().len(), encoded_bytes);
    sealed
}

#[test]
fn outgoing_accepts_the_exact_wire_byte_limit_and_defers_the_next_event() {
    let mut outgoing = Outgoing::default();
    let first = sized_event(1, 300_000);
    let second = sized_event(2, 300_000);
    let remaining = BATCH_BYTES.checked_sub(600_003).unwrap();
    let third = sized_event(3, remaining);
    assert!(outgoing.offer(first));
    assert!(outgoing.offer(second));
    assert!(outgoing.offer(third));
    let next = Sealed::new(Event::new(id('a', 4), 4, Body::Omitted {}).unwrap()).unwrap();
    assert!(!outgoing.offer(next));
    let batch = outgoing.into_batch().unwrap();
    assert_eq!(batch.events().len(), 3);
    assert!(batch.more());
}

#[test]
fn answers_from_another_peer_or_ahead_of_local_history_change_nothing() {
    let mut local = Model::new(origin('a'));
    let Ok(request) = offer(&local, &origin('b'));
    let request = request.unwrap();
    for (from, seen, rejection) in [
        (origin('c'), 0, Rejection::WrongPeer),
        (origin('b'), 1, Rejection::AheadOfHistory),
    ] {
        let incoming = Event::new(
            crate::chat::id::EventId::new(from.clone(), core::num::NonZeroU64::new(1).unwrap()),
            1,
            Body::Omitted {},
        )
        .unwrap();
        let answer = Answer {
            from: from.clone(),
            seen,
            batch: Batch::new(alloc::vec![incoming], false).unwrap(),
            rejected: None,
        };
        let Ok(result) = accept(&mut local, &request, answer);
        assert_eq!(result, Err(rejection));
        assert_eq!(local.cursor(&from).map(|cursor| cursor.seen), Ok(0));
        assert_eq!(local.ordered().len(), 0);
    }
}
