use super::super::fixtures::{register, store};
use super::{Request, observe};
use crate::layout::State;
use domyjob_core::chat::card::Mode;
use domyjob_core::chat::event::{Body, Intent, Stamp};
use domyjob_core::chat::id::{AgentId, Conversation, Line, Origin, Text};
use domyjob_core::chat::notification::{Delivery, Notice};
use domyjob_core::chat::policy::{Priority, audience};

const LEGACY_SCHEMA: &[u8] =
    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/formats/chat-v1.txt"));

fn notice(recipient: &AgentId) -> Notice {
    Notice {
        recipients: domyjob_core::chat::event::Members::try_from(vec![recipient.clone()]).unwrap(),
        urgent: false,
        subject: None,
        question: None,
    }
}

fn absent(state: &State) {
    assert!(
        matches!(std::fs::symlink_metadata(state.root()), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
    );
}

fn pair(root: &tempfile::TempDir) -> (super::super::Store, AgentId, AgentId, Conversation) {
    let store = store(root, "local");
    let alice = register(&store, "alice", Mode::Interactive);
    let bob = register(&store, "bob", Mode::Interactive);
    let conversation = Conversation::direct(&alice, &bob).unwrap();
    (store, alice, bob, conversation)
}

fn message(
    store: &super::super::Store,
    participants: (&AgentId, &AgentId, &Conversation),
    text: &str,
    notice: Option<Notice>,
) -> domyjob_core::chat::event::Event {
    store
        .write(|tx| {
            tx.author(
                Priority::Ordinary,
                Body::Message {
                    conversation: participants.2.clone(),
                    from: participants.0.name().clone(),
                    text: Text::try_from(text.to_owned()).unwrap(),
                    audience: audience([participants.0, participants.1]).unwrap(),
                    intent: Intent::Send {},
                    at: Stamp::from_unix_millis(1),
                    notice,
                },
            )
        })
        .unwrap()
}

#[test]
fn absent_state_is_not_initialized_by_passive_observation() {
    let root = tempfile::tempdir().unwrap();
    let state = State::at(&root.path().join("absent"));
    let snapshot = observe(&state, &Request::default()).unwrap();
    assert!(snapshot.agents.is_empty());
    absent(&state);
}

#[test]
fn an_explicit_viewing_identity_cannot_silently_fall_back_on_absent_state() {
    let root = tempfile::tempdir().unwrap();
    let state = State::at(&root.path().join("absent"));
    let result = observe(
        &state,
        &Request {
            initial_actor: Some("alice".to_owned()),
            ..Request::default()
        },
    );
    assert!(matches!(
        result.unwrap_err(),
        super::StoreError::Corrupt("the requested viewing state is absent")
    ));
    absent(&state);
}

#[test]
fn passive_observation_preserves_actual_database_bytes_and_recipient_receipts() {
    let root = tempfile::tempdir().unwrap();
    let (store, alice, bob, conversation) = pair(&root);
    let message = message(
        &store,
        (&alice, &bob, &conversation),
        "private 界",
        Some(notice(&bob)),
    );
    store
        .write(|tx| tx.receipt(&bob, &message, Delivery::Delivered))
        .unwrap();
    let before = database_bytes(&store.paths().database());
    let state = State::at(&root.path().join("local"));
    let request = Request {
        actor: Some(bob.clone()),
        conversation: Some(conversation),
        ..Request::default()
    };
    for _ in 0..3 {
        let snapshot = observe(&state, &request).unwrap();
        assert_eq!(snapshot.messages.len(), 1);
        assert_eq!(snapshot.messages.first().unwrap().text, "private 界");
        assert_eq!(
            snapshot.messages.first().unwrap().recipients,
            [(bob.clone(), Delivery::Delivered)]
        );
    }
    assert_eq!(database_bytes(&store.paths().database()), before);
    assert_eq!(
        store
            .read(|read| super::super::delivery(read, &bob, message.id()))
            .unwrap(),
        Delivery::Delivered
    );
}

#[test]
fn direct_chat_labels_identify_the_viewers_counterpart_and_preserve_canonical_identity() {
    let root = tempfile::tempdir().unwrap();
    let (store, alice, bob, conversation) = pair(&root);
    let mut card = store
        .read(|read| super::super::profile(read, &bob))
        .unwrap()
        .unwrap();
    card.display_name = Line::try_from("Builder Bob".to_owned()).unwrap();
    store
        .write(|tx| {
            tx.author(
                Priority::Ordinary,
                Body::Profile {
                    agent: bob.name().clone(),
                    card: Box::new(card),
                },
            )
        })
        .unwrap();
    message(&store, (&alice, &bob, &conversation), "hello", None);
    let before = database_bytes(&store.paths().database());
    for (actor, expected) in [(alice, "Builder Bob"), (bob, "alice")] {
        let observed = observe(
            &State::at(&root.path().join("local")),
            &Request {
                actor: Some(actor),
                ..Request::default()
            },
        )
        .unwrap();
        let chat = observed.chats.first().unwrap();
        assert_eq!(chat.label, expected);
        assert_eq!(chat.id, conversation);
    }
    assert_eq!(database_bytes(&store.paths().database()), before);
}

fn database_bytes(path: &std::path::Path) -> Vec<u8> {
    const MAX_FIXTURE_DATABASE_BYTES: usize = 16_777_216;
    crate::bounded::read(
        crate::state_io::open_read(path).unwrap().unwrap(),
        MAX_FIXTURE_DATABASE_BYTES,
    )
    .unwrap()
    .unwrap()
}

fn set_schema(
    store: &super::super::Store,
    specimen: &[u8],
    missing: &[super::super::tables::TextTable],
) {
    let format: String = blake3::hash(specimen)
        .to_hex()
        .chars()
        .take(crate::formats::chat().len())
        .collect();
    let database = super::super::open_database(store.paths()).unwrap();
    let write = database.begin_write().unwrap();
    write
        .open_table(super::super::tables::META)
        .unwrap()
        .insert("format", format.as_str())
        .unwrap();
    for table in missing {
        assert!(write.delete_table(*table).unwrap());
    }
    write.commit().unwrap();
}

fn assert_missing_table_without_writes(
    store: &super::super::Store,
    state: &State,
    table: super::super::tables::TextTable,
) {
    let before = database_bytes(&store.paths().database());
    assert!(matches!(
        observe(state, &Request::default()),
        Err(super::StoreError::Table(redb::TableError::TableDoesNotExist(found)))
            if found == redb::TableHandle::name(&table)
    ));
    assert_eq!(database_bytes(&store.paths().database()), before);
}

#[test]
fn legacy_observation_reads_history_without_notification_tables_or_database_writes() {
    use super::super::tables::{ACTIVITY, DELIVERY, ESCALATIONS, URGENT};

    let root = tempfile::tempdir().unwrap();
    let (store, alice, bob, conversation) = pair(&root);
    let sent = message(
        &store,
        (&alice, &bob, &conversation),
        "legacy history",
        None,
    );
    set_schema(
        &store,
        LEGACY_SCHEMA,
        &[ACTIVITY, DELIVERY, ESCALATIONS, URGENT],
    );
    let before = database_bytes(&store.paths().database());
    let state = State::at(&root.path().join("local"));
    for request in [
        Request::default(),
        Request {
            actor: Some(bob),
            conversation: Some(conversation),
            ..Request::default()
        },
    ] {
        let observed = observe(&state, &request).unwrap();
        assert_eq!(observed.agents.len(), 2);
        assert!(observed.agents.iter().all(|agent| agent.activity.is_none()));
        if request.actor.is_some() {
            assert_eq!(observed.messages.len(), 1);
            let found = observed.messages.first().unwrap();
            assert_eq!(found.id, *sent.id());
            assert_eq!(found.text, "legacy history");
            assert_eq!(found.recipients, Vec::<(AgentId, Delivery)>::new());
        }
        assert_eq!(database_bytes(&store.paths().database()), before);
    }
}

#[test]
fn notification_formats_refuse_missing_notification_tables_without_writing() {
    use super::super::tables::{ACTIVITY, DELIVERY};

    let schemas: &[&[u8]] = &[
        include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/formats/chat.txt")),
        include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/formats/chat-v2-notifications.txt"
        )),
    ];
    for schema in schemas {
        for table in [ACTIVITY, DELIVERY] {
            let root = tempfile::tempdir().unwrap();
            let store = store(&root, "local");
            set_schema(&store, schema, &[table]);
            assert_missing_table_without_writes(
                &store,
                &State::at(&root.path().join("local")),
                table,
            );
        }
    }
}

