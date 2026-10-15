#![expect(
    unsafe_code,
    reason = "This leaf borrows live console handles and checks Windows SDK outputs before admission"
)]

use std::io;
use std::os::windows::io::{AsRawHandle as _, BorrowedHandle};

use domyjob_core::terminal_input::{self, WindowsKey};
use windows_sys::Win32::Foundation::{WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::System::Console::{
    CONSOLE_SCREEN_BUFFER_INFO, DISABLE_NEWLINE_AUTO_RETURN, ENABLE_ECHO_INPUT,
    ENABLE_EXTENDED_FLAGS, ENABLE_LINE_INPUT, ENABLE_PROCESSED_INPUT, ENABLE_PROCESSED_OUTPUT,
    ENABLE_QUICK_EDIT_MODE, ENABLE_VIRTUAL_TERMINAL_INPUT, ENABLE_VIRTUAL_TERMINAL_PROCESSING,
    ENABLE_WINDOW_INPUT, GetConsoleMode, GetConsoleScreenBufferInfo, INPUT_RECORD, KEY_EVENT,
    ReadConsoleInputW, SetConsoleMode, WINDOW_BUFFER_SIZE_EVENT,
};
use windows_sys::Win32::System::Threading::WaitForSingleObject;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::VK_MENU;

const INPUT_DISABLED: u32 = ENABLE_PROCESSED_INPUT
    | ENABLE_LINE_INPUT
    | ENABLE_ECHO_INPUT
    | ENABLE_QUICK_EDIT_MODE
    | ENABLE_VIRTUAL_TERMINAL_INPUT;
const INPUT_ENABLED: u32 = ENABLE_WINDOW_INPUT | ENABLE_EXTENDED_FLAGS;
const OUTPUT_ENABLED: u32 =
    ENABLE_PROCESSED_OUTPUT | ENABLE_VIRTUAL_TERMINAL_PROCESSING | DISABLE_NEWLINE_AUTO_RETURN;

const _: () = assert!(terminal_input::windows_input_mode(0) == INPUT_ENABLED);
const _: () = assert!(terminal_input::WINDOWS_ALT_KEY == VK_MENU);
const _: () =
    assert!(terminal_input::windows_input_mode(u32::MAX) == !INPUT_DISABLED | INPUT_ENABLED);
const _: () = assert!(terminal_input::windows_output_mode(0) == OUTPUT_ENABLED);
const _: () = assert!(terminal_input::windows_output_mode(u32::MAX) == u32::MAX);

#[derive(Debug, Clone, Copy)]
pub(super) enum Record {
    Key(WindowsKey),
    Resize,
    Other,
}

fn checked(status: i32) -> io::Result<()> {
    if status == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

pub(super) fn mode(handle: BorrowedHandle<'_>) -> io::Result<u32> {
    let mut mode = 0;
    checked(unsafe { GetConsoleMode(handle.as_raw_handle(), &raw mut mode) })?;
    Ok(mode)
}

pub(super) fn set_mode(handle: BorrowedHandle<'_>, mode: u32) -> io::Result<()> {
    checked(unsafe { SetConsoleMode(handle.as_raw_handle(), mode) })
}

pub(super) fn window(handle: BorrowedHandle<'_>) -> io::Result<[i16; 4]> {
    let mut info = CONSOLE_SCREEN_BUFFER_INFO::default();
    checked(unsafe { GetConsoleScreenBufferInfo(handle.as_raw_handle(), &raw mut info) })?;
    let window = info.srWindow;
    Ok([window.Left, window.Top, window.Right, window.Bottom])
}

pub(super) fn records(handle: BorrowedHandle<'_>, millis: u32) -> io::Result<Vec<Record>> {
    match unsafe { WaitForSingleObject(handle.as_raw_handle(), millis) } {
        WAIT_TIMEOUT => return Ok(Vec::new()),
        WAIT_OBJECT_0 => {}
        WAIT_FAILED => return Err(io::Error::last_os_error()),
        _other => {
            return Err(io::Error::other(
                "the console wait returned an invalid status",
            ));
        }
    }
    let mut records = [INPUT_RECORD::default(); super::INPUT_BATCH];
    let capacity = u32::try_from(records.len()).map_err(io::Error::other)?;
    let mut count = 0;
    checked(unsafe {
        ReadConsoleInputW(
            handle.as_raw_handle(),
            records.as_mut_ptr(),
            capacity,
            &raw mut count,
        )
    })?;
    if count == 0 || count > capacity {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "the console read returned an invalid input count",
        ));
    }
    let count = usize::try_from(count).map_err(io::Error::other)?;
    Ok(records.into_iter().take(count).map(record).collect())
}

fn record(record: INPUT_RECORD) -> Record {
    match u32::from(record.EventType) {
        KEY_EVENT => {
            let key = unsafe { record.Event.KeyEvent };
            Record::Key(WindowsKey {
                down: key.bKeyDown,
                repeats: key.wRepeatCount,
                virtual_key: key.wVirtualKeyCode,
                unit: unsafe { key.uChar.UnicodeChar },
                control_state: key.dwControlKeyState,
            })
        }
        WINDOW_BUFFER_SIZE_EVENT => Record::Resize,
        _other => Record::Other,
    }
}
