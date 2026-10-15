use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use domyjob_core::chat::card::Mode;
use domyjob_core::chat::event::{Body, Chain, Event, Intent, Members, Stamp};
use domyjob_core::chat::id::{AgentId, Conversation, Line, Text};
use domyjob_core::chat::notification::{Delivery, MILLIS_PER_SECOND, Notice, Question};
use domyjob_core::chat::policy::{Priority, audience};
use redb::backends::FileBackend;
use redb::{Database, StorageBackend};

use super::fixtures::{register, store};
use super::{Store, StoreError, Tx, tables};

const RECEIPT_FAILURE: &str = "injected receipt storage failure";

fn message(
    store: &Store,
    recipient: &AgentId,
    question: Option<Question>,
) -> Result<Event, StoreError> {
    let sender = register(store, "sender", Mode::Interactive);
    store.write(|tx| {
        tx.author(
            Priority::Ordinary,
            Body::Message {
                conversation: Conversation::direct(&sender, recipient)?,
                from: sender.name().clone(),
                text: Text::try_from("review".to_owned())?,
                audience: audience([&sender, recipient])?,
                intent: if question.is_some() {
                    Intent::Ask {
                        responder: recipient.clone(),
                        chain: Chain::default(),
                    }
                } else {
                    Intent::Send {}
                },
                at: Stamp::from_unix_millis(0),
                notice: Some(Notice {
                    recipients: Members::try_from(vec![recipient.clone()])?,
                    urgent: true,
                    subject: None,
                    question,
                }),
            },
        )
    })
}

