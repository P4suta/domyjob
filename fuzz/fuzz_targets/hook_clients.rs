#![no_main]

use domyjob_core::hook_clients::{CodexCommand, EVENTS, codex_merge};
use domyjob_core::ingress;
use libfuzzer_sys::fuzz_target;
use serde_json::{Value, json};

const OWNER_STATUS: &str = "domyjob: chat lifecycle";

fn foreign(group: &Value) -> bool {
    let Some(handlers) = group.get("hooks").and_then(Value::as_array) else {
        return true;
    };
    handlers.len() != 1
        || !handlers.first().is_some_and(|handler| {
            handler.get("type") == Some(&json!("command"))
                && handler.get("statusMessage") == Some(&json!(OWNER_STATUS))
                && handler.get("command").is_some_and(Value::is_string)
                && handler.get("commandWindows").is_some_and(Value::is_string)
        })
}

fuzz_target!(|data: &[u8]| {
    let Ok(document) = std::str::from_utf8(data) else {
        return;
    };
    let commands = EVENTS.map(|(_, phase)| CodexCommand {
        command: format!("literal-{phase}"),
        windows: format!("windows-{phase}"),
    });
    let Ok(merged) = codex_merge(document, &commands) else {
        return;
    };
    assert_eq!(codex_merge(merged.as_str(), &commands).unwrap(), merged);
    let original: Value = if document.trim().is_empty() {
        json!({})
    } else {
        ingress::foreign_json(document).unwrap()
    };
    let after: Value = ingress::foreign_json(merged.as_str()).unwrap();
    for (name, value) in original.as_object().unwrap() {
        if name != "hooks" {
            assert_eq!(after.get(name), Some(value));
        }
    }
    let updated = after.get("hooks").unwrap();
    if let Some(events) = original.get("hooks").and_then(Value::as_object) {
        for (event, groups) in events {
            if EVENTS.iter().any(|(name, _)| *name == event) {
                let expected: Vec<_> = groups
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|group| foreign(group))
                    .collect();
                let actual = updated.get(event).unwrap().as_array().unwrap();
                assert_eq!(actual.len(), expected.len().saturating_add(1));
                assert!(actual.iter().take(expected.len()).eq(expected.into_iter()));
            } else {
                assert_eq!(updated.get(event), Some(groups));
            }
        }
    }
});
