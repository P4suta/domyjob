use std::io::IsTerminal as _;

use anstyle::{AnsiColor, Color, Style};
use unicode_width::UnicodeWidthStr as _;

use crate::clock::Timestamp;
use crate::domain::{JobId, MachineName};
use crate::protocol::{Job, State};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Good,
    Bad,
    Busy,
    Waiting,
    Stopped,
    Dim,
    Strong,
    Hint,
}

const fn fg(color: AnsiColor) -> Style {
    Style::new().fg_color(Some(Color::Ansi(color)))
}

#[must_use]
pub const fn style(tone: Tone) -> Style {
    match tone {
        Tone::Good => fg(AnsiColor::Green),
        Tone::Bad => fg(AnsiColor::Red).bold(),
        Tone::Busy => fg(AnsiColor::Yellow),
        Tone::Waiting => fg(AnsiColor::Blue),
        Tone::Stopped => fg(AnsiColor::Magenta),
        Tone::Dim => Style::new().dimmed(),
        Tone::Strong => Style::new().bold(),
        Tone::Hint => fg(AnsiColor::Cyan),
    }
}

#[must_use]
pub fn paint(tone: Tone, text: &str) -> String {
    let style = style(tone);
    format!("{style}{text}{style:#}")
}

const MACHINE_COLORS: [AnsiColor; 6] = [
    AnsiColor::Cyan,
    AnsiColor::Magenta,
    AnsiColor::Blue,
    AnsiColor::BrightCyan,
    AnsiColor::BrightMagenta,
    AnsiColor::BrightBlue,
];

#[must_use]
pub fn machine(name: &MachineName) -> String {
    let digest = blake3::hash(name.as_str().as_bytes());
    let pick = digest.as_bytes().first().copied().unwrap_or(0);
    let color = MACHINE_COLORS
        .get(
            usize::from(pick)
                .checked_rem(MACHINE_COLORS.len())
                .unwrap_or(0),
        )
        .copied()
        .unwrap_or(AnsiColor::Cyan);
    let style = fg(color).bold();
    format!("{style}{name}{style:#}")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Symbol {
    Succeeded,
    Failed,
    Running,
    Queued,
    Stopped,
    Lost,
    Sending,
    Hint,
    Gutter,
    Warning,
}

#[must_use]
pub fn unicode() -> bool {
    supports_unicode::on(supports_unicode::Stream::Stderr)
}

#[must_use]
pub fn symbol(symbol: Symbol) -> &'static str {
    let fancy = unicode();
    match (symbol, fancy) {
        (Symbol::Succeeded, true) => "✓",
        (Symbol::Succeeded, false) => "ok",
        (Symbol::Failed, true) => "✗",
        (Symbol::Failed, false) => "x",
        (Symbol::Running, true) => "●",
        (Symbol::Running, false) => "*",
        (Symbol::Queued, true) => "○",
        (Symbol::Queued, false) => "o",
        (Symbol::Stopped, true) => "⊘",
        (Symbol::Stopped, false) => "-",
        (Symbol::Lost, _) => "?",
        (Symbol::Sending, true) => "↑",
        (Symbol::Sending, false) => "^",
        (Symbol::Hint, true) => "→",
        (Symbol::Hint, false) => "->",
        (Symbol::Gutter, true) => "│",
        (Symbol::Gutter, false) => "|",
        (Symbol::Warning, true) => "▲",
        (Symbol::Warning, false) => "!",
    }
}

#[must_use]
pub const fn state_look(state: State) -> (Symbol, Tone) {
    match state {
        State::Succeeded => (Symbol::Succeeded, Tone::Good),
        State::Failed | State::Errored => (Symbol::Failed, Tone::Bad),
        State::Running | State::Preparing => (Symbol::Running, Tone::Busy),
        State::Queued => (Symbol::Queued, Tone::Waiting),
        State::Killed => (Symbol::Stopped, Tone::Stopped),
        State::Lost => (Symbol::Lost, Tone::Stopped),
    }
}

#[must_use]
pub fn state_badge(state: State) -> String {
    let (mark, tone) = state_look(state);
    paint(tone, &format!("{} {}", symbol(mark), state.as_str()))
}

pub const SHORT_ID: usize = 8;

#[must_use]
pub fn unique_prefix(id: &str, among: &[&str]) -> usize {
    let mut needed = 4usize.min(id.len());
    for other in among.iter().filter(|other| **other != id) {
        let common = id
            .chars()
            .zip(other.chars())
            .take_while(|(a, b)| a == b)
            .count();
        needed = needed.max(common.saturating_add(1));
    }
    needed.min(id.len())
}

#[must_use]
pub fn short_id(id: &JobId, among: &[&str]) -> String {
    let text = id.as_str();
    let bold = unique_prefix(text, among).min(SHORT_ID);
    let (head, rest) = text.split_at(bold.min(text.len()));
    let tail = rest.get(..SHORT_ID.saturating_sub(bold)).unwrap_or(rest);
    format!("{}{}", paint(Tone::Strong, head), paint(Tone::Dim, tail))
}

