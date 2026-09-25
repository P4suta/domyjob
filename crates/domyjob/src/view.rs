use std::fmt::Write as _;
use std::io::IsTerminal as _;

use crate::clock::Timestamp;
use crate::domain::MachineName;
use crate::protocol::{Digest, Job, State};
use crate::ui::{self, Symbol, Tone};

#[must_use]
pub fn stdout_is_a_person() -> bool {
    std::io::stdout().is_terminal()
}

fn reference(machine: &MachineName, job: &Job) -> String {
    let id = job.spec.id.as_str();
    format!("{machine}:{}", id.get(..ui::SHORT_ID).unwrap_or(id))
}

fn next_step(machine: &MachineName, job: &Job) -> Option<String> {
    let target = reference(machine, job);
    match job.state() {
        State::Failed => Some(format!(
            "domyjob logs {target} --grep 'error|panicked|FAILED' --context 3"
        )),
        State::Running | State::Preparing | State::Queued => {
            Some(format!("domyjob logs {target} -f"))
        }
        State::Succeeded | State::Errored | State::Killed | State::Lost => None,
    }
}

pub fn job_detail(machine: &MachineName, job: &Job) -> Result<String, std::fmt::Error> {
    let mut out = job_header(machine, job)?;
    hint(&mut out, machine, job)?;
    Ok(out)
}

fn hint(out: &mut String, machine: &MachineName, job: &Job) -> std::fmt::Result {
    if let Some(step) = next_step(machine, job) {
        writeln!(
            out,
            "  {} {step}",
            ui::paint(Tone::Hint, ui::symbol(Symbol::Hint))
        )?;
    }
    Ok(())
}

fn job_header(machine: &MachineName, job: &Job) -> Result<String, std::fmt::Error> {
    let now = Timestamp::observe();
    let id = job.spec.id.as_str();
    let mut out = ui::job_line(machine, job, (&[id], ui::Columns::default()), Some(now));
    out.push('\n');
    let command = job.spec.command.display();
    writeln!(out, "  {}", ui::paint(Tone::Dim, &command))?;
    if let Some(reason) = job.reason() {
        writeln!(out, "  {}", ui::paint(Tone::Bad, reason.as_raw_str()))?;
    }
    Ok(out)
}

pub fn digest(machine: &MachineName, digest: &Digest) -> Result<String, std::fmt::Error> {
    let job = &digest.job;
    let mut out = job_header(machine, job)?;
    let shown = crate::domain::len_u64(digest.tail.len());
    let summary = if shown < digest.lines {
        format!(
            "log {} lines · {} · the last {shown}",
            digest.lines,
            ui::bytes(digest.bytes)
        )
    } else {
        format!("log {} lines · {}", digest.lines, ui::bytes(digest.bytes))
    };
    writeln!(out, "  {}", ui::paint(Tone::Dim, &summary))?;
    let gutter = ui::paint(Tone::Dim, ui::symbol(Symbol::Gutter));
    for line in &digest.tail {
        writeln!(out, "  {gutter} {line}")?;
    }
    hint(&mut out, machine, job)?;
    Ok(out)
}

pub fn listing(
    jobs: &[(MachineName, Job)],
    unreachable: &[(MachineName, String)],
) -> Result<String, std::fmt::Error> {
    let now = Timestamp::observe();
    let ids: Vec<&str> = jobs.iter().map(|(_, job)| job.spec.id.as_str()).collect();
    let rows: Vec<(&MachineName, &Job)> = jobs.iter().map(|(m, j)| (m, j)).collect();
    let columns = ui::Columns::of(&rows);
    let mut out = String::new();
    let mut current: Option<&MachineName> = None;
    for (machine, job) in jobs {
        if current != Some(machine) {
            if current.is_some() {
                out.push('\n');
            }
            let count = jobs.iter().filter(|(m, _)| m == machine).count();
            writeln!(
                out,
                "{}  {}",
                ui::machine(machine),
                ui::paint(
                    Tone::Dim,
                    &format!("{count} {}", if count == 1 { "job" } else { "jobs" })
                )
            )?;
            current = Some(machine);
        }
        writeln!(
            out,
            "  {}",
            ui::job_line(machine, job, (&ids, columns), Some(now))
        )?;
    }
    if jobs.is_empty() && unreachable.is_empty() {
        writeln!(
            out,
            "{}",
            ui::paint(
                Tone::Dim,
                "No jobs yet. `domyjob run MACHINE -- COMMAND` starts one."
            )
        )?;
    }
    for (machine, why) in unreachable {
        writeln!(
            out,
            "{} {} unreachable: {}  {} domyjob doctor {machine}",
            ui::paint(Tone::Bad, "!"),
            ui::machine(machine),
            ui::fit(why, 80),
            ui::paint(Tone::Hint, ui::symbol(Symbol::Hint))
        )?;
    }
    Ok(out)
}

