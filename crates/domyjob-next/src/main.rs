use std::process::ExitCode;
use std::str::FromStr;

use clap::{Parser, Subcommand};
use domyjob_core::domain::{Command as JobCommand, JobId, JobReference, MachineName, SubmissionId};
use domyjob_core::wire::CleanTarget;

mod app;
mod identity;
mod source_fingerprint;
mod store;
mod transport;

#[derive(Debug, Parser)]
#[command(name = "domyjob-next", about = "Run persistent jobs over SSH")]
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
    Doctor {
        machine: String,
    },
    On {
        machine: String,
        #[arg(long)]
        submission: Option<String>,
        #[arg(long)]
        wait: bool,
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
    Run {
        machine: String,
        #[arg(long)]
        submission: Option<String>,
        #[arg(long)]
        wait: bool,
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
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

fn run(command: Command) -> Result<ExitCode, transport::TransportError> {
    match command {
        Command::Doctor { machine } => {
            transport::doctor(&MachineName::try_from(machine)?)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::On {
            machine,
            submission,
            wait,
            command,
        } => {
            let machine = MachineName::try_from(machine)?;
            let command = JobCommand::try_from(command)?;
            let submission = submission.map(SubmissionId::try_from).transpose()?;
            transport::on(&machine, submission, command, wait)
        }
        Command::Run {
            machine,
            submission,
            wait,
            command,
        } => {
            let machine = MachineName::try_from(machine)?;
            let command = JobCommand::try_from(command)?;
            let submission = submission.map(SubmissionId::try_from).transpose()?;
            transport::run(&machine, submission, command, wait)
        }
        Command::Ls { machine } => {
            transport::ls(&MachineName::try_from(machine)?)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Clean { machine, job } => {
            let target = match job {
                Some(job) => CleanTarget::Job(JobId::try_from(job)?),
                None => CleanTarget::Finished,
            };
            transport::clean(&MachineName::try_from(machine)?, target)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Status { job } => {
            transport::status(&JobReference::try_from(job)?)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Wait { job } => transport::wait(&JobReference::try_from(job)?),
        Command::Kill { job } => transport::kill(&JobReference::try_from(job)?),
        Command::Logs { job } => {
            transport::logs(&JobReference::try_from(job)?)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Node { reap } => {
            if let Some(group) = reap {
                domyjob::proc::reap(group.0.get());
            } else {
                transport::node()?;
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Worker { job, ready_event } => {
            let job = JobId::try_from(job)?;
            let ready_event = ready_event
                .map(domyjob::domain::BlobId::try_from)
                .transpose()
                .map_err(app::AppError::from)?;
            app::worker(&job, ready_event.as_ref())?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn main() -> ExitCode {
    let internal = std::env::args_os()
        .nth(1)
        .as_deref()
        .is_some_and(|command| {
            command == std::ffi::OsStr::new("node") || command == std::ffi::OsStr::new("worker")
        });
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
        let parsed = Cli::try_parse_from(["domyjob-next", "node", "--reap", "17"]);
        assert!(matches!(
            parsed,
            Ok(Cli {
                command: Command::Node { reap: Some(_) }
            })
        ));
        for value in ["0", "-1", "2147483648", "invalid"] {
            let _error = Cli::try_parse_from(["domyjob-next", "node", "--reap", value])
                .expect_err("invalid process group");
        }
    }
}
