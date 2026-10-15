use super::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, Modifier, WINDOWS_ALT_KEY};

const HIGH_FIRST: u16 = 0xd800;
const HIGH_LAST: u16 = 0xdbff;
const LOW_FIRST: u16 = 0xdc00;
const LOW_LAST: u16 = 0xdfff;
const SURROGATE_SHIFT: u32 = 10;
const SUPPLEMENTARY_FIRST: u32 = 0x10000;
const LEFT_ALT: u32 = 2;
const RIGHT_ALT: u32 = 1;
const LEFT_CONTROL: u32 = 8;
const RIGHT_CONTROL: u32 = 4;
const SHIFT: u32 = 16;
const CONTROL_LAST: u16 = 26;
const VK_BACK: u16 = 8;
const VK_TAB: u16 = 9;
const VK_RETURN: u16 = 13;
const VK_ESCAPE: u16 = 27;
const VK_PRIOR: u16 = 33;
const VK_NEXT: u16 = 34;
const VK_END: u16 = 35;
const VK_HOME: u16 = 36;
const VK_LEFT: u16 = 37;
const VK_UP: u16 = 38;
const VK_RIGHT: u16 = 39;
const VK_DOWN: u16 = 40;
const VK_INSERT: u16 = 45;
const VK_DELETE: u16 = 46;
const VK_F1: u16 = 112;
const VK_F24: u16 = 135;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowsKey {
    pub down: i32,
    pub repeats: u16,
    pub virtual_key: u16,
    pub unit: u16,
    pub control_state: u32,
}

impl WindowsKey {
    const fn alt_text(self) -> bool {
        self.down == 0 && self.virtual_key == WINDOWS_ALT_KEY && self.unit != 0
    }