#[must_use]
pub fn ago(then: Timestamp, now: Timestamp) -> String {
    let seconds = then.until(now).millis() / 1000;
    match seconds {
        ..10 => "just now".to_owned(),
        10..60 => format!("{seconds}s ago"),
        60..3600 => format!("{}m ago", seconds / 60),
        3600..86_400 => format!("{}h ago", seconds / 3600),
        _ => then.local_date(),
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Columns {
    pub machine: usize,
    pub label: usize,
}

impl Columns {
    #[must_use]
    pub fn of(rows: &[(&MachineName, &Job)]) -> Self {
        Self {
            machine: rows
                .iter()
                .map(|(machine, _)| machine.as_str().width())
                .max()
                .unwrap_or(0),
            label: rows
                .iter()
                .map(|(_, job)| label(job).width().min(LABEL_ROOM))
                .max()
                .unwrap_or(0),
        }
    }
}

const LABEL_ROOM: usize = 40;

pub fn label(job: &Job) -> String {
    job.spec
        .name
        .as_ref()
        .map_or_else(|| job.spec.command.headline(), ToString::to_string)
}

fn pad(text: &str, shown: usize, to: usize) -> String {
    format!("{text}{}", " ".repeat(to.saturating_sub(shown)))
}

#[must_use]
pub fn width() -> usize {
    match terminal_size::terminal_size() {
        Some((terminal_size::Width(columns), _)) => usize::from(columns),
        None => 100,
    }
}

#[must_use]
pub fn fit(text: &str, room: usize) -> String {
    if text.width() <= room {
        return text.to_owned();
    }
    let ellipsis = if unicode() { "…" } else { "..." };
    let keep = room.saturating_sub(ellipsis.width());
    let mut out = String::new();
    let mut used = 0usize;
    for c in text.chars() {
        let wide = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if used.saturating_add(wide) > keep {
            break;
        }
        used = used.saturating_add(wide);
        out.push(c);
    }
    out.push_str(ellipsis);
    out
}

#[must_use]
pub fn bytes(count: u64) -> String {
    bytesize::ByteSize::b(count)
        .display()
        .si_short()
        .to_string()
}

#[must_use]
pub fn stderr_is_live() -> bool {
    std::io::stderr().is_terminal() && std::env::var_os("TERM").is_none_or(|term| term != "dumb")
}

#[must_use]
pub fn job_line(
    machine_name: &MachineName,
    job: &Job,
    (among, columns): (&[&str], Columns),
    now: Option<Timestamp>,
) -> String {
    let state = job.state();
    let (mark, tone) = state_look(state);
    let label = fit(&label(job), LABEL_ROOM);
    let mut facts = vec![paint(tone, state.as_str())];
    if let Some(code) = job.exit_code().filter(|code| *code != 0) {
        facts.push(paint(tone, &format!("exit {code}")));
    }
    if let Some(took) = job.took() {
        facts.push(match now {
            Some(_) => took.to_string(),
            None => format!("after {took}"),
        });
    }
    if !job.behind.is_empty() {
        let holders: Vec<&str> = job
            .behind
            .iter()
            .map(|holder| holder.as_str().get(..SHORT_ID).unwrap_or(holder.as_str()))
            .collect();
        facts.push(paint(
            Tone::Waiting,
            &format!("behind {}", holders.join(", ")),
        ));
    }
    if let Some(now) = now {
        facts.push(paint(Tone::Dim, &ago(job.spec.submitted_at, now)));
    }
    format!(
        "{} {}{}{}  {}  {}",
        paint(tone, symbol(mark)),
        machine(machine_name),
        paint(Tone::Dim, ":"),
        pad(
            &short_id(&job.spec.id, among),
            SHORT_ID.saturating_add(machine_name.as_str().width()),
            SHORT_ID.saturating_add(columns.machine)
        ),
        pad(&label, label.width(), columns.label),
        facts.join(&paint(Tone::Dim, " · "))
    )
}

pub fn report_error(what: &str, why: Option<&str>, try_this: Option<&str>) {
    let mut out = anstream::stderr();
    let mut text = format!("{} {what}\n", paint(Tone::Bad, "error:"));
    if let Some(why) = why {
        text.push_str("  ");
        text.push_str(&paint(Tone::Dim, why));
        text.push('\n');
    }
    if let Some(hint) = try_this {
        text.push_str("  ");
        text.push_str(&paint(
            Tone::Hint,
            &format!("{} try:", symbol(Symbol::Hint)),
        ));
        text.push(' ');
        text.push_str(hint);
        text.push('\n');
    }
    match std::io::Write::write_all(&mut out, text.as_bytes()) {
        Ok(()) | Err(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shortest_unambiguous_prefix_is_found_and_never_below_four() {
        assert_eq!(unique_prefix("3EE9T9SY", &["3EE9T9SY", "3D83KHAF"]), 4);
        assert_eq!(unique_prefix("3EE9T9SY", &["3EE9T9SY", "3EE9X000"]), 5);
        assert_eq!(unique_prefix("ABCDEFGH", &[]), 4);
    }

    #[test]
    fn long_text_is_cut_to_its_room_with_an_ellipsis() {
        assert_eq!(fit("short", 10), "short");
        let cut = fit("cargo clippy --workspace --all-targets", 12);
        assert!(cut.width() <= 12, "{cut}");
        assert!(cut.starts_with("cargo"));
        assert!(fit("日本語のコマンド名", 7).width() <= 7);
    }

    #[test]
    fn byte_counts_read_like_people_write_them() {
        assert_eq!(bytes(826_437), "826.4k");
    }
}
