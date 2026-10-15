use std::collections::VecDeque;
use std::io;

use domyjob_core::terminal_input::{KeyCode, KeyEvent, KeyModifiers, Utf16Decoder, WindowsKey};

use super::{Input, Mode, fixture, kernel};

const ORIGINAL_MODE: u32 = 0x1234;
const REFUSED_MODE: u32 = 0xabcd;
const SETUP_ERRNO: i32 = 5;
const READ_ERRNO: i32 = 6;
const KEY_Q: u16 = 81;
const CHAR_Q: u16 = 113;

fn mode() -> Mode {
    Mode {
        handle: tempfile::tempfile().unwrap().into(),
        original: ORIGINAL_MODE,
    }
}

fn input() -> Input {
    Input {
        mode: mode(),
        pending: VecDeque::new(),
        decoder: Utf16Decoder::default(),
    }
}

fn key() -> kernel::Record {
    kernel::Record::Key(WindowsKey {
        down: 1,
        repeats: 1,
        virtual_key: KEY_Q,
        unit: CHAR_Q,
        control_state: 0,
    })
}

#[test]
fn a_failed_console_setup_keeps_the_exact_mode_for_every_restore() {
    let mode = mode();
    let failure = mode
        .setup_using(REFUSED_MODE, |_handle, mode| {
            assert_eq!(mode, REFUSED_MODE);
            Err(io::Error::from_raw_os_error(SETUP_ERRNO))
        })
        .unwrap_err();
    assert_eq!(failure.raw_os_error(), Some(SETUP_ERRNO));
    let mut restored = Vec::new();
    for _attempt in 0..2 {
        mode.restore_using(|_handle, mode| {
            restored.push(mode);
            Ok(())
        })
        .unwrap();
    }
    assert_eq!(restored, [ORIGINAL_MODE, ORIGINAL_MODE]);
}

#[test]
fn invalid_poll_budgets_and_oversized_batches_refuse_before_consuming_keys() {
    let mut input = input();
    let expected = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
    input.pending.push_back(expected);
    let mut reads = 0;
    let budget_failure = input
        .next_using(
            super::MAX_POLL_MILLIS.saturating_add(1),
            |_handle, _millis| {
                reads += 1;
                Ok(Vec::new())
            },
        )
        .unwrap_err();
    assert_eq!(budget_failure.kind(), io::ErrorKind::InvalidInput);
    assert_eq!(reads, 0);
    assert_eq!(input.pending.pop_front(), Some(expected));
    let batch_failure = input
        .next_using(0, |_handle, _millis| {
            Ok(vec![key(); super::INPUT_BATCH.saturating_add(1)])
        })
        .unwrap_err();
    assert_eq!(batch_failure.kind(), io::ErrorKind::InvalidData);
    assert!(input.pending.is_empty());
}

#[test]
fn console_batches_keep_keys_across_resize_and_preserve_native_errors() {
    let mut input = input();
    let expected = Some(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE));
    let key = input
        .next_using(0, |_handle, millis| {
            assert_eq!(millis, 0);
            Ok(vec![
                kernel::Record::Other,
                key(),
                kernel::Record::Resize,
                key(),
            ])
        })
        .unwrap();
    assert_eq!(key, expected);
    let mut reads = 0;
    assert_eq!(
        input
            .next_using(0, |_handle, _millis| {
                reads += 1;
                Err(io::Error::from_raw_os_error(READ_ERRNO))
            })
            .unwrap(),
        expected
    );
    assert_eq!(reads, 0);
    let failure = input
        .next_using(0, |_handle, _millis| {
            Err(io::Error::from_raw_os_error(READ_ERRNO))
        })
        .unwrap_err();
    assert_eq!(failure.raw_os_error(), Some(READ_ERRNO));
}

#[test]
fn non_console_handles_keep_the_native_constructor_error() {
    let file = tempfile::tempfile().unwrap();
    let error = Mode::capture(file.into()).unwrap_err();
    assert_eq!(error.raw_os_error(), Some(READ_ERRNO));
}

#[test]
fn owned_conpty_preserves_unicode_keys_resize_and_original_console_modes() {
    if let Some(root) = fixture::child_root() {
        fixture::child(&root);
        return;
    }
    let qualified = concat!(
        module_path!(),
        "::owned_conpty_preserves_unicode_keys_resize_and_original_console_modes"
    );
    let test = qualified
        .strip_prefix(concat!(env!("CARGO_CRATE_NAME"), "::"))
        .unwrap();
    let report = fixture::run(test).unwrap();
    assert_eq!(
        report.keys,
        fixture::expected_keys(),
        "owned record trace: {:?}",
        report.records
    );
    assert_eq!(report.before, report.after);
    assert_eq!(report.size, fixture::RESIZED);
}
