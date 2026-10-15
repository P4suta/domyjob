use super::*;

#[kani::proof]
fn console_modes_preserve_every_unselected_bit() {
    let original: u32 = kani::any();
    let disabled = WINDOWS_PROCESSED_INPUT
        | WINDOWS_LINE_INPUT
        | WINDOWS_ECHO_INPUT
        | WINDOWS_QUICK_EDIT
        | WINDOWS_VIRTUAL_INPUT;
    let enabled = WINDOWS_WINDOW_INPUT | WINDOWS_EXTENDED_FLAGS;
    let output = WINDOWS_PROCESSED_OUTPUT | WINDOWS_VIRTUAL_OUTPUT | WINDOWS_DELAYED_WRAP;
    let input = windows_input_mode(original);
    assert_eq!(input & disabled, 0);
    assert_eq!(input & enabled, enabled);
    assert_eq!(
        input & !(disabled | enabled),
        original & !(disabled | enabled)
    );
    assert_eq!(windows_output_mode(original), original | output);
    kani::cover!(original == 0);
    kani::cover!(original == u32::MAX);
    kani::cover!(original & 16 != 0 && input & 16 != 0);
}

#[kani::proof]
fn console_viewports_are_checked_inclusive_rectangles() {
    let left: i16 = kani::any();
    let top: i16 = kani::any();
    let right: i16 = kani::any();
    let bottom: i16 = kani::any();
    let viewport = console_viewport(left, top, right, bottom);
    assert_eq!(
        viewport.is_some(),
        left >= 0 && top >= 0 && right >= left && bottom >= top
    );
    if let Some(viewport) = viewport {
        assert_eq!(
            i32::from(viewport.width),
            i32::from(right) - i32::from(left) + 1
        );
        assert_eq!(
            i32::from(viewport.height),
            i32::from(bottom) - i32::from(top) + 1
        );
        assert!(viewport.width > 0 && viewport.height > 0);
    }
    kani::cover!(viewport.is_some_and(|area| area.width == 1 && area.height == 1));
    kani::cover!(viewport.is_some_and(|area| area.width == 32_768));
    kani::cover!(left < 0 && viewport.is_none());
    kani::cover!(right < left && viewport.is_none());
}

#[kani::proof]
fn native_poll_and_descriptor_admission_are_finite() {
    let fd: i32 = kani::any();
    let millis: u64 = kani::any();
    let span = descriptor_span(fd);
    assert_eq!(span.is_some(), (0..MAX_TERMINAL_FD).contains(&fd));
    if let Some(span) = span {
        assert!(span > fd && span <= MAX_TERMINAL_FD);
        assert_eq!(span.checked_sub(1), Some(fd));
    }
    let nanos = poll_nanos(millis);
    assert_eq!(nanos.is_some(), millis <= MAX_POLL_MILLIS);
    if let Some(nanos) = nanos {
        assert_eq!(u64::try_from(nanos), Ok(millis * 1_000_000));
        assert!(nanos <= 100_000_000);
    }
    kani::cover!(fd == 0 && span == Some(1));
    kani::cover!(fd == MAX_TERMINAL_FD && span.is_none());
    kani::cover!(millis == MAX_POLL_MILLIS && nanos.is_some());
    kani::cover!(millis == u64::MAX && nanos.is_none());
}

#[kani::proof]
#[kani::unwind(5)]
fn every_unicode_scalar_survives_utf16_input() {
    let character: char = kani::any();
    let mut units = [0_u16; 2];
    let encoded = character.encode_utf16(&mut units);
    let mut decoder = Utf16Decoder::default();
    let mut result = None;
    for &unit in encoded.iter() {
        result = decoder.decode(unit);
    }
    assert_eq!(result, Some(character));
    let low: u16 = kani::any();
    kani::assume((0xdc00..=0xdfff).contains(&low));
    decoder.clear();
    assert_eq!(decoder.decode(low), None);
    kani::cover!(character == 'q');
    kani::cover!(character == '界');
    kani::cover!(character == '🦀');
}

#[kani::proof]
#[kani::unwind(6)]
fn every_unicode_scalar_survives_byte_fragmented_utf8() {
    let character: char = kani::any();
    kani::assume(!character.is_ascii());
    let mut bytes = [0_u8; 4];
    let encoded = character.encode_utf8(&mut bytes);
    let mut decoder = Decoder::default();
    let mut result = Decoded::Pending;
    for &byte in encoded.as_bytes() {
        result = decoder.decode(byte);
    }
    assert_eq!(
        result,
        Decoded::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
    );
    assert!(!decoder.pending());
    assert_eq!(decoder.buffered_bytes(), 0);
    kani::cover!(character == 'λ');
    kani::cover!(character == '界');
    kani::cover!(character == '🦀');
}

#[kani::proof]
#[kani::unwind(6)]
fn console_key_records_preserve_press_repeat_and_release() {
    let down: i32 = kani::any();
    let repeats: u16 = kani::any();
    let control_state: u32 = kani::any();
    let unit: u16 = kani::any();
    kani::assume((1..=26).contains(&unit));
    let mut decoder = Utf16Decoder::default();
    let result = decoder
        .windows_key(WindowsKey {
            down,
            repeats,
            virtual_key: 0,
            unit,
            control_state,
        })
        .unwrap();
    assert!(result.modifiers.contains(KeyModifiers::CONTROL));
    assert_eq!(
        result.code,
        KeyCode::Char(char::from_u32(u32::from(unit) + u32::from(b'a') - 1).unwrap())
    );
    assert_eq!(
        result.kind,
        if down == 0 {
            KeyEventKind::Release
        } else if repeats > 1 {
            KeyEventKind::Repeat
        } else {
            KeyEventKind::Press
        }
    );
    kani::cover!(result.kind == KeyEventKind::Press);
    kani::cover!(result.kind == KeyEventKind::Repeat);
    kani::cover!(result.kind == KeyEventKind::Release);
}

#[kani::proof]
fn rejects_disabling_existing_mouse_input() {
    let original = 16;
    kani::assert(
        windows_input_mode(original) & original == 0,
        "NEGATIVE_TERMINAL_MODE_CONTROL",
    );
}

#[kani::proof]
fn rejects_unbounded_native_poll() {
    kani::assert(
        poll_nanos(u64::MAX).is_some(),
        "NEGATIVE_TERMINAL_POLL_CONTROL",
    );
}

#[kani::proof]
fn rejects_an_inverted_console_viewport() {
    kani::assert(
        console_viewport(1, 0, 0, 0).is_some(),
        "NEGATIVE_TERMINAL_VIEWPORT_CONTROL",
    );
}

#[kani::proof]
fn rejects_an_unpaired_utf16_low_surrogate() {
    kani::assert(
        Utf16Decoder::default().decode(0xdc00).is_some(),
        "NEGATIVE_TERMINAL_UTF16_CONTROL",
    );
}
