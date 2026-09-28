//! The `domyjob chat` command line.

use std::io::Write as _;
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

fn print(json_mode: bool, value: &serde_json::Value, text: &str) -> Result<(), CliError> {
    let mut output = std::io::stdout().lock();
    if json_mode {
        writeln!(output, "{value}")?;
    } else if !text.is_empty() {
        writeln!(output, "{text}")?;
    }
    Ok(())
}

fn findings(json_mode: bool, findings: &[Finding]) -> Result<ExitCode, CliError> {
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
    print(json_mode, &json!({"findings": findings}), &text)?;
    Ok(if findings.iter().all(|finding| finding.ok) {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// Run a command that works on the store without an acting agent, or hand the command back.
fn machine(
    command: ChatCommand,
    json_mode: bool,
) -> Result<Result<ExitCode, ChatCommand>, CliError> {
    Ok(Ok(match command {
        ChatCommand::Setup {
            machines,
            replace,
            no_service,
            no_clients,
        } => findings(
            json_mode,
            &setup::setup(
                &machines,
                setup::Steps {
                    replace,
                    service: !no_service,
                    clients: !no_clients,
                },
            )?,
        )?,
        ChatCommand::Doctor => findings(json_mode, &setup::doctor()?)?,
        ChatCommand::Serve => {
            super::serve::serve()?;
            ExitCode::SUCCESS
        }
        ChatCommand::Service { command } => {
            let store = Store::open()?;
            let finding = match command {
                ServiceCommand::Install => setup::service_install(&store)?,
                ServiceCommand::Uninstall => setup::service_uninstall(&store)?,
                ServiceCommand::Status => setup::service_status(&store)?,
            };
            findings(json_mode, &[finding])?
        }
        ChatCommand::Sync { machine } => sync_now(machine.as_deref(), json_mode)?,
        ChatCommand::Peer { command } => peer(command, json_mode)?,
        ChatCommand::Reset { yes: false } => {
            return Err(CliError::Usage(
                "reset deletes this machine's chat history; confirm with --yes",
            ));
        }
        ChatCommand::Reset { yes: true } => reset(json_mode)?,
        ChatCommand::Clean { target } => clean(target, json_mode)?,
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

fn sync_now(machine: Option<&str>, json_mode: bool) -> Result<ExitCode, CliError> {
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
    print(json_mode, &json!(report), &text)?;
    Ok(if report.complete() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

fn reset(json_mode: bool) -> Result<ExitCode, CliError> {
    let store = Store::open()?;
    if matches!(setup::service_status(&store)?, Finding { ok: true, .. }) {
        setup::service_uninstall(&store)?;
    }
    let fresh = Store::reset(&crate::layout::State::here()?)?;
    print(
        json_mode,
        &json!({"origin": fresh.origin()}),
        &format!("new chat identity {}", fresh.origin()),
    )?;
    Ok(ExitCode::SUCCESS)
}

fn clean(target: String, json_mode: bool) -> Result<ExitCode, CliError> {
    let store = Store::open()?;
    let conversation = Conversation::try_from(target).map_err(|_invalid| {
        CliError::Usage("clean takes a conversation ID such as room:ORIGIN:NAME")
    })?;
    let removed = store.clean(&conversation)?;
    print(
        json_mode,
        &json!({"cleaned": removed}),
        &format!("cleaned {removed} events"),
    )?;
    Ok(ExitCode::SUCCESS)
}

fn peer(command: PeerCommand, json_mode: bool) -> Result<ExitCode, CliError> {
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
            print(json_mode, &json!({"peers": peers, "links": links}), &text)?;
        }
        PeerCommand::Remove { machine } => {
            let removed = store.unpin(&machine)?;
            print(
                json_mode,
                &json!({"removed": removed}),
                if removed { "removed" } else { "not a peer" },
            )?;
        }
        PeerCommand::Replace { machine } => {
            let origin =
                sync::identify(&mut sync::Ssh::new(&machine)?, Deadline::after_seconds(900))?;
            store.pin(&machine, &origin, true)?;
            print(
                json_mode,
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
fn watch(session: &Session, args: &ThreadArgs, json_mode: bool) -> Result<ExitCode, CliError> {
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
                    print(json_mode, &single.json(), &single.text())?;
                }
            }
        }
        pulse.next(Deadline::after_millis(2000))?;
    }
}

pub(crate) fn run(args: ChatArgs) -> Result<ExitCode, CliError> {
    let ChatArgs {
        json,
        actor,
        command,
    } = args;
    let command = match machine(command, json)? {
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
        return watch(&session, thread, json);
    }
    let read = reads(&command);
    let outcome = ops::around(&mut session, read, |session| act(session, command))?;
    print(json, &outcome.json(), &outcome.text())?;
    Ok(ExitCode::from(outcome.exit_code()))
}