    const fn event_kind(self) -> KeyEventKind {
        if self.alt_text() {
            KeyEventKind::Press
        } else if self.down == 0 {
            KeyEventKind::Release
        } else if self.repeats > 1 {
            KeyEventKind::Repeat
        } else {
            KeyEventKind::Press
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Utf16Decoder {
    high: Option<u16>,
}

impl Utf16Decoder {
    #[must_use]
    pub fn decode(&mut self, unit: u16) -> Option<char> {
        if (HIGH_FIRST..=HIGH_LAST).contains(&unit) {
            self.high = Some(unit);
            return None;
        }
        let high = self.high.take();
        let scalar = if (LOW_FIRST..=LOW_LAST).contains(&unit) {
            u32::from(high?.checked_sub(HIGH_FIRST)?)
                .checked_shl(SURROGATE_SHIFT)?
                .checked_add(u32::from(unit.checked_sub(LOW_FIRST)?))?
                .checked_add(SUPPLEMENTARY_FIRST)?
        } else {
            u32::from(unit)
        };
        char::from_u32(scalar)
    }

    pub const fn clear(&mut self) {
        self.high = None;
    }

    #[must_use]
    pub fn windows_key(&mut self, key: WindowsKey) -> Option<KeyEvent> {
        let mut modifiers = KeyModifiers::NONE;
        for (mask, modifier) in [
            (LEFT_ALT | RIGHT_ALT, Modifier::Alt),
            (LEFT_CONTROL | RIGHT_CONTROL, Modifier::Control),
            (SHIFT, Modifier::Shift),
        ] {
            if key.control_state & mask != 0 {
                modifiers = modifiers.with(modifier);
            }
        }
        let code = if key.alt_text() {
            KeyCode::Char(self.decode(key.unit)?)
        } else {
            self.key_code(key, &mut modifiers)?
        };
        self.clear();
        Some(KeyEvent::new(code, modifiers).with_kind(key.event_kind()))
    }

    fn key_code(&mut self, key: WindowsKey, modifiers: &mut KeyModifiers) -> Option<KeyCode> {
        Some(match key.virtual_key {
            VK_BACK => KeyCode::Backspace,
            VK_TAB if modifiers.contains(KeyModifiers::SHIFT) => KeyCode::BackTab,
            VK_TAB => KeyCode::Tab,
            VK_RETURN => KeyCode::Enter,
            VK_ESCAPE => KeyCode::Esc,
            VK_PRIOR => KeyCode::PageUp,
            VK_NEXT => KeyCode::PageDown,
            VK_END => KeyCode::End,
            VK_HOME => KeyCode::Home,
            VK_LEFT => KeyCode::Left,
            VK_UP => KeyCode::Up,
            VK_RIGHT => KeyCode::Right,
            VK_DOWN => KeyCode::Down,
            VK_INSERT => KeyCode::Insert,
            VK_DELETE => KeyCode::Delete,
            VK_F1..=VK_F24 => {
                KeyCode::F(u32::from(key.virtual_key.saturating_sub(VK_F1)).saturating_add(1))
            }
            _ => match key.unit {
                0 => return None,
                1..=CONTROL_LAST => {
                    *modifiers = modifiers.with(Modifier::Control);
                    let scalar = u32::from(key.unit)
                        .saturating_add(u32::from(b'a'))
                        .saturating_sub(1);
                    KeyCode::Char(char::from_u32(scalar)?)
                }
                unit => KeyCode::Char(self.decode(unit)?),
            },
        })
    }
}

#[cfg(kani)]
mod proofs {
    use super::*;

    #[kani::proof]
    fn native_key_phase_preserves_alt_text_and_ordinary_releases() {
        let key = WindowsKey {
            down: kani::any(),
            repeats: kani::any(),
            virtual_key: kani::any(),
            unit: kani::any(),
            control_state: kani::any(),
        };
        let alt_text = key.down == 0 && key.virtual_key == WINDOWS_ALT_KEY && key.unit != 0;
        assert_eq!(key.alt_text(), alt_text);
        assert_eq!(
            key.event_kind() == KeyEventKind::Release,
            key.down == 0 && !alt_text
        );
        assert_eq!(
            key.event_kind() == KeyEventKind::Repeat,
            key.down != 0 && key.repeats > 1
        );
        assert_eq!(
            key.event_kind() == KeyEventKind::Press,
            alt_text || (key.down != 0 && key.repeats <= 1)
        );
        kani::cover!(alt_text && key.event_kind() == KeyEventKind::Press);
        kani::cover!(key.down == 0 && key.unit == 0 && key.event_kind() == KeyEventKind::Release);
        kani::cover!(
            key.down == 0
                && key.virtual_key != WINDOWS_ALT_KEY
                && key.unit != 0
                && key.event_kind() == KeyEventKind::Release
        );
        kani::cover!(key.event_kind() == KeyEventKind::Repeat);
    }

    #[kani::proof]
    #[kani::unwind(6)]
    fn every_alt_text_scalar_survives_interleaved_zero_unit_modifiers() {
        let character: char = kani::any();
        kani::assume(character != '\0');
        let mut units = [0_u16; 2];
        let mut decoder = Utf16Decoder::default();
        let mut delivered = None;
        for &unit in character.encode_utf16(&mut units).iter() {
            assert!(
                decoder
                    .windows_key(WindowsKey {
                        down: 1,
                        repeats: 1,
                        virtual_key: WINDOWS_ALT_KEY,
                        unit: 0,
                        control_state: LEFT_ALT
                    })
                    .is_none()
            );
            delivered = decoder.windows_key(WindowsKey {
                down: 0,
                repeats: 1,
                virtual_key: WINDOWS_ALT_KEY,
                unit,
                control_state: 0,
            });
        }
        assert_eq!(
            delivered,
            Some(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
        );
        kani::cover!(character == 'q');
        kani::cover!(character == '\u{3}');
        kani::cover!(character == '🦀');
    }

    #[kani::proof]
    fn rejects_dropping_an_alt_text_release() {
        let key = WindowsKey {
            down: 0,
            repeats: 1,
            virtual_key: WINDOWS_ALT_KEY,
            unit: 113,
            control_state: 0,
        };
        kani::assert(
            key.event_kind() == KeyEventKind::Release,
            "NEGATIVE_TERMINAL_ALT_TEXT_CONTROL",
        );
    }
}
