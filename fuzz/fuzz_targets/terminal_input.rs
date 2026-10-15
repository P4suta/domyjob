#![no_main]

use domyjob_core::terminal_input::{self, Decoded, Decoder, KeyCode, Utf16Decoder, WindowsKey};
use libfuzzer_sys::fuzz_target;

const QUAD_BYTES: usize = 4;
const UNIT_BYTES: usize = 2;
const KIND_BIT: u8 = 1;

fuzz_target!(|data: &[u8]| {
    let mut decoder = Decoder::default();
    for &byte in data {
        if let Decoded::Key(event) = decoder.decode(byte)
            && let KeyCode::Char(character) = event.code
        {
            assert!(char::from_u32(u32::from(character)).is_some());
        }
        assert!(decoder.buffered_bytes() <= terminal_input::MAX_SEQUENCE_BYTES);
    }
    let _quiet = decoder.quiet();
    assert!(decoder.buffered_bytes() <= terminal_input::MAX_SEQUENCE_BYTES);
    let _end = decoder.finish();
    assert!(!decoder.pending());
    assert_eq!(decoder.buffered_bytes(), 0);
    let mut windows = Utf16Decoder::default();
    for chunk in data.chunks_exact(QUAD_BYTES) {
        let (key, unit) = chunk.split_at(UNIT_BYTES);
        let record = WindowsKey {
            down: i32::from(chunk.first().copied().unwrap_or_default() & KIND_BIT),
            repeats: u16::from(chunk.last().copied().unwrap_or_default()),
            virtual_key: u16::from_le_bytes(key.try_into().unwrap()),
            unit: u16::from_le_bytes(unit.try_into().unwrap()),
            control_state: u32::from_le_bytes(chunk.try_into().unwrap()),
        };
        let _key = windows.windows_key(record);
    }
    if let Ok(text) = core::str::from_utf8(data) {
        for character in text.chars() {
            let mut units = [0_u16; UNIT_BYTES];
            let mut decoder = Utf16Decoder::default();
            let mut decoded = None;
            for &unit in character.encode_utf16(&mut units).iter() {
                decoded = decoder.decode(unit);
            }
            assert_eq!(decoded, Some(character));
        }
    }
});
