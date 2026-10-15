use domyjob_core::hook_clients::{CodexCommand, EVENTS, codex_merge};
use domyjob_core::ingress;
use serde_json::{Value, json};

fn commands(program: &str) -> [CodexCommand; 4] {
    EVENTS.map(|(_, phase)| CodexCommand {
        command: format!("{program} chat hook {phase} --client codex --if-bound"),
        windows: format!("windows-{program}-{phase}"),
    })
}

#[test]
fn codex_setup_preserves_foreign_hooks_and_replaces_only_its_own_groups() {
    let original = json!({"description":"owner config", "hooks":{
        "SessionStart":[{"matcher":"resume", "hooks":[{"type":"command","command":"owner-start"}]}],
        "Stop":[{"hooks":[{"type":"command","command":"owner-stop"}]}],
        "PreCompact":[{"hooks":[{"type":"command","command":"owner-compact"}]}]
    }});
    let first = codex_merge(&original.to_string(), &commands("old")).unwrap();
    let second = codex_merge(first.as_str(), &commands("new")).unwrap();
    let value: Value = ingress::foreign_json(second.as_str()).unwrap();
    assert_eq!(value.get("description"), original.get("description"));
    let before = original.get("hooks").unwrap();
    let after = value.get("hooks").unwrap();
    assert_eq!(after.get("PreCompact"), before.get("PreCompact"));
    for event in ["SessionStart", "Stop"] {
        let groups = after.get(event).unwrap().as_array().unwrap();
        assert_eq!(groups.len(), 2);
        assert_eq!(
            groups.first(),
            before.get(event).unwrap().as_array().unwrap().first()
        );
    }
    assert!(!second.as_str().contains("old chat hook"));
    assert_eq!(
        codex_merge(second.as_str(), &commands("new")).unwrap(),
        second
    );
}

#[test]
fn malformed_hook_documents_are_refused_without_replacing_the_user_document() {
    for document in [
        "null",
        "[]",
        "{",
        "{\"hooks\":null}",
        "{\"hooks\":{\"Stop\":{}}}",
        "{\"hooks\":{\"Stop\":[null]}}",
    ] {
        codex_merge(document, &commands("domyjob")).unwrap_err();
    }
}

#[test]
fn whitespace_cannot_bypass_the_hook_document_byte_limit() {
    let oversized = " ".repeat(ingress::MAX_FOREIGN_BYTES.saturating_add(1));
    codex_merge(&oversized, &commands("domyjob")).unwrap_err();
}
