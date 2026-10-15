use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use domyjob_core::domain::{Command as JobCommand, JobId, JobReference, MachineName, SubmissionId};
use domyjob_core::process_group::ProcessGroup as ReaperGroup;
use domyjob_core::wire::CleanTarget;

use crate::output::Output;

mod raw {
    #![expect(
        clippy::disallowed_methods,
        reason = "the main entry point alone obtains process output"
    )]

    pub(super) fn output() -> crate::output::Output {
        crate::output::Output::of_process()
    }
}

mod app;
mod bounded;
mod builds;
mod chat;
#[path = "platform/file_kind.rs"]
mod file_kind;
mod formats;
mod identity;
mod layout;
mod lock;
mod mcp;
mod output;
mod platform;
mod process;
mod retention;
mod source;
mod source_archive;
mod source_fingerprint;
mod state_io;
mod store;
#[cfg(test)]
mod testing;
mod transport;
mod tui;
mod watch_event;
mod workspace;

#[derive(Debug, Parser)]
#[command(name = "domyjob", about = "Run persistent jobs over SSH")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    #[command(about = "Open the read-only human dashboard")]
    Tui {
        #[arg(long = "as", value_name = "LOCAL_AGENT")]
        actor: Option<String>,
    },
    #[command(about = "Conversations between AI agents on this and other machines")]
    Chat(chat::cli::ChatArgs),
    #[command(about = "Serve the chat tools to an AI client over the MCP stdio transport")]
    Mcp {
        #[arg(long = "as", value_name = "AGENT")]
        #[arg(help = "Act as this local agent from the start")]
        actor: Option<String>,
        #[arg(long, hide = true)]
        #[arg(help = "The managed turn this server works inside, for delegated asks")]
        turn: Option<String>,
    },
    #[command(hide = true)]
    ChatWorker {
        #[arg(long)]
        agent: String,
        #[arg(long, hide = true)]
        ready_event: Option<String>,
    },
    Doctor {
        machine: String,
    },
    On(JobArgs),
    Run(JobArgs),
    Ls {
        machine: String,
    },
    Clean {
        machine: String,
        #[arg(long)]
        job: Option<String>,
    },
    Status(ResultReadArgs),
    Wait(ResultReadArgs),
    Kill {
        job: String,
    },
    Logs(ResultReadArgs),
    #[command(hide = true)]
    Node {
        #[arg(long, hide = true)]
        reap: Option<ReaperGroup>,
        #[arg(long, hide = true, requires = "reap")]
        reap_scope: Option<String>,
    },
    #[command(hide = true)]
    PublishNode,
    #[command(hide = true)]
    JobExec {
        scope: String,
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
    #[command(hide = true)]
    Worker {
        job: String,
        #[arg(long, hide = true)]
        ready_event: Option<String>,
    },
}

#[derive(Debug, Args)]
struct ResultReadArgs {
    job: String,
    #[arg(
        long = "keep",
        hide = true,
        num_args = 0,
        default_value = "preserve",
        default_missing_value = "preserve"
    )]
    retention: transport::ResultRetention,
}

#[derive(Debug, Args)]
struct JobArgs {
    machine: String,
    #[arg(long)]
    submission: Option<String>,
    #[arg(long)]
    wait: bool,
    #[arg(last = true, required = true)]
    command: Vec<String>,
}

fn submit_job(
    args: JobArgs,
    submit: fn(
        &MachineName,
        transport::Submission,
        &Output,
    ) -> Result<ExitCode, transport::TransportError>,
    output: &Output,
) -> Result<ExitCode, transport::TransportError> {
    let machine = MachineName::try_from(args.machine)?;
    let submission = transport::Submission {
        id: args.submission.map(SubmissionId::try_from).transpose()?,
        command: JobCommand::try_from(args.command)?,
        wait: args.wait,
    };
    submit(&machine, submission, output)
}