#[test]
fn aborted_receipt_transactions_leave_every_stage_pending() {
    let root = tempfile::tempdir().unwrap();
    let local = store(&root, "local");
    let recipient = register(&local, "recipient", Mode::Interactive);
    let message = message(&local, &recipient, None).unwrap();
    let seen = local.local_seen().unwrap();
    for stage in [Delivery::Delivered, Delivery::Read, Delivery::Acknowledged] {
        let error = local
            .write(|tx| {
                tx.receipt(&recipient, &message, stage)?;
                Err::<(), _>(StoreError::Io(io::Error::other(RECEIPT_FAILURE)))
            })
            .unwrap_err();
        assert!(matches!(error, StoreError::Io(ref error) if error.to_string() == RECEIPT_FAILURE));
        let reopened = store(&root, "local");
        assert_eq!(reopened.local_seen().unwrap(), seen);
        assert_eq!(
            reopened
                .read(|read| super::delivery(read, &recipient, message.id()))
                .unwrap(),
            Delivery::Pending
        );
        assert_eq!(
            reopened
                .read(|read| super::urgent(read, &recipient))
                .unwrap(),
            [message.id().clone()]
        );
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FailurePoint {
    Write,
    DurabilitySync,
}

#[derive(Debug)]
struct FailingBackend {
    inner: FileBackend,
    armed: Arc<AtomicBool>,
    point: FailurePoint,
}

impl FailingBackend {
    fn check(&self, point: FailurePoint) -> io::Result<()> {
        if self.point == point && self.armed.load(Ordering::SeqCst) {
            Err(io::Error::new(io::ErrorKind::WriteZero, RECEIPT_FAILURE))
        } else {
            Ok(())
        }
    }
}

impl StorageBackend for FailingBackend {
    fn len(&self) -> io::Result<u64> {
        self.inner.len()
    }
    fn read(&self, offset: u64, out: &mut [u8]) -> io::Result<()> {
        self.inner.read(offset, out)
    }
    fn set_len(&self, len: u64) -> io::Result<()> {
        self.inner.set_len(len)
    }
    fn sync_data(&self) -> io::Result<()> {
        self.check(FailurePoint::DurabilitySync)?;
        self.inner.sync_data()
    }
    fn write(&self, offset: u64, data: &[u8]) -> io::Result<()> {
        self.check(FailurePoint::Write)?;
        self.inner.write(offset, data)
    }
}

fn fail_receipt_commit(
    local: &Store,
    (recipient, message): (&AgentId, &Event),
    stage: Delivery,
    point: FailurePoint,
) -> Result<redb::CommitError, StoreError> {
    let armed = Arc::new(AtomicBool::new(false));
    let file = crate::state_io::open_lock(&local.paths().database())?;
    let database = Database::builder().create_with_backend(FailingBackend {
        inner: FileBackend::new(file)?,
        armed: Arc::clone(&armed),
        point,
    })?;
    let transaction = database.begin_write()?;
    Tx::new(&transaction, local.origin()).receipt(recipient, message, stage)?;
    armed.store(true, Ordering::SeqCst);
    let error = transaction.commit().err().ok_or(StoreError::Corrupt(
        "the injected commit unexpectedly succeeded",
    ))?;
    drop(database);
    Ok(error)
}

#[test]
fn receipt_commit_errors_reconcile_atomic_state_and_retry_without_duplicate_events() {
    for point in [FailurePoint::Write, FailurePoint::DurabilitySync] {
        for stage in [Delivery::Delivered, Delivery::Read, Delivery::Acknowledged] {
            let root = tempfile::tempdir().unwrap();
            let local = store(&root, "local");
            let recipient = register(&local, "recipient", Mode::Interactive);
            let message = message(&local, &recipient, None).unwrap();
            let seen = local.local_seen().unwrap();
            let error = fail_receipt_commit(&local, (&recipient, &message), stage, point).unwrap();
            assert!(
                matches!(error, redb::CommitError::Storage(redb::StorageError::Io(ref error)) if error.to_string() == RECEIPT_FAILURE)
            );
            let reopened = store(&root, "local");
            let observed = reopened
                .read(|read| super::delivery(read, &recipient, message.id()))
                .unwrap();
            match point {
                FailurePoint::Write => assert_eq!(observed, Delivery::Pending),
                FailurePoint::DurabilitySync => {
                    assert!(observed == Delivery::Pending || observed == stage);
                }
            }
            let committed = observed == stage;
            assert_eq!(reopened.local_seen().unwrap(), seen + u64::from(committed));
            let expected_urgent = if committed && stage == Delivery::Acknowledged {
                Vec::new()
            } else {
                vec![message.id().clone()]
            };
            assert_eq!(
                reopened
                    .read(|read| super::urgent(read, &recipient))
                    .unwrap(),
                expected_urgent
            );
            reopened
                .write(|tx| tx.receipt(&recipient, &message, stage))
                .unwrap();
            assert_eq!(reopened.local_seen().unwrap(), seen + 1);
            assert_eq!(
                reopened
                    .read(|read| super::delivery(read, &recipient, message.id()))
                    .unwrap(),
                stage
            );
        }
    }
}

fn replace_generation(store: &Store, generation: u64) -> Result<u64, StoreError> {
    store.write(|tx| {
        let mut counters = tx.transaction().open_table(tables::COUNTERS)?;
        let previous = tables::get_number(&counters, "generation")?;
        counters.insert("generation", generation)?;
        Ok(previous)
    })
}

#[test]
fn interrupted_fallback_rolls_back_and_replays_once_after_reopening() {
    let root = tempfile::tempdir().unwrap();
    let local = store(&root, "local");
    let recipient = register(&local, "recipient", Mode::Interactive);
    let fallback = register(&local, "fallback", Mode::Interactive);
    let question = Question::new(
        Line::try_from("Approve or explain".to_owned()).unwrap(),
        1,
        fallback.clone(),
    )
    .unwrap();
    let message = message(&local, &recipient, Some(question)).unwrap();
    let seen = local.local_seen().unwrap();
    let generation = replace_generation(&local, u64::MAX).unwrap();
    let error = local
        .escalate(Stamp::from_unix_millis(MILLIS_PER_SECOND))
        .unwrap_err();
    assert!(matches!(
        error,
        StoreError::Corrupt("chat counter overflow")
    ));
    let reopened = store(&root, "local");
    assert_eq!(reopened.local_seen().unwrap(), seen);
    assert!(
        reopened
            .read(|read| super::escalation(read, message.id()))
            .unwrap()
            .is_none()
    );
    assert_eq!(
        reopened
            .read(|read| super::urgent(read, &fallback))
            .unwrap(),
        Vec::new()
    );
    replace_generation(&reopened, generation).unwrap();
    assert_eq!(
        reopened
            .escalate(Stamp::from_unix_millis(MILLIS_PER_SECOND))
            .unwrap(),
        1
    );
    let forwarded = reopened
        .read(|read| super::escalation(read, message.id()))
        .unwrap()
        .unwrap();
    let restarted = store(&root, "local");
    assert_eq!(
        restarted
            .escalate(Stamp::from_unix_millis(MILLIS_PER_SECOND))
            .unwrap(),
        0
    );
    assert_eq!(
        restarted
            .read(|read| super::escalation(read, message.id()))
            .unwrap(),
        Some(forwarded.clone())
    );
    assert_eq!(
        restarted
            .read(|read| super::urgent(read, &fallback))
            .unwrap(),
        [forwarded]
    );
}
