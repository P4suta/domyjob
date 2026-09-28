//! The `domyjob chat` command line.

use std::process::ExitCode;

use clap::{Args, Subcommand};
use domyjob_core::chat::card::Tool;
use domyjob_core::chat::id::Conversation;
use serde_json::json;

use super::args::{
    AskArgs, DirectoryArgs, InboxArgs, JoinArgs, MemberArgs, MessageArgs, NameArgs, ProfileArgs,
    ReplyArgs, RoomArgs, SendArgs, StartArgs, ThreadArgs, TopicArgs, UpdateArgs,
};
use super::ops::{self, OpsError, RoomChange, Session};
use super::pulse::{Pulse, PulseError};
use super::setup::{self, Finding, SetupError};
use super::store::{Store, StoreError};
use super::sync;
use super::view::Outcome;
use crate::output::Output;
use crate::platform::clock::Deadline;

#[derive(Debug, Args)]
pub(crate) struct ChatArgs {
    /// Print structured JSON.
    #[arg(long, global = true)]
    json: bool,
    /// Act as this local agent; defaults to `DOMYJOB_CHAT_AGENT`.
    #[arg(long = "as", global = true, value_name = "AGENT")]
    actor: Option<String>,
    #[command(subcommand)]
    command: ChatCommand,
}

#[derive(Debug, Subcommand)]
enum ChatCommand {
    /// Pin peers, start the background service, and register the MCP server with AI clients.
    Setup {
        /// SSH aliases of the machines to exchange messages with.
        machines: Vec<String>,
        /// Accept a peer whose chat identity changed.
        #[arg(long)]
        replace: bool,
        /// Do not install the background service.
        #[arg(long)]
        no_service: bool,
        /// Do not register the MCP server with Claude Code, Codex, or OpenCode.
        #[arg(long)]
        no_clients: bool,
    },
    /// Check the service, peers, AI clients, and local agents.
    Doctor,
    /// Exchange messages with every peer, or one, now.
    Sync { machine: Option<String> },
    /// Manage pinned peers.
    Peer {
        #[command(subcommand)]
        command: PeerCommand,
    },
    /// Manage the background service.
    Service {
        #[command(subcommand)]
        command: ServiceCommand,
    },
    #[command(hide = true)]
    Serve,
    /// Replace this machine's chat identity and delete its chat history.
    Reset {
        /// Confirm the deletion.
        #[arg(long)]
        yes: bool,
    },
    /// Delete a conversation's content on this machine once its peers hold it.
    Clean { target: String },
    /// Find agents and rooms by what they do.
    Directory(DirectoryArgs),
    /// Show the acting agent.
    Whoami,
    /// Change the acting agent's profile.
    Profile(ProfileArgs),
    /// Set or clear the acting agent's status.
    Status {
        /// The status; omit it to clear.
        text: Option<String>,
    },
    /// Register, change, or remove agents on this machine.
    Agent {
        #[command(subcommand)]
        command: AgentCommand,
    },
    /// Create and change rooms.
    Room {
        #[command(subcommand)]
        command: RoomCommand,
    },
    /// Send a message.
    Send(SendArgs),
    /// Ask one agent and wait for its answer.
    Ask(AskArgs),
    /// Reply to an exact message ID.
    Reply(ReplyArgs),
    /// Withdraw an ask you sent.
    Withdraw(MessageArgs),
    /// Show or wait for an ask's ending.
    Wait(MessageArgs),
    /// Read unread messages addressed to the acting agent.
    Inbox(InboxArgs),
    /// Read a conversation.
    Thread(ThreadArgs),
    /// Follow a conversation as it grows.
    Watch(ThreadArgs),
}

#[derive(Debug, Subcommand)]
enum PeerCommand {
    List,
    Remove {
        machine: String,
    },
    /// Accept a peer's new chat identity after it was reset.
    Replace {
        machine: String,
    },
}

#[derive(Debug, Subcommand)]
enum ServiceCommand {
    Install,
    Uninstall,
    Status,
}

#[derive(Debug, Subcommand)]
enum AgentCommand {
    /// Register a managed agent whose turns this machine runs.
    Start(StartArgs),
    /// Register an interactive session's agent.
    Join(JoinArgs),
    Update(UpdateArgs),
    Remove(NameArgs),
    List(DirectoryArgs),
}

