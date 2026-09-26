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
        writeln!(out, "  {}", ui::paint(Tone::Bad, &reason.to_string()))?;
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

pub fn machine_listing(machine: &MachineName, jobs: &[Job]) -> Result<String, std::fmt::Error> {
    let now = Timestamp::observe();
    let ids: Vec<&str> = jobs.iter().map(|job| job.spec.id.as_str()).collect();
    let rows: Vec<(&MachineName, &Job)> = jobs.iter().map(|job| (machine, job)).collect();
    let columns = ui::Columns::of(&rows);
    let mut out = String::new();
    let count = jobs.len();
    writeln!(
        out,
        "{}  {}",
        ui::machine(machine),
        ui::paint(
            Tone::Dim,
            &format!("{count} {}", if count == 1 { "job" } else { "jobs" })
        )
    )?;
    for job in jobs {
        writeln!(
            out,
            "  {}",
            ui::job_line(machine, job, (&ids, columns), Some(now))
        )?;
    }
    Ok(out)
}

pub fn unreachable_line(machine: &MachineName, why: &str) -> Result<String, std::fmt::Error> {
    let mut out = String::new();
    writeln!(
        out,
        "{} {} unreachable: {}  {} domyjob doctor {machine}",
        ui::paint(Tone::Bad, "!"),
        ui::machine(machine),
        ui::fit(why, 80),
        ui::paint(Tone::Hint, ui::symbol(Symbol::Hint))
    )?;
    Ok(out)
}

#[must_use]
pub fn no_jobs() -> String {
    ui::paint(
        Tone::Dim,
        "No jobs yet. `domyjob run <machine> -- <command>` starts one.",
    )
}

