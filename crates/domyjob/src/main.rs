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
mod chat_retirement;
#[path = "platform/file_kind.rs"]
mod file_kind;
mod formats;
mod identity;
mod job_cache;
mod layout;
mod lock;
mod output;
mod platform;
mod process;
mod service;
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

#[derive(Debug, Subcommand)]
enum Command {
    #[command(about = "Manage the macOS login service and retire chat integrations")]
    Service(service::ServiceArgs),
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
        #[arg(long, help = "Keep the completed result and logs after reading")]
        keep: bool,
    },
    Wait {
        job: String,
        #[arg(long, help = "Keep the completed result and logs after reading")]
        keep: bool,
    },
    Kill {
        job: String,
    },
    Logs {
        job: String,
        #[arg(long, help = "Keep the completed result and logs after reading")]
        keep: bool,
    },
    #[command(hide = true)]
    PublishNode,
    #[command(hide = true)]
    Node {
        #[arg(long, hide = true)]
        reap: Option<ReaperGroup>,
        #[arg(long, hide = true, requires = "reap")]
        reap_scope: Option<String>,
        #[arg(long, hide = true, requires = "reap")]
        reap_marker: Option<std::path::PathBuf>,
    },
    #[command(hide = true)]
    JobExec {
        scope: String,
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
    #[command(hide = true)]
    JobReap {
        name: String,
        marker: std::path::PathBuf,
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
    #[arg(
        long,
        requires = "wait",
        help = "Keep the completed result and logs after reading"
    )]
    keep: bool,
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
        retention: result_retention(args.keep),
    };
    submit(&machine, submission, output)
}

const fn result_retention(keep: bool) -> transport::ResultRetention {
    if keep {
        transport::ResultRetention::Preserve
    } else {
        transport::ResultRetention::Consume
    }
}

#[derive(Debug, thiserror::Error)]
enum MainError {
    #[error(transparent)]
    Transport(#[from] transport::TransportError),
    #[error(transparent)]
    Service(#[from] service::ServiceError),
    #[error(transparent)]
    Invalid(#[from] domyjob_core::domain::Invalid),
    #[error(transparent)]
    App(#[from] app::AppError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

fn run_node(
    reap: Option<ReaperGroup>,
    scope: Option<String>,
    marker: Option<&std::path::Path>,
    output: &Output,
) -> Result<ExitCode, MainError> {
    if let Some(group) = reap {
        let scope = scope.map(process::resources::Scope::parse).transpose()?;
        process::reap(group.get(), scope.as_ref(), marker)?;
    } else {
        transport::node(output)?;
    }
    Ok(ExitCode::SUCCESS)
}

fn run(command: Command) -> Result<ExitCode, MainError> {
    let output = raw::output();
    match command {
        Command::Service(args) => {
            service::run(&args, &output)?;
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
            let target = clean_target(job)?;
            transport::clean(&MachineName::try_from(machine)?, target, &output)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Status { job, keep } => {
            transport::status(
                &JobReference::try_from(job)?,
                result_retention(keep),
                &output,
            )?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Wait { job, keep } => Ok(transport::wait(
            &JobReference::try_from(job)?,
            result_retention(keep),
            &output,
        )?),
        Command::Kill { job } => Ok(transport::kill(&JobReference::try_from(job)?, &output)?),
        Command::Logs { job, keep } => {
            transport::logs(
                &JobReference::try_from(job)?,
                result_retention(keep),
                &output,
            )?;
            Ok(ExitCode::SUCCESS)
        }
        Command::PublishNode => Ok(transport::publish_node()?),
        Command::Node {
            reap,
            reap_scope,
            reap_marker,
        } => run_node(reap, reap_scope, reap_marker.as_deref(), &output),
        Command::JobReap { name, marker } => {
            process::reap_owned(&name, &marker)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::JobExec { scope, command } => {
            let scope = process::resources::Scope::parse(scope)?;
            let command = JobCommand::try_from(command)?;
            process::resources::exec(&scope, &command)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Worker { job, ready_event } => {
            let job = JobId::try_from(job)?;
            let ready_event = readiness(ready_event)?;
            app::worker(&job, ready_event.as_ref())?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn readiness(value: Option<String>) -> Result<Option<process::ReadyToken>, MainError> {
    Ok(value
        .map(process::ReadyToken::parse)
        .transpose()
        .map_err(app::AppError::from)?)
}

fn clean_target(job: Option<String>) -> Result<CleanTarget, MainError> {
    match job {
        Some(job) => Ok(CleanTarget::Job(JobId::try_from(job)?)),
        None => Ok(CleanTarget::Finished),
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
            Some("node" | "publish-node" | "worker" | "job-exec" | "job-reap"),
            _
        ) | (Some("service"), Some("serve"))
    )
}

fn main() -> ExitCode {
    let internal = internal();
    let command = Cli::parse().command;
    let _build = match builds::hold_current() {
        Ok(held) => held,
        Err(error) => {
            eprintln!("domyjob: marking this build in use failed: {error}");
            return ExitCode::FAILURE;
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
    fn retired_agent_chat_entry_points_are_rejected() {
        for arguments in [
            vec!["domyjob", "chat", "directory"],
            vec!["domyjob", "mcp"],
            vec!["domyjob", "chat-worker", "--agent", "reviewer"],
            vec!["domyjob", "tui"],
        ] {
            assert!(
                Cli::try_parse_from(&arguments).is_err(),
                "retired entry point was accepted: {arguments:?}"
            );
        }
    }

    #[test]
    fn completed_results_can_be_kept_explicitly_after_retrieval() {
        for operation in ["status", "wait", "logs"] {
            let parsed =
                Cli::try_parse_from(["domyjob", operation, "linux:JOB", "--keep"]).unwrap();
            assert!(matches!(
                parsed.command,
                Command::Status { keep: true, .. }
                    | Command::Wait { keep: true, .. }
                    | Command::Logs { keep: true, .. }
            ));
        }
        let parsed = Cli::try_parse_from([
            "domyjob", "run", "linux", "--wait", "--keep", "--", "cargo", "test",
        ])
        .unwrap();
        assert!(matches!(
            parsed.command,
            Command::Run(super::JobArgs {
                keep: true,
                wait: true,
                ..
            })
        ));
        Cli::try_parse_from(["domyjob", "run", "linux", "--keep", "--", "cargo", "test"])
            .unwrap_err();
    }

    #[test]
    fn reaper_command_accepts_only_positive_groups() {
        let parsed = Cli::try_parse_from(["domyjob", "node", "--reap", "17"]);
        assert!(matches!(
            parsed,
            Ok(Cli {
                command: Command::Node { reap: Some(_), .. }
            })
        ));
        for value in ["0", "-1", "2147483648", "invalid"] {
            let _error = Cli::try_parse_from(["domyjob", "node", "--reap", value])
                .expect_err("invalid process group");
        }
    }
}
