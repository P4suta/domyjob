use alloc::string::String;
use core::fmt;

pub mod geometry;

pub const MAX_ROWS: usize = 256;
pub const MAX_QUERY_BYTES: usize = 128;
pub const STALE_MILLIS: u64 = 30_000;
pub const MIN_VIEW_WIDTH: u16 = 32;
pub const MIN_VIEW_HEIGHT: u16 = 10;
pub const SPLIT_VIEW_WIDTH: u16 = 76;
pub const SIDEBAR_VIEW_WIDTH: u16 = 112;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Viewport {
    Minimal,
    Compact,
    Split,
    Sidebar,
}

#[must_use]
pub const fn viewport(width: u16, height: u16) -> Viewport {
    if width < MIN_VIEW_WIDTH || height < MIN_VIEW_HEIGHT {
        Viewport::Minimal
    } else if width < SPLIT_VIEW_WIDTH {
        Viewport::Compact
    } else if width < SIDEBAR_VIEW_WIDTH {
        Viewport::Split
    } else {
        Viewport::Sidebar
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Section {
    #[default]
    Machines,
    Agents,
    Chats,
}

impl Section {
    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::Machines => Self::Agents,
            Self::Agents => Self::Chats,
            Self::Chats => Self::Machines,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Movement {
    Previous,
    Next,
    First,
    Last,
}

#[must_use]
pub const fn navigate(selected: Option<usize>, count: usize, movement: Movement) -> Option<usize> {
    let Some(last) = count.checked_sub(1) else {
        return None;
    };
    let current = match selected {
        Some(current) if current < count => current,
        Some(_) | None => 0,
    };
    Some(match movement {
        Movement::Previous => current.saturating_sub(1),
        Movement::Next => {
            if !matches!(selected, Some(index) if index < count) {
                return Some(0);
            }
            let next = current.saturating_add(1);
            if next > last { last } else { next }
        }
        Movement::First => 0,
        Movement::Last => last,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityMatch {
    Exact,
    Different,
}

#[must_use]
pub const fn retained_selection(
    identity: IdentityMatch,
    position: usize,
    count: usize,
) -> Option<usize> {
    if matches!(identity, IdentityMatch::Exact) && position < count {
        Some(position)
    } else {
        None
    }
}

#[must_use]
pub const fn viewing_identity(identity: IdentityMatch, registered: bool) -> bool {
    matches!(identity, IdentityMatch::Exact) && registered
}

#[must_use]
pub const fn selection_after_refresh(
    had_selection: bool,
    retained: Option<usize>,
    count: usize,
) -> Option<usize> {
    match retained {
        Some(position) if position < count => Some(position),
        Some(_) | None if had_selection => None,
        Some(_) | None => navigate(None, count, Movement::First),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    Unknown,
    Fresh(u64),
    Stale(u64),
}

#[must_use]
pub const fn freshness(observed: Option<u64>, now: Option<u64>, limit: u64) -> Freshness {
    let (Some(observed), Some(now)) = (observed, now) else {
        return Freshness::Unknown;
    };
    match now.checked_sub(observed) {
        None => Freshness::Unknown,
        Some(elapsed) if elapsed >= limit => Freshness::Stale(elapsed),
        Some(elapsed) => Freshness::Fresh(elapsed),
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Refresh {
    accepted: u64,
    pending: Option<u64>,
}

impl Refresh {
    #[must_use]
    pub const fn pending(self) -> Option<u64> {
        self.pending
    }

    pub fn request(&mut self) -> Option<u64> {
        if self.pending.is_some() {
            return None;
        }
        let generation = self.accepted.checked_add(1)?;
        self.pending = Some(generation);
        Some(generation)
    }

    pub fn complete(&mut self, generation: u64) -> bool {
        if self.pending != Some(generation) || generation <= self.accepted {
            return false;
        }
        self.accepted = generation;
        self.pending = None;
        true
    }

    pub fn complete_view(&mut self, generation: u64, scope: IdentityMatch) -> bool {
        self.complete(generation) && matches!(scope, IdentityMatch::Exact)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TerminalStage {
    #[default]
    Untouched,
    RawAttempted,
    ScreenAttempted,
    CursorAttempted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Restore {
    Cursor,
    Screen,
    Raw,
}

impl TerminalStage {
    #[must_use]
    pub const fn owns(self, restore: Restore) -> bool {
        match restore {
            Restore::Cursor => matches!(self, Self::CursorAttempted),
            Restore::Screen => matches!(self, Self::ScreenAttempted | Self::CursorAttempted),
            Restore::Raw => !matches!(self, Self::Untouched),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayChar {
    Literal(char),
    Escaped(char),
}

#[must_use]
pub const fn display_char(character: char) -> DisplayChar {
    if character.is_control()
        || matches!(character, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{200e}' | '\u{200f}')
    {
        DisplayChar::Escaped(character)
    } else {
        DisplayChar::Literal(character)
    }
}

#[must_use]
pub fn display(text: &str) -> DisplayText {
    let mut shown = String::new();
    for character in text.chars() {
        match display_char(character) {
            DisplayChar::Literal(character) => shown.push(character),
            DisplayChar::Escaped(character) => shown.extend(character.escape_default()),
        }
    }
    DisplayText(shown)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayText(String);

impl DisplayText {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for DisplayText {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

pub fn query_push(query: &mut String, character: char) -> bool {
    if !query_admits(query.len(), character) {
        return false;
    }
    query.push(character);
    true
}

#[must_use]
pub const fn query_admits(used: usize, character: char) -> bool {
    matches!(display_char(character), DisplayChar::Literal(_))
        && matches!(used.checked_add(character.len_utf8()), Some(total) if total <= MAX_QUERY_BYTES)
}

#[cfg(kani)]
mod proofs;
