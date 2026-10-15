use super::{Decoded, KeyCode, KeyEvent, KeyModifiers, control, sequence::Sequence, unicode};

pub const MAX_SEQUENCE_BYTES: usize = 32;
const ESCAPE: u8 = 27;
const BACKSPACE: u8 = 8;
const DELETE: u8 = 127;
const TAB: u8 = 9;
const CARRIAGE_RETURN: u8 = 13;
const LINE_FEED: u8 = 10;
const CONTROL_LAST: u8 = 26;
const ASCII_LAST: u8 = 127;
const CONTINUATION_MASK: u8 = 192;
const CONTINUATION: u8 = 128;
const TWO_FIRST: u8 = 194;
const TWO_LAST: u8 = 223;
const THREE_FIRST: u8 = 224;
const THREE_LAST: u8 = 239;
const FOUR_FIRST: u8 = 240;
const FOUR_LAST: u8 = 244;
const TWO_BYTES: usize = 2;
const THREE_BYTES: usize = 3;
const FOUR_BYTES: usize = 4;
const FINAL_FIRST: u8 = 64;
const FINAL_LAST: u8 = 126;
const PARAMETER_FIRST: u8 = 32;
const LEGACY_MOUSE_BYTES: u8 = 3;

#[derive(Debug, Clone, Copy, Default)]
enum State {
    #[default]
    Ground,
    Escape,
    Utf8,
    Control,
    Discard,
    Mouse(u8),
}

#[derive(Debug, Clone, Copy)]
pub struct Decoder {
    state: State,
    sequence: Sequence,
    expected: usize,
    modifiers: KeyModifiers,
}

impl Default for Decoder {
    fn default() -> Self {
        Self {
            state: State::Ground,
            sequence: Sequence::default(),
            expected: 0,
            modifiers: KeyModifiers::NONE,
        }
    }
}

impl Decoder {
    const fn reset(&mut self) {
        self.state = State::Ground;
        self.sequence.clear();
        self.expected = 0;
        self.modifiers = KeyModifiers::NONE;
    }

    #[must_use]
    pub fn decode(&mut self, byte: u8) -> Decoded {
        match self.state {
            State::Ground => self.ground(byte, KeyModifiers::NONE),
            State::Escape => self.escape(byte),
            State::Utf8 => self.utf8(byte),
            State::Control => self.control(byte),
            State::Discard => {
                if (FINAL_FIRST..=FINAL_LAST).contains(&byte) {
                    self.reset();
                }
                Decoded::Ignored
            }
            State::Mouse(remaining) => {
                if remaining <= 1 {
                    self.reset();
                } else {
                    self.state = State::Mouse(remaining.saturating_sub(1));
                }
                Decoded::Ignored
            }
        }
    }

    fn ground(&mut self, byte: u8, modifiers: KeyModifiers) -> Decoded {
        self.modifiers = modifiers;
        let code = match byte {
            ESCAPE => {
                self.state = State::Escape;
                return Decoded::Pending;
            }
            BACKSPACE | DELETE => KeyCode::Backspace,
            TAB => KeyCode::Tab,
            CARRIAGE_RETURN | LINE_FEED => KeyCode::Enter,
            0 => KeyCode::Null,
            1..=CONTROL_LAST => {
                let code = u32::from(byte)
                    .saturating_add(u32::from(b'a'))
                    .saturating_sub(1);
                let Some(character) = char::from_u32(code) else {
                    return Decoded::Ignored;
                };
                return Decoded::Key(KeyEvent::new(
                    KeyCode::Char(character),
                    modifiers.with(super::Modifier::Control),
                ));
            }
            _ if byte <= ASCII_LAST => KeyCode::Char(char::from(byte)),
            _ => return self.start_utf8(byte),
        };
        Decoded::Key(KeyEvent::new(code, modifiers))
    }

    fn start_utf8(&mut self, byte: u8) -> Decoded {
        self.expected = match byte {
            TWO_FIRST..=TWO_LAST => TWO_BYTES,
            THREE_FIRST..=THREE_LAST => THREE_BYTES,
            FOUR_FIRST..=FOUR_LAST => FOUR_BYTES,
            _ => {
                self.reset();
                return Decoded::Ignored;
            }
        };
        self.sequence.clear();
        if self.sequence.push(byte) {
            self.state = State::Utf8;
            Decoded::Pending
        } else {
            self.reset();
            Decoded::Ignored
        }
    }

    fn escape(&mut self, byte: u8) -> Decoded {
        if byte == b'[' || byte == b'O' {
            self.sequence.clear();
            self.state = State::Control;
            Decoded::Pending
        } else if byte == ESCAPE {
            Decoded::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        } else {
            self.reset();
            self.ground(byte, KeyModifiers::ALT)
        }
    }

    fn utf8(&mut self, byte: u8) -> Decoded {
        if byte & CONTINUATION_MASK != CONTINUATION {
            self.reset();
            return self.ground(byte, KeyModifiers::NONE);
        }
        if !self.sequence.push(byte) {
            self.reset();
            return Decoded::Ignored;
        }
        if self.sequence.len() < self.expected {
            return Decoded::Pending;
        }
        let value = self.sequence.content().and_then(unicode::scalar);
        let modifiers = self.modifiers;
        self.reset();
        match value {
            Some(character) => Decoded::Key(KeyEvent::new(KeyCode::Char(character), modifiers)),
            None => Decoded::Ignored,
        }
    }

    fn control(&mut self, byte: u8) -> Decoded {
        if !(PARAMETER_FIRST..=FINAL_LAST).contains(&byte) {
            self.reset();
            return self.ground(byte, KeyModifiers::NONE);
        }
        let final_byte = (FINAL_FIRST..=FINAL_LAST).contains(&byte);
        if !self.sequence.push(byte) {
            self.reset();
            if !final_byte {
                self.state = State::Discard;
            }
            return Decoded::Ignored;
        }
        if !final_byte {
            return Decoded::Pending;
        }
        let legacy_mouse = self.sequence.len() == 1 && byte == b'M';
        let key = self.sequence.content().and_then(control::key);
        self.reset();
        if legacy_mouse {
            self.state = State::Mouse(LEGACY_MOUSE_BYTES);
        }
        match key {
            Some(key) => Decoded::Key(key),
            None => Decoded::Ignored,
        }
    }

    #[must_use]
    pub const fn quiet(&mut self) -> Decoded {
        if matches!(self.state, State::Escape) {
            self.finish()
        } else {
            Decoded::Pending
        }
    }

    #[must_use]
    pub const fn finish(&mut self) -> Decoded {
        let result = if matches!(self.state, State::Escape) {
            Decoded::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        } else {
            Decoded::Ignored
        };
        self.reset();
        result
    }

    #[must_use]
    pub const fn pending(&self) -> bool {
        !matches!(self.state, State::Ground)
    }

    #[must_use]
    pub const fn buffered_bytes(&self) -> usize {
        self.sequence.len()
    }
}

#[cfg(kani)]
mod proofs {
    use super::*;

    #[kani::proof]
    #[kani::unwind(6)]
    fn rejects_emitting_a_truncated_utf8_sequence() {
        let mut decoder = Decoder::default();
        let _first = decoder.decode(0xe7);
        kani::assert(
            matches!(decoder.quiet(), Decoded::Key(_)),
            "NEGATIVE_TERMINAL_UTF8_CONTROL",
        );
    }
}
