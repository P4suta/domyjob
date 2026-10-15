use domyjob_core::chat::card::Mode;
use domyjob_core::chat::event::{Intent, Stamp};
use domyjob_core::chat::id::{Conversation, EventId, Text};
use domyjob_core::chat::policy::audience;
use serde_json::json;

use super::*;
use crate::chat::args::MessageArgs;
use crate::chat::store::fixtures::{assert_removed_after_drop, register, store};
use crate::chat::store::{Store, handoff};

const BODY: &str = "continuation for issue 47";
const PRIVATE_BODY: &str = "unrelated private message";

pub(crate) struct Fixture {
    pub(crate) sender: Session,
    pub(crate) continuing: Session,
    pub(crate) descriptor: Descriptor,
    private: EventId,
    root: tempfile::TempDir,
}

impl Fixture {
    pub(crate) fn observed(&self) -> bool {
        self.continuing
            .store
            .read(|read| {
                handoff::observed(read, self.continuing.me().unwrap(), self.descriptor.id())
            })
            .unwrap()
    }
}

fn exchange(first: &Store, second: &Store) {
    let offer = first.offer(second.origin()).unwrap().unwrap();
    let answer = second.respond(&offer).unwrap().unwrap();
    assert!(!first.accept(&offer, answer).unwrap().unwrap().more);
}

pub(crate) fn fixture() -> Fixture {
    let root = tempfile::tempdir().unwrap();
    let first = store(&root, "first");
    let second = store(&root, "second");
    let sender = register(&first, "sender", Mode::Interactive);
    let previous = register(&first, "previous", Mode::Interactive);
    let continuing = register(&second, "continuing", Mode::Interactive);
    let private = first
        .write(|tx| {
            tx.author(
                Priority::Ordinary,
                Body::Message {
                    from: sender.name().clone(),
                    conversation: Conversation::direct(&sender, &previous)?,
                    audience: audience([&sender, &previous])?,
                    text: Text::try_from(PRIVATE_BODY.to_owned())?,
                    intent: Intent::Send {},
                    at: Stamp::from_unix_millis(1),
                    notice: None,
                },
            )
        })
        .unwrap();
    let mut sender_session = Session::with_store(first);
    sender_session.bind("sender").unwrap();
    let mut reader_session = Session::with_store(second);
    reader_session.bind("continuing").unwrap();
    let args = domyjob_core::ingress::value(json!({
        "project":"owner/project", "scope":"issue-47",
        "intended_recipient":{"kind":"agent","agent":previous},
        "permitted_readers":[previous,continuing],"text":BODY
    }))
    .unwrap();
    let Outcome::HandoffCreated(descriptor) = create_handoff(&sender_session, &args).unwrap()
    else {
        panic!("handoff creation outcome");
    };
    exchange(&sender_session.store, &reader_session.store);
    Fixture {
        sender: sender_session,
        continuing: reader_session,
        descriptor,
        private: private.id().clone(),
        root,
    }
}

#[test]
fn fixture_closes_sessions_before_removing_their_directory() {
    let fixture = fixture();
    let path = fixture.root.path().to_owned();
    assert_removed_after_drop(fixture, &path);
}

pub(crate) fn read_args(descriptor: &Descriptor) -> HandoffReadArgs {
    domyjob_core::ingress::value(json!({"descriptor":descriptor,"max_bytes":BODY.len()})).unwrap()
}

#[test]
fn a_continuing_identity_reads_one_handoff_without_the_previous_inbox() {
    let mut fixture = fixture();
    let list: HandoffListArgs = domyjob_core::ingress::value(
        json!({"project":"owner/project","scope":"issue-47","limit":1}),
    )
    .unwrap();
    let metadata = handoffs(&fixture.continuing, &list).unwrap();
    assert!(!metadata.json().to_string().contains(BODY));
    assert!(!metadata.json().to_string().contains(PRIVATE_BODY));
    let Outcome::HandoffMetadata {
        reader,
        descriptors,
    } = metadata
    else {
        panic!("metadata outcome");
    };
    assert_eq!(&reader, fixture.continuing.me().unwrap());
    assert_eq!(descriptors, vec![fixture.descriptor.clone()]);
    assert!(matches!(
        crate::chat::ops::read_message(
            &fixture.continuing,
            &MessageArgs {
                message: fixture.private.to_string(),
                timeout: 0
            }
        ),
        Err(OpsError::Usage("a full-body read addresses a message"))
    ));
    assert!(matches!(
        fixture.continuing.bind("previous"),
        Err(OpsError::NotLocal(_))
    ));
    assert_eq!(
        fixture.continuing.me().unwrap().name().as_str(),
        "continuing"
    );
    let outcome = read_handoff(&fixture.continuing, &read_args(&fixture.descriptor)).unwrap();
    assert_eq!(outcome.json().get("text"), Some(&json!(BODY)));
    assert_eq!(outcome.json().get("reader"), Some(&json!(reader)));
    observe_handoff(&fixture.continuing, outcome.pending_handoff().unwrap()).unwrap();
    exchange(&fixture.continuing.store, &fixture.sender.store);
    assert!(
        fixture
            .sender
            .store
            .read(|transaction| handoff::observed(transaction, &reader, fixture.descriptor.id()))
            .unwrap()
    );
}

#[test]
fn an_ungranted_local_agent_cannot_discover_or_read_the_handoff() {
    let fixture = fixture();
    register(&fixture.continuing.store, "unrelated", Mode::Interactive);
    let mut stranger = Session::with_store(fixture.continuing.store.clone());
    stranger.bind("unrelated").unwrap();
    let list: HandoffListArgs =
        domyjob_core::ingress::value(json!({"project":"owner/project","scope":"issue-47"}))
            .unwrap();
    let Outcome::HandoffMetadata { descriptors, .. } = handoffs(&stranger, &list).unwrap() else {
        panic!("metadata outcome");
    };
    let empty: [Descriptor; 0] = [];
    assert_eq!(descriptors, empty);
    assert!(matches!(
        read_handoff(&stranger, &read_args(&fixture.descriptor)),
        Err(OpsError::Store(crate::chat::store::StoreError::Handoff(
            domyjob_core::chat::handoff::ReadRefusal::Reader
        )))
    ));
}

#[test]
fn a_foreign_peer_receives_only_a_placeholder() {
    let fixture = fixture();
    let root = tempfile::tempdir().unwrap();
    let foreign = store(&root, "foreign");
    exchange(&fixture.sender.store, &foreign);
    let event = foreign
        .read(|read| crate::chat::store::event(read, fixture.descriptor.id()))
        .unwrap()
        .unwrap();
    assert_eq!(event.body(), &Body::Omitted {});
}
