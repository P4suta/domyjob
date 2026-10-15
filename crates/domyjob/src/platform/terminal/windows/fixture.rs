#![expect(
    unsafe_code,
    clippy::disallowed_methods,
    reason = "This test leaf owns its pipes and exact child and retains the pseudoconsole through every spawn borrow"
)]

use std::fs::File;
use std::io::{self, Read as _, Write as _};
use std::num::NonZeroIsize;
use std::os::windows::io::{AsHandle as _, AsRawHandle as _, FromRawHandle as _, OwnedHandle};
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;

use domyjob_core::terminal_input::{self, KeyCode, KeyEvent, KeyModifiers};
use serde::{Deserialize, Serialize};
use windows_spawn::{AsPseudoConsole, Child, Command, DropPolicy, SpawnOptions};
use windows_sys::Win32::Foundation::{ERROR_BROKEN_PIPE, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Console::{
    COORD, ClosePseudoConsole, CreatePseudoConsole, ResizePseudoConsole,
};
use windows_sys::Win32::System::Pipes::CreatePipe;

use super::super::DisplayModes as _;
use super::{Display, Input, kernel};
use crate::platform::clock;
use crate::state_io;

const CHILD_ENV: &str = "DOMYJOB_CONPTY_FIXTURE_ROOT";
const READY_FILE: &str = "ready";
const REPORT_FILE: &str = "report";
const READY: &[u8] = b"R";
const BUDGET_MILLIS: u64 = 5000;
const POLL_MILLIS: u64 = 10;
const OUTPUT_PREFIX_BYTES: usize = 16_384;
const OUTPUT_CHUNK_BYTES: usize = 4096;
const MAX_RECORD_TRACE: usize = 64;
const INITIAL: (u16, u16) = (128, 36);
pub(super) const RESIZED: (u16, u16) = (48, 12);
const INPUT_BYTES: &[u8] = "λ界🦀\u{1b}[A\u{3}q".as_bytes();

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Report {
    pub(super) keys: Vec<String>,
    pub(super) records: Vec<String>,
    pub(super) before: (u32, u32),
    pub(super) after: (u32, u32),
    pub(super) size: (u16, u16),
}

#[derive(Debug, thiserror::Error)]
#[error("ConPTY fixture failed: primary={primary:?}; cleanup={cleanup:?}; output={output}")]
pub(super) struct Failure {
    #[source]
    primary: Option<io::Error>,
    cleanup: Vec<io::Error>,
    output: String,
}

impl Failure {
    fn primary(error: io::Error) -> Self {
        Self {
            primary: Some(error),
            cleanup: Vec::new(),
            output: String::new(),
        }
    }
}

#[derive(Debug)]
struct PseudoConsole(NonZeroIsize);

unsafe impl AsPseudoConsole for PseudoConsole {
    fn raw_pseudoconsole(&self) -> isize {
        self.0.get()
    }
}

impl Drop for PseudoConsole {
    fn drop(&mut self) {
        unsafe { ClosePseudoConsole(self.0.get()) };
    }
}

fn owned(handle: HANDLE) -> io::Result<OwnedHandle> {
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        Err(io::Error::other(
            "the fixture received an invalid pipe handle",
        ))
    } else {
        Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
    }
}

