use std::collections::BTreeSet;
use std::process::ExitCode;

use clap::{Args, Subcommand};
use domyjob_core::chat::card::Tool;
use domyjob_core::chat::id::{Conversation, EventId};
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
use super::view::{MessageView, Outcome};
use crate::output::Output;
use crate::platform::clock::Deadline;

#[derive(Debug, Args)]
pub(crate) struct ChatArgs {
    #[arg(long, global = true)]
    #[arg(help = "Print structured JSON")]
    json: bool,
    #[arg(long = "as", global = true, value_name = "AGENT")]
    #[arg(help = "Act as this local agent; defaults to `DOMYJOB_CHAT_AGENT`")]
    actor: Option<String>,
    #[command(subcommand)]
    command: ChatCommand,
}

#[derive(Debug, Subcommand)]
enum ChatCommand {
    #[command(
        about = "Pin peers, start the background service, and register the MCP server with AI clients"
    )]
    Setup(SetupArgs),
    #[command(about = "Check the service, peers, AI clients, and local agents")]
    Doctor,
    #[command(about = "Exchange messages with every peer, or one, now")]
    Sync { machine: Option<String> },
    #[command(about = "Manage pinned peers")]
    Peer {
        #[command(subcommand)]
        command: PeerCommand,
    },
    #[command(about = "Manage the background service")]
    Service {
        #[command(subcommand)]
        command: ServiceCommand,
    },
    #[command(hide = true)]
    Serve,
    #[command(about = "Replace this machine's chat identity and delete its chat history")]
    Reset {
        #[arg(long)]
        #[arg(help = "Confirm the deletion")]
        yes: bool,
    },
    #[command(about = "Delete a conversation's content on this machine once its peers hold it")]
    Clean { target: String },
    #[command(about = "Find agents and rooms by what they do")]
    Directory(DirectoryArgs),
    #[command(about = "Show the acting agent")]
    Whoami,
    #[command(about = "Change the acting agent's profile")]
    Profile(ProfileArgs),
    #[command(about = "Set or clear the acting agent's status")]
    Status {
        #[arg(help = "The status; omit it to clear")]
        text: Option<String>,
    },
    #[command(about = "Register, change, or remove agents on this machine")]
    Agent {
        #[command(subcommand)]
        command: AgentCommand,
    },
    #[command(about = "Create and change rooms")]
    Room {
        #[command(subcommand)]
        command: RoomCommand,
    },
    #[command(about = "Send a message")]
    Send(SendArgs),
    #[command(about = "Ask one agent and wait for its answer")]
    Ask(AskArgs),
    #[command(about = "Reply to an exact message ID")]
    Reply(ReplyArgs),
    #[command(about = "Withdraw an ask you sent")]
    Withdraw(MessageArgs),
    #[command(about = "Show or wait for an ask's ending")]
    Wait(MessageArgs),
    #[command(about = "Read unread messages addressed to the acting agent")]
    Inbox(InboxArgs),
    #[command(about = "Read a conversation")]
    Thread(ThreadArgs),
    #[command(about = "Follow a conversation as it grows")]
    Watch(ThreadArgs),
}

#[derive(Debug, Args)]
struct SetupArgs {
    #[arg(help = "SSH aliases of the machines to exchange messages with")]
    machines: Vec<String>,
    #[arg(long)]
    #[arg(help = "Accept a peer whose chat identity changed")]
    replace: bool,
    #[arg(long)]
    #[arg(help = "Do not install the background service")]
    no_service: bool,
    #[arg(long)]
    #[arg(help = "Do not register the MCP server with Claude Code, Codex, or OpenCode")]
    no_clients: bool,
}

impl SetupArgs {
    const fn steps(&self) -> setup::Steps {
        setup::Steps {
            replace: self.replace,
            service: !self.no_service,
            clients: !self.no_clients,
        }
    }
}

#[derive(Debug, Subcommand)]
enum PeerCommand {
    List,
    Remove {
        machine: String,
    },
    #[command(about = "Accept a peer's new chat identity after it was reset")]
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
    #[command(about = "Register a managed agent whose turns this machine runs")]
    Start(StartArgs),
    #[command(about = "Register an interactive session's agent")]
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
    environment_value(std::env::var(name))
}