pub fn final_line(machine: &MachineName, job: &Job) -> Result<String, std::fmt::Error> {
    let id = job.spec.id.as_str();
    let mut out = ui::job_line(machine, job, (&[id], ui::Columns::default()), None);
    if let Some(reason) = job.reason() {
        write!(out, "\n  {}", ui::paint(Tone::Bad, reason.as_raw_str()))?;
    }
    if let Some(step) = next_step(machine, job).filter(|_| !job.succeeded()) {
        write!(
            out,
            "\n  {} {step}",
            ui::paint(Tone::Hint, ui::symbol(Symbol::Hint))
        )?;
    }
    Ok(out)
}

pub fn first_run() -> Result<String, std::fmt::Error> {
    let mut out = String::new();
    writeln!(out, "{}", ui::paint(Tone::Strong, "No machines yet."))?;
    writeln!(out, "  Any host your ssh config knows works as it is:")?;
    writeln!(
        out,
        "    {}",
        ui::paint(Tone::Hint, "domyjob run myhost -- uname -a")
    )?;
    writeln!(out, "  Or give one a name and labels:")?;
    writeln!(
        out,
        "    {}",
        ui::paint(Tone::Hint, "domyjob machines add linux --label gpu")
    )?;
    writeln!(
        out,
        "  Without ssh: {} there, then {} here",
        ui::paint(Tone::Hint, "domyjob serve --pair"),
        ui::paint(Tone::Hint, "domyjob pair WORDS")
    )?;
    Ok(out)
}

#[derive(Debug)]
pub struct Seen<'a> {
    pub name: &'a MachineName,
    pub transport: &'a str,
    pub host: &'a str,
    pub labels: &'a [String],
    pub facts: Option<&'a crate::remote::Facts>,
}

