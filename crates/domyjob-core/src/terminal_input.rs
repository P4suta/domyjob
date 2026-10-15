const CONTROL: u8 = 1;
const ALT: u8 = 2;
const SHIFT: u8 = 4;
const WINDOWS_PROCESSED_INPUT: u32 = 1;
const WINDOWS_LINE_INPUT: u32 = 2;
const WINDOWS_ECHO_INPUT: u32 = 4;
const WINDOWS_WINDOW_INPUT: u32 = 8;
const WINDOWS_QUICK_EDIT: u32 = 64;
const WINDOWS_EXTENDED_FLAGS: u32 = 128;
const WINDOWS_VIRTUAL_INPUT: u32 = 512;
const WINDOWS_PROCESSED_OUTPUT: u32 = 1;
const WINDOWS_VIRTUAL_OUTPUT: u32 = 4;
const WINDOWS_DELAYED_WRAP: u32 = 8;
pub const MAX_TERMINAL_FD: i32 = 65_536;
pub const MAX_POLL_MILLIS: u64 = 100;
pub const WINDOWS_ALT_KEY: u16 = 18;
const NANOS_PER_MILLI: i64 = 1_000_000;

mod control;
mod decoder;
#[cfg(kani)]
mod proofs;
mod sequence;
mod unicode;
mod utf16;

pub use decoder::{Decoder, MAX_SEQUENCE_BYTES};
pub use utf16::{Utf16Decoder, WindowsKey};

#[must_use]
pub const fn descriptor_span(fd: i32) -> Option<i32> {
    if fd < 0 || fd >= MAX_TERMINAL_FD {
        None
    } else {
        fd.checked_add(1)
    }
}

#[must_use]
pub fn poll_nanos(millis: u64) -> Option<i64> {
    if millis > MAX_POLL_MILLIS {
        return None;
    }
    match i64::try_from(millis) {
        Ok(millis) => millis.checked_mul(NANOS_PER_MILLI),
        Err(_too_large) => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConsoleViewport {
    pub width: u16,
    pub height: u16,
}

#[must_use]
pub fn console_viewport(left: i16, top: i16, right: i16, bottom: i16) -> Option<ConsoleViewport> {
    let Ok(left) = u16::try_from(left) else {
        return None;
    };
    let Ok(top) = u16::try_from(top) else {
        return None;
    };
    let Ok(right) = u16::try_from(right) else {
        return None;
    };
    let Ok(bottom) = u16::try_from(bottom) else {
        return None;
    };
    Some(ConsoleViewport {
        width: right.checked_sub(left)?.checked_add(1)?,
        height: bottom.checked_sub(top)?.checked_add(1)?,
    })
}

#[must_use]
pub const fn windows_input_mode(original: u32) -> u32 {
    let removed = WINDOWS_PROCESSED_INPUT
        | WINDOWS_LINE_INPUT
        | WINDOWS_ECHO_INPUT
        | WINDOWS_QUICK_EDIT
        | WINDOWS_VIRTUAL_INPUT;
    (original & !removed) | WINDOWS_WINDOW_INPUT | WINDOWS_EXTENDED_FLAGS
}

#[must_use]
pub const fn windows_output_mode(original: u32) -> u32 {
    original | WINDOWS_PROCESSED_OUTPUT | WINDOWS_VIRTUAL_OUTPUT | WINDOWS_DELAYED_WRAP
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyCode {
    Backspace,
    Enter,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
    Tab,
    BackTab,
    Delete,
    Insert,
    Esc,
    F(u32),
    Char(char),
    Null,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Modifier {
    Control,
    Alt,
    Shift,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyModifiers(u8);

impl KeyModifiers {
    pub const NONE: Self = Self(0);
    pub const CONTROL: Self = Self(CONTROL);
    pub const ALT: Self = Self(ALT);
    pub const SHIFT: Self = Self(SHIFT);

    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    #[must_use]
    pub const fn with(self, modifier: Modifier) -> Self {
        let flag = match modifier {
            Modifier::Control => CONTROL,
            Modifier::Alt => ALT,
            Modifier::Shift => SHIFT,
        };
        Self(self.0 | flag)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyEventKind {
    Press,
    Repeat,
    Release,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyEvent {
    pub code: KeyCode,
    pub modifiers: KeyModifiers,
    pub kind: KeyEventKind,
}

impl KeyEvent {
    #[must_use]
    pub const fn new(code: KeyCode, modifiers: KeyModifiers) -> Self {
        Self {
            code,
            modifiers,
            kind: KeyEventKind::Press,
        }
    }

    #[must_use]
    pub const fn with_kind(self, kind: KeyEventKind) -> Self {
        Self { kind, ..self }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decoded {
    Key(KeyEvent),
    Pending,
    Ignored,
}
