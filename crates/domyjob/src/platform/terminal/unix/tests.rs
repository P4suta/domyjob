use std::io;

use domyjob_core::terminal_input::{KeyCode, KeyEvent, KeyModifiers};
use rustix::fd::OwnedFd;
use rustix::fs::{Mode, OFlags};
use rustix::pty::OpenptFlags;
use rustix::termios::{Termios, Winsize};

use super::{Display, Input};

fn pair() -> (OwnedFd, OwnedFd) {
    let master = rustix::pty::openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY).unwrap();
    rustix::io::fcntl_setfd(&master, rustix::io::FdFlags::CLOEXEC).unwrap();
    rustix::pty::grantpt(&master).unwrap();
    rustix::pty::unlockpt(&master).unwrap();
    let name = rustix::pty::ptsname(&master, Vec::new()).unwrap();
    let slave = rustix::fs::open(
        name.as_c_str(),
        OFlags::RDWR | OFlags::NOCTTY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .unwrap();
    (master, slave)
}

fn same_modes(left: &Termios, right: &Termios) {
    assert_eq!(left.input_modes, right.input_modes);
    assert_eq!(left.output_modes, right.output_modes);
    assert_eq!(left.control_modes, right.control_modes);
    assert_eq!(left.local_modes, right.local_modes);
    assert_eq!(
        format!("{:?}", left.special_codes),
        format!("{:?}", right.special_codes)
    );
    assert_eq!(left.input_speed(), right.input_speed());
    assert_eq!(left.output_speed(), right.output_speed());
}

fn write(handle: &OwnedFd, bytes: &[u8]) {
    assert_eq!(rustix::io::write(handle, bytes).unwrap(), bytes.len());
}

const fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

#[test]
fn real_pty_raw_mode_restores_the_original_modes_idempotently() {
    let (master, slave) = pair();
    let original = rustix::termios::tcgetattr(&slave).unwrap();
    let input = Input::capture(slave).unwrap();
    input.setup().unwrap();
    let raw = rustix::termios::tcgetattr(&input.handle).unwrap();
    let mut expected = original.clone();
    expected.make_raw();
    same_modes(&raw, &expected);
    for _ in 0..2 {
        input.restore().unwrap();
        write(&master, b"restored\n");
        assert!(super::ready(&input.handle, 100).unwrap());
        let mut line = [0_u8; 32];
        let count = rustix::io::read(&input.handle, &mut line).unwrap();
        assert_eq!(line.get(..count).unwrap(), b"restored\n");
        same_modes(
            &rustix::termios::tcgetattr(&input.handle).unwrap(),
            &original,
        );
    }
}

#[cfg(target_os = "macos")]
#[test]
fn real_pty_restores_clear_pending_input_before_reading_without_consuming_queued_input() {
    use rustix::termios::{LocalModes, OptionalActions};

    let (master, slave) = pair();
    let mut canonical = rustix::termios::tcgetattr(&slave).unwrap();
    canonical.local_modes.insert(LocalModes::ICANON);
    canonical.local_modes.remove(LocalModes::PENDIN);
    rustix::termios::tcsetattr(&slave, OptionalActions::Now, &canonical).unwrap();
    assert_eq!(rustix::io::ioctl_fionread(&slave).unwrap(), 0);
    let original = rustix::termios::tcgetattr(&slave).unwrap();
    assert!(!original.local_modes.contains(LocalModes::PENDIN));
    let input = Input::capture(slave).unwrap();
    input.setup().unwrap();
    let queued = b"next command\n";
    write(&master, queued);
    assert_eq!(
        rustix::io::ioctl_fionread(&input.handle).unwrap(),
        u64::try_from(queued.len()).unwrap()
    );
    input.restore().unwrap();
    same_modes(
        &rustix::termios::tcgetattr(&input.handle).unwrap(),
        &original,
    );
    let mut line = [0_u8; 32];
    let count = rustix::io::read(&input.handle, &mut line).unwrap();
    assert_eq!(line.get(..count).unwrap(), queued);
    input.restore().unwrap();
    same_modes(
        &rustix::termios::tcgetattr(&input.handle).unwrap(),
        &original,
    );
}

