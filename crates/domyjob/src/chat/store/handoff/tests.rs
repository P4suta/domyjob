use std::cell::Cell;

use domyjob_core::chat::card::Mode;
use domyjob_core::chat::event::Stamp;
use domyjob_core::chat::handoff::ReadRefusal;
use domyjob_core::chat::id::Origin;
use domyjob_core::chat::policy::Priority;

use super::*;
use crate::chat::store::fixtures::{assert_removed_after_drop, handoff_metadata, register, store};
use crate::chat::store::tables::EVENTS;

const CHAT_FORMAT_HEX_CHARS: usize = 16;

struct Fixture {
    store: super::super::Store,
    sender: AgentId,
    previous: AgentId,
    continuing: AgentId,
    descriptor: Descriptor,
    root: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let store = store(&root, "local");
        let sender = register(&store, "sender", Mode::Interactive);
        let previous = register(&store, "previous", Mode::Interactive);
        let continuing = register(&store, "continuing", Mode::Interactive);
        let descriptor = publish(&store, &sender, &previous, &continuing);
        Self {
            store,
            sender,
            previous,
            continuing,
            descriptor,
            root,
        }
    }

    fn body(&self) -> ReadBody {
        self.store
            .read(|transaction| {
                read(
                    transaction,
                    &self.continuing,
                    &self.descriptor,
                    ReadLimit::MAX,
                )
            })
            .unwrap()
    }

    fn observed(&self, reader: &AgentId) -> bool {
        self.store
            .read(|transaction| observed(transaction, reader, self.descriptor.id()))
            .unwrap()
    }
}

#[test]
fn fixture_closes_the_store_before_removing_its_directory() {
    let fixture = Fixture::new();
    let path = fixture.root.path().to_owned();
    assert_removed_after_drop(fixture, &path);
}

fn publish(
    store: &super::super::Store,
    sender: &AgentId,
    previous: &AgentId,
    continuing: &AgentId,
) -> Descriptor {
    let metadata = handoff_metadata(sender, previous, continuing);
    let text = Text::try_from("only the scoped handoff".to_owned()).unwrap();
    let event = store
        .write(|tx| {
            tx.author(
                Priority::Ordinary,
                Body::Handoff {
                    metadata: metadata.clone(),
                    text: text.clone(),
                    at: Stamp::from_unix_millis(1),
                },
            )
        })
        .unwrap();
    Descriptor::new(event.id().clone(), metadata, &text).unwrap()
}

#[test]
fn metadata_discovery_never_decodes_the_body_record() {
    let fixture = Fixture::new();
    let descriptor = &fixture.descriptor;
    fixture
        .store
        .write(|tx| {
            tx.transaction()
                .open_table(EVENTS)?
                .insert(event_key(descriptor.id()).as_str(), "invalid private body")?;
            Ok(())
        })
        .unwrap();
    let discovered = fixture
        .store
        .read(|read| {
            discover(
                read,
                &fixture.continuing,
                (
                    descriptor.metadata().project(),
                    descriptor.metadata().scope(),
                ),
                MAX_HANDOFFS,
            )
        })
        .unwrap();
    assert_eq!(discovered, vec![descriptor.clone()]);
    assert!(matches!(
        fixture.store.read(|transaction| read(
            transaction,
            &fixture.continuing,
            descriptor,
            ReadLimit::MAX
        )),
        Err(StoreError::Record(_))
    ));
}

#[test]
fn copied_metadata_headers_are_refused_before_discovery_or_body_fetch() {
    let fixture = Fixture::new();
    let first = &fixture.descriptor;
    let second = publish(
        &fixture.store,
        &fixture.sender,
        &fixture.previous,
        &fixture.continuing,
    );
    fixture
        .store
        .write(|tx| {
            tx.transaction()
                .open_table(HANDOFF_HEADERS)?
                .insert(event_key(first.id()).as_str(), encode(&second)?.as_str())?;
            tx.transaction()
                .open_table(EVENTS)?
                .insert(event_key(first.id()).as_str(), "invalid private body")?;
            Ok(())
        })
        .unwrap();
    assert!(matches!(
        fixture.store.read(|read| discover(
            read,
            &fixture.continuing,
            (first.metadata().project(), first.metadata().scope()),
            MAX_HANDOFFS,
        )),
        Err(StoreError::Corrupt("handoff header identity"))
    ));
    assert!(matches!(
        fixture.store.read(|transaction| read(
            transaction,
            &fixture.continuing,
            first,
            ReadLimit::MAX
        )),
        Err(StoreError::Corrupt("handoff header identity"))
    ));
}

