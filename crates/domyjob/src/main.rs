use std::process::ExitCode;
use std::str::FromStr;

use clap::{Args, Parser, Subcommand};
use domyjob_core::domain::{Command as JobCommand, JobId, JobReference, MachineName, SubmissionId};
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
mod watch_event;
mod workspace;

#[derive(Debug, Parser)]
#[command(name = "domyjob", about = "Run persistent jobs over SSH")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Clone, Copy)]
struct ReaperGroup(std::num::NonZeroI32);

impl FromStr for ReaperGroup {
    type Err = &'static str;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let group: i32 = text.parse().map_err(|_invalid| "invalid process group")?;
        let group = std::num::NonZeroI32::new(group).ok_or("invalid process group")?;
        if group.get() < 0 {
            return Err("invalid process group");
        }
        Ok(Self(group))
    }
}

#[derive(Debug, Subcommand)]
enum Command {
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
    Status {
        job: String,
    },
    Wait {
        job: String,
    },
    Kill {
        job: String,
    },
    Logs {
        job: String,
    },
    #[command(hide = true)]
    Node {
        #[arg(long, hide = true)]
        reap: Option<ReaperGroup>,
    },
    #[command(hide = true)]
    Worker {
        job: String,
        #[arg(long, hide = true)]
        ready_event: Option<String>,
    },
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
    Io(#[from] std::io::Error),
}

fn run(command: Command) -> Result<ExitCode, MainError> {
    let output = raw::output();
    match command {
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
        Command::Status { job } => {
            transport::status(&JobReference::try_from(job)?, &output)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Wait { job } => Ok(transport::wait(&JobReference::try_from(job)?, &output)?),
        Command::Kill { job } => Ok(transport::kill(&JobReference::try_from(job)?, &output)?),
        Command::Logs { job } => {
            transport::logs(&JobReference::try_from(job)?, &output)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Node { reap } => {
            if let Some(group) = reap {
                process::reap(group.0.get());
            } else {
                transport::node(&output)?;
            }
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
    let words: Vec<std::ffi::OsString> = std::env::args_os().skip(1).take(2).collect();
    let word = |index: usize| words.get(index).and_then(|word| word.to_str());
    matches!(
        (word(0), word(1)),
        (Some("node" | "worker" | "chat-worker" | "mcp"), _) | (Some("chat"), Some("serve"))
    )
}

fn main() -> ExitCode {
    let internal = internal();
    let _build = match builds::hold_current() {
        Ok(held) => held,
        Err(error) => {
            eprintln!("domyjob: marking this build in use failed: {error}");
            None
        }
    };
    if !internal {
        match transport::refresh_local() {
            Ok(Some(code)) => return code,
            Ok(None) => {}
            Err(error) => {
                eprintln!("domyjob: {error}");
                return ExitCode::FAILURE;
            }
        }
    }
    match run(Cli::parse().command) {
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
    fn reaper_command_accepts_only_positive_groups() {
        let parsed = Cli::try_parse_from(["domyjob", "node", "--reap", "17"]);
        assert!(matches!(
            parsed,
            Ok(Cli {
                command: Command::Node { reap: Some(_) }
            })
        ));
        for value in ["0", "-1", "2147483648", "invalid"] {
            let _error = Cli::try_parse_from(["domyjob", "node", "--reap", value])
                .expect_err("invalid process group");
        }
    }
}