#[test]
fn real_pty_delivers_fragmented_unicode_and_cursor_keys_across_quiet_polls() {
    let (master, slave) = pair();
    let mut input = Input::capture(slave).unwrap();
    input.setup().unwrap();
    write(&master, b"\xe7");
    assert_eq!(input.next(0).unwrap(), None);
    assert_eq!(input.next(0).unwrap(), None);
    write(&master, b"\x95\x8c\xf0\x9f\xa6\x80\x1b[");
    assert_eq!(input.next(100).unwrap(), Some(key(KeyCode::Char('界'))));
    assert_eq!(input.next(100).unwrap(), Some(key(KeyCode::Char('🦀'))));
    assert_eq!(input.next(0).unwrap(), None);
    write(&master, b"A\x1bOB\x03q");
    assert_eq!(input.next(100).unwrap(), Some(key(KeyCode::Up)));
    assert_eq!(input.next(100).unwrap(), Some(key(KeyCode::Down)));
    assert_eq!(
        input.next(100).unwrap(),
        Some(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
    );
    assert_eq!(input.next(100).unwrap(), Some(key(KeyCode::Char('q'))));
    input.restore().unwrap();
}

#[test]
fn real_pty_lone_escape_and_oversized_sequence_recover_without_ghost_keys() {
    let (master, slave) = pair();
    let mut input = Input::capture(slave).unwrap();
    input.setup().unwrap();
    write(&master, b"\x1b");
    assert_eq!(input.next(0).unwrap(), None);
    assert_eq!(input.next(0).unwrap(), Some(key(KeyCode::Esc)));
    write(&master, b"\x1b[");
    write(&master, &[b'9'; 80]);
    write(&master, b"Aq");
    assert_eq!(input.next(100).unwrap(), Some(key(KeyCode::Char('q'))));
    input.restore().unwrap();
}

#[test]
fn real_pty_resize_uses_the_current_visible_dimensions() {
    let (_master, slave) = pair();
    let display = Display { handle: slave };
    for (width, height) in [(128, 36), (48, 12)] {
        rustix::termios::tcsetwinsize(
            &display.handle,
            Winsize {
                ws_col: width,
                ws_row: height,
                ws_xpixel: 0,
                ws_ypixel: 0,
            },
        )
        .unwrap();
        assert_eq!(display.frame_size().unwrap(), (width, height));
    }
}

#[test]
fn oversized_poll_refuses_before_consuming_a_real_pty_key() {
    let (master, slave) = pair();
    let mut input = Input::capture(slave).unwrap();
    input.setup().unwrap();
    write(&master, b"q");
    assert_eq!(
        input.next(101).unwrap_err().kind(),
        io::ErrorKind::InvalidInput
    );
    assert_eq!(input.next(100).unwrap(), Some(key(KeyCode::Char('q'))));
    input.restore().unwrap();
}

#[test]
fn a_nonterminal_fd_refuses_mode_capture() {
    let (read, _write) = rustix::pipe::pipe().unwrap();
    assert_eq!(
        Input::capture(read).unwrap_err().raw_os_error(),
        Some(rustix::io::Errno::NOTTY.raw_os_error())
    );
}

#[test]
fn a_setup_failure_after_the_real_mode_change_keeps_original_restoration() {
    let (master, slave) = pair();
    let input = Input::capture(slave).unwrap();
    let original = input.original.clone();
    let failure = input
        .setup_using(|handle, raw| {
            rustix::termios::tcsetattr(handle, rustix::termios::OptionalActions::Now, raw)?;
            Err(io::Error::from_raw_os_error(7))
        })
        .unwrap_err();
    assert_eq!(failure.raw_os_error(), Some(7));
    let mut expected = original.clone();
    expected.make_raw();
    same_modes(
        &rustix::termios::tcgetattr(&input.handle).unwrap(),
        &expected,
    );
    input.restore().unwrap();
    write(&master, b"preserved\n");
    assert!(super::ready(&input.handle, 100).unwrap());
    let mut line = [0_u8; 32];
    let count = rustix::io::read(&input.handle, &mut line).unwrap();
    assert_eq!(line.get(..count).unwrap(), b"preserved\n");
    same_modes(
        &rustix::termios::tcgetattr(&input.handle).unwrap(),
        &original,
    );
}