#[derive(Debug, thiserror::Error)]
enum MainError {
    #[error(transparent)]
    Transport(#[from] transport::TransportError),
    #[error(transparent)]
    Chat(#[from] chat::cli::CliError),
    #[error(transparent)]
    Runner(#[from] chat::runner::RunnerError),
    #[error(transparent)]
    Mcp(#[from] mcp::McpError),
    #[error(transparent)]
    Invalid(#[from] domyjob_core::domain::Invalid),
    #[error(transparent)]
    App(#[from] app::AppError),
    #[error(transparent)]
    Dashboard(#[from] tui::TuiError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

fn run(command: Command) -> Result<ExitCode, MainError> {
    let output = raw::output();
    match command {
        Command::Tui { actor } => {
            tui::run(actor.as_deref(), output)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Chat(args) => Ok(chat::cli::run(args, &output)?),
        Command::Mcp { actor, turn } => {
            mcp::serve(output, actor.as_deref(), turn.as_deref())?;
            Ok(ExitCode::SUCCESS)
        }
        Command::ChatWorker { agent, ready_event } => {
            let ready_event = ready_event
                .map(process::ReadyToken::parse)
                .transpose()
                .map_err(app::AppError::from)?;
            chat::runner::worker(&agent, ready_event.as_ref())?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Doctor { machine } => {
            transport::doctor(&MachineName::try_from(machine)?, &output)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::On(args) => Ok(submit_job(args, transport::on, &output)?),
        Command::Run(args) => Ok(submit_job(args, transport::run, &output)?),
        Command::Ls { machine } => {
            transport::ls(&MachineName::try_from(machine)?, &output)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Clean { machine, job } => {
            let target = match job {
                Some(job) => CleanTarget::Job(JobId::try_from(job)?),
                None => CleanTarget::Finished,
            };
            transport::clean(&MachineName::try_from(machine)?, target, &output)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Status(ResultReadArgs { job, retention }) => {
            transport::status(&JobReference::try_from(job)?, retention, &output)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Wait(ResultReadArgs { job, retention }) => Ok(transport::wait(
            &JobReference::try_from(job)?,
            retention,
            &output,
        )?),
        Command::Kill { job } => Ok(transport::kill(&JobReference::try_from(job)?, &output)?),
        Command::Logs(ResultReadArgs { job, retention }) => {
            transport::logs(&JobReference::try_from(job)?, retention, &output)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Node { reap, reap_scope } => {
            if let Some(group) = reap {
                let scope = reap_scope
                    .map(process::resources::Scope::parse)
                    .transpose()?;
                process::reap(group.get(), scope.as_ref());
            } else {
                transport::node(&output)?;
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::PublishNode => Ok(transport::publish_node()?),
        Command::JobExec { scope, command } => {
            let scope = process::resources::Scope::parse(scope)?;
            let command = JobCommand::try_from(command)?;
            process::resources::exec(&scope, &command)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Worker { job, ready_event } => {
            let job = JobId::try_from(job)?;
            let ready_event = ready_event
                .map(process::ReadyToken::parse)
                .transpose()
                .map_err(app::AppError::from)?;
            app::worker(&job, ready_event.as_ref())?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn internal() -> bool {
    const INTERNAL_COMMAND_WORDS: usize = 2;
    let words: Vec<std::ffi::OsString> = std::env::args_os()
        .skip(1)
        .take(INTERNAL_COMMAND_WORDS)
        .collect();
    let word = |index: usize| words.get(index).and_then(|word| word.to_str());
    matches!(
        (word(0), word(1)),
        (
            Some("node" | "worker" | "chat-worker" | "job-exec" | "publish-node" | "mcp"),
            _
        ) | (Some("chat"), Some("serve"))
    )
}

fn main() -> ExitCode {
    let internal = internal();
    let command = Cli::parse().command.unwrap_or(Command::Tui { actor: None });
    let dashboard = matches!(command, Command::Tui { .. });
    if dashboard && !platform::terminal::interactive(&raw::output().into_stream()) {
        eprintln!("domyjob: {}", tui::TuiError::NotTerminal);
        return ExitCode::FAILURE;
    }
    let _build = match builds::hold_current() {
        Ok(held) => held,
        Err(error) => {
            eprintln!("domyjob: marking this build in use failed: {error}");
            None
        }
    };
    if !internal && !dashboard {
        match transport::refresh_local() {
            Ok(Some(code)) => return code,
            Ok(None) => {}
            Err(error) => {
                eprintln!("domyjob: {error}");
                return ExitCode::FAILURE;
            }
        }
    }
    match run(command) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("domyjob: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser as _;

    use super::{Cli, Command};

    #[test]
    fn job_reads_accept_keep_as_the_preserving_compatibility_form() {
        let job = "beta:00000000000000000000000000000001";
        for command in ["status", "wait", "logs"] {
            for arguments in [
                vec!["domyjob", command, "--keep", job],
                vec!["domyjob", command, job, "--keep"],
                vec!["domyjob", command, job],
            ] {
                let parsed = Cli::try_parse_from(&arguments);
                assert!(parsed.is_ok(), "{arguments:?}: {parsed:?}");
                assert!(matches!(
                    parsed.unwrap().command,
                    Some(Command::Status(args) | Command::Wait(args) | Command::Logs(args))
                        if args.job == job
                            && args.retention == super::transport::ResultRetention::Preserve
                ));
            }
            let help = Cli::try_parse_from(["domyjob", command, "--help"])
                .unwrap_err()
                .to_string();
            assert!(!help.contains("--keep"), "{help}");
        }
    }

    #[test]
    fn keep_is_only_a_zero_argument_result_read_option() {
        let job = "beta:00000000000000000000000000000001";
        for arguments in [
            vec!["domyjob", "status", "--keep=false", job],
            vec!["domyjob", "wait", "--keep=preserve", job],
            vec!["domyjob", "logs", "--keep", "false", job],
            vec!["domyjob", "logs", "--unknown", job],
            vec!["domyjob", "kill", "--keep", job],
            vec!["domyjob", "tui", "--keep"],
        ] {
            assert!(Cli::try_parse_from(&arguments).is_err(), "{arguments:?}");
        }
    }

    #[test]
    fn human_dashboard_is_the_default_and_has_an_explicit_command() {
        let default = Cli::try_parse_from(["domyjob"]).unwrap();
        assert!(default.command.is_none());
        let explicit = Cli::try_parse_from(["domyjob", "tui"]).unwrap();
        assert!(matches!(
            explicit.command,
            Some(Command::Tui { actor: None })
        ));
        let selected = Cli::try_parse_from(["domyjob", "tui", "--as", "alice"]).unwrap();
        assert!(matches!(
            selected.command,
            Some(Command::Tui { actor: Some(actor) }) if actor == "alice"
        ));
    }

    #[test]
    fn reaper_command_accepts_only_positive_groups() {
        let parsed = Cli::try_parse_from(["domyjob", "node", "--reap", "17"]);
        assert!(matches!(
            parsed,
            Ok(Cli {
                command: Some(Command::Node { reap: Some(_), .. })
            })
        ));
        for value in ["0", "-1", "2147483648", "invalid"] {
            let _error = Cli::try_parse_from(["domyjob", "node", "--reap", value])
                .expect_err("invalid process group");
        }
    }
}
