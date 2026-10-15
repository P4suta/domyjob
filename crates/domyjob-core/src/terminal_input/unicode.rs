const TWO_FIRST: u8 = 0xc2;
const TWO_LAST: u8 = 0xdf;
const THREE_FIRST: u8 = 0xe0;
const THREE_LAST: u8 = 0xef;
const FOUR_FIRST: u8 = 0xf0;
const FOUR_LAST: u8 = 0xf4;
const TWO_BYTES: usize = 2;
const THREE_BYTES: usize = 3;
const FOUR_BYTES: usize = 4;
const TWO_MASK: u8 = 0x1f;
const THREE_MASK: u8 = 0x0f;
const FOUR_MASK: u8 = 0x07;
const TWO_MINIMUM: u32 = 0x80;
const THREE_MINIMUM: u32 = 0x800;
const FOUR_MINIMUM: u32 = 0x10000;
const CONTINUATION_MASK: u8 = 0xc0;
const CONTINUATION: u8 = 0x80;
const PAYLOAD_MASK: u8 = 0x3f;
const PAYLOAD_SHIFT: u32 = 6;

pub(super) fn scalar(bytes: &[u8]) -> Option<char> {
    let (&first, rest) = bytes.split_first()?;
    let (length, mask, minimum) = match first {
        TWO_FIRST..=TWO_LAST => (TWO_BYTES, TWO_MASK, TWO_MINIMUM),
        THREE_FIRST..=THREE_LAST => (THREE_BYTES, THREE_MASK, THREE_MINIMUM),
        FOUR_FIRST..=FOUR_LAST => (FOUR_BYTES, FOUR_MASK, FOUR_MINIMUM),
        _ => return None,
    };
    if bytes.len() != length {
        return None;
    }
    let mut scalar = u32::from(first & mask);
    for &byte in rest {
        if byte & CONTINUATION_MASK != CONTINUATION {
            return None;
        }
        scalar = scalar
            .checked_shl(PAYLOAD_SHIFT)?
            .checked_add(u32::from(byte & PAYLOAD_MASK))?;
    }
    if scalar < minimum {
        return None;
    }
    char::from_u32(scalar)
}

#[cfg(kani)]
mod proofs {
    use super::*;

    #[kani::proof]
    #[kani::unwind(6)]
    fn only_canonical_utf8_sequences_admit_a_scalar() {
        let bytes: [u8; FOUR_BYTES] = kani::any();
        let length = usize::from(kani::any::<u8>() % 3).saturating_add(TWO_BYTES);
        let input = bytes.get(..length).unwrap();
        let result = scalar(input);
        if let Some(character) = result {
            assert!(!character.is_ascii());
            let mut canonical = [0_u8; FOUR_BYTES];
            assert_eq!(character.encode_utf8(&mut canonical).as_bytes(), input);
        }
        kani::cover!(length == TWO_BYTES && result.is_some());
        kani::cover!(length == THREE_BYTES && result.is_some());
        kani::cover!(length == FOUR_BYTES && result.is_some());
        kani::cover!(input == [0xc0, 0x80] && result.is_none());
        kani::cover!(input == [0xed, 0xa0, 0x80] && result.is_none());
        kani::cover!(input == [0xf4, 0x90, 0x80, 0x80] && result.is_none());
    }

    #[kani::proof]
    #[kani::unwind(6)]
    fn rejects_an_overlong_utf8_scalar() {
        kani::assert(
            scalar(&[0xc0, 0x80]).is_some(),
            "NEGATIVE_TERMINAL_CANONICAL_CONTROL",
        );
    }
}
