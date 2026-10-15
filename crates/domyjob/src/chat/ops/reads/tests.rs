use domyjob_core::chat::card::Mode;
use domyjob_core::chat::event::{Body, Event, Intent};
use domyjob_core::chat::id::{AgentId, Conversation, Text};
use domyjob_core::chat::notification::Delivery;
use domyjob_core::chat::policy::{Priority, audience};

use crate::chat::args::{InboxArgs, MessageArgs};
use crate::chat::ops::{self, Session};
use crate::chat::store::fixtures::{register, store};

pub(crate) struct Fixture {
    _root: tempfile::TempDir,
    pub(crate) session: Session,
    pub(crate) sender: AgentId,
    pub(crate) recipient: AgentId,
    pub(crate) message: Event,
}

pub(crate) fn fixture() -> Fixture {
    let root = tempfile::tempdir().unwrap();
    let local = store(&root, "local");
    let sender = register(&local, "sender", Mode::Interactive);
    let recipient = register(&local, "recipient", Mode::Interactive);
    let message = local
        .write(|tx| {
            tx.author(
                Priority::Ordinary,
                Body::Message {
                    conversation: Conversation::direct(&sender, &recipient).unwrap(),
                    from: sender.name().clone(),
                    text: Text::try_from("unread body".to_owned()).unwrap(),
                    audience: audience([&sender, &recipient]).unwrap(),
                    intent: Intent::Send {},
                    at: crate::platform::clock::stamp(),
                    notice: None,
                },
            )
        })
        .unwrap();
    let mut session = Session::with_store(local);
    session.bind(recipient.name().as_str()).unwrap();
    Fixture {
        _root: root,
        session,
        sender,
        recipient,
        message,
    }
}

pub(crate) fn stage(fixture: &Fixture) -> Delivery {
    fixture
        .session
        .store
        .read(|read| crate::chat::store::delivery(read, &fixture.recipient, fixture.message.id()))
        .unwrap()
}

pub(crate) fn read_args(fixture: &Fixture) -> MessageArgs {
    MessageArgs {
        message: fixture.message.id().to_string(),
        timeout: 0,
    }
}

#[test]
fn anonymous_reads_refuse_identity_before_parsing_or_looking_up_message_ids() {
    let fixture = fixture();
    let anonymous = Session::with_store(fixture.session.store.clone());
    let seen = anonymous.store.local_seen().unwrap();
    let missing = format!("{}:ffffffffffffffff", fixture.session.store.origin());
    for message in [
        "unbound".to_owned(),
        missing,
        fixture.message.id().to_string(),
    ] {
        let args = MessageArgs {
            message,
            timeout: 0,
        };
        assert!(matches!(
            ops::read_message(&anonymous, &args),
            Err(ops::OpsError::Anonymous)
        ));
    }
    assert_eq!(anonymous.store.local_seen().unwrap(), seen);
    assert_eq!(stage(&fixture), Delivery::Pending);
}

#[test]
fn prepared_reads_remain_unread_until_successful_completion() {
    for inbox in [false, true] {
        let fixture = fixture();
        let outcome = if inbox {
            ops::inbox(
                &fixture.session,
                &InboxArgs {
                    all: false,
                    limit: 10,
                },
            )
            .unwrap()
        } else {
            ops::read_message(&fixture.session, &read_args(&fixture)).unwrap()
        };
        assert_eq!(stage(&fixture), Delivery::Pending);
        let pending = outcome.pending_read().unwrap();
        ops::complete_read(&fixture.session, pending).unwrap();
        assert_eq!(stage(&fixture), Delivery::Read);
        let seen = fixture.session.store.local_seen().unwrap();
        ops::complete_read(&fixture.session, pending).unwrap();
        assert_eq!(fixture.session.store.local_seen().unwrap(), seen);
    }
}

#[test]
fn a_later_identity_cannot_complete_another_readers_selection() {
    let mut fixture = fixture();
    let outcome = ops::read_message(&fixture.session, &read_args(&fixture)).unwrap();
    fixture
        .session
        .bind(fixture.sender.name().as_str())
        .unwrap();
    ops::complete_read(&fixture.session, outcome.pending_read().unwrap()).unwrap_err();
    assert_eq!(stage(&fixture), Delivery::Pending);
}

#[test]
fn read_admission_refuses_an_unaddressed_identity_before_emitting_a_body() {
    let mut fixture = fixture();
    let stranger = register(&fixture.session.store, "stranger", Mode::Interactive);
    fixture.session.bind(stranger.name().as_str()).unwrap();
    ops::read_message(&fixture.session, &read_args(&fixture)).unwrap_err();
    assert_eq!(stage(&fixture), Delivery::Pending);
}

#[test]
fn explicit_acknowledgment_commits_without_response_delivery() {
    let fixture = fixture();
    ops::acknowledge(&fixture.session, &read_args(&fixture)).unwrap();
    assert_eq!(stage(&fixture), Delivery::Acknowledged);
}

#[test]
fn pending_read_counts_replenish_the_bounded_unread_window() {
    let fixture = fixture();
    fixture
        .session
        .store
        .write(|tx| {
            for _ in 0..super::super::MAX_LIMIT {
                tx.author(Priority::Ordinary, fixture.message.body().clone())?;
            }
            Ok(())
        })
        .unwrap();
    let outcome = ops::inbox(
        &fixture.session,
        &InboxArgs {
            all: false,
            limit: 1,
        },
    )
    .unwrap();
    assert_eq!(
        outcome.json().get("unread"),
        Some(&serde_json::json!(super::super::MAX_LIMIT))
    );
    assert_eq!(stage(&fixture), Delivery::Pending);
}