fn environment_value(
    value: Result<String, std::env::VarError>,
) -> Result<Option<String>, CliError> {
    match value {
        Ok(value) if !value.is_empty() => Ok(Some(value)),
        Ok(_) | Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => {
            Err(CliError::Usage("chat environment variables must be UTF-8"))
        }
    }
}

fn session_inputs(
    actor: Option<String>,
    mut read_environment: impl FnMut(&str) -> Result<Option<String>, CliError>,
) -> Result<(Option<String>, Option<String>), CliError> {
    let actor = match actor {
        Some(actor) => Some(actor),
        None => read_environment("DOMYJOB_CHAT_AGENT")?,
    };
    let turn = read_environment("DOMYJOB_CHAT_TURN")?;
    Ok((actor, turn))
}

struct Printer<'out> {
    output: &'out Output,
    json: bool,
}

impl Printer<'_> {
    fn print(&self, value: &serde_json::Value, text: &str) -> Result<(), CliError> {
        print_using(self.json, value, text, |line| self.output.line(line))
    }
}

fn print_using(
    json: bool,
    value: &serde_json::Value,
    text: &str,
    write: impl FnOnce(&dyn std::fmt::Display) -> std::io::Result<()>,
) -> Result<(), CliError> {
    if json {
        write(value)?;
    } else if !text.is_empty() {
        write(&text)?;
    }
    Ok(())
}

fn findings(out: &Printer<'_>, findings: &[Finding]) -> Result<ExitCode, CliError> {
    findings_using(findings, |value, text| out.print(value, text))
}

