//! Managed turns: one detached worker per busy agent runs its queue one ask at a time.

use std::fmt::Write as _;
use std::path::Path;

use domyjob_core::chat::card::Card;
use domyjob_core::chat::event::Body;
use domyjob_core::chat::id::{AgentId, Conversation, Invalid, Text};

use super::provider::{self, Incomplete};
use super::store::{self, Completion, Store, StoreError, Turn};
use crate::lock::LockError;
use crate::process::chat::{self as process, ChatProcessError, Invocation};
use crate::process::{ProcessError, ReadyToken};

#[derive(Debug, thiserror::Error)]
pub(crate) enum RunnerError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Lock(#[from] LockError),
    #[error(transparent)]
    Process(#[from] ChatProcessError),
    #[error(transparent)]
    Launch(#[from] ProcessError),
    #[error(transparent)]
    Incomplete(#[from] Incomplete),
    #[error(transparent)]
    Invalid(#[from] Invalid),
    #[error("{0} is not an agent of this machine")]
    NotLocal(String),
    #[error("the AI CLI resumed a different session than the one it was given")]
    SessionChanged,
}

/// Start a worker for every managed local agent with waiting asks that has none running.
pub(crate) fn dispatch(store: &Store) -> Result<(), RunnerError> {
    for agent in store.dispatchable()? {
        let _launch = store.launch_lock(&agent)?;
        if let Some(free) = store.try_agent_lock(&agent)? {
            drop(free);
            let log = crate::state_io::open_append(&store.root().join("worker.log"))
                .map_err(StoreError::from)?;
            crate::process::launch_chat_worker(&agent.to_string(), log)?;
        }
    }
    Ok(())
}

/// Run `agent`'s queue until it is empty; a second worker for the same agent exits at once.
pub(crate) fn worker(agent: &str, ready: Option<&ReadyToken>) -> Result<(), RunnerError> {
    let store = Store::open()?;
    let agent = AgentId::try_from(agent.to_owned())?;
    if agent.origin() != store.origin() {
        return Err(RunnerError::NotLocal(agent.to_string()));
    }
    let guard = store.try_agent_lock(&agent)?;
    crate::process::announce_ready(ready)?;
    let Some(guard) = guard else {
        return Ok(());
    };
    store.recover(&agent)?;
    loop {
        let launch = store.launch_lock(&agent)?;
        let Some(turn) = store.claim(&agent)? else {
            drop(guard);
            drop(launch);
            return Ok(());
        };
        drop(launch);
        let completion = match run(&store, &turn) {
            Ok(completion) => completion,
            Err(error) => {
                eprintln!("domyjob: chat turn {} failed: {error}", turn.request.id());
                Completion::Failed
            }
        };
        store.finish(&turn, completion)?;
    }
}

fn run(store: &Store, turn: &Turn) -> Result<Completion, RunnerError> {
    let prompt = prompt(store, turn)?;
    let output = process::run(&Invocation {
        tool: turn.card.tool,
        access: turn.card.access,
        cwd: Path::new(&turn.config.cwd),
        session: turn.config.session.as_deref(),
        prompt: &prompt,
        agent: &turn.agent,
        turn: turn.request.id(),
    })?;
    let answer = provider::parse(turn.card.tool, &output)?;
    if turn
        .config
        .session
        .as_deref()
        .is_some_and(|previous| previous != answer.session)
    {
        return Err(RunnerError::SessionChanged);
    }
    Ok(Completion::Answered {
        text: Text::try_from(answer.text)?,
        session: answer.session,
    })
}

fn describe(card: Option<&Card>, agent: &AgentId) -> String {
    let Some(card) = card else {
        return agent.to_string();
    };
    let mut text = format!("{} ({agent})", card.display_name.as_str());
    if let Some(role) = &card.role {
        let _written = write!(text, ", {}", role.as_str());
    }
    text
}

/// The managed turn's instructions: who asks, where, and how the answer is stored.
fn prompt(store: &Store, turn: &Turn) -> Result<String, RunnerError> {
    let Body::Message {
        conversation, text, ..
    } = turn.request.body()
    else {
        return Err(RunnerError::Store(StoreError::Corrupt(
            "a turn answers a message",
        )));
    };
    let asker = turn
        .request
        .author()
        .ok_or(StoreError::Corrupt("an ask has an author"))?;
    let place = store.read(|read| {
        let asker_card = store::profile(read, &asker)?;
        let place = match conversation {
            Conversation::Direct(_) => "a direct message".to_owned(),
            Conversation::Room(named) => {
                let state = store::room(read, named)?;
                let topic = state
                    .as_ref()
                    .and_then(|room| room.topic.as_ref())
                    .map_or(String::new(), |topic| {
                        format!(" about \"{}\"", topic.as_str())
                    });
                format!("the room {}{topic}", named.name())
            }
        };
        Ok(format!(
            "{} asked you in {place}",
            describe(asker_card.as_ref(), &asker)
        ))
    })?;
    Ok(format!(
        "You are {}.\n{place}; the message ID is {}.\n\n{}\n\n\
Give your answer as your final response; domyjob stores it as the reply to this exact message.\n\
To consult another agent, find one with the chat_directory tool and ask it with chat_ask.",
        describe(Some(&turn.card), &turn.agent),
        turn.request.id(),
        text.as_str(),
    ))
}