#[test]
fn a_denied_reader_cannot_reach_the_body_port() {
    let fixture = Fixture::new();
    let descriptor = &fixture.descriptor;
    let stranger = AgentId::new(
        fixture.continuing.name().clone(),
        Origin::from_entropy([9; 16]),
    );
    let calls = Cell::new(0);
    let result = load_using(descriptor, &stranger, (descriptor, ReadLimit::MAX), |_id| {
        calls.set(calls.get() + 1);
        Ok(None)
    });
    assert!(matches!(
        result,
        Err(StoreError::Handoff(ReadRefusal::Reader))
    ));
    assert_eq!(calls.get(), 0);
    let discovered = fixture
        .store
        .read(|read| {
            discover(
                read,
                &stranger,
                (
                    descriptor.metadata().project(),
                    descriptor.metadata().scope(),
                ),
                MAX_HANDOFFS,
            )
        })
        .unwrap();
    let empty: [Descriptor; 0] = [];
    assert_eq!(discovered, empty);
}

#[test]
fn an_admitted_read_records_only_the_actual_reader_and_is_idempotent() {
    let fixture = Fixture::new();
    let body = fixture.body();
    assert_eq!(body.reader, fixture.continuing);
    assert_eq!(body.text.as_str(), "only the scoped handoff");
    assert!(!fixture.observed(&fixture.continuing));
    fixture.store.write(|tx| body.pending().commit(tx)).unwrap();
    let count = fixture.store.local_seen().unwrap();
    fixture.store.write(|tx| body.pending().commit(tx)).unwrap();
    assert_eq!(fixture.store.local_seen().unwrap(), count);
    assert!(fixture.observed(&fixture.continuing));
    assert!(!fixture.observed(&fixture.previous));
}

#[test]
fn an_aborted_observation_transaction_is_retryable_without_a_false_reader() {
    let fixture = Fixture::new();
    let body = fixture.body();
    let before = fixture.store.local_seen().unwrap();
    let result: Result<(), StoreError> = fixture.store.write(|tx| {
        body.pending().commit(tx)?;
        Err(StoreError::Corrupt("injected observation abort"))
    });
    assert!(matches!(
        result,
        Err(StoreError::Corrupt("injected observation abort"))
    ));
    assert_eq!(fixture.store.local_seen().unwrap(), before);
    assert!(!fixture.observed(&fixture.continuing));
    fixture.store.write(|tx| body.pending().commit(tx)).unwrap();
    assert!(fixture.observed(&fixture.continuing));
}

#[test]
fn all_previous_formats_preserve_identity_and_history_when_adding_handoff_indexes() {
    use super::super::tables::{HANDOFF_HEADERS, HANDOFF_READERS, HANDOFF_READS, META};
    use redb::ReadableTableMetadata;
    let root = tempfile::tempdir().unwrap();
    for (name, specimen) in [
        (
            "v1",
            include_bytes!("../../../../formats/chat-v1.txt").as_slice(),
        ),
        (
            "v2",
            include_bytes!("../../../../formats/chat-v2-notifications.txt").as_slice(),
        ),
    ] {
        let format: String = blake3::hash(specimen)
            .to_hex()
            .chars()
            .take(CHAT_FORMAT_HEX_CHARS)
            .collect();
        let original = store(&root, name);
        let agent = register(&original, "continuing", Mode::Interactive);
        let history = original.local_seen().unwrap();
        original
            .write(|tx| {
                tx.transaction()
                    .open_table(META)?
                    .insert("format", format.as_str())?;
                for table in [HANDOFF_HEADERS, HANDOFF_READERS, HANDOFF_READS] {
                    tx.transaction().delete_table(table)?;
                }
                Ok(())
            })
            .unwrap();
        let upgraded = store(&root, name);
        assert_eq!(upgraded.origin(), original.origin());
        assert_eq!(upgraded.local_seen().unwrap(), history);
        assert!(
            upgraded
                .read(|read| crate::chat::store::profile(read, &agent))
                .unwrap()
                .is_some()
        );
        upgraded
            .read(|read| {
                for table in [HANDOFF_HEADERS, HANDOFF_READERS, HANDOFF_READS] {
                    assert_eq!(read.table(table)?.len()?, 0);
                }
                Ok(())
            })
            .unwrap();
    }
}

#[test]
fn resetting_the_store_refuses_a_prepared_old_identity_observation() {
    let fixture = Fixture::new();
    let body = fixture.body();
    let replacement = super::super::Store::reset(&crate::layout::State::at(
        &fixture.root.path().join("local"),
    ))
    .unwrap();
    assert_ne!(fixture.store.origin(), replacement.origin());
    assert!(matches!(
        fixture.store.write(|tx| body.pending().commit(tx)),
        Err(StoreError::IdentityChanged)
    ));
    assert_eq!(replacement.local_seen().unwrap(), 0);
}