#[test]
fn legacy_observation_refuses_missing_baseline_tables() {
    use super::super::tables::{MACHINES, PEERS, PROFILES};

    for table in [PROFILES, MACHINES, PEERS] {
        let root = tempfile::tempdir().unwrap();
        let store = store(&root, "local");
        set_schema(&store, LEGACY_SCHEMA, &[table]);
        assert_missing_table_without_writes(&store, &State::at(&root.path().join("local")), table);
    }
}

#[test]
fn legacy_observation_preserves_notification_table_type_errors() {
    use super::super::tables::{ACTIVITY, DELIVERY};

    for table in [ACTIVITY, DELIVERY] {
        let root = tempfile::tempdir().unwrap();
        let store = store(&root, "local");
        set_schema(&store, LEGACY_SCHEMA, &[table]);
        let name = redb::TableHandle::name(&table);
        let database = super::super::open_database(store.paths()).unwrap();
        let write = database.begin_write().unwrap();
        drop(
            write
                .open_table(redb::TableDefinition::<&str, u64>::new(name))
                .unwrap(),
        );
        write.commit().unwrap();
        drop(database);
        let before = database_bytes(&store.paths().database());
        assert!(matches!(
            observe(&State::at(&root.path().join("local")), &Request::default()),
            Err(super::StoreError::Table(redb::TableError::TableTypeMismatch { table: found, .. }))
                if found == name
        ));
        assert_eq!(database_bytes(&store.paths().database()), before);
    }
}

