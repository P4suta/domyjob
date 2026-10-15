use super::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, Modifier};

const DECIMAL: u32 = 10;
const ASCII_ZERO: u8 = b'0';
const MAX_FIELDS: usize = 3;
const DEFAULT_MODIFIER: u32 = 1;
const MAX_MODIFIER: u32 = 8;
const SHIFT: u32 = 1;
const ALT: u32 = 2;
const CONTROL: u32 = 4;
const REPEAT: u32 = 2;
const RELEASE: u32 = 3;
const F2: KeyCode = KeyCode::F(2);
const F3: KeyCode = KeyCode::F(3);
const F4: KeyCode = KeyCode::F(4);
const TILDE_KEYS: [(u32, KeyCode); 20] = [
    (1, KeyCode::Home),
    (2, KeyCode::Insert),
    (3, KeyCode::Delete),
    (4, KeyCode::End),
    (5, KeyCode::PageUp),
    (6, KeyCode::PageDown),
    (7, KeyCode::Home),
    (8, KeyCode::End),
    (11, KeyCode::F(1)),
    (12, KeyCode::F(2)),
    (13, KeyCode::F(3)),
    (14, KeyCode::F(4)),
    (15, KeyCode::F(5)),
    (17, KeyCode::F(6)),
    (18, KeyCode::F(7)),
    (19, KeyCode::F(8)),
    (20, KeyCode::F(9)),
    (21, KeyCode::F(10)),
    (23, KeyCode::F(11)),
    (24, KeyCode::F(12)),
];

fn fields(bytes: &[u8]) -> Option<[u32; MAX_FIELDS]> {
    let mut fields = [0_u32; MAX_FIELDS];
    let mut position = 0usize;
    for &byte in bytes {
        if byte == b';' || byte == b':' {
            position = position.checked_add(1)?;
            if position >= MAX_FIELDS {
                return None;
            }
        } else {
            let digit = byte.checked_sub(ASCII_ZERO)?;
            if u32::from(digit) >= DECIMAL {
                return None;
            }
            let slot = fields.get_mut(position)?;
            *slot = slot.checked_mul(DECIMAL)?.checked_add(u32::from(digit))?;
        }
    }
    Some(fields)
}

fn modifiers(value: u32) -> Option<KeyModifiers> {
    let value = match value {
        0 => DEFAULT_MODIFIER,
        value => value,
    };
    if value > MAX_MODIFIER {
        return None;
    }
    let bits = value.checked_sub(DEFAULT_MODIFIER)?;
    let mut modifiers = KeyModifiers::NONE;
    for (flag, modifier) in [
        (SHIFT, Modifier::Shift),
        (ALT, Modifier::Alt),
        (CONTROL, Modifier::Control),
    ] {
        if bits & flag != 0 {
            modifiers = modifiers.with(modifier);
        }
    }
    Some(modifiers)
}

pub(super) fn key(bytes: &[u8]) -> Option<KeyEvent> {
    let (&last, parameters) = bytes.split_last()?;
    let [first, modifier, kind] = fields(parameters)?;
    let code = match last {
        b'A' => KeyCode::Up,
        b'B' => KeyCode::Down,
        b'C' => KeyCode::Right,
        b'D' => KeyCode::Left,
        b'H' => KeyCode::Home,
        b'F' => KeyCode::End,
        b'Z' => return Some(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT)),
        b'P' => KeyCode::F(1),
        b'Q' => F2,
        b'R' => F3,
        b'S' => F4,
        b'~' => TILDE_KEYS.iter().find(|(value, _)| *value == first)?.1,
        b'u' => match first {
            0 => KeyCode::Null,
            value => KeyCode::Char(char::from_u32(value)?),
        },
        _ => return None,
    };
    let event = KeyEvent::new(code, modifiers(modifier)?);
    let kind = match kind {
        0 | DEFAULT_MODIFIER => KeyEventKind::Press,
        REPEAT => KeyEventKind::Repeat,
        RELEASE => KeyEventKind::Release,
        _ => return None,
    };
    Some(event.with_kind(kind))
}

#[cfg(kani)]
mod proofs {
    use super::*;

    #[kani::proof]
    #[kani::unwind(6)]
    fn cursor_modifier_fields_require_the_complete_supported_domain() {
        let digit: u8 = kani::any();
        let result = key(&[b'1', b';', digit, b'A']);
        let empty_default = digit == b';' || digit == b':';
        assert_eq!(
            result.is_some(),
            (b'0'..=b'8').contains(&digit) || empty_default
        );
        if let Some(result) = result {
            assert_eq!(result.code, KeyCode::Up);
            assert_eq!(result.kind, KeyEventKind::Press);
            let value = if empty_default {
                DEFAULT_MODIFIER
            } else {
                u32::from(digit - b'0').max(DEFAULT_MODIFIER)
            };
            let bits = value - DEFAULT_MODIFIER;
            assert_eq!(
                result.modifiers.contains(KeyModifiers::SHIFT),
                bits & SHIFT != 0
            );
            assert_eq!(
                result.modifiers.contains(KeyModifiers::ALT),
                bits & ALT != 0
            );
            assert_eq!(
                result.modifiers.contains(KeyModifiers::CONTROL),
                bits & CONTROL != 0
            );
        }
        kani::cover!(digit == b'0' && result.is_some());
        kani::cover!(digit == b'8' && result.is_some());
        kani::cover!(digit == b'9' && result.is_none());
        kani::cover!(empty_default && result.is_some());
    }

    #[kani::proof]
    #[kani::unwind(6)]
    fn rejects_an_unsupported_cursor_modifier() {
        kani::assert(key(b"1;9A").is_some(), "NEGATIVE_TERMINAL_CSI_CONTROL");
    }
}