#[derive(Debug, Subcommand)]
enum RoomCommand {
    Create(RoomArgs),
    Add(MemberArgs),
    Remove(MemberArgs),
    Topic(TopicArgs),
    Close(NameArgs),
    List,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum CliError {
    #[error(transparent)]
    Ops(#[from] OpsError),
    #[error(transparent)]
    Setup(#[from] SetupError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Sync(#[from] sync::SyncError),
    #[error(transparent)]
    Serve(#[from] super::serve::ServeError),
    #[error(transparent)]
    Pulse(#[from] PulseError),
    #[error("writing output: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Usage(&'static str),
}

fn environment(name: &str) -> Result<Option<String>, CliError> {
    match std::env::var(name) {
        Ok(value) if !value.is_empty() => Ok(Some(value)),
        Ok(_) | Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => {
            Err(CliError::Usage("chat environment variables must be UTF-8"))
        }
    }
}

/// Where a chat command prints: one JSON document, or text lines.
struct Printer<'out> {
    output: &'out Output,
    json: bool,
}

impl Printer<'_> {
    fn print(&self, value: &serde_json::Value, text: &str) -> Result<(), CliError> {
        if self.json {
            self.output.line(value)?;
        } else if !text.is_empty() {
            self.output.line(text)?;
        }
        Ok(())
    }
}

fn findings(out: &Printer<'_>, findings: &[Finding]) -> Result<ExitCode, CliError> {
    let text = findings
        .iter()
        .map(|finding| {
            let mark = if finding.ok { "ok  " } else { "FIX " };
            let fix = finding
                .fix
                .as_ref()
                .map_or_else(String::new, |fix| format!("\n      → {fix}"));
            format!("{mark}{:<8} {}{fix}", finding.area, finding.detail)
        })
        .collect::<Vec<_>>()
        .join("\n");
    out.print(&json!({"findings": findings}), &text)?;
    Ok(if findings.iter().all(|finding| finding.ok) {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// Run a command that works on the store without an acting agent, or hand the command back.
fn machine(
    command: ChatCommand,
    out: &Printer<'_>,
) -> Result<Result<ExitCode, ChatCommand>, CliError> {
    Ok(Ok(match command {
        ChatCommand::Setup {
            machines,
            replace,
            no_service,
            no_clients,
        } => findings(
            out,
            &setup::setup(
                &machines,
                setup::Steps {
                    replace,
                    service: !no_service,
                    clients: !no_clients,
                },
            )?,
        )?,
        ChatCommand::Doctor => findings(out, &setup::doctor()?)?,
        ChatCommand::Serve => {
            super::serve::serve()?;
            ExitCode::SUCCESS
        }
        ChatCommand::Service { command } => {
            let paths = crate::layout::State::here()?.chat();
            let finding = match command {
                ServiceCommand::Install => setup::service_install(&paths)?,
                ServiceCommand::Uninstall => setup::service_uninstall(&paths)?,
                ServiceCommand::Status => setup::service_status(&paths)?,
            };
            findings(out, &[finding])?
        }
        ChatCommand::Sync { machine } => sync_now(machine.as_deref(), out)?,
        ChatCommand::Peer { command } => peer(command, out)?,
        ChatCommand::Reset { yes: false } => {
            return Err(CliError::Usage(
                "reset deletes this machine's chat history; confirm with --yes",
            ));
        }
        ChatCommand::Reset { yes: true } => reset(out)?,
        ChatCommand::Clean { target } => clean(target, out)?,
        other @ (ChatCommand::Directory(_)
        | ChatCommand::Whoami
        | ChatCommand::Profile(_)
        | ChatCommand::Status { .. }
        | ChatCommand::Agent { .. }
        | ChatCommand::Room { .. }
        | ChatCommand::Send(_)
        | ChatCommand::Ask(_)
        | ChatCommand::Reply(_)
        | ChatCommand::Withdraw(_)
        | ChatCommand::Wait(_)
        | ChatCommand::Inbox(_)
        | ChatCommand::Thread(_)
        | ChatCommand::Watch(_)) => return Ok(Err(other)),
    }))
}

fn sync_now(machine: Option<&str>, out: &Printer<'_>) -> Result<ExitCode, CliError> {
    let report = sync::sync(&Store::open()?, machine, Deadline::after_seconds(120))?;
    let text = report
        .peers
        .iter()
        .map(|peer| {
            format!(
                "{} {:?} {}",
                peer.machine,
                peer.state,
                peer.detail.as_deref().unwrap_or("")
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    out.print(&json!(report), &text)?;
    Ok(if report.complete() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// Stop the service and replace the chat store without opening the old one, which may be unreadable.
fn replace_store(state: &crate::layout::State) -> Result<Store, CliError> {
    let paths = state.chat();
    if matches!(setup::service_status(&paths)?, Finding { ok: true, .. }) {
        setup::service_uninstall(&paths)?;
    }
    Ok(Store::reset(state)?)
}

fn reset(out: &Printer<'_>) -> Result<ExitCode, CliError> {
    let fresh = replace_store(&crate::layout::State::here()?)?;
    out.print(
        &json!({"origin": fresh.origin()}),
        &format!("new chat identity {}", fresh.origin()),
    )?;
    Ok(ExitCode::SUCCESS)
}

fn clean(target: String, out: &Printer<'_>) -> Result<ExitCode, CliError> {
    let store = Store::open()?;
    let conversation = Conversation::try_from(target).map_err(|_invalid| {
        CliError::Usage("clean takes a conversation ID such as room:ORIGIN:NAME")
    })?;
    let removed = store.clean(&conversation)?;
    out.print(
        &json!({"cleaned": removed}),
        &format!("cleaned {removed} events"),
    )?;
    Ok(ExitCode::SUCCESS)
}

fn peer(command: PeerCommand, out: &Printer<'_>) -> Result<ExitCode, CliError> {
    let store = Store::open()?;
    match command {
        PeerCommand::List => {
            let peers = store.peers()?;
            let links = store.links()?;
            let text = peers
                .iter()
                .map(|(alias, origin)| {
                    let state = links.get(alias).map_or_else(
                        || "never synchronized".to_owned(),
                        |link| format!("{:?}", link.state),
                    );
                    format!("{alias} {origin} {state}")
                })
                .collect::<Vec<_>>()
                .join("\n");
            out.print(&json!({"peers": peers, "links": links}), &text)?;
        }
        PeerCommand::Remove { machine } => {
            let removed = store.unpin(&machine)?;
            out.print(
                &json!({"removed": removed}),
                if removed { "removed" } else { "not a peer" },
            )?;
        }
        PeerCommand::Replace { machine } => {
            let origin =
                sync::identify(&mut sync::Ssh::new(&machine)?, Deadline::after_seconds(900))?;
            store.pin(&machine, &origin, true)?;
            out.print(
                &json!({"machine": machine, "origin": origin}),
                &format!("{machine} is now {origin}"),
            )?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Whether the command only reads, so synchronizing before it suffices.
const fn reads(command: &ChatCommand) -> bool {
    matches!(
        command,
        ChatCommand::Directory(_)
            | ChatCommand::Whoami
            | ChatCommand::Inbox(_)
            | ChatCommand::Thread(_)
            | ChatCommand::Wait(_)
            | ChatCommand::Room {
                command: RoomCommand::List
            }
            | ChatCommand::Agent {
                command: AgentCommand::List(_)
            }
    )
}

fn act(session: &mut Session, command: ChatCommand) -> Result<Outcome, OpsError> {
    Ok(match command {
        ChatCommand::Directory(args)
        | ChatCommand::Agent {
            command: AgentCommand::List(args),
        } => ops::directory(session, &args)?,
        ChatCommand::Whoami => ops::whoami(session)?,
        ChatCommand::Profile(args) => ops::profile(session, &args)?,
        ChatCommand::Status { text } => ops::profile(
            session,
            &ProfileArgs {
                status: Some(text.unwrap_or_default()),
                ..ProfileArgs::default()
            },
        )?,
        ChatCommand::Agent { command } => match command {
            AgentCommand::Start(args) => ops::start(session, &args)?,
            AgentCommand::Join(join) => {
                let tool = join
                    .tool
                    .map(Tool::from)
                    .ok_or(OpsError::Usage("pass --tool"))?;
                ops::join(session, &join, tool)?
            }
            AgentCommand::Update(args) => ops::update(session, &args)?,
            AgentCommand::Remove(args) => ops::remove(session, &args)?,
            AgentCommand::List(args) => ops::directory(session, &args)?,
        },
        ChatCommand::Room { command } => match command {
            RoomCommand::Create(args) => ops::create_room(session, &args)?,
            RoomCommand::Add(args) => ops::change_room(session, &args, RoomChange::Add)?,
            RoomCommand::Remove(args) => ops::change_room(session, &args, RoomChange::Remove)?,
            RoomCommand::Topic(args) => ops::set_topic(session, &args)?,
            RoomCommand::Close(args) => ops::close_room(session, &args)?,
            RoomCommand::List => ops::rooms(session)?,
        },
        ChatCommand::Send(args) => ops::send(session, &args)?,
        ChatCommand::Ask(args) => ops::ask(session, &args)?,
        ChatCommand::Reply(args) => ops::reply(session, &args)?,
        ChatCommand::Withdraw(args) => ops::withdraw(session, &args)?,
        ChatCommand::Wait(args) => ops::wait(session, &args)?,
        ChatCommand::Inbox(args) => ops::inbox(session, &args)?,
        ChatCommand::Thread(args) | ChatCommand::Watch(args) => ops::thread(session, &args)?,
        ChatCommand::Setup { .. }
        | ChatCommand::Doctor
        | ChatCommand::Sync { .. }
        | ChatCommand::Peer { .. }
        | ChatCommand::Service { .. }
        | ChatCommand::Serve
        | ChatCommand::Reset { .. }
        | ChatCommand::Clean { .. } => return Err(OpsError::Usage("not an agent operation")),
    })
}

/// Print a conversation and keep printing new events until interrupted.
fn watch(session: &Session, args: &ThreadArgs, out: &Printer<'_>) -> Result<ExitCode, CliError> {
    let mut pulse = Pulse::new(&session.store, 2000)?;
    let mut shown = std::collections::BTreeSet::new();
    loop {
        session.refresh()?;
        if let Outcome::Events { events, .. } = ops::thread(session, args)? {
            for event in events {
                if shown.insert(event.id.clone()) {
                    let single = Outcome::Events {
                        events: vec![event],
                        unread: None,
                    };
                    out.print(&single.json(), &single.text())?;
                }
            }
        }
        pulse.next(Deadline::after_millis(2000))?;
    }
}

pub(crate) fn run(args: ChatArgs, output: &Output) -> Result<ExitCode, CliError> {
    let ChatArgs {
        json,
        actor,
        command,
    } = args;
    let out = Printer { output, json };
    let command = match machine(command, &out)? {
        Ok(code) => return Ok(code),
        Err(command) => command,
    };
    let actor = match actor {
        Some(actor) => Some(actor),
        None => environment("DOMYJOB_CHAT_AGENT")?,
    };
    let turn = environment("DOMYJOB_CHAT_TURN")?;
    let mut session = Session::open(actor.as_deref(), turn.as_deref())?;
    if let ChatCommand::Watch(thread) = &command {
        return watch(&session, thread, &out);
    }
    let read = reads(&command);
    let outcome = ops::around(&mut session, read, |session| act(session, command))?;
    out.print(&outcome.json(), &outcome.text())?;
    Ok(ExitCode::from(outcome.exit_code()))
}

#[cfg(test)]
mod tests {
    use super::replace_store;
    use crate::chat::store::Store;
    use crate::layout::State;

    #[test]
    fn a_store_this_build_cannot_read_can_still_be_reset() {
        let root = tempfile::tempdir().unwrap();
        let state = State::at(&root.path().join("state"));
        let old = Store::open_in(&state).unwrap();
        old.forget_format_for_test().unwrap();
        assert!(
            Store::open_in(&state).is_err(),
            "the unreadable store is refused"
        );
        let fresh = replace_store(&state).unwrap();
        assert_ne!(fresh.origin(), old.origin());
        Store::open_in(&state).unwrap();
    }
}
