use std::io::Write;
use std::process::Stdio;

use serde::Serialize;

use crate::config::{Config, ConfigError};
use crate::domain::MachineName;
use crate::protocol::{Job, Phase, State};
use crate::template::{Arg, Bindings, TemplateError};

#[derive(Debug, thiserror::Error)]
pub enum NotifyError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error("notifier {notifier}: {source}")]
    Template {
        notifier: String,
        source: TemplateError,
    },
    #[error("notifier {notifier}: the command is empty")]
    Empty { notifier: String },
    #[error("notifier {notifier}: {program} failed: {detail}")]
    Failed {
        notifier: String,
        program: String,
        detail: String,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct Event<'a> {
    pub machine: &'a MachineName,
    pub state: &'static str,
    pub exit_code: Option<i32>,
    pub job: &'a Job,
}

#[must_use]
pub fn summary(machine: &MachineName, job: &Job) -> String {
    let label = job
        .spec
        .name
        .as_ref()
        .map_or_else(|| job.spec.command.headline(), ToString::to_string);
    let took = match &job.phase {
        Phase::Finished {
            started_at: Some(start),
            finished_at,
            ..
        } => {
            format!(" in {}", start.until(*finished_at))
        }
        Phase::Finished {
            started_at: None, ..
        }
        | Phase::Queued
        | Phase::Preparing { .. }
        | Phase::Running { .. } => String::new(),
    };
    let state = job.state();
    let code = match (state, job.exit_code()) {
        (State::Failed, Some(code)) => format!(" (exit {code})"),
        _ => String::new(),
    };
    format!("{label} {}{code} on {machine}{took}", state.as_str())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Argument {
    None,
    Config(crate::config::ConfigText),
    User(crate::input::UserText),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotifyTarget {
    notifier: String,
    argument: Argument,
}

impl NotifyTarget {
    #[must_use]
    pub fn from_config(text: &crate::config::ConfigText) -> Self {
        let (notifier, argument) = text.split_once(':');
        Self {
            notifier: notifier.as_config_str().to_owned(),
            argument: argument.map_or(Argument::None, Argument::Config),
        }
    }

    #[must_use]
    pub fn from_user(text: &crate::input::UserText) -> Self {
        let (notifier, argument) = text.split_once(':');
        Self {
            notifier: notifier.as_user_str().to_owned(),
            argument: argument.map_or(Argument::None, Argument::User),
        }
    }

    fn arg(&self) -> Arg {
        match &self.argument {
            Argument::None => Arg::literal(""),
            Argument::Config(text) => Arg::config(text),
            Argument::User(text) => Arg::user(text),
        }
    }
}

pub fn send(
    config: &Config,
    target: &NotifyTarget,
    machine: &MachineName,
    job: &Job,
) -> Result<(), NotifyError> {
    let state = job.state();
    let text = format!(
        "domyjob: {}\n",
        crate::terminal::neutralize(&summary(machine, job))
    );
    let json = serde_json::to_value(Event {
        machine,
        state: state.as_str(),
        exit_code: job.exit_code(),
        job,
    })
    .map_err(|e| failed(target.notifier.as_str(), "encoding", &e.to_string()))?;
    deliver(config, target, &text, &json)
}

#[derive(Debug, Clone, Copy)]
pub struct Lost<'a> {
    pub machine: &'a MachineName,
    pub reference: &'a str,
    pub why: &'a str,
}

pub fn lost(
    config: &Config,
    target: &NotifyTarget,
    Lost {
        machine,
        reference,
        why,
    }: Lost<'_>,
) -> Result<(), NotifyError> {
    let text = format!(
        "domyjob: lost track of {reference} on {machine} ({}); it may still be running there\n",
        crate::terminal::neutralize(why)
    );
    let json = serde_json::json!({
        "machine": machine,
        "state": "lost",
        "job": reference,
        "why": why,
    });
    deliver(config, target, &text, &json)
}

fn deliver(
    config: &Config,
    target: &NotifyTarget,
    text: &str,
    json: &serde_json::Value,
) -> Result<(), NotifyError> {
    let name = target.notifier.as_str();
    let notifier = config.notifier(name)?;
    let argv = notifier
        .command()
        .client()
        .render(&Bindings::new().with("target", target.arg()))
        .map_err(|source| NotifyError::Template {
            notifier: name.to_owned(),
            source,
        })?;
    let Some(invocation) = crate::spawn::Invocation::from_words(argv) else {
        return Err(NotifyError::Empty {
            notifier: name.to_owned(),
        });
    };
    let program = invocation.display();
    let program = program.as_str();
    let payload = match notifier.stdin {
        crate::config::StdinFormat::Text => text.as_bytes().to_vec(),
        crate::config::StdinFormat::Json => {
            serde_json::to_vec(json).map_err(|e| failed(name, program, &e.to_string()))?
        }
    };
    let mut child = invocation
        .command()
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| failed(name, program, &e.to_string()))?;
    if let Some(mut stdin) = child.stdin.take() {
        match stdin.write_all(&payload) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {}
            Err(e) => return Err(failed(name, program, &e.to_string())),
        }
    }
    let out = child
        .wait_with_output()
        .map_err(|e| failed(name, program, &e.to_string()))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(failed(
            name,
            program,
            String::from_utf8_lossy(&out.stderr).trim(),
        ))
    }
}