fn pipe() -> io::Result<(OwnedHandle, OwnedHandle)> {
    let mut reader = std::ptr::null_mut();
    let mut writer = std::ptr::null_mut();
    if unsafe { CreatePipe(&raw mut reader, &raw mut writer, std::ptr::null(), 0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let reader = owned(reader);
    let writer = owned(writer);
    Ok((reader?, writer?))
}

fn coord((width, height): (u16, u16)) -> io::Result<COORD> {
    Ok(COORD {
        X: i16::try_from(width).map_err(io::Error::other)?,
        Y: i16::try_from(height).map_err(io::Error::other)?,
    })
}

fn hresult(status: i32) -> io::Result<()> {
    if status < 0 {
        Err(io::Error::from_raw_os_error(status))
    } else {
        Ok(())
    }
}

fn drain(mut output: File) -> io::Result<Vec<u8>> {
    let mut retained = Vec::new();
    let mut buffer = [0_u8; OUTPUT_CHUNK_BYTES];
    loop {
        match output.read(&mut buffer) {
            Ok(0) => return Ok(retained),
            Ok(count) => {
                let available = OUTPUT_PREFIX_BYTES.saturating_sub(retained.len());
                retained.extend(buffer.iter().take(count.min(available)).copied());
            }
            Err(error)
                if error.raw_os_error()
                    == Some(i32::try_from(ERROR_BROKEN_PIPE).map_err(io::Error::other)?) =>
            {
                return Ok(retained);
            }
            Err(error) => return Err(error),
        }
    }
}

#[derive(Debug)]
struct Console {
    pty: Option<PseudoConsole>,
    writer: File,
    output: Option<JoinHandle<io::Result<Vec<u8>>>>,
}

impl Console {
    fn create() -> io::Result<(Self, (OwnedHandle, OwnedHandle))> {
        let size = coord(INITIAL)?;
        let (input_reader, input_writer) = pipe()?;
        let (output_reader, output_writer) = pipe()?;
        let output = std::thread::spawn(move || drain(File::from(output_reader)));
        let mut raw = 0;
        let result = hresult(unsafe {
            CreatePseudoConsole(
                size,
                input_reader.as_raw_handle(),
                output_writer.as_raw_handle(),
                0,
                &raw mut raw,
            )
        });
        if let Err(error) = result {
            drop(output_writer);
            drop(output.join());
            return Err(error);
        }
        let Some(raw) = NonZeroIsize::new(raw) else {
            drop(output_writer);
            drop(output.join());
            return Err(io::Error::other(
                "the fixture received a missing pseudoconsole",
            ));
        };
        Ok((
            Self {
                pty: Some(PseudoConsole(raw)),
                writer: File::from(input_writer),
                output: Some(output),
            },
            (input_reader, output_writer),
        ))
    }

    fn borrow(&self) -> io::Result<&PseudoConsole> {
        self.pty
            .as_ref()
            .ok_or_else(|| io::Error::other("the fixture pseudoconsole has already closed"))
    }

    fn resize(&self) -> io::Result<()> {
        hresult(unsafe { ResizePseudoConsole(self.borrow()?.0.get(), coord(RESIZED)?) })
    }

    fn close(&mut self) -> (Vec<u8>, Vec<io::Error>) {
        drop(self.pty.take());
        let mut failures = Vec::new();
        let output = match self.output.take().map(JoinHandle::join) {
            Some(Ok(Ok(bytes))) => bytes,
            Some(Ok(Err(error))) => {
                failures.push(error);
                Vec::new()
            }
            Some(Err(_panic)) => {
                failures.push(io::Error::other("the fixture output reader panicked"));
                Vec::new()
            }
            None => Vec::new(),
        };
        (output, failures)
    }
}

impl Drop for Console {
    fn drop(&mut self) {
        drop(self.close());
    }
}

fn await_ready(root: &Path, child: &mut Child) -> io::Result<()> {
    let deadline = clock::Deadline::after_millis(BUDGET_MILLIS);
    loop {
        if let Some(bytes) =
            state_io::read_bytes(&root.join(READY_FILE)).map_err(io::Error::other)?
        {
            return if bytes == READY {
                Ok(())
            } else {
                Err(io::Error::other(
                    "the fixture readiness signal is malformed",
                ))
            };
        }
        if let Some(status) = child.try_wait()? {
            return Err(io::Error::other(format!(
                "the fixture exited before readiness: {status}"
            )));
        }
        if deadline.expired() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "the fixture did not become ready",
            ));
        }
        clock::pause_millis(POLL_MILLIS);
    }
}

fn await_exit(child: &mut Child) -> io::Result<()> {
    let deadline = clock::Deadline::after_millis(BUDGET_MILLIS);
    loop {
        if let Some(status) = child.try_wait()? {
            return if status.success() {
                Ok(())
            } else {
                Err(io::Error::other(format!("the fixture failed: {status}")))
            };
        }
        if deadline.expired() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "the fixture did not finish",
            ));
        }
        clock::pause_millis(POLL_MILLIS);
    }
}

fn exercise(console: &mut Console, child: &mut Child, root: &Path) -> io::Result<Report> {
    await_ready(root, child)?;
    console.resize()?;
    console.writer.write_all(INPUT_BYTES)?;
    await_exit(child)?;
    let bytes = state_io::read_bytes(&root.join(REPORT_FILE))
        .map_err(io::Error::other)?
        .ok_or_else(|| io::Error::other("the fixture completion report is missing"))?;
    domyjob_core::ingress::json(&bytes, OUTPUT_PREFIX_BYTES).map_err(io::Error::other)
}

