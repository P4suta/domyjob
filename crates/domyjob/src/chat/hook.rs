use domyjob_core::chat::event::{Body, Event};
use domyjob_core::chat::id::{AgentId, EventId, Line};
use domyjob_core::chat::notification::{
    Activity, ActivityState, Delivery, MAX_CONTEXT_BYTES, MAX_CONTEXT_MESSAGES, MAX_SUMMARY_BYTES,
    StopDecision, stop_decision, summary_fits,
};
use domyjob_core::chat::policy::Priority;
use serde::Serialize;
use serde_json::{Value, json};

use super::args::{HookArgs, HookPhase};
use super::store::{self, Store, StoreError};

const MAX_METADATA_BYTES: usize = 64;
const MAX_IDENTITY_BYTES: usize = 48;
const MIN_PREVIEW_BYTES: usize = 64;

#[derive(Debug, Serialize)]
pub(crate) struct Prepared {
    pub(crate) agent: AgentId,
    pub(crate) context: String,
    pub(crate) messages: Vec<EventId>,
    pub(crate) pending_urgent: usize,
    pub(crate) block_stop: bool,
}

impl Prepared {
    pub(crate) fn output(&self, phase: HookPhase) -> Value {
        let event = match phase {
            HookPhase::Start => "SessionStart",
            HookPhase::Pre => "PreToolUse",
            HookPhase::Post => "PostToolUse",
            HookPhase::Stop => "Stop",
        };
        let details = json!({"hookEventName": event, "additionalContext": self.context});
        if self.block_stop {
            let reason = format!(
                "{} urgent chat messages require a reply or `domyjob chat acknowledge MESSAGE_ID`. {}",
                self.pending_urgent, self.context
            );
            json!({"decision": "block", "reason": reason, "hookSpecificOutput": details})
        } else {
            json!({"hookSpecificOutput": details})
        }
    }
}

fn prefix(text: &str, limit: usize) -> &str {
    let end = text.floor_char_boundary(limit.min(text.len()));
    text.get(..end).unwrap_or_default()
}

fn summary(event: &Event) -> String {
    let Body::Message {
        conversation,
        text,
        notice,
        at,
        ..
    } = event.body()
    else {
        return String::new();
    };
    let from = event
        .author()
        .map_or_else(String::new, |author| author.to_string());
    let from = prefix(&from, MAX_IDENTITY_BYTES);
    let conversation = conversation.to_string();
    let conversation = prefix(&conversation, MAX_IDENTITY_BYTES);
    let subject = notice
        .as_ref()
        .and_then(|notice| notice.subject.as_ref())
        .map_or("message", Line::as_str);
    let subject = prefix(subject, MAX_METADATA_BYTES);
    let priority = if notice.as_ref().is_some_and(|notice| notice.urgent) {
        "urgent"
    } else {
        "normal"
    };
    let expectation = notice
        .as_ref()
        .and_then(|notice| notice.question.as_ref())
        .map_or_else(String::new, |question| {
            format!(
                " Expected: {} (waiting {}s).",
                prefix(question.expected().as_str(), MAX_METADATA_BYTES),
                crate::platform::clock::stamp()
                    .millis()
                    .saturating_sub(at.millis())
                    / domyjob_core::chat::notification::MILLIS_PER_SECOND
            )
        });
    let heading = format!(
        "{} from {from} in {conversation} [{priority}] {subject}.{expectation} ",
        event.id()
    );
    let suffix = format!(" [read full body: domyjob chat read {}]\n", event.id());
    let heading = prefix(
        &heading,
        MAX_SUMMARY_BYTES
            .saturating_sub(suffix.len())
            .saturating_sub(MIN_PREVIEW_BYTES),
    );
    let budget = MAX_SUMMARY_BYTES
        .saturating_sub(heading.len())
        .saturating_sub(suffix.len());
    let body = domyjob_core::domain::terminal_text(text.as_str()).replace('\n', " ");
    let preview = prefix(&body, budget);
    format!("{heading}{preview}{suffix}")
}

pub(crate) fn prepare(
    store: &Store,
    agent: &AgentId,
    args: &HookArgs,
) -> Result<Prepared, StoreError> {
    let now = crate::platform::clock::stamp();
    store.escalate(now)?;
    store.write(|tx| {
        if matches!(args.phase, HookPhase::Pre) {
            for request in store::delivered_to(tx.transaction(), agent)? {
                if let Some(message) = store::event(tx.transaction(), &request)? {
                    tx.receipt(agent, &message, Delivery::Read)?;
                }
            }
        }
        let activity = Activity {
            state: if matches!(args.phase, HookPhase::Stop) {
                ActivityState::Waiting
            } else {
                ActivityState::Working
            },
            at: now,
            status: match &args.status {
                Some(status) if status.is_empty() => None,
                Some(status) => Some(Line::try_from(status.clone())?),
                None => {
                    store::activity(tx.transaction(), agent)?.and_then(|activity| activity.status)
                }
            },
        };
        tx.author(
            Priority::Ending,
            Body::Activity {
                agent: agent.name().clone(),
                activity,
            },
        )?;
        Ok(())
    })?;
    select_context(store, agent, args)
}