fn failed(notifier: &str, program: &str, detail: &str) -> NotifyError {
    NotifyError::Failed {
        notifier: notifier.to_owned(),
        program: program.to_owned(),
        detail: detail.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::Timestamp;
    use crate::domain::{Concurrency, JobId};
    use crate::protocol::{Command, Location, Outcome, Spec, Supervisor};

    fn finished(outcome: Outcome) -> Job {
        Job {
            spec: Spec {
                id: JobId::generate().unwrap(),
                name: None,
                command: Command::Script("cargo test".into()),
                location: Location::Home,
                env_names: std::collections::BTreeSet::new(),
                shell: None,
                concurrency: Concurrency::DEFAULT,
                sequence: 1,
                submitted_by: crate::authz::Submitter::Owner,
                submitted_at: Timestamp::at_millis(0),
            },
            phase: Phase::Finished {
                started_at: Some(Timestamp::at_millis(1000)),
                finished_at: Timestamp::at_millis(13_000),
                outcome,
            },
            supervisor: Supervisor::Alive,
            behind: Vec::new(),
        }
    }

    #[test]
    fn summaries_read_as_sentences() {
        let machine: MachineName = "linux".parse().unwrap();
        assert_eq!(
            summary(&machine, &finished(Outcome::Succeeded)),
            "cargo test succeeded on linux in 12s"
        );
        assert_eq!(
            summary(&machine, &finished(Outcome::Failed { exit_code: 101 })),
            "cargo test failed (exit 101) on linux in 12s"
        );
    }

    #[test]
    fn notifiers_are_user_defined_commands() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("event.json");
        let quoted = |text: String| serde_json::Value::String(text).to_string();
        let posix = quoted(out.display().to_string());
        let windows = quoted(format!("findstr . > \"{}\"", out.display()));
        let config = Config::layered(
            &format!(
                "[notifiers.file]\nstdin = \"json\"\nrun = [\"sh\", \"-c\", \"cat > \\\"$0\\\"\", {posix}]\nrun_windows = [\"cmd\", \"/c\", {windows}]\n"
            ),
            "t",
        )
        .unwrap();
        let machine: MachineName = "box".parse().unwrap();
        let target = |name: &str| {
            NotifyTarget::from_user(&crate::input::UserText::from_cli(name.to_owned()))
        };
        send(
            &config,
            &target("file"),
            &machine,
            &finished(Outcome::Succeeded),
        )
        .unwrap();
        let written: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&out).unwrap()).unwrap();
        assert_eq!(written.get("state"), Some(&serde_json::json!("succeeded")));
        assert_eq!(written.get("exit_code"), Some(&serde_json::json!(0)));
        assert!(matches!(
            send(
                &config,
                &target("nope"),
                &machine,
                &finished(Outcome::Killed)
            ),
            Err(NotifyError::Config(_))
        ));
    }
}
