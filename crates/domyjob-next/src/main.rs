use std::process::ExitCode;

use clap::{Parser, Subcommand};
use domyjob_core::domain::MachineName;

mod transport;

#[derive(Debug, Parser)]
#[command(name = "domyjob-next", about = "SSH job runner under construction")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Doctor {
        machine: String,
    },
    #[command(hide = true)]
    Node,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Doctor { machine } => MachineName::try_from(machine)
            .map_err(transport::TransportError::from)
            .and_then(|machine| transport::doctor(&machine)),
        Command::Node => transport::node(),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("domyjob: {error}");
            ExitCode::FAILURE
        }
    }
}
