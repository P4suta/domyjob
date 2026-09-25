use std::collections::BTreeMap;
use std::sync::Mutex;

use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};

use crate::client::Stage;
use crate::domain::MachineName;
use crate::ui::{self, Symbol, Tone};

#[derive(Debug)]
struct Live {
    multi: MultiProgress,
    bars: Mutex<BTreeMap<MachineName, ProgressBar>>,
}

#[derive(Debug)]
pub struct Board {
    live: Option<Live>,
    waiting: bool,
    quiet: bool,
}

fn row(mark: Symbol, tone: Tone, machine: &MachineName, text: &str) -> String {
    format!(
        "{} {}  {text}",
        ui::paint(tone, ui::symbol(mark)),
        ui::machine(machine)
    )
}

impl Board {
    #[must_use]
    pub fn new(waiting: bool) -> Self {
        let live = ui::stderr_is_live().then(|| Live {
            multi: MultiProgress::with_draw_target(ProgressDrawTarget::stderr()),
            bars: Mutex::new(BTreeMap::new()),
        });
        Self {
            live,
            waiting,
            quiet: false,
        }
    }

    #[must_use]
    pub const fn silent() -> Self {
        Self {
            live: None,
            waiting: true,
            quiet: true,
        }
    }

    #[must_use]
    pub const fn is_quiet(&self) -> bool {
        self.quiet
    }

    #[must_use]
    pub const fn is_live(&self) -> bool {
        self.live.is_some()
    }

    fn bar(live: &Live, machine: &MachineName) -> Option<ProgressBar> {
        let Ok(mut bars) = live.bars.lock() else {
            return None;
        };
        let bar = bars.entry(machine.clone()).or_insert_with(|| {
            let bar = live.multi.add(ProgressBar::new_spinner());
            if let Ok(style) = ProgressStyle::with_template("{msg}") {
                bar.set_style(style);
            }
            bar
        });
        Some(bar.clone())
    }

    pub fn stage(&self, machine: &MachineName, stage: Stage<'_>) {
        let Some(live) = &self.live else {
            if !self.quiet {
                plain(machine, stage);
            }
            return;
        };
        let Some(bar) = Self::bar(live, machine) else {
            return;
        };
        match stage {
            Stage::Connecting => {
                bar.set_message(row(Symbol::Queued, Tone::Waiting, machine, "connecting"));
            }
            Stage::Connected => {
                bar.set_message(row(Symbol::Running, Tone::Busy, machine, "connected"));
            }
            Stage::Sending { files, bytes } => bar.set_message(row(
                Symbol::Sending,
                Tone::Busy,
                machine,
                &format!("sending {files} files · {}", ui::bytes(bytes)),
            )),
            Stage::UpToDate => bar.set_message(row(
                Symbol::Running,
                Tone::Busy,
                machine,
                "has every file already",
            )),
            Stage::Submitted(job) => {
                let full = job.spec.id.as_str();
                let text = full.get(..ui::SHORT_ID).unwrap_or(full).to_owned();
                if let Some(behind) = queued_behind(job) {
                    bar.set_message(row(
                        Symbol::Queued,
                        Tone::Waiting,
                        machine,
                        &format!("{} {behind}", ui::paint(Tone::Dim, &text)),
                    ));
                    if !self.waiting {
                        bar.finish();
                    }
                } else if self.waiting {
                    bar.set_message(row(
                        Symbol::Running,
                        Tone::Busy,
                        machine,
                        &format!("running {}", ui::paint(Tone::Dim, &text)),
                    ));
                } else {
                    bar.finish_with_message(row(
                        Symbol::Succeeded,
                        Tone::Good,
                        machine,
                        &format!(
                            "submitted {}{}",
                            ui::paint(Tone::Dim, &format!("{machine}:")),
                            ui::paint(Tone::Strong, &text)
                        ),
                    ));
                }
            }
        }
    }

    pub fn refused(&self, machine: &MachineName, why: &str, hint: Option<&str>) {
        let why = why
            .strip_prefix(machine.as_str())
            .and_then(|rest| rest.strip_prefix(": "))
            .unwrap_or(why);
        let hint = hint.map(|hint| {
            format!(
                "  {} {hint}",
                ui::paint(Tone::Hint, &format!("{} try:", ui::symbol(Symbol::Hint)))
            )
        });
        match &self.live {
            Some(live) => {
                if let Some(bar) = Self::bar(live, machine) {
                    bar.finish_with_message(row(
                        Symbol::Failed,
                        Tone::Bad,
                        machine,
                        &ui::fit(why, ui::width().saturating_sub(20)),
                    ));
                }
                if let Some(hint) = hint {
                    match live.multi.println(hint) {
                        Ok(()) | Err(_) => {}
                    }
                }
            }
            None => {
                let mut text = format!("domyjob: {machine}: {why}\n");
                if let Some(hint) = hint {
                    text.push_str(&hint);
                    text.push('\n');
                }
                match std::io::Write::write_all(&mut anstream::stderr(), text.as_bytes()) {
                    Ok(()) | Err(_) => {}
                }
            }
        }
    }

    pub fn above<R>(&self, write: impl FnOnce() -> R) -> R {
        match &self.live {
            Some(live) => live.multi.suspend(write),
            None => write(),
        }
    }

    pub fn finished(&self, machine: &MachineName, text: &str) {
        match &self.live {
            Some(live) => {
                if let Some(bar) = Self::bar(live, machine) {
                    bar.finish_and_clear();
                }
                match live.multi.println(text) {
                    Ok(()) | Err(_) => {}
                }
            }
            None => {
                let mut out = anstream::stderr();
                match std::io::Write::write_all(&mut out, format!("{text}\n").as_bytes()) {
                    Ok(()) | Err(_) => {}
                }
            }
        }
    }

    pub fn clear(&self) {
        if let Some(live) = &self.live {
            match live.multi.clear() {
                Ok(()) | Err(_) => {}
            }
        }
    }
}

#[must_use]
pub fn queued_behind(job: &crate::protocol::Job) -> Option<String> {
    if job.behind.is_empty() {
        return None;
    }
    let holders: Vec<&str> = job
        .behind
        .iter()
        .map(|holder| {
            holder
                .as_str()
                .get(..ui::SHORT_ID)
                .unwrap_or(holder.as_str())
        })
        .collect();
    Some(format!(
        "queued behind {} running {}: {}",
        holders.len(),
        if holders.len() == 1 { "job" } else { "jobs" },
        holders.join(", ")
    ))
}

fn plain(machine: &MachineName, stage: Stage<'_>) {
    match stage {
        Stage::Connecting => eprintln!("domyjob: {machine}: connecting"),
        Stage::Connected => eprintln!("domyjob: {machine}: connected"),
        Stage::Sending { files, bytes } => {
            eprintln!(
                "domyjob: {machine}: sending {files} files ({}) it does not have yet",
                ui::bytes(bytes)
            );
        }
        Stage::UpToDate => eprintln!("domyjob: {machine}: already has every file"),
        Stage::Submitted(job) => {
            let id = &job.spec.id;
            eprintln!(
                "domyjob: {machine}: submitted {machine}:{id}; it runs whether or not this command stays"
            );
            if let Some(behind) = queued_behind(job) {
                eprintln!("domyjob: {machine}: {behind}; it starts when one of them finishes");
            }
        }
    }
}