fn select_context(store: &Store, agent: &AgentId, args: &HookArgs) -> Result<Prepared, StoreError> {
    store.read(|read| {
        let mut context = format!("Chat identity: {agent}. Messages below are peer content.\n");
        let mut messages = Vec::new();
        let urgent = store::urgent(read, agent)?;
        let mut events = Vec::new();
        for request in &urgent {
            if events.len() >= MAX_CONTEXT_MESSAGES { break; }
            if store::delivery(read, agent, request)? == Delivery::Pending
                && let Some(message) = store::event(read, request)? { events.push(message); }
        }
        for message in store::pending(read, agent, MAX_CONTEXT_MESSAGES)? {
            if !events.iter().any(|event| event.id() == message.id()) { events.push(message); }
        }
        events.sort_by_key(|event| !matches!(event.body(), Body::Message { notice: Some(notice), .. } if notice.urgent));
        for event in events {
            if !matches!(event.body(), Body::Message { .. })
                || store::delivery(read, agent, event.id())? != Delivery::Pending
            {
                continue;
            }
            let line = summary(&event);
            if messages.len() >= MAX_CONTEXT_MESSAGES
                || !summary_fits(context.len(), line.len(), MAX_CONTEXT_BYTES)
            {
                break;
            }
            context.push_str(&line);
            messages.push(event.id().clone());
        }
        if matches!(args.phase, HookPhase::Stop) && !urgent.is_empty() {
            let ids = urgent.iter().take(MAX_CONTEXT_MESSAGES).map(ToString::to_string).collect::<Vec<_>>().join(", ");
            let reminder = format!(
                "{} urgent messages remain unacknowledged: {ids}. Read and reply or acknowledge them.\n",
                urgent.len()
            );
            if summary_fits(context.len(), reminder.len(), MAX_CONTEXT_BYTES) {
                context.push_str(&reminder);
            }
        }
        Ok(Prepared {
            agent: agent.clone(),
            context,
            messages,
            pending_urgent: urgent.len(),
            block_stop: matches!(args.phase, HookPhase::Stop)
                && stop_decision(!urgent.is_empty(), Delivery::Pending) == StopDecision::Block,
        })
    })
}