pub fn final_line(machine: &MachineName, job: &Job) -> Result<String, std::fmt::Error> {
    let id = job.spec.id.as_str();
    let mut out = ui::job_line(machine, job, (&[id], ui::Columns::default()), None);
    if let Some(reason) = job.reason() {
        write!(out, "\n  {}", ui::paint(Tone::Bad, &reason.to_string()))?;
    }
    for note in &job.notes {
        write!(
            out,
            "\n  {}",
            ui::paint(Tone::Dim, &format!("note: {note}"))
        )?;
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

pub fn machine_card(
    machine: &MachineName,
    report: &crate::protocol::Report,
    jobs: &[Job],
    now: Timestamp,
) -> Result<String, std::fmt::Error> {
    let mut out = String::new();
    let health = if report.paused {
        ui::paint(Tone::Stopped, ui::symbol(Symbol::Stopped))
    } else if report.disk_short {
        ui::paint(Tone::Bad, ui::symbol(Symbol::Warning))
    } else {
        ui::paint(Tone::Good, ui::symbol(Symbol::Running))
    };
    writeln!(
        out,
        "{health} {}  {}",
        ui::machine(machine),
        resources(report)?.join(&ui::paint(Tone::Dim, " · "))
    )?;
    writeln!(out, "  {}", activity(jobs)?.join("   "))?;
    attention(&mut out, machine, report, (jobs, now))?;
    Ok(out)
}

fn resources(report: &crate::protocol::Report) -> Result<Vec<String>, std::fmt::Error> {
    let mut facts = vec![ui::paint(Tone::Dim, &report.os.to_string())];
    let mut cores = format!("{} cores", report.cores);
    if let Some([one, ..]) = report.load_hundredths {
        let load = format!("load {}.{:02}", one / 100, one % 100);
        let busy = u64::from(one) > u64::from(report.cores).saturating_mul(100);
        write!(
            cores,
            " {}",
            if busy {
                ui::paint(Tone::Bad, &load)
            } else {
                load
            }
        )?;
    }
    facts.push(cores);
    facts.push(format!(
        "mem {} / {}",
        ui::bytes(report.memory_total.saturating_sub(report.memory_available)),
        ui::bytes(report.memory_total)
    ));
    let disk = format!("disk {} free", ui::bytes(report.disk_available));
    facts.push(if report.disk_short {
        ui::paint(
            Tone::Bad,
            &format!("{disk} {} low", ui::symbol(Symbol::Warning)),
        )
    } else {
        disk
    });
    Ok(facts)
}

fn activity(jobs: &[Job]) -> Result<Vec<String>, std::fmt::Error> {
    let running: Vec<&Job> = jobs
        .iter()
        .filter(|job| matches!(job.state(), State::Running | State::Preparing))
        .collect();
    let queued = jobs
        .iter()
        .filter(|job| job.state() == State::Queued)
        .count();
    let mut activity = Vec::new();
    if !running.is_empty() {
        let shown: Vec<String> = running
            .iter()
            .take(3)
            .map(|job| {
                let took = job
                    .took()
                    .map_or_else(String::new, |took| format!(" {took}"));
                format!(
                    "{}{}",
                    ui::fit(&ui::label(job), 24),
                    ui::paint(Tone::Dim, &took)
                )
            })
            .collect();
        let more = running.len().saturating_sub(shown.len());
        let mut line = format!(
            "{} {} running: {}",
            ui::paint(Tone::Busy, ui::symbol(Symbol::Running)),
            running.len(),
            shown.join(&ui::paint(Tone::Dim, " · "))
        );
        if more > 0 {
            write!(line, "{}", ui::paint(Tone::Dim, &format!(" · {more} more")))?;
        }
        activity.push(line);
    }
    if queued > 0 {
        activity.push(format!(
            "{} {queued} queued",
            ui::paint(Tone::Waiting, ui::symbol(Symbol::Queued))
        ));
    }
    if activity.is_empty() {
        activity.push(ui::paint(Tone::Dim, "idle"));
    }
    Ok(activity)
}

fn attention(
    out: &mut String,
    machine: &MachineName,
    report: &crate::protocol::Report,
    (jobs, now): (&[Job], Timestamp),
) -> std::fmt::Result {
    let hint = ui::paint(Tone::Hint, ui::symbol(Symbol::Hint));
    if let Some(last) = jobs
        .iter()
        .filter(|job| job.is_settled())
        .max_by_key(|job| job.spec.sequence)
        && !last.succeeded()
    {
        let id = last.spec.id.as_str();
        writeln!(
            out,
            "  {} {} {} {}  {hint} domyjob digest {machine}:{}",
            ui::paint(Tone::Bad, ui::symbol(Symbol::Failed)),
            ui::fit(&ui::label(last), 24),
            ui::paint(Tone::Bad, last.state().as_str()),
            ui::paint(Tone::Dim, &ui::ago(last.spec.submitted_at, now)),
            id.get(..ui::SHORT_ID).unwrap_or(id)
        )?;
    }
    if report.paused {
        writeln!(
            out,
            "  {} paused: takes no new jobs  {hint} domyjob machines resume {machine}",
            ui::paint(Tone::Stopped, ui::symbol(Symbol::Stopped)),
        )?;
    }
    if report.disk_short {
        writeln!(
            out,
            "  {} the disk is nearly full  {hint} domyjob clean {machine}",
            ui::paint(Tone::Bad, ui::symbol(Symbol::Warning)),
        )?;
    }
    Ok(())
}

pub fn cleaned(
    machine: &MachineName,
    cleaned: &crate::protocol::Cleaned,
) -> Result<String, std::fmt::Error> {
    let mut out = String::new();
    let total = cleaned
        .items
        .iter()
        .fold(0u64, |sum, item| sum.saturating_add(item.bytes));
    let verb = if cleaned.applied {
        "freed"
    } else {
        "would free"
    };
    if cleaned.items.is_empty() {
        writeln!(
            out,
            "{}  {}",
            ui::machine(machine),
            ui::paint(Tone::Dim, "nothing to free")
        )?;
        return Ok(out);
    }
    writeln!(
        out,
        "{}  {verb} {}",
        ui::machine(machine),
        ui::paint(Tone::Strong, &ui::bytes(total))
    )?;
    let mut items: Vec<&crate::protocol::Freeable> = cleaned.items.iter().collect();
    items.sort_by_key(|item| std::cmp::Reverse(item.bytes));
    for item in items.iter().take(8) {
        writeln!(
            out,
            "  {:>9}  {}",
            ui::bytes(item.bytes),
            ui::paint(Tone::Dim, &item.what.to_string())
        )?;
    }
    if items.len() > 8 {
        writeln!(
            out,
            "  {}",
            ui::paint(
                Tone::Dim,
                &format!("… and {} more", items.len().saturating_sub(8))
            )
        )?;
    }
    Ok(out)
}

pub fn history(
    all: &[crate::history::Series],
    (now, everything): (Timestamp, bool),
) -> Result<String, std::fmt::Error> {
    let mut out = String::new();
    if all.is_empty() {
        writeln!(out, "{}", ui::paint(Tone::Dim, "No finished jobs yet."))?;
        return Ok(out);
    }
    let (series, once): (Vec<&crate::history::Series>, Vec<&crate::history::Series>) =
        all.iter().partition(|one| everything || one.runs > 1);
    let machine_width = series
        .iter()
        .map(|one| one.machine.as_str().len())
        .max()
        .unwrap_or(0);
    let labels: Vec<String> = series.iter().map(|one| ui::fit(&one.label, 28)).collect();
    let label_width = labels
        .iter()
        .map(|label| unicode_width::UnicodeWidthStr::width(label.as_str()))
        .max()
        .unwrap_or(0);
    for (one, label) in series.iter().copied().zip(&labels) {
        let marks: String = one
            .recent
            .iter()
            .map(|state| {
                let (mark, tone) = ui::state_look(*state);
                ui::paint(tone, ui::symbol(mark))
            })
            .collect();
        let rate = one
            .succeeded
            .saturating_mul(100)
            .checked_div(one.runs)
            .unwrap_or(0);
        let mut facts = vec![format!("{rate}% of {}", one.runs)];
        if let Some(typical) = one.typical {
            facts.push(format!("typically {typical}"));
        }
        facts.push(ui::ago(one.last, now));
        writeln!(
            out,
            "{}{}  {}{}  {}{}  {}",
            ui::machine(&one.machine),
            " ".repeat(machine_width.saturating_sub(one.machine.as_str().len())),
            label,
            " ".repeat(
                label_width.saturating_sub(unicode_width::UnicodeWidthStr::width(label.as_str()))
            ),
            marks,
            " ".repeat(12usize.saturating_sub(one.recent.len())),
            ui::paint(Tone::Dim, &facts.join(" · "))
        )?;
    }
    if !once.is_empty() {
        let marks: String = once
            .iter()
            .filter_map(|one| one.recent.last())
            .map(|state| {
                let (mark, tone) = ui::state_look(*state);
                ui::paint(tone, ui::symbol(mark))
            })
            .collect();
        writeln!(
            out,
            "{} {marks}  {}",
            ui::paint(
                Tone::Dim,
                &format!("and {} jobs that ran once:", once.len())
            ),
            ui::paint(Tone::Dim, "`--all` lists them")
        )?;
    }
    Ok(out)
}

pub fn unreachable_card(
    machine: &MachineName,
    why: &str,
    hint: Option<&str>,
) -> Result<String, std::fmt::Error> {
    let mut out = String::new();
    writeln!(
        out,
        "{} {}  {}",
        ui::paint(Tone::Bad, ui::symbol(Symbol::Failed)),
        ui::machine(machine),
        ui::paint(Tone::Bad, why)
    )?;
    writeln!(
        out,
        "  {} {}",
        ui::paint(Tone::Hint, ui::symbol(Symbol::Hint)),
        hint.unwrap_or("domyjob doctor")
    )?;
    Ok(out)
}

pub fn first_run() -> Result<String, std::fmt::Error> {
    let mut out = String::new();
    writeln!(out, "{}", ui::paint(Tone::Strong, "No machines yet."))?;
    writeln!(out, "  Add a host your ssh config knows:")?;
    writeln!(
        out,
        "    {}",
        ui::paint(Tone::Hint, "domyjob machines add linux --label gpu")
    )?;
    writeln!(out, "  Or reach one directly:")?;
    writeln!(
        out,
        "    {}",
        ui::paint(Tone::Hint, "domyjob run ssh:myhost -- uname -a")
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
                let version = facts.hello.version.to_string();
                let shown = if version == crate::protocol::VERSION {
                    version
                } else {
                    stale.push(row.name.clone());
                    ui::paint(Tone::Busy, &format!("{version} !"))
                };
                (
                    facts.hello.os.to_string(),
                    facts.hello.arch.to_string(),
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
        "  {} {}{}  {:<16}  {} {}  {}",
        ui::paint(Tone::Good, ui::symbol(Symbol::Succeeded)),
        ui::machine(name),
        " ".repeat(widest.saturating_sub(name.as_str().len())),
        format!("{}/{}", hello.os, hello.arch),
        hello.version,
        ui::paint(Tone::Dim, &format!("wire {}", hello.wire)),
        ui::paint(Tone::Dim, &hello.shell.to_string())
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
        "reason": job.reason(),
        "notes": job.notes,
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
pub mod tests {
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
        let listed =
            crate::terminal::clean(&machine_listing(&machine, std::slice::from_ref(&job)).unwrap());
        assert!(listed.contains("3m02s · "), "{listed}");
        assert!(listed.contains("just now"), "{listed}");
        assert!(listed.starts_with("win  1 job\n"), "{listed}");
    }

    #[test]
    fn a_machine_card_says_what_runs_what_failed_and_what_needs_doing() {
        let report = crate::protocol::Report {
            host: crate::terminal::RemoteText::new("box".to_owned()),
            os: crate::terminal::RemoteText::new("Linux".to_owned()),
            cores: 16,
            load_hundredths: Some([320, 250, 100]),
            memory_total: 32_000_000_000,
            memory_available: 20_000_000_000,
            disk_total: 500_000_000_000,
            disk_available: 3_000_000_000,
            disk_short: true,
            uptime_seconds: 60,
            paused: true,
        };
        let running = {
            let mut job = sample();
            job.spec.id = "0BBBBBBBBBBBBBBB".parse().unwrap();
            job.spec.name = Some("tests".parse().unwrap());
            job.phase = Phase::Running {
                started_at: Timestamp::observe(),
                pid: 1,
                workspace: String::new(),
            };
            job
        };
        let failed = {
            let mut job = sample();
            job.spec.sequence = 2;
            job.phase = Phase::Finished {
                started_at: None,
                finished_at: Timestamp::observe(),
                outcome: crate::protocol::Outcome::Failed { exit_code: 1 },
            };
            job
        };
        let machine: MachineName = "linux".parse().unwrap();
        let card = crate::terminal::clean(
            &machine_card(
                &machine,
                &report,
                &[running, failed, sample()],
                Timestamp::observe(),
            )
            .unwrap(),
        );
        for wanted in [
            "16 cores load 3.20",
            "1 running: tests",
            "1 queued",
            "failed",
            "domyjob digest linux:0AAAAAAA",
            "domyjob machines resume linux",
            "domyjob clean linux",
            "low",
        ] {
            assert!(card.contains(wanted), "{wanted} missing from\n{card}");
        }
        let idle = crate::protocol::Report {
            disk_short: false,
            paused: false,
            load_hundredths: None,
            ..report
        };
        let quiet = crate::terminal::clean(
            &machine_card(&machine, &idle, &[], Timestamp::observe()).unwrap(),
        );
        assert!(quiet.contains("idle") && !quiet.contains("clean") && !quiet.contains("load"));
    }

    #[test]
    fn an_unreachable_machine_and_a_cleaning_read_as_one_line_and_a_list() {
        let machine: MachineName = "linux".parse().unwrap();
        let lost = crate::terminal::clean(
            &unreachable_card(&machine, "went silent", Some("domyjob setup linux")).unwrap(),
        );
        assert!(lost.contains("went silent") && lost.contains("domyjob setup linux"));
        let cleaned = crate::terminal::clean(
            &cleaned(
                &machine,
                &crate::protocol::Cleaned {
                    applied: false,
                    items: vec![crate::protocol::Freeable {
                        what: crate::terminal::RemoteText::new("workspace a/0".to_owned()),
                        bytes: 2_000_000,
                    }],
                },
            )
            .unwrap(),
        );
        assert!(cleaned.contains("would free") && cleaned.contains("workspace a/0"));
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

    #[must_use]
    pub fn sample() -> Job {
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
            notes: Vec::new(),
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
                "notes",
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
