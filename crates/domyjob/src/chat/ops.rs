use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use domyjob_core::chat::card::{Access, Card, Mode, Skills, Tag, Tool};
use domyjob_core::chat::event::{Body, Chain, Intent, Members, Outcome as Ending};
use domyjob_core::chat::id::{
    AgentId, AgentName, Conversation, EventId, Invalid, Line, Paragraph, RoomName, Text,
};
use domyjob_core::chat::ledger::{Ending as Resolved, Room};
use domyjob_core::chat::policy::{self, Priority, Refusal};

use super::address::{self, Book};
use super::args::{
    AskArgs, DirectoryArgs, InboxArgs, JoinArgs, MemberArgs, MessageArgs, NameArgs, ProfileArgs,
    ReplyArgs, RoomArgs, SendArgs, StartArgs, ThreadArgs, TopicArgs, UpdateArgs,
};
use super::pulse::{Pulse, PulseError};
use super::store::{self, LocalAgent, Reader, Store, StoreError};
use super::sync::{self, Report, SyncError};
use super::view::{self, AskState, Outcome, Presence};
use crate::lock::{OsLock, Probe};
use crate::platform::clock::{self, Deadline};

#[derive(Debug, thiserror::Error)]
pub(crate) enum OpsError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Sync(#[from] SyncError),
    #[error(transparent)]
    Pulse(#[from] PulseError),
    #[error(transparent)]
    Lock(#[from] crate::lock::LockError),
    #[error(transparent)]
    Refused(#[from] Refusal),
    #[error(transparent)]
    Runner(#[from] super::runner::RunnerError),
    #[error(transparent)]
    Invalid(#[from] Invalid),
    #[error("{0}")]
    Usage(&'static str),
    #[error(
        "this operation needs an agent identity: pass --as NAME, set DOMYJOB_CHAT_AGENT, or call chat_join"
    )]
    Anonymous,
    #[error("{0} is not an agent of this machine; register it first")]
    NotLocal(String),
}

impl From<domyjob_core::chat::ledger::Failure<StoreError>> for OpsError {
    fn from(failure: domyjob_core::chat::ledger::Failure<StoreError>) -> Self {
        Self::Store(failure.into())
    }
}

#[derive(Debug)]
pub(crate) struct Session {
    pub(crate) store: Store,
    actor: Option<AgentId>,
    turn: Option<EventId>,
    cancelled: Arc<AtomicBool>,
}

const SYNC_SECONDS: u64 = 15;
pub(crate) const MAX_WAIT_SECONDS: u64 = 600;
const MAX_LIMIT: usize = 500;

impl Session {
    pub(crate) fn open(actor: Option<&str>, turn: Option<&str>) -> Result<Self, OpsError> {
        let store = Store::open()?;
        let mut session = Self {
            store,
            actor: None,
            turn: turn
                .map(|turn| EventId::try_from(turn.to_owned()))
                .transpose()?,
            cancelled: Arc::new(AtomicBool::new(false)),
        };
        if let Some(actor) = actor {
            session.bind(actor)?;
        }
        Ok(session)
    }

    #[cfg(test)]
    pub(crate) fn with_store(store: Store) -> Self {
        Self {
            store,
            actor: None,
            turn: None,
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(crate) fn bind(&mut self, actor: &str) -> Result<AgentId, OpsError> {
        let book = self.book()?;
        let agent = address::local_agent(&book, actor)?;
        if !book.agents.contains(&agent) {
            return Err(OpsError::NotLocal(agent.to_string()));
        }
        self.actor = Some(agent.clone());
        Ok(agent)
    }

    #[must_use]
    pub(crate) fn clone_for(&self, cancelled: Arc<AtomicBool>) -> Self {
        Self {
            store: self.store.clone(),
            actor: self.actor.clone(),
            turn: self.turn.clone(),
            cancelled,
        }
    }

    pub(crate) fn adopt(&mut self, other: &Self) {
        self.actor.clone_from(&other.actor);
    }

    fn me(&self) -> Result<&AgentId, OpsError> {
        self.actor.as_ref().ok_or(OpsError::Anonymous)
    }

    pub(crate) fn live(&self) -> Result<bool, OpsError> {
        Ok(OsLock::probe(&self.store.paths().service_lock())? == Probe::Held)
    }

    pub(crate) fn book(&self) -> Result<Book, OpsError> {
        let peers = self.store.peers()?;
        Ok(self.store.read(|read| {
            let directory = store::directory(read)?;
            Ok(Book {
                local: Some(self.store.origin().clone()),
                peers,
                machines: directory.machines,
                agents: directory.agents.into_keys().collect(),
                rooms: store::rooms(read)?
                    .into_iter()
                    .filter_map(|(conversation, _)| match conversation {
                        Conversation::Room(room) => Some(room),
                        Conversation::Direct(_) => None,
                    })
                    .collect(),
            })
        })?)
    }

    pub(crate) fn refresh(&self) -> Result<Option<Report>, OpsError> {
        if self.live()? {
            return Ok(None);
        }
        Ok(Some(sync::sync(
            &self.store,
            None,
            Deadline::after_seconds(SYNC_SECONDS),
        )?))
    }
}

pub(crate) fn around(
    session: &mut Session,
    read: bool,
    work: impl FnOnce(&mut Session) -> Result<Outcome, OpsError>,
) -> Result<Outcome, OpsError> {
    if read {
        warn(session.refresh()?.as_ref());
    }
    let outcome = work(session)?;
    if !read {
        warn(session.refresh()?.as_ref());
    }
    Ok(outcome)
}

fn warn(report: Option<&Report>) {
    for peer in report
        .into_iter()
        .flat_map(|report| &report.peers)
        .filter(|peer| peer.state != store::LinkState::Synced)
    {
        eprintln!(
            "domyjob: chat peer {} not synchronized: {}",
            peer.machine,
            peer.detail.as_deref().unwrap_or("")
        );
    }
}

fn line<const MAX: usize>(text: &str) -> Result<Option<Line<MAX>>, Invalid> {
    let text = text.trim();
    if text.is_empty() {
        Ok(None)
    } else {
        Line::try_from(text.to_owned()).map(Some)
    }
}

fn apply_profile(card: &mut Card, profile: &ProfileArgs) -> Result<(), Invalid> {
    if let Some(name) = &profile.display_name {
        card.display_name = line(name)?.ok_or(Invalid("display name"))?;
    }
    if let Some(role) = &profile.role {
        card.role = line(role)?;
    }
    if let Some(description) = &profile.description {
        let text = description.trim();
        card.description = if text.is_empty() {
            None
        } else {
            Some(Paragraph::try_from(text.to_owned())?)
        };
    }
    if let Some(skills) = &profile.skills {
        card.skills = Skills::collect(
            skills
                .iter()
                .map(|skill| Tag::try_from(skill.trim().to_lowercase()))
                .collect::<Result<Vec<_>, _>>()?,
        )?;
    }
    if let Some(project) = &profile.project {
        card.project = line(project)?;
    }
    if let Some(status) = &profile.status {
        card.status = line(status)?;
    }
    Ok(())
}

fn project_of(cwd: &str) -> Option<Line<128>> {
    let name = Path::new(cwd).file_name()?.to_str()?;
    match Line::try_from(name.to_owned()) {
        Ok(project) => Some(project),
        Err(_unprintable) => None,
    }
}

fn new_card(
    name: &AgentName,
    (tool, mode, access): (Tool, Mode, Access),
    cwd: &str,
) -> Result<Card, Invalid> {
    Ok(Card {
        display_name: Line::try_from(name.to_string())?,
        role: None,
        description: None,
        skills: Skills::default(),
        project: project_of(cwd),
        status: None,
        tool,
        mode,
        access,
    })
}

fn absolute_directory(cwd: &str) -> Result<String, OpsError> {
    let path = Path::new(cwd);
    let metadata = std::fs::metadata(path)
        .map_err(|_missing| OpsError::Usage("the working directory must exist"))?;
    if !path.is_absolute() || !metadata.is_dir() {
        return Err(OpsError::Usage(
            "the working directory must be an existing absolute directory",
        ));
    }
    Ok(cwd.to_owned())
}

fn text(value: &str) -> Result<Text, OpsError> {
    Ok(Text::try_from(value.to_owned())?)
}

fn clamp(limit: usize) -> usize {
    limit.clamp(1, MAX_LIMIT)
}

pub(crate) fn join(
    session: &mut Session,
    args: &JoinArgs,
    tool: Tool,
) -> Result<Outcome, OpsError> {
    let name = address::handle::<AgentName>(&args.name)?;
    let agent = AgentId::new(name.clone(), session.store.origin().clone());
    let cwd = std::env::current_dir()
        .map_err(|_unknown| OpsError::Usage("the current directory is unavailable"))?
        .to_string_lossy()
        .into_owned();
    let access = args.access.map_or(Access::Write, Access::from);
    let card = session.store.write(|tx| {
        let existing = store::profile(tx.transaction(), &agent)?;
        if existing
            .as_ref()
            .is_some_and(|card| card.mode == Mode::Managed)
        {
            return Err(StoreError::Refused(Refusal::Invalid(Invalid(
                "a managed agent already uses this name",
            ))));
        }
        let mut card = match existing {
            Some(card) => card,
            None => new_card(&name, (tool, Mode::Interactive, access), &cwd)?,
        };
        card.tool = tool;
        card.access = access;
        apply_profile(&mut card, &args.profile)?;
        tx.configure(
            &name,
            Some(&LocalAgent {
                cwd: cwd.clone(),
                session: None,
            }),
        )?;
        tx.author(
            Priority::Ordinary,
            Body::Profile {
                agent: name.clone(),
                card: Box::new(card.clone()),
            },
        )?;
        Ok(card)
    })?;
    session.actor = Some(agent.clone());
    Ok(Outcome::Agent {
        agent,
        card,
        presence: Presence::default(),
    })
}

pub(crate) fn start(session: &Session, args: &StartArgs) -> Result<Outcome, OpsError> {
    let name = address::handle::<AgentName>(&args.name)?;
    let cwd = absolute_directory(&args.cwd)?;
    let tool = Tool::from(args.tool);
    if let Some(resume) = &args.resume
        && !crate::process::chat::resumable(tool, resume)
    {
        return Err(OpsError::Usage(
            "the resumed session ID is not valid for this client",
        ));
    }
    let access = args.access.map_or(Access::Read, Access::from);
    let mut card = new_card(&name, (tool, Mode::Managed, access), &cwd)?;
    apply_profile(&mut card, &args.profile)?;
    let agent = AgentId::new(name.clone(), session.store.origin().clone());
    session.store.write(|tx| {
        if store::profile(tx.transaction(), &agent)?.is_some() {
            return Err(StoreError::Refused(Refusal::Invalid(Invalid(
                "an agent with this name already exists here",
            ))));
        }
        tx.configure(
            &name,
            Some(&LocalAgent {
                cwd: cwd.clone(),
                session: args.resume.clone(),
            }),
        )?;
        tx.author(
            Priority::Ordinary,
            Body::Profile {
                agent: name.clone(),
                card: Box::new(card.clone()),
            },
        )?;
        Ok(())
    })?;
    Ok(Outcome::Agent {
        agent,
        card,
        presence: Presence::default(),
    })
}

pub(crate) fn update(session: &Session, args: &UpdateArgs) -> Result<Outcome, OpsError> {
    let name = address::handle::<AgentName>(&args.name)?;
    let cwd = args.cwd.as_deref().map(absolute_directory).transpose()?;
    let agent = AgentId::new(name.clone(), session.store.origin().clone());
    let card = session.store.write(|tx| {
        let mut config = store::agent_config(tx.transaction(), &name)?
            .ok_or_else(|| StoreError::Unknown(name.to_string()))?;
        if let Some(cwd) = &cwd {
            config.cwd.clone_from(cwd);
        }
        if args.reset_session {
            config.session = None;
        }
        tx.configure(&name, Some(&config))?;
        store::profile(tx.transaction(), &agent)?
            .ok_or_else(|| StoreError::Unknown(agent.to_string()))
    })?;
    Ok(Outcome::Agent {
        agent,
        card,
        presence: Presence::default(),
    })
}

pub(crate) fn remove(session: &Session, args: &NameArgs) -> Result<Outcome, OpsError> {
    let name = address::handle::<AgentName>(&args.name)?;
    let agent = AgentId::new(name.clone(), session.store.origin().clone());
    let ended = session.store.write(|tx| {
        store::profile(tx.transaction(), &agent)?
            .ok_or_else(|| StoreError::Unknown(agent.to_string()))?;
        let mut ended = 0_usize;
        for (request, responder) in store::open_asks(tx.transaction())? {
            if responder == agent
                && let Some(event) = store::event(tx.transaction(), &request)?
            {
                tx.end(&event, &agent, Ending::Unavailable)?;
                ended = ended.saturating_add(1);
            }
        }
        tx.author(
            Priority::Ending,
            Body::Left {
                agent: name.clone(),
            },
        )?;
        tx.configure(&name, None)?;
        Ok(ended)
    })?;
    Ok(Outcome::Removed { agent, ended })
}

pub(crate) fn profile(session: &Session, args: &ProfileArgs) -> Result<Outcome, OpsError> {
    let agent = session.me()?.clone();
    let card = session.store.write(|tx| {
        let mut card = store::profile(tx.transaction(), &agent)?
            .ok_or_else(|| StoreError::Unknown(agent.to_string()))?;
        apply_profile(&mut card, args)?;
        tx.author(
            Priority::Ordinary,
            Body::Profile {
                agent: agent.name().clone(),
                card: Box::new(card.clone()),
            },
        )?;
        Ok(card)
    })?;
    Ok(Outcome::Agent {
        agent,
        card,
        presence: Presence::default(),
    })
}

pub(crate) fn whoami(session: &Session) -> Result<Outcome, OpsError> {
    let agent = session.me()?.clone();
    let (card, presence) = session.store.read(|read| {
        let directory = store::directory(read)?;
        directory
            .agents
            .get(&agent)
            .cloned()
            .ok_or_else(|| StoreError::Unknown(agent.to_string()))
    })?;
    Ok(Outcome::Agent {
        agent,
        card,
        presence,
    })
}

pub(crate) fn directory(session: &Session, args: &DirectoryArgs) -> Result<Outcome, OpsError> {
    let book = session.book()?;
    let links = session.store.links()?;
    let (directory, rooms) = session
        .store
        .read(|read| Ok((store::directory(read)?, store::rooms(read)?)))?;
    Ok(Outcome::Directory(view::directory(
        (&book, &links),
        directory,
        rooms,
        args.query.as_deref().unwrap_or(""),
    )))
}

pub(crate) fn rooms(session: &Session) -> Result<Outcome, OpsError> {
    let book = session.book()?;
    let rooms = session.store.read(store::rooms)?;
    Ok(Outcome::Rooms(view::rooms(&book, rooms)))
}

fn members(book: &Book, me: &AgentId, names: &[String]) -> Result<Members, OpsError> {
    let mut agents = vec![me.clone()];
    for name in names {
        let agent = book.agent(name)?;
        if !agents.contains(&agent) {
            agents.push(agent);
        }
    }
    agents.sort();
    Ok(Members::try_from(agents)?)
}

pub(crate) fn create_room(session: &Session, args: &RoomArgs) -> Result<Outcome, OpsError> {
    let me = session.me()?.clone();
    let book = session.book()?;
    let name = address::handle::<RoomName>(&args.name)?;
    let room = address::local_room(&book, name.as_str())?;
    let state = Room {
        topic: args.topic.as_deref().map(line).transpose()?.flatten(),
        members: members(&book, &me, &args.members)?,
    };
    session.store.write(|tx| {
        if store::room(tx.transaction(), &room)?.is_some() {
            return Err(StoreError::Refused(Refusal::Invalid(Invalid(
                "a room with this name already exists here",
            ))));
        }
        tx.author(
            Priority::Ordinary,
            Body::Room {
                name: name.clone(),
                topic: state.topic.clone(),
                members: state.members.clone(),
            },
        )
    })?;
    Ok(Outcome::Rooms(view::rooms(
        &book,
        vec![(Conversation::Room(room), state)],
    )))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RoomChange {
    Add,
    Remove,
}

pub(crate) fn change_room(
    session: &Session,
    args: &MemberArgs,
    change: RoomChange,
) -> Result<Outcome, OpsError> {
    let book = session.book()?;
    let room = address::local_room(&book, &args.room)?;
    let member = book.agent(&args.member)?;
    edit_room(session, &book, &room, |state| {
        let mut agents: Vec<AgentId> = state.members.agents().to_vec();
        match change {
            RoomChange::Add if !agents.contains(&member) => agents.push(member.clone()),
            RoomChange::Remove => agents.retain(|agent| *agent != member),
            RoomChange::Add => {}
        }
        agents.sort();
        state.members = Members::try_from(agents)?;
        Ok(())
    })
}

pub(crate) fn set_topic(session: &Session, args: &TopicArgs) -> Result<Outcome, OpsError> {
    let book = session.book()?;
    let room = address::local_room(&book, &args.room)?;
    let topic = line(&args.topic)?;
    edit_room(session, &book, &room, |state| {
        state.topic.clone_from(&topic);
        Ok(())
    })
}

fn edit_room(
    session: &Session,
    book: &Book,
    room: &domyjob_core::chat::id::RoomId,
    edit: impl FnOnce(&mut Room) -> Result<(), Invalid>,
) -> Result<Outcome, OpsError> {
    let state = session.store.write(|tx| {
        let mut state = store::room(tx.transaction(), room)?
            .ok_or_else(|| StoreError::Unknown(room.name().to_string()))?;
        edit(&mut state)?;
        tx.author(
            Priority::Ordinary,
            Body::Room {
                name: room.name().clone(),
                topic: state.topic.clone(),
                members: state.members.clone(),
            },
        )?;
        Ok(state)
    })?;
    Ok(Outcome::Rooms(view::rooms(
        book,
        vec![(Conversation::Room(room.clone()), state)],
    )))
}

pub(crate) fn close_room(session: &Session, args: &NameArgs) -> Result<Outcome, OpsError> {
    let book = session.book()?;
    let room = address::local_room(&book, &args.name)?;
    session.store.write(|tx| {
        store::room(tx.transaction(), &room)?
            .ok_or_else(|| StoreError::Unknown(room.name().to_string()))?;
        tx.author(
            Priority::Ordinary,
            Body::RoomClosed {
                name: room.name().clone(),
            },
        )
    })?;
    Ok(Outcome::Rooms(Vec::new()))
}

fn route(
    reader: &impl Reader,
    conversation: &Conversation,
    me: &AgentId,
) -> Result<(domyjob_core::chat::id::Audience, Option<Room>), StoreError> {
    match conversation {
        Conversation::Direct(pair) => Ok((policy::audience(pair.agents())?, None)),
        Conversation::Room(room) => {
            let state = store::room(reader, room)?
                .ok_or_else(|| StoreError::Unknown(room.name().to_string()))?;
            policy::check_member(&state, me)?;
            Ok((policy::audience(state.members.agents())?, Some(state)))
        }
    }
}

fn message(
    conversation: &Conversation,
    me: &AgentId,
    audience: domyjob_core::chat::id::Audience,
    (text, intent): (Text, Intent),
) -> Body {
    Body::Message {
        conversation: conversation.clone(),
        from: me.name().clone(),
        text,
        audience,
        intent,
        at: clock::stamp(),
    }
}

pub(crate) fn send(session: &Session, args: &SendArgs) -> Result<Outcome, OpsError> {
    let me = session.me()?.clone();
    let conversation = session.book()?.conversation(&me, &args.target)?;
    let body = text(&args.text)?;
    let event = session.store.write(|tx| {
        let (audience, _) = route(tx.transaction(), &conversation, &me)?;
        tx.author(
            Priority::Ordinary,
            message(&conversation, &me, audience, (body, Intent::Send {})),
        )
    })?;
    Ok(Outcome::Posted(event))
}

fn delegation(session: &Session, me: &AgentId) -> Result<Chain, OpsError> {
    let Some(turn) = &session.turn else {
        return Ok(Chain::default());
    };
    let request = session.store.read(|read| store::event(read, turn))?;
    Ok(
        match request.as_ref().map(|event| (event.author(), event.body())) {
            Some((
                Some(asker),
                Body::Message {
                    intent: Intent::Ask { responder, chain },
                    ..
                },
            )) if responder == me => policy::delegated(chain, asker)?,
            Some(_) | None => Chain::default(),
        },
    )
}

pub(crate) fn ask(session: &Session, args: &AskArgs) -> Result<Outcome, OpsError> {
    if args.timeout > MAX_WAIT_SECONDS {
        return Err(OpsError::Usage("the timeout is at most 600 seconds"));
    }
    let me = session.me()?.clone();
    let book = session.book()?;
    let conversation = book.conversation(&me, &args.target)?;
    let responder = match (&conversation, &args.to) {
        (Conversation::Direct(pair), None) => pair
            .other(&me)
            .cloned()
            .ok_or(OpsError::Usage("you are not in this conversation"))?,
        (Conversation::Direct(_), Some(_)) => {
            return Err(OpsError::Usage("--to is only for asks in a room"));
        }
        (Conversation::Room(_), Some(to)) => book.agent(to)?,
        (Conversation::Room(_), None) => {
            return Err(OpsError::Usage("an ask in a room needs --to AGENT"));
        }
    };
    let chain = delegation(session, &me)?;
    policy::check_ask(&me, &responder, &chain)?;
    let body = text(&args.text)?;
    let request = session.store.write(|tx| {
        let (audience, room) = route(tx.transaction(), &conversation, &me)?;
        if let Some(room) = &room {
            policy::check_member(room, &responder)?;
        }
        tx.author(
            Priority::Ordinary,
            message(
                &conversation,
                &me,
                audience,
                (
                    body,
                    Intent::Ask {
                        responder: responder.clone(),
                        chain,
                    },
                ),
            ),
        )
    })?;
    super::runner::dispatch(&session.store)?;
    let state = await_ending(session, request.id(), args.timeout)?;
    Ok(Outcome::Asked(state))
}

pub(crate) fn wait(session: &Session, args: &MessageArgs) -> Result<Outcome, OpsError> {
    if args.timeout > MAX_WAIT_SECONDS {
        return Err(OpsError::Usage("the timeout is at most 600 seconds"));
    }
    let request = EventId::try_from(args.message.clone())?;
    Ok(Outcome::Asked(await_ending(
        session,
        &request,
        args.timeout,
    )?))
}

fn ask_state(session: &Session, request: &EventId) -> Result<AskState, OpsError> {
    let book = session.book()?;
    Ok(session.store.read(|read| {
        let event =
            store::event(read, request)?.ok_or_else(|| StoreError::Unknown(request.to_string()))?;
        let resolution = store::resolution(read, request)?;
        let answer = match &resolution {
            Some(resolution) if resolution.ending == Resolved::Answered => {
                store::event(read, &resolution.event)?
            }
            Some(_) | None => None,
        };
        let working = store::directory(read)?
            .agents
            .values()
            .any(|(_, presence)| presence.working_on.as_ref() == Some(request));
        Ok(view::ask_state(
            &book,
            &view::AskFacts {
                request: &event,
                resolution: resolution.as_ref(),
                answer: answer.as_ref(),
                working,
            },
        ))
    })?)
}

fn await_ending(session: &Session, request: &EventId, timeout: u64) -> Result<AskState, OpsError> {
    let deadline = Deadline::after_seconds(timeout);
    let mut pulse = Pulse::new(&session.store, 1000)?;
    loop {
        let state = ask_state(session, request)?;
        if state.ended() || deadline.expired() || session.cancelled.load(Ordering::Acquire) {
            return Ok(state);
        }
        if !session.live()? {
            let _report = sync::sync(
                &session.store,
                None,
                deadline.min(Deadline::after_seconds(SYNC_SECONDS)),
            )?;
        }
        pulse.next(deadline)?;
    }
}

pub(crate) fn reply(session: &Session, args: &ReplyArgs) -> Result<Outcome, OpsError> {
    let me = session.me()?.clone();
    let request = EventId::try_from(args.message.clone())?;
    let body = text(&args.text)?;
    let event = session.store.write(|tx| {
        let parent = store::event(tx.transaction(), &request)?
            .ok_or_else(|| StoreError::Unknown(request.to_string()))?;
        let (conversation, audience) = parent
            .body()
            .thread()
            .map(|(conversation, audience)| (conversation.clone(), audience.clone()))
            .ok_or_else(|| StoreError::Unknown(request.to_string()))?;
        if let Body::Message {
            intent: Intent::Ask { responder, .. },
            ..
        } = parent.body()
            && *responder == me
        {
            policy::check_resolution(
                store::resolution(tx.transaction(), &request)?.as_ref(),
                session.store.origin(),
            )?;
        }
        tx.author(
            Priority::Ending,
            message(
                &conversation,
                &me,
                audience,
                (
                    body,
                    Intent::Reply {
                        request: request.clone(),
                    },
                ),
            ),
        )
    })?;
    Ok(Outcome::Posted(event))
}

pub(crate) fn withdraw(session: &Session, args: &MessageArgs) -> Result<Outcome, OpsError> {
    let me = session.me()?.clone();
    let request = EventId::try_from(args.message.clone())?;
    let event = session.store.write(|tx| {
        let parent = store::event(tx.transaction(), &request)?
            .ok_or_else(|| StoreError::Unknown(request.to_string()))?;
        if parent.author().as_ref() != Some(&me) {
            return Err(StoreError::Refused(Refusal::Invalid(Invalid(
                "only the asker may withdraw an ask",
            ))));
        }
        policy::check_resolution(
            store::resolution(tx.transaction(), &request)?.as_ref(),
            session.store.origin(),
        )?;
        let (conversation, audience) = parent
            .body()
            .thread()
            .ok_or_else(|| StoreError::Unknown(request.to_string()))?;
        tx.author(
            Priority::Ending,
            Body::Resolved {
                request: request.clone(),
                conversation: conversation.clone(),
                audience: audience.clone(),
                agent: me.name().clone(),
                outcome: Ending::Withdrawn,
            },
        )
    })?;
    Ok(Outcome::Posted(event))
}

pub(crate) fn inbox(session: &Session, args: &InboxArgs) -> Result<Outcome, OpsError> {
    let me = session.me()?.clone();
    let limit = clamp(args.limit);
    let (events, unread) = session.store.write(|tx| {
        let cursor = tx.read_cursor(&me)?;
        let after = if args.all { String::new() } else { cursor };
        let events = store::inbox(tx.transaction(), &me, &after, limit)?;
        if let Some(last) = events.last() {
            tx.mark_read(&me, last)?;
        }
        let unread = store::inbox(tx.transaction(), &me, &tx.read_cursor(&me)?, MAX_LIMIT)?.len();
        Ok((events, unread))
    })?;
    Ok(Outcome::Events {
        events: view::messages(&session.book()?, &events),
        unread: Some(unread),
    })
}

pub(crate) fn unread(session: &Session) -> Result<Option<usize>, OpsError> {
    let Some(me) = session.actor.clone() else {
        return Ok(None);
    };
    Ok(Some(session.store.write(|tx| {
        let cursor = tx.read_cursor(&me)?;
        Ok(store::inbox(tx.transaction(), &me, &cursor, MAX_LIMIT)?.len())
    })?))
}

pub(crate) fn thread(session: &Session, args: &ThreadArgs) -> Result<Outcome, OpsError> {
    let book = session.book()?;
    let conversation = match (
        Conversation::try_from(args.target.clone()),
        session.actor.as_ref(),
    ) {
        (Ok(conversation), _) => conversation,
        (Err(_), Some(me)) => book.conversation(me, &args.target)?,
        (Err(_), None) => Conversation::Room(book.room(&args.target)?),
    };
    let before = args.before.clone().map(EventId::try_from).transpose()?;
    let events = session.store.read(|read| {
        let before = before
            .as_ref()
            .map(|id| store::event(read, id))
            .transpose()?
            .flatten();
        store::thread(read, &conversation, clamp(args.limit), before.as_ref())
    })?;
    Ok(Outcome::Events {
        events: view::messages(&book, &events),
        unread: None,
    })
}

pub(crate) fn publish_machine(store: &Store) -> Result<(), OpsError> {
    Ok(store.publish_machine()?)
}