#[test]
fn legacy_observation_refuses_corrupt_or_unavailable_notification_records() {
    use super::super::tables::{ACTIVITY, DELIVERY, delivery_key};

    for (table, record) in [
        (ACTIVITY, Some("{")),
        (DELIVERY, Some("{")),
        (DELIVERY, None),
    ] {
        let root = tempfile::tempdir().unwrap();
        let (store, alice, bob, conversation) = pair(&root);
        let sent = message(
            &store,
            (&alice, &bob, &conversation),
            "notification",
            Some(notice(&bob)),
        );
        set_schema(&store, LEGACY_SCHEMA, &[]);
        let key = if redb::TableHandle::name(&table) == redb::TableHandle::name(&ACTIVITY) {
            bob.to_string()
        } else {
            delivery_key(&bob, sent.id())
        };
        let database = super::super::open_database(store.paths()).unwrap();
        let write = database.begin_write().unwrap();
        if let Some(record) = record {
            write
                .open_table(table)
                .unwrap()
                .insert(key.as_str(), record)
                .unwrap();
        } else {
            assert!(write.delete_table(table).unwrap());
        }
        write.commit().unwrap();
        drop(database);
        let before = database_bytes(&store.paths().database());
        let result = observe(
            &State::at(&root.path().join("local")),
            &Request {
                actor: Some(bob),
                conversation: Some(conversation),
                ..Request::default()
            },
        );
        if record.is_some() {
            assert!(matches!(result, Err(super::StoreError::Record(_))));
        } else {
            assert!(matches!(
                result,
                Err(super::StoreError::Corrupt(
                    "notification receipt table is absent"
                ))
            ));
        }
        assert_eq!(database_bytes(&store.paths().database()), before);
    }
}

#[test]
fn foreign_identity_and_unrelated_conversation_are_refused() {
    let root = tempfile::tempdir().unwrap();
    let (_store, alice, bob, _conversation) = pair(&root);
    let foreign = AgentId::new(alice.name().clone(), Origin::from_entropy([7; 16]));
    let state = State::at(&root.path().join("local"));
    let foreign_error = observe(
        &state,
        &Request {
            actor: Some(foreign),
            ..Request::default()
        },
    )
    .unwrap_err();
    assert!(matches!(
        foreign_error,
        super::StoreError::Corrupt("the viewing identity is not a current local agent")
    ));
    let conversation_error = observe(
        &state,
        &Request {
            actor: Some(alice.clone()),
            conversation: Some(Conversation::direct(&alice, &bob).unwrap()),
            ..Request::default()
        },
    )
    .unwrap_err();
    assert!(matches!(
        conversation_error,
        super::StoreError::Corrupt("the viewing identity cannot access this conversation")
    ));
}

#[test]
fn a_passive_view_does_not_initialize_an_existing_empty_database() {
    let root = tempfile::tempdir().unwrap();
    let store = store(&root, "local");
    crate::state_io::write_bytes(&store.paths().database(), b"").unwrap();
    let state = State::at(&root.path().join("local"));
    let _error = observe(&state, &Request::default()).unwrap_err();
    assert_eq!(
        crate::state_io::read_bytes(&store.paths().database())
            .unwrap()
            .unwrap(),
        b""
    );
}

#[test]
fn the_exact_registered_viewer_is_admitted_beyond_the_bounded_directory_page() {
    use domyjob_core::chat::id::AgentName;
    use domyjob_core::dashboard::MAX_ROWS;

    let root = tempfile::tempdir().unwrap();
    let store = store(&root, "local");
    let viewer = register(&store, "zz-viewer", Mode::Interactive);
    let card = store
        .read(|read| super::super::profile(read, &viewer))
        .unwrap()
        .unwrap();
    store
        .write(|tx| {
            for index in 0..MAX_ROWS {
                tx.author(
                    Priority::Ordinary,
                    Body::Profile {
                        agent: AgentName::try_from(format!("a{index}"))?,
                        card: Box::new(card.clone()),
                    },
                )?;
            }
            Ok(())
        })
        .unwrap();
    let state = State::at(&root.path().join("local"));
    for request in [
        Request {
            actor: Some(viewer.clone()),
            ..Request::default()
        },
        Request {
            initial_actor: Some(viewer.name().to_string()),
            ..Request::default()
        },
    ] {
        let observed = observe(&state, &request).unwrap();
        assert_eq!(observed.actor, Some(viewer.clone()));
        assert!(observed.agents.len() <= MAX_ROWS);
    }
}
