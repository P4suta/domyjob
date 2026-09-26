use std::path::PathBuf;
use std::process::ExitCode;

use crate::client::{self, ClientError, Context, Order, Sending};
use crate::config::ConfigError;
use crate::domain::Revision;
use crate::notify::NotifyTarget;
use crate::protocol::Workspace;
use crate::snapshot::{self, Detected};
use crate::template::Arg;

const GIT_POST_RECEIVE: &str = r#"#!/bin/sh
zero=0000000000000000000000000000000000000000
while read -r old new ref; do
  [ "$new" = "$zero" ] && continue
  nohup domyjob trigger --event push --source git --root "$PWD" --ref "$ref" --rev "$new" \
    >>"${DOMYJOB_TRIGGER_LOG:-/dev/null}" 2>&1 &
done
"#;

#[must_use]
pub fn script(source: &str) -> Option<&'static str> {
    match source {
        "git" => Some(GIT_POST_RECEIVE),
        _ => None,
    }
}

#[derive(Debug, Clone, clap::Args)]
pub struct Event {
    #[arg(long = "event", help = "What happened, such as push")]
    pub kind: String,
    #[arg(long = "ref", help = "The ref it happened to, such as refs/heads/main")]
    pub reference: String,
    #[arg(long, help = "The revision to send")]
    pub rev: Revision,
    #[arg(
        long,
        help = "The version control source the revision lives in, such as git"
    )]
    pub source: String,
    #[arg(long, help = "The repository")]
    pub root: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Consideration {
    Accepts,
    OtherSource,
    OtherEvent,
    OtherRepository,
    OtherRef,
}

fn consider(
    rule: &crate::config::TriggerConf,
    fired: &Event,
    root: &std::path::Path,
) -> Consideration {
    if rule.source != fired.source {
        return Consideration::OtherSource;
    }
    if rule.event != fired.kind {
        return Consideration::OtherEvent;
    }
    let same_repository = match std::fs::canonicalize(&rule.repository) {
        Ok(repository) => repository == root,
        Err(_missing) => false,
    };
    if !same_repository {
        return Consideration::OtherRepository;
    }
    if rule
        .refs
        .iter()
        .any(|pattern| crate::project::glob(pattern, &fired.reference))
    {
        Consideration::Accepts
    } else {
        Consideration::OtherRef
    }
}

struct Firing<'a> {
    name: &'a str,
    rule: &'a crate::config::TriggerConf,
    fired: &'a Event,
    root: &'a std::path::Path,
}

fn fire(ctx: &Context, firing: &Firing<'_>) -> Result<(), ClientError> {
    let Firing {
        name,
        rule,
        fired,
        root,
    } = *firing;
    let root = root.to_path_buf();
    let conf = ctx
        .config
        .sources
        .get(&rule.source)
        .ok_or_else(|| ConfigError::Missing {
            kind: "source",
            name: rule.source.clone(),
        })?;
    let detected = Detected {
        name: &rule.source,
        source: conf,
        root: root.clone(),
    };
    let snapshot = snapshot::from_revision(&detected, &fired.rev)?;
    let prepared = client::prepared(ctx, &root, snapshot, None)?;
    let order = Order {
        queue: crate::protocol::Queue::Slot,
        targets: rule.on.clone(),
        words: rule.run.iter().map(Arg::config).collect(),
        runner: rule.runner.clone(),
        rev: Some(fired.rev.clone()),
        sending: Sending::Directory,
        workspace: Workspace::Fresh,
        start: root.clone(),
        root: Some(root),
        env: std::collections::BTreeMap::new(),
        shell: None,
        name: match name.parse() {
            Ok(job_name) => Some(job_name),
            Err(_not_a_job_name) => None,
        },
    };
    let (submitted, rejected) = {
        let board = crate::board::Board::new(false);
        client::submit_prepared(ctx, &order, Some(prepared), &|machine, stage| {
            board.stage(machine, stage);
        })?
    };
    for item in &rejected {
        eprintln!(
            "domyjob: trigger {name} on {}: {}",
            item.machine, item.error
        );
    }
    let targets: Vec<NotifyTarget> = rule
        .notify
        .iter()
        .flatten()
        .map(NotifyTarget::from_config)
        .collect();
    for item in &submitted {
        println!(
            "domyjob: trigger {name} started {}:{}",
            item.machine.name, item.job.spec.id
        );
        let (machine, job) =
            client::wait(ctx, &format!("{}:{}", item.machine.name, item.job.spec.id))?;
        for target in &targets {
            if let Err(error) = crate::notify::send(&ctx.config, target, &machine.name, &job) {
                eprintln!("domyjob: {error}");
            }
        }
    }
    Ok(())
}

pub fn trigger(fired: &Event) -> Result<ExitCode, ClientError> {
    let ctx = Context::load()?;
    let root = std::fs::canonicalize(&fired.root).map_err(|e| {
        ClientError::Io(crate::failure::IoFailure {
            action: "resolving",
            path: fired.root.clone(),
            source: e,
        })
    })?;
    let matching: Vec<(&String, &crate::config::TriggerConf)> = ctx
        .config
        .triggers
        .iter()
        .filter(|(_, rule)| matches!(consider(rule, fired, &root), Consideration::Accepts))
        .collect();
    if matching.is_empty() {
        eprintln!(
            "domyjob: no local trigger accepts {} {} in {}",
            fired.kind,
            fired.reference,
            root.display()
        );
        return Ok(ExitCode::SUCCESS);
    }
    for (name, rule) in matching {
        fire(
            &ctx,
            &Firing {
                name,
                rule,
                fired,
                root: &root,
            },
        )?;
    }
    Ok(ExitCode::SUCCESS)
}