pub fn machines(rows: &[Seen<'_>]) -> Result<String, std::fmt::Error> {
    if rows.is_empty() {
        return first_run();
    }
    let mut table = comfy_table::Table::new();
    table
        .load_style(comfy_table::presets::NOTHING)
        .set_header(["NAME", "OS", "ARCH", "DOMYJOB", "VIA", "HOST", "LABELS"]);
    let mut stale = Vec::new();
    for row in rows {
        let (os, arch, version) = row.facts.map_or_else(
            || {
                (
                    "-".to_owned(),
                    "-".to_owned(),
                    ui::paint(Tone::Dim, "not contacted"),
                )
            },
            |facts| {
                let version = facts.hello.version.as_raw_str();
                let shown = if version == crate::protocol::VERSION {
                    version.to_owned()
                } else {
                    stale.push(row.name.clone());
                    ui::paint(Tone::Busy, &format!("{version} !"))
                };
                (
                    facts.hello.os.as_raw_str().to_owned(),
                    facts.hello.arch.as_raw_str().to_owned(),
                    shown,
                )
            },
        );
        table.add_row([
            ui::machine(row.name),
            os,
            arch,
            version,
            row.transport.to_owned(),
            row.host.to_owned(),
            row.labels.join(", "),
        ]);
    }
    let mut out = format!("{table}\n");
    for name in stale {
        writeln!(
            out,
            "{} {} runs another version; domyjob setup {name}",
            ui::paint(Tone::Hint, ui::symbol(Symbol::Hint)),
            ui::machine(&name)
        )?;
    }
    Ok(out)
}

pub fn doctor_local(protection: &str, fits: bool, state: &str) -> Result<String, std::fmt::Error> {
    let mut out = String::new();
    writeln!(out, "{}", ui::paint(Tone::Strong, "This machine"))?;
    writeln!(
        out,
        "  {} key     {protection}",
        ui::paint(Tone::Good, ui::symbol(Symbol::Succeeded))
    )?;
    if fits {
        writeln!(
            out,
            "  {} state   {}",
            ui::paint(Tone::Good, ui::symbol(Symbol::Succeeded)),
            ui::paint(Tone::Dim, state)
        )?;
    } else {
        writeln!(
            out,
            "  {} state   {state} is too long for local sockets; set DOMYJOB_STATE to a shorter directory",
            ui::paint(Tone::Bad, ui::symbol(Symbol::Failed))
        )?;
    }
    writeln!(out, "{}", ui::paint(Tone::Strong, "Machines"))?;
    Ok(out)
}

pub fn doctor_reached(
    name: &MachineName,
    facts: &crate::remote::Facts,
    widest: usize,
) -> Result<String, std::fmt::Error> {
    let mut out = String::new();
    let hello = &facts.hello;
    writeln!(
        out,
        "  {} {}{}  {:<16}  {}  {}",
        ui::paint(Tone::Good, ui::symbol(Symbol::Succeeded)),
        ui::machine(name),
        " ".repeat(widest.saturating_sub(name.as_str().len())),
        format!("{}/{}", hello.os, hello.arch),
        hello.version,
        ui::paint(Tone::Dim, hello.shell.as_raw_str())
    )?;
    Ok(out)
}

pub fn doctor_failed(
    name: &MachineName,
    error: &str,
    hint: Option<&str>,
    widest: usize,
) -> Result<String, std::fmt::Error> {
    let mut out = String::new();
    writeln!(
        out,
        "  {} {}{}  {}",
        ui::paint(Tone::Bad, ui::symbol(Symbol::Failed)),
        ui::machine(name),
        " ".repeat(widest.saturating_sub(name.as_str().len())),
        error
    )?;
    if let Some(hint) = hint {
        writeln!(
            out,
            "    {} {}",
            ui::paint(Tone::Hint, &format!("{} try:", ui::symbol(Symbol::Hint))),
            hint
        )?;
    }
    Ok(out)
}

pub const JSON_SCHEMA: u32 = 2;

#[must_use]
pub fn job_summary_json(machine: &MachineName, job: &Job) -> serde_json::Value {
    serde_json::json!({
        "schema": JSON_SCHEMA,
        "job": format!("{machine}:{}", job.spec.id),
        "machine": machine,
        "state": job.state().as_str(),
        "exit_code": job.exit_code(),
        "name": job.spec.name,
        "command": job.spec.command.display(),
        "reason": job.reason().map(|reason| reason.as_raw_str().to_owned()),
        "behind": job.behind.iter().map(|holder| format!("{machine}:{holder}")).collect::<Vec<_>>(),
    })
}

#[must_use]
pub fn job_json(machine: &MachineName, job: &Job) -> serde_json::Value {
    let mut value = job_summary_json(machine, job);
    if let Some(fields) = value.as_object_mut() {
        fields.insert(
            "detail".to_owned(),
            match serde_json::to_value(job) {
                Ok(detail) => detail,
                Err(_unencodable) => serde_json::Value::Null,
            },
        );
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Command, Location, Phase, Spec, Supervisor};

    #[test]
    fn the_last_word_on_a_finished_job_says_how_long_it_took_not_how_long_ago() {
        let mut job = sample();
        job.phase = Phase::Finished {
            started_at: Some(Timestamp::at_millis(1_000)),
            finished_at: Timestamp::at_millis(183_000),
            outcome: crate::protocol::Outcome::Succeeded,
        };
        let machine: MachineName = "win".parse().unwrap();
        let last = crate::terminal::clean(&final_line(&machine, &job).unwrap());
        assert!(last.contains("succeeded · after 3m02s"), "{last}");
        assert!(!last.contains("ago"), "{last}");
        let listed = crate::terminal::clean(&listing(&[(machine, job)], &[]).unwrap());
        assert!(listed.contains("3m02s · "), "{listed}");
        assert!(listed.contains("just now"), "{listed}");
        assert!(listed.starts_with("win  1 job\n"), "{listed}");
    }

    #[test]
    fn a_queued_job_says_how_many_it_waits_behind() {
        let mut job = sample();
        assert_eq!(crate::board::queued_behind(&job), None);
        job.behind = vec!["0AAAAAAAAAAAAAAA".parse().unwrap()];
        assert_eq!(
            crate::board::queued_behind(&job).unwrap(),
            "queued behind 1 running job: 0AAAAAAA"
        );
        job.behind.push("0BBBBBBBBBBBBBBB".parse().unwrap());
        assert_eq!(
            crate::board::queued_behind(&job).unwrap(),
            "queued behind 2 running jobs: 0AAAAAAA, 0BBBBBBB"
        );
    }

    fn sample() -> Job {
        Job {
            spec: Spec {
                id: "0AAAAAAAAAAAAAAA".parse().unwrap(),
                name: None,
                command: Command::Script("true".into()),
                location: Location::Home,
                env_names: std::collections::BTreeSet::new(),
                shell: None,
                concurrency: crate::domain::Concurrency::DEFAULT,
                sequence: 1,
                submitted_by: crate::authz::Submitter::Owner,
                submitted_at: Timestamp::observe(),
            },
            phase: Phase::Queued,
            supervisor: Supervisor::Alive,
            behind: Vec::new(),
        }
    }

    #[test]
    fn the_job_json_keeps_its_shape_for_scripts_and_agents() {
        let job = sample();
        let machine: MachineName = "linux".parse().unwrap();
        let mut keys: Vec<String> = job_json(&machine, &job)
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "behind",
                "command",
                "detail",
                "exit_code",
                "job",
                "machine",
                "name",
                "reason",
                "schema",
                "state"
            ]
        );
        assert_eq!(
            job_summary_json(&machine, &job).get("job").unwrap(),
            "linux:0AAAAAAAAAAAAAAA"
        );
    }
}