fn findings_using(
    findings: &[Finding],
    write: impl FnOnce(&serde_json::Value, &str) -> Result<(), CliError>,
) -> Result<ExitCode, CliError> {
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
    write(&json!({"findings": findings}), &text)?;
    Ok(if findings.iter().all(|finding| finding.ok) {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

fn machine(
    command: ChatCommand,
    out: &Printer<'_>,
) -> Result<Result<ExitCode, ChatCommand>, CliError> {
    match command {
        ChatCommand::Setup(args) => {
            findings(out, &setup::setup(&args.machines, args.steps())?).map(Ok)
        }
        ChatCommand::Doctor => findings(out, &setup::doctor()?).map(Ok),
        ChatCommand::Serve => {
            super::serve::serve()?;
            Ok(Ok(ExitCode::SUCCESS))
        }
        ChatCommand::Service { command } => {
            let paths = crate::layout::State::here()?.chat();
            let finding = match command {
                ServiceCommand::Install => setup::service_install(&paths),
                ServiceCommand::Uninstall => setup::service_uninstall(&paths),
                ServiceCommand::Status => setup::service_status(&paths),
            }?;
            findings(out, &[finding]).map(Ok)
        }
        ChatCommand::Sync { machine } => sync_now(machine.as_deref(), out).map(Ok),
        ChatCommand::Peer { command } => peer(command, out).map(Ok),
        ChatCommand::Reset { yes: false } => Err(CliError::Usage(
            "reset deletes this machine's chat history; confirm with --yes",
        )),
        ChatCommand::Reset { yes: true } => reset(out).map(Ok),
        ChatCommand::Clean { target } => clean(target, out).map(Ok),
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
        | ChatCommand::Watch(_)) => Ok(Err(other)),
    }
}

fn sync_now(machine: Option<&str>, out: &Printer<'_>) -> Result<ExitCode, CliError> {
    let report = sync::sync(&Store::open()?, machine, Deadline::after_seconds(120))?;
    sync_report(&report, |value, text| out.print(value, text))
}

fn sync_report(
    report: &sync::Report,
    write: impl FnOnce(&serde_json::Value, &str) -> Result<(), CliError>,
) -> Result<ExitCode, CliError> {
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
    write(&json!(report), &text)?;
    Ok(if report.complete() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

fn replace_store(state: &crate::layout::State) -> Result<Store, CliError> {
    let paths = state.chat();
    if setup::service_present(&paths)? {
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
    let conversation = clean_conversation(target)?;
    let removed = store.clean(&conversation)?;
    out.print(
        &json!({"cleaned": removed}),
        &format!("cleaned {removed} events"),
    )?;
    Ok(ExitCode::SUCCESS)
}

fn clean_conversation(target: String) -> Result<Conversation, CliError> {
    Conversation::try_from(target).map_err(|_invalid| {
        CliError::Usage("clean takes a conversation ID such as room:ORIGIN:NAME")
    })
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
            let (value, text) = peer_removal_report(store.unpin(&machine)?);
            out.print(&value, text)?;
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

fn peer_removal_report(removed: bool) -> (serde_json::Value, &'static str) {
    (
        json!({"removed": removed}),
        if removed { "removed" } else { "not a peer" },
    )
}

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
        ChatCommand::Setup(_)
        | ChatCommand::Doctor
        | ChatCommand::Sync { .. }
        | ChatCommand::Peer { .. }
        | ChatCommand::Service { .. }
        | ChatCommand::Serve
        | ChatCommand::Reset { .. }
        | ChatCommand::Clean { .. } => return Err(OpsError::Usage("not an agent operation")),
    })
}

fn watch(session: &Session, args: &ThreadArgs, out: &Printer<'_>) -> Result<ExitCode, CliError> {
    let mut pulse = Pulse::new(&session.store, 2000)?;
    let mut shown = BTreeSet::new();
    loop {
        session.refresh()?;
        if let Outcome::Events { events, .. } = ops::thread(session, args)? {
            emit_new_events(&mut shown, events, |single| {
                out.print(&single.json(), &single.text())
            })?;
        }
        pulse.next(Deadline::after_millis(2000))?;
    }
}

fn emit_new_events(
    shown: &mut BTreeSet<EventId>,
    events: Vec<MessageView>,
    mut emit: impl FnMut(&Outcome) -> Result<(), CliError>,
) -> Result<(), CliError> {
    for event in events {
        if shown.insert(event.id.clone()) {
            emit(&Outcome::Events {
                events: vec![event],
                unread: None,
            })?;
        }
    }
    Ok(())
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
    let (actor, turn) = session_inputs(actor, environment)?;
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
    use super::{
        CliError, SetupArgs, clean_conversation, emit_new_events, environment_value,
        findings_using, peer_removal_report, print_using, replace_store, session_inputs,
        sync_report,
    };
    use crate::chat::setup::Finding;
    use crate::chat::store::{LinkState, Store};
    use crate::chat::sync::{PeerReport, Report};
    use crate::chat::view::MessageView;
    use crate::layout::State;
    use domyjob_core::chat::id::{Conversation, EventId, Origin};

    fn assert_io_error(error: CliError, expected: std::io::ErrorKind) {
        let CliError::Io(source) = error else {
            panic!("expected an I/O error, got {error}");
        };
        assert_eq!(source.kind(), expected);
    }

    fn broken_pipe(_value: &serde_json::Value, _text: &str) -> Result<(), CliError> {
        Err(CliError::Io(std::io::ErrorKind::BrokenPipe.into()))
    }

    fn finding(area: &'static str, ok: bool, detail: &str, fix: Option<&str>) -> Finding {
        Finding {
            area,
            ok,
            detail: detail.to_owned(),
            fix: fix.map(str::to_owned),
        }
    }

    #[derive(clap::Parser)]
    struct SetupOptions {
        #[command(flatten)]
        args: SetupArgs,
    }

    #[test]
    fn peer_removal_reports_the_same_status_in_json_and_text() {
        for (removed, expected) in [(true, "removed"), (false, "not a peer")] {
            let (value, text) = peer_removal_report(removed);
            assert_eq!(value, serde_json::json!({"removed": removed}));
            assert_eq!(text, expected);
            for json in [false, true] {
                let mut rendered = String::new();
                print_using(json, &value, text, |line| {
                    rendered = line.to_string();
                    Ok(())
                })
                .unwrap();
                assert_eq!(
                    rendered,
                    if json {
                        value.to_string()
                    } else {
                        expected.to_owned()
                    }
                );
            }
        }
    }

    #[test]
    fn refreshed_watch_views_emit_each_identity_once_and_preserve_output_errors() {
        let origin = Origin::from_entropy([1; 16]);
        let conversation = Conversation::try_from(format!("room:{origin}:release")).unwrap();
        let event = |seq| MessageView {
            id: EventId::new(origin.clone(), std::num::NonZeroU64::new(seq).unwrap()),
            conversation: conversation.clone(),
            from: "worker@local".to_owned(),
            kind: "send",
            text: Some(format!("event {seq}")),
            responder: None,
            request: None,
            at: None,
        };
        let mut shown = std::collections::BTreeSet::new();
        let mut emitted = Vec::new();
        for events in [
            vec![event(1), event(2), event(1)],
            vec![event(2), event(3)],
            vec![event(1), event(3)],
        ] {
            emit_new_events(&mut shown, events, |single| {
                emitted.push((single.json(), single.text()));
                Ok(())
            })
            .unwrap();
        }
        assert_eq!(emitted.len(), 3);
        for ((value, text), seq) in emitted.iter().zip([1, 2, 3]) {
            let id = event(seq).id;
            assert_eq!(value.pointer("/events/0/id"), Some(&serde_json::json!(id)));
            assert_eq!(value.get("unread"), Some(&serde_json::Value::Null));
            assert_eq!(value.get("events").unwrap().as_array().unwrap().len(), 1);
            assert_eq!(text, &format!("{id} worker@local: event {seq}"));
        }
        let mut failed = Vec::new();
        let error = emit_new_events(&mut shown, vec![event(2), event(4), event(5)], |single| {
            failed.push(single.json());
            Err(CliError::Io(std::io::ErrorKind::BrokenPipe.into()))
        })
        .unwrap_err();
        assert_io_error(error, std::io::ErrorKind::BrokenPipe);
        assert_eq!(failed.len(), 1);
        assert_eq!(
            failed.first().unwrap().pointer("/events/0/id"),
            Some(&serde_json::json!(event(4).id))
        );
        assert!(!shown.contains(&event(5).id));
    }

    #[test]
    fn absent_and_empty_chat_environment_values_are_unset() {
        assert_eq!(
            environment_value(Err(std::env::VarError::NotPresent)).unwrap(),
            None
        );
        assert_eq!(environment_value(Ok(String::new())).unwrap(), None);
    }

    #[test]
    fn nonempty_chat_environment_values_are_preserved() {
        for value in ["agent", "event:turn", " "] {
            assert_eq!(
                environment_value(Ok(value.to_owned())).unwrap().as_deref(),
                Some(value)
            );
        }
    }

    #[test]
    fn non_unicode_chat_environment_values_retain_the_usage_error() {
        let value = std::ffi::OsString::from("an environment value rejected by std::env::var");
        let error = environment_value(Err(std::env::VarError::NotUnicode(value))).unwrap_err();
        assert!(matches!(
            error,
            CliError::Usage("chat environment variables must be UTF-8")
        ));
    }

    #[test]
    fn a_supplied_actor_takes_precedence_and_the_turn_is_still_read() {
        let (actor, turn) = session_inputs(Some("explicit".to_owned()), |name| {
            assert_eq!(name, "DOMYJOB_CHAT_TURN");
            Ok(Some("event:turn".to_owned()))
        })
        .unwrap();
        assert_eq!(actor.as_deref(), Some("explicit"));
        assert_eq!(turn.as_deref(), Some("event:turn"));
    }

    #[test]
    fn an_absent_actor_is_read_before_the_turn() {
        let mut names = Vec::new();
        let (actor, turn) = session_inputs(None, |name| {
            names.push(name.to_owned());
            Ok(Some(name.to_owned()))
        })
        .unwrap();
        assert_eq!(names, ["DOMYJOB_CHAT_AGENT", "DOMYJOB_CHAT_TURN"]);
        assert_eq!(actor.as_deref(), Some("DOMYJOB_CHAT_AGENT"));
        assert_eq!(turn.as_deref(), Some("DOMYJOB_CHAT_TURN"));
    }

    #[test]
    fn an_unreadable_turn_value_is_refused_even_with_a_supplied_actor() {
        let error = session_inputs(Some("explicit".to_owned()), |name| {
            assert_eq!(name, "DOMYJOB_CHAT_TURN");
            Err(CliError::Usage("the turn cannot be decoded"))
        })
        .unwrap_err();
        assert!(matches!(
            error,
            CliError::Usage("the turn cannot be decoded")
        ));
    }

    #[test]
    fn json_output_is_emitted_even_without_text() {
        let mut lines = Vec::new();
        print_using(true, &serde_json::json!({"agent": "agent"}), "", |line| {
            lines.push(line.to_string());
            Ok(())
        })
        .unwrap();
        assert_eq!(lines, [r#"{"agent":"agent"}"#]);
    }

    #[test]
    fn blank_text_output_is_silent() {
        print_using(false, &serde_json::json!({"agent": "agent"}), "", |_line| {
            panic!("blank text must not produce an output line");
        })
        .unwrap();
    }

    #[test]
    fn chat_output_errors_propagate_in_both_formats() {
        for json in [true, false] {
            let error = print_using(
                json,
                &serde_json::json!({"agent": "agent"}),
                "agent",
                |_line| Err(std::io::ErrorKind::BrokenPipe.into()),
            )
            .unwrap_err();
            assert_io_error(error, std::io::ErrorKind::BrokenPipe);
        }
    }

    #[test]
    fn a_failed_finding_is_marked_and_returns_failure() {
        let entries = [
            finding("service", true, "ready", None),
            finding("peer", false, "missing", Some("run chat setup")),
        ];
        let code = findings_using(&entries, |value, text| {
            assert_eq!(
                value.pointer("/findings/0/ok"),
                Some(&serde_json::json!(true))
            );
            assert_eq!(
                value.pointer("/findings/1/ok"),
                Some(&serde_json::json!(false))
            );
            assert_eq!(
                text,
                "ok  service  ready\nFIX peer     missing\n      → run chat setup"
            );
            Ok(())
        })
        .unwrap();
        assert_eq!(code, std::process::ExitCode::FAILURE);
    }

    #[test]
    fn findings_without_a_failure_return_success() {
        let entries = [finding("service", true, "ready", None)];
        let empty: &[Finding] = &[];
        for findings in [empty, entries.as_slice()] {
            let code = findings_using(findings, |_value, _text| Ok(())).unwrap();
            assert_eq!(code, std::process::ExitCode::SUCCESS);
        }
    }

    #[test]
    fn finding_output_failures_retain_their_error() {
        let error = findings_using(&[], broken_pipe).unwrap_err();
        assert_io_error(error, std::io::ErrorKind::BrokenPipe);
    }

    #[test]
    fn setup_flags_control_service_clients_and_identity_replacement() {
        use clap::Parser;

        for (arguments, expected) in [
            (vec!["setup", "peer"], (false, true, true)),
            (vec!["setup", "--no-service", "peer"], (false, false, true)),
            (vec!["setup", "--no-clients", "peer"], (false, true, false)),
            (
                vec!["setup", "--replace", "--no-service", "--no-clients", "peer"],
                (true, false, false),
            ),
        ] {
            let args = SetupOptions::try_parse_from(arguments).unwrap().args;
            assert_eq!(args.machines, ["peer"]);
            let steps = args.steps();
            assert_eq!((steps.replace, steps.service, steps.clients), expected);
        }
    }

    #[test]
    fn sync_reports_all_peer_states_and_fails_when_any_peer_is_incomplete() {
        for (state, detail, expected) in [
            (LinkState::Synced, None, std::process::ExitCode::SUCCESS),
            (LinkState::Deferred, None, std::process::ExitCode::FAILURE),
            (
                LinkState::Failed,
                Some("offline"),
                std::process::ExitCode::FAILURE,
            ),
        ] {
            let report = Report {
                peers: vec![PeerReport {
                    machine: "peer".to_owned(),
                    state,
                    detail: detail.map(str::to_owned),
                }],
            };
            let code = sync_report(&report, |value, text| {
                assert_eq!(
                    value.pointer("/peers/0/machine"),
                    Some(&serde_json::json!("peer"))
                );
                assert_eq!(text, format!("peer {state:?} {}", detail.unwrap_or("")));
                Ok(())
            })
            .unwrap();
            assert_eq!(code, expected);
        }
    }

    #[test]
    fn a_service_of_any_build_counts_as_present() {
        let root = tempfile::tempdir().unwrap();
        let state = State::at(&root.path().join("state"));
        let paths = state.chat();
        Store::open_in(&state).unwrap();
        assert!(!crate::chat::setup::service_present(&paths).unwrap());
        crate::state_io::write_bytes(
            &paths.service_record(),
            br#"{"program":"/old/build/domyjob"}"#,
        )
        .unwrap();
        assert!(crate::chat::setup::service_present(&paths).unwrap());
    }

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

    #[test]
    fn cleaning_accepts_only_full_conversation_ids() {
        let valid_target = "room:00000000000000000000000000000000:room";
        assert_eq!(
            clean_conversation(valid_target.to_owned())
                .unwrap()
                .to_string(),
            valid_target
        );
        for target in ["room", "invalid conversation"] {
            let error = clean_conversation(target.to_owned()).unwrap_err();
            assert!(matches!(
                error,
                CliError::Usage("clean takes a conversation ID such as room:ORIGIN:NAME")
            ));
        }
    }
}