pub(super) fn run(test: &str) -> Result<Report, Failure> {
    let temporary = tempfile::tempdir().map_err(Failure::primary)?;
    let root = temporary.path().join("console");
    state_io::private_dir(&root).map_err(|error| Failure::primary(io::Error::other(error)))?;
    let executable = std::env::current_exe().map_err(Failure::primary)?;
    let (mut console, child_ends) = Console::create().map_err(Failure::primary)?;
    let mut command = Command::new(executable);
    command
        .args(["--exact", test, "--nocapture"])
        .env(CHILD_ENV, &root);
    let spawned = console.borrow().and_then(|console| {
        command.spawn_with(
            SpawnOptions::new()
                .pseudoconsole(console)
                .drop_policy(DropPolicy::KillTree),
        )
    });
    drop(child_ends);
    let mut child = match spawned {
        Ok(child) => child,
        Err(error) => {
            let (output, cleanup) = console.close();
            return Err(Failure {
                primary: Some(error),
                cleanup,
                output: String::from_utf8_lossy(&output).into_owned(),
            });
        }
    };
    let result = exercise(&mut console, &mut child, &root);
    let mut cleanup = Vec::new();
    if result.is_err() {
        if let Err(error) = child.kill() {
            cleanup.push(error);
        }
        if let Err(error) = child.wait() {
            cleanup.push(error);
        }
    }
    let (output, errors) = console.close();
    cleanup.extend(errors);
    match result {
        Ok(report) if cleanup.is_empty() => Ok(report),
        result => Err(Failure {
            primary: result.err(),
            cleanup,
            output: String::from_utf8_lossy(&output).into_owned(),
        }),
    }
}

pub(super) fn child_root() -> Option<PathBuf> {
    std::env::var_os(CHILD_ENV).map(PathBuf::from)
}

pub(super) fn expected_keys() -> Vec<String> {
    [
        KeyEvent::new(KeyCode::Char('λ'), KeyModifiers::NONE),
        KeyEvent::new(KeyCode::Char('界'), KeyModifiers::NONE),
        KeyEvent::new(KeyCode::Char('🦀'), KeyModifiers::NONE),
        KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
    ]
    .into_iter()
    .map(|key| format!("{key:?}"))
    .collect()
}

fn keys(input: &mut Input) -> (Vec<String>, Vec<String>) {
    let deadline = clock::Deadline::after_millis(BUDGET_MILLIS);
    let mut keys = Vec::new();
    let mut records = Vec::new();
    while !deadline.expired() {
        if let Some(key) = input
            .next_observed(POLL_MILLIS, kernel::records, |record| {
                if records.len() < MAX_RECORD_TRACE {
                    records.push(format!("{record:?}"));
                }
            })
            .unwrap()
            && key.kind != terminal_input::KeyEventKind::Release
        {
            let finished = key.code == KeyCode::Char('q');
            keys.push(format!("{key:?}"));
            if finished {
                return (keys, records);
            }
        }
    }
    panic!("the fixture input did not deliver its final key");
}

pub(super) fn child(root: &Path) {
    let mut input = Input::new(&io::stdin()).unwrap();
    let display = Display::new(&io::stdout()).unwrap();
    let before = (input.mode.original, display.mode.original);
    input.setup().unwrap();
    display.setup().unwrap();
    assert_eq!(
        kernel::mode(input.mode.handle.as_handle()).unwrap(),
        terminal_input::windows_input_mode(before.0)
    );
    assert_eq!(
        kernel::mode(display.mode.handle.as_handle()).unwrap(),
        terminal_input::windows_output_mode(before.1)
    );
    assert_eq!(display.frame_size().unwrap(), INITIAL);
    state_io::write_bytes(&root.join(READY_FILE), READY).unwrap();
    let (keys, records) = keys(&mut input);
    let size = display.frame_size().unwrap();
    input.restore().unwrap();
    display.restore().unwrap();
    input.restore().unwrap();
    display.restore().unwrap();
    let after = (
        kernel::mode(input.mode.handle.as_handle()).unwrap(),
        kernel::mode(display.mode.handle.as_handle()).unwrap(),
    );
    let report = Report {
        keys,
        records,
        before,
        after,
        size,
    };
    state_io::write_bytes(
        &root.join(REPORT_FILE),
        &serde_json::to_vec(&report).unwrap(),
    )
    .unwrap();
}
