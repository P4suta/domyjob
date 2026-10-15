use alloc::string::{String, ToString};
use alloc::vec::Vec;

use serde_json::{Value, json};

use crate::ingress;

pub const EVENTS: [(&str, &str); 4] = [
    ("SessionStart", "start"),
    ("PreToolUse", "pre"),
    ("PostToolUse", "post"),
    ("Stop", "stop"),
];
const STATUS: &str = "domyjob: chat lifecycle";
const TIMEOUT_SECONDS: u64 = 30;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexCommand {
    pub command: String,
    pub windows: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexHooks(String);

impl CodexHooks {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, thiserror::Error)]
pub enum HookError {
    #[error(transparent)]
    Json(#[from] ingress::JsonError),
    #[error("the hook configuration has an invalid {0}")]
    Shape(&'static str),
}

fn replace_owned<T>(groups: &mut Vec<T>, owned: impl Fn(&T) -> bool, replacement: T) {
    let length = compact_foreign(groups, owned);
    groups.truncate(length);
    groups.push(replacement);
}

fn compact_foreign<T>(groups: &mut [T], owned: impl Fn(&T) -> bool) -> usize {
    let mut retained = 0_usize;
    for read in 0..groups.len() {
        if groups.get(read).is_some_and(|group| !owned(group)) {
            groups.swap(retained, read);
            retained = retained.saturating_add(1);
        }
    }
    retained
}

fn owned_group(group: &Value) -> bool {
    let Some(handlers) = group.get("hooks").and_then(Value::as_array) else {
        return false;
    };
    if handlers.len() != 1 {
        return false;
    }
    handlers.first().is_some_and(|handler| {
        handler.get("type").and_then(Value::as_str) == Some("command")
            && handler.get("statusMessage").and_then(Value::as_str) == Some(STATUS)
            && handler.get("command").is_some_and(Value::is_string)
            && handler.get("commandWindows").is_some_and(Value::is_string)
    })
}

fn event_groups<'a>(document: &'a mut Value, event: &str) -> Result<&'a mut Vec<Value>, HookError> {
    let root = document.as_object_mut().ok_or(HookError::Shape("root"))?;
    let hooks = root.entry("hooks").or_insert_with(|| json!({}));
    let events = hooks.as_object_mut().ok_or(HookError::Shape("hooks map"))?;
    let groups = events.entry(event).or_insert_with(|| json!([]));
    let groups = groups
        .as_array_mut()
        .ok_or(HookError::Shape("event groups"))?;
    for group in groups.iter() {
        if !group.is_object()
            || group
                .get("hooks")
                .is_some_and(|handlers| !handlers.is_array())
        {
            return Err(HookError::Shape("matcher group"));
        }
    }
    Ok(groups)
}

pub fn codex_merge(document: &str, commands: &[CodexCommand; 4]) -> Result<CodexHooks, HookError> {
    if document.len() > ingress::MAX_FOREIGN_BYTES {
        return Err(ingress::JsonError::TooLarge {
            limit: ingress::MAX_FOREIGN_BYTES,
        }
        .into());
    }
    let mut parsed: Value = if document.trim().is_empty() {
        json!({})
    } else {
        ingress::foreign_json(document)?
    };
    let original = parsed.clone();
    for ((event, _), command) in EVENTS.into_iter().zip(commands) {
        let groups = event_groups(&mut parsed, event)?;
        replace_owned(
            groups,
            owned_group,
            json!({"hooks":[{
                "type":"command", "command":command.command, "commandWindows":command.windows,
                "statusMessage":STATUS, "timeout":TIMEOUT_SECONDS
            }]}),
        );
    }
    if parsed == original {
        return Ok(CodexHooks(String::from(document)));
    }
    let encoded = parsed.to_string();
    if encoded.len() > ingress::MAX_FOREIGN_BYTES {
        return Err(ingress::JsonError::TooLarge {
            limit: ingress::MAX_FOREIGN_BYTES,
        }
        .into());
    }
    Ok(CodexHooks(encoded))
}

#[must_use]
pub fn claude_settings(program: &str) -> Value {
    let hooks: serde_json::Map<String, Value> = EVENTS
        .into_iter()
        .map(|(event, phase)| {
            (
                String::from(event),
                json!([{"hooks":[{
                    "type":"command", "command":program,
                    "args":["chat","hook",phase,"--client","claude","--if-bound"],
                    "timeout":TIMEOUT_SECONDS
                }]}]),
            )
        })
        .collect();
    json!({"hooks":hooks})
}

#[cfg(kani)]
mod proofs {
    const MAX_GROUPS: usize = 4;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct Group {
        id: u8,
        owned: bool,
    }

    #[kani::proof]
    #[kani::unwind(6)]
    fn hook_updates_preserve_foreign_groups_in_order() {
        let length = usize::from(kani::any::<u8>());
        kani::assume(length <= MAX_GROUPS);
        let [first, second, third, fourth]: [bool; MAX_GROUPS] = kani::any();
        let original = [
            Group {
                id: 0,
                owned: first,
            },
            Group {
                id: 1,
                owned: second,
            },
            Group {
                id: 2,
                owned: third,
            },
            Group {
                id: 3,
                owned: fourth,
            },
        ];
        let mut groups = original;
        let retained =
            super::compact_foreign(groups.get_mut(..length).unwrap(), |group| group.owned);
        let mut previous = None;
        let prefix = groups.get(..retained).unwrap();
        for group in prefix {
            assert!(!group.owned);
            assert!(original.get(..length).unwrap().contains(group));
            assert!(previous.is_none_or(|id| id < group.id));
            previous = Some(group.id);
        }
        for group in original.iter().take(length) {
            assert_eq!(prefix.contains(group), !group.owned);
        }
        kani::cover!(length == 0);
        kani::cover!(original.iter().take(length).any(|group| group.owned));
        kani::cover!(original.iter().take(length).any(|group| !group.owned));
    }

    #[kani::proof]
    fn rejects_foreign_hook_loss() {
        let foreign = Group {
            id: 0,
            owned: false,
        };
        let mut groups = [foreign];
        let retained = super::compact_foreign(&mut groups, |group| group.owned);
        kani::assert(retained == 0, "NEGATIVE_HOOK_CONTROL");
    }
}
