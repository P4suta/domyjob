use core::convert::Infallible;

use super::{
    Admitted, Cursor, Decision, Failure, Ledger, Parent, Rejection, Resolution, admit, receive,
};
use crate::chat::event::{Body, Digest, Event, Intent, Outcome, Sealed};
use crate::chat::fixtures::{agent, ask, id, origin, text};
use crate::chat::id::{Conversation, EventId, Invalid, Origin};
use crate::chat::model::Model;
use crate::chat::notification::Delivery;
use crate::chat::policy::audience;

struct Limited {
    model: Model,
    capacity: usize,
    substituted_parent: Option<(EventId, Event)>,
}

impl Ledger for Limited {
    type Error = Infallible;

    fn local(&self) -> &Origin {
        self.model.local()
    }

    fn cursor(&self, origin: &Origin) -> Result<Cursor, Infallible> {
        self.model.cursor(origin)
    }

    fn digest(&self, id: &EventId) -> Result<Option<Digest>, Infallible> {
        self.model.digest(id)
    }

    fn parent(&self, id: &EventId) -> Result<Parent, Infallible> {
        if let Some((requested, event)) = &self.substituted_parent
            && requested == id
        {
            return Ok(Parent::Present(alloc::boxed::Box::new(event.clone())));
        }
        self.model.parent(id)
    }

    fn resolution(&self, request: &EventId) -> Result<Option<Resolution>, Infallible> {
        self.model.resolution(request)
    }

    fn clock(&self) -> Result<u64, Infallible> {
        self.model.clock()
    }

    fn has_room(&self) -> Result<bool, Infallible> {
        Ok(self.model.ordered().len() < self.capacity)
    }

    fn commit(&mut self, sealed: &Sealed, admitted: Admitted) -> Result<(), Infallible> {
        self.model.commit(sealed, admitted)
    }
}

#[test]
fn a_storage_limit_preserves_the_admitted_prefix_for_a_later_retry() {
    let mut sender = Model::new(origin('b'));
    let events = [
        sender.write(Body::Omitted {}).unwrap(),
        sender.write(Body::Omitted {}).unwrap(),
    ];
    let mut receiver = Limited {
        model: Model::new(origin('a')),
        capacity: 1,
        substituted_parent: None,
    };
    let Ok(first) = receive(&mut receiver, &origin('b'), &events);
    assert_eq!(first.appended, 1);
    assert_eq!(first.seen, 1);
    assert_eq!(first.rejected, Some(Rejection::ResourceLimit));
    assert_eq!(
        receiver.model.ordered(),
        events.first().into_iter().collect::<alloc::vec::Vec<_>>()
    );
    receiver.capacity = 2;
    let Ok(retry) = receive(&mut receiver, &origin('b'), &events);
    assert_eq!(retry.appended, 1);
    assert_eq!(retry.seen, 2);
    assert_eq!(retry.rejected, None);
    assert_eq!(
        receiver.model.ordered(),
        events.iter().collect::<alloc::vec::Vec<_>>()
    );
}

#[test]
fn a_full_receiver_still_accepts_an_exact_retry() {
    let mut sender = Model::new(origin('b'));
    let events = [sender.write(Body::Omitted {}).unwrap()];
    let mut receiver = Limited {
        model: Model::new(origin('a')),
        capacity: 1,
        substituted_parent: None,
    };
    let Ok(first) = receive(&mut receiver, &origin('b'), &events);
    assert_eq!(first.appended, 1);
    let Ok(retry) = receive(&mut receiver, &origin('b'), &events);
    assert_eq!(retry.appended, 0);
    assert_eq!(retry.seen, 1);
    assert_eq!(retry.rejected, None);
}

#[derive(Clone, Copy)]
enum DependentBody {
    Receipt,
    Started,
    Resolution,
    Reply,
}

#[derive(Debug)]
struct ParentObservation {
    expected_event: EventId,
    outcome: Result<Decision, Failure<Infallible>>,
}

impl ParentObservation {
    fn rejected(&self) -> bool {
        matches!(&self.outcome, Err(Failure::Rejected(Rejection::Mismatch { event })) if event == &self.expected_event)
    }
}

fn observe_substituted_parent(kind: DependentBody) -> Result<ParentObservation, Invalid> {
    const REQUEST_SEQUENCE: u64 = 1;
    const REPLACEMENT_SEQUENCE: u64 = 2;
    const PARENT_CLOCK: u64 = 1;
    const DEPENDENT_CLOCK: u64 = 2;
    let sender = agent("alice", 'a');
    let recipient = agent("bob", 'b');
    let request = id('a', REQUEST_SEQUENCE);
    let replacement = Event::new(
        id('a', REPLACEMENT_SEQUENCE),
        PARENT_CLOCK,
        ask(&sender, &recipient),
    )?;
    let conversation = Conversation::direct(&sender, &recipient)?;
    let audience = audience([&sender, &recipient])?;
    let body = match kind {
        DependentBody::Receipt => Body::Receipt {
            request: request.clone(),
            conversation,
            audience,
            agent: recipient.name().clone(),
            delivery: Delivery::Read,
        },
        DependentBody::Started => Body::TurnStarted {
            request: request.clone(),
            conversation,
            audience,
            agent: recipient.name().clone(),
        },
        DependentBody::Resolution => Body::Resolved {
            request: request.clone(),
            conversation,
            audience,
            agent: recipient.name().clone(),
            outcome: Outcome::Failed,
        },
        DependentBody::Reply => Body::Message {
            conversation,
            from: recipient.name().clone(),
            text: text("answer"),
            audience,
            intent: Intent::Reply {
                request: request.clone(),
            },
            at: crate::chat::event::Stamp::from_unix_millis(DEPENDENT_CLOCK),
            notice: None,
        },
    };
    let ledger = Limited {
        model: Model::new(origin('c')),
        capacity: usize::MAX,
        substituted_parent: Some((request, replacement)),
    };
    let event = Event::new(id('b', REQUEST_SEQUENCE), DEPENDENT_CLOCK, body)?;
    let sealed = Sealed::new(event)?;
    let outcome = admit(&ledger, &sealed);
    Ok(ParentObservation {
        expected_event: sealed.event().id().clone(),
        outcome,
    })
}

#[test]
fn receipts_reject_a_present_parent_with_a_different_identity() {
    let observed = observe_substituted_parent(DependentBody::Receipt).unwrap();
    assert!(observed.rejected(), "{observed:?}");
}

#[test]
fn started_turns_reject_a_present_parent_with_a_different_identity() {
    let observed = observe_substituted_parent(DependentBody::Started).unwrap();
    assert!(observed.rejected(), "{observed:?}");
}

#[test]
fn resolutions_reject_a_present_parent_with_a_different_identity() {
    let observed = observe_substituted_parent(DependentBody::Resolution).unwrap();
    assert!(observed.rejected(), "{observed:?}");
}

#[test]
fn replies_reject_a_present_parent_with_a_different_identity() {
    let observed = observe_substituted_parent(DependentBody::Reply).unwrap();
    assert!(observed.rejected(), "{observed:?}");
}