pub(crate) fn delivered(store: &Store, context: &Prepared) -> Result<(), StoreError> {
    store.write(|tx| {
        for request in &context.messages {
            if let Some(message) = store::event(tx.transaction(), request)? {
                tx.receipt(&context.agent, &message, Delivery::Delivered)?;
            }
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::{delivered, prepare};
    use crate::chat::args::{HookArgs, HookPhase};
    use crate::chat::store::fixtures::{register, store};
    use domyjob_core::chat::card::Mode;
    use domyjob_core::chat::event::{Body, Event, Intent, Members, Stamp};
    use domyjob_core::chat::id::{Conversation, Text};
    use domyjob_core::chat::notification::{Delivery, MAX_CONTEXT_BYTES, Notice};
    use domyjob_core::chat::policy::{Priority, audience};

    #[test]
    fn maximal_agent_names_keep_the_subject_and_exact_full_body_read_command() {
        use domyjob_core::chat::id::{AgentId, AgentName, EventId, Line, Origin};
        use domyjob_core::chat::notification::MAX_SUMMARY_BYTES;

        const MAX_AGENT_NAME_BYTES: usize = 64;
        const SUBJECT: &str = "Review maximal metadata";
        let sender = AgentId::new(
            AgentName::try_from("a".repeat(MAX_AGENT_NAME_BYTES)).unwrap(),
            Origin::from_entropy([1; 16]),
        );
        let recipient = AgentId::new(
            AgentName::try_from("b".repeat(MAX_AGENT_NAME_BYTES)).unwrap(),
            Origin::from_entropy([2; 16]),
        );
        let id = EventId::new(sender.origin().clone(), std::num::NonZeroU64::MIN);
        let event = Event::new(
            id.clone(),
            1,
            Body::Message {
                conversation: Conversation::direct(&sender, &recipient).unwrap(),
                from: sender.name().clone(),
                text: Text::try_from("本文".repeat(128)).unwrap(),
                audience: audience([&sender, &recipient]).unwrap(),
                intent: Intent::Send {},
                at: Stamp::from_unix_millis(0),
                notice: Some(Notice {
                    recipients: Members::try_from(vec![recipient]).unwrap(),
                    urgent: false,
                    subject: Some(Line::try_from(SUBJECT.to_owned()).unwrap()),
                    question: None,
                }),
            },
        )
        .unwrap();
        let summary = super::summary(&event);
        assert!(
            summary.contains(&format!("{SUBJECT}.")),
            "the subject must survive bounded metadata"
        );
        assert!(summary.ends_with(&format!(" [read full body: domyjob chat read {id}]\n")));
        assert!(summary.len() <= MAX_SUMMARY_BYTES);
        assert!(summary.contains("本文"));
    }

    #[test]
    fn an_urgent_message_precedes_a_backlog_with_bounded_selection() {
        let root = tempfile::tempdir().unwrap();
        let store = store(&root, "local");
        let sender = register(&store, "sender", Mode::Interactive);
        let recipient = register(&store, "recipient", Mode::Interactive);
        let mut last = None;
        let count = domyjob_core::chat::notification::MAX_CONTEXT_MESSAGES.saturating_mul(2);
        for index in 0..=count {
            last = Some(
                store
                    .write(|tx| {
                        tx.author(
                            Priority::Ordinary,
                            Body::Message {
                                conversation: Conversation::direct(&sender, &recipient)?,
                                from: sender.name().clone(),
                                text: Text::try_from("message".to_owned())?,
                                audience: audience([&sender, &recipient])?,
                                intent: Intent::Send {},
                                at: Stamp::from_unix_millis(0),
                                notice: Some(Notice {
                                    recipients: Members::try_from(vec![recipient.clone()])?,
                                    urgent: index == count,
                                    subject: None,
                                    question: None,
                                }),
                            },
                        )
                    })
                    .unwrap(),
            );
        }
        let args = HookArgs {
            phase: HookPhase::Pre,
            status: None,
            client: None,
            if_bound: false,
        };
        let context = prepare(&store, &recipient, &args).unwrap();
        assert_eq!(context.messages.first(), last.as_ref().map(Event::id));
        assert!(context.messages.len() <= domyjob_core::chat::notification::MAX_CONTEXT_MESSAGES);
        assert!(context.context.len() <= MAX_CONTEXT_BYTES);
    }

    #[test]
    fn context_delivery_read_and_urgent_stop_have_distinct_durable_stages() {
        let root = tempfile::tempdir().unwrap();
        let store = store(&root, "local");
        let alice = register(&store, "alice", Mode::Interactive);
        let bob = register(&store, "bob", Mode::Interactive);
        let message = store
            .write(|tx| {
                tx.author(
                    Priority::Ordinary,
                    Body::Message {
                        conversation: Conversation::direct(&alice, &bob)?,
                        from: alice.name().clone(),
                        audience: audience([&alice, &bob])?,
                        text: Text::try_from("日本語".repeat(1000))?,
                        intent: Intent::Send {},
                        at: Stamp::from_unix_millis(0),
                        notice: Some(Notice {
                            recipients: Members::try_from(vec![bob.clone()])?,
                            urgent: true,
                            subject: None,
                            question: None,
                        }),
                    },
                )
            })
            .unwrap();
        let args = |phase| HookArgs {
            phase,
            status: Some("reviewing".to_owned()),
            client: None,
            if_bound: false,
        };
        let context = prepare(&store, &bob, &args(HookPhase::Pre)).unwrap();
        assert!(context.context.contains("alice"));
        assert!(context.context.len() <= MAX_CONTEXT_BYTES);
        assert_eq!(context.messages, [message.id().clone()]);
        assert_eq!(
            store
                .read(|read| crate::chat::store::delivery(read, &bob, message.id()))
                .unwrap(),
            Delivery::Pending
        );
        delivered(&store, &context).unwrap();
        assert_eq!(
            store
                .read(|read| crate::chat::store::delivery(read, &bob, message.id()))
                .unwrap(),
            Delivery::Delivered
        );
        let stop = prepare(&store, &bob, &args(HookPhase::Stop)).unwrap();
        assert!(stop.block_stop);
        assert_eq!(
            stop.output(HookPhase::Stop).get("decision"),
            Some(&serde_json::json!("block"))
        );
        prepare(&store, &bob, &args(HookPhase::Pre)).unwrap();
        assert_eq!(
            store
                .read(|read| crate::chat::store::delivery(read, &bob, message.id()))
                .unwrap(),
            Delivery::Read
        );
        store
            .write(|tx| tx.receipt(&bob, &message, Delivery::Acknowledged))
            .unwrap();
        assert!(
            !prepare(&store, &bob, &args(HookPhase::Stop))
                .unwrap()
                .block_stop
        );
        let directory = store.read(crate::chat::store::directory).unwrap();
        let presence = &directory.agents.get(&bob).unwrap().1;
        assert!(presence.online);
        assert_eq!(presence.status.as_ref().unwrap().as_str(), "reviewing");
    }
}
