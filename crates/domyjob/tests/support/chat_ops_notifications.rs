use domyjob_core::chat::card::Mode;
use domyjob_core::chat::id::Invalid;

use crate::chat::args::{NotificationArgs, RoomArgs, SendArgs};
use crate::chat::ops::{self, OpsError, Session};
use crate::chat::store::StoreError;
use crate::chat::store::fixtures::{register, store};

#[test]
fn empty_recipient_routes_cannot_discard_invalid_question_arguments() {
    let root = tempfile::tempdir().unwrap();
    let local = store(&root, "local");
    let sender = register(&local, "sender", Mode::Interactive);
    let mut session = Session::with_store(local);
    session.bind(sender.name().as_str()).unwrap();
    ops::create_room(
        &session,
        &RoomArgs {
            name: "notes".to_owned(),
            members: Vec::new(),
            topic: None,
        },
    )
    .unwrap();
    ops::send(
        &session,
        &SendArgs {
            target: "notes".to_owned(),
            text: "ordinary note".to_owned(),
            notification: NotificationArgs::default(),
        },
    )
    .unwrap();
    let seen = session.store.local_seen().unwrap();
    for (arguments, expected) in [
        (
            serde_json::json!({"expected":"Approve"}),
            "expected and fallback must be supplied together",
        ),
        (
            serde_json::json!({"fallback":"sender"}),
            "expected and fallback must be supplied together",
        ),
        (
            serde_json::json!({"expected":"Approve","fallback":"sender"}),
            "expected answers and fallback escalation require chat ask",
        ),
    ] {
        let args: SendArgs = domyjob_core::ingress::value(serde_json::json!({
            "target":"notes", "text":"question", "notification":arguments
        }))
        .unwrap();
        let error = ops::send(&session, &args).unwrap_err();
        assert!(
            matches!(error, OpsError::Store(StoreError::Invalid(Invalid(reason))) if reason == expected)
        );
        assert_eq!(session.store.local_seen().unwrap(), seen);
    }
}
