#![expect(
    unsafe_code,
    reason = "Toolhelp reports process parents; a native overlapped pipe reproduces OpenSSH stdin"
)]

use std::io;
use std::io::Write as _;
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_FLAG_OVERLAPPED, PIPE_ACCESS_INBOUND, WriteFile,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::Pipes::{
    CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT,
};

use crate::Context as _;

const INPUT_PREFIX: &[u8] = b"\x00\xff\x80\x01\x02\x03";
const INPUT_DEADLINE: Duration = Duration::from_secs(5);
const PIPE_BUFFER_BYTES: u32 = 4096;
const INPUT_SPLIT_BYTES: usize = 3;
const INPUT_POLL_MILLIS: u64 = 10;
const MAX_INPUT_DIAGNOSTIC_BYTES: u64 = 65_536;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InputSchedule {
    Buffered,
    Pending,
    ZeroWrite,
    Truncated,
    NativeError,
    ObserverError,
}

struct InputCase<'a> {
    root: &'a Path,
    shell: &'a str,
    schedule: InputSchedule,
}

struct InputCapture {
    output: tempfile::NamedTempFile,
    errors: tempfile::NamedTempFile,
}

impl InputSchedule {
    const fn failure(self) -> Option<&'static str> {
        match self {
            Self::Buffered | Self::Pending | Self::ZeroWrite => None,
            Self::Truncated => Some("truncated bootstrap source"),
            Self::NativeError => Some("native error 5"),
            Self::ObserverError => Some("injected pending observer failure"),
        }
    }
}

fn input_pipe(root: &Path) -> io::Result<(std::fs::File, std::fs::File)> {
    let identity = root
        .file_name()
        .ok_or_else(|| io::Error::other("fixture root missing"))?;
    let name = format!(r"\\.\pipe\{}-bootstrap-input", identity.to_string_lossy());
    let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
    let handle = unsafe {
        CreateNamedPipeW(
            wide.as_ptr(),
            PIPE_ACCESS_INBOUND | FILE_FLAG_OVERLAPPED,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
            1,
            PIPE_BUFFER_BYTES,
            PIPE_BUFFER_BYTES,
            0,
            std::ptr::null(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let reader = unsafe { std::fs::File::from_raw_handle(handle) };
    let writer = std::fs::OpenOptions::new().write(true).open(name)?;
    Ok((reader, writer))
}

pub(super) fn bootstrap_input_contract(root: &Path) -> Result<(), crate::Failure> {
    for (shell, schedules) in [
        (
            "powershell.exe",
            &[
                InputSchedule::Buffered,
                InputSchedule::Pending,
                InputSchedule::ZeroWrite,
                InputSchedule::Truncated,
                InputSchedule::NativeError,
                InputSchedule::ObserverError,
            ][..],
        ),
        (
            "pwsh.exe",
            &[InputSchedule::Buffered, InputSchedule::Pending][..],
        ),
    ] {
        for &schedule in schedules {
            input_case(root, shell, schedule)?;
        }
    }
    Ok(())
}

fn input_script(root: &Path, schedule: InputSchedule) -> Result<String, crate::Failure> {
    let implementation = include_str!("../../../src/transport/bootstrap/windows-input.cs").replace(
        "@SOURCE_MAX@",
        &domyjob_core::wire::MAX_SNAPSHOT_BYTES.to_string(),
    );
    let prefix = root.join("bootstrap-input-prefix.bin");
    let marker = root.join("bootstrap-input-pending.txt");
    let executable = std::env::current_exe().context("locating the binary input fixture")?;
    let observation = if schedule == InputSchedule::ObserverError {
        "throw 'injected pending observer failure'"
    } else {
        ""
    };
    let observer = if schedule == InputSchedule::Buffered || schedule == InputSchedule::NativeError
    {
        "$null".to_owned()
    } else {
        format!(
            "[Action]{{[IO.File]::AppendAllText('{}',\"pending`n\"); {observation}}}",
            quote(&marker)
        )
    };
    Ok(format!("$ErrorActionPreference='Stop'; Add-Type -TypeDefinition @'\n{implementation}\n'@\ntry {{
if ([DomyjobBootstrapInput]::ReadBytes(0).Length -ne 0) {{ throw 'zero read consumed input' }}
foreach ($invalid in @(-1, {})) {{ try {{ $null=[DomyjobBootstrapInput]::ReadBytes($invalid); throw 'invalid count accepted' }} catch {{ if ($_.Exception.InnerException -isnot [ArgumentOutOfRangeException]) {{ throw }} }} }}
$source=[DomyjobBootstrapInput]::ReadBytes({}, {observer});
[IO.File]::WriteAllBytes('{}',$source);
$s=New-Object Diagnostics.ProcessStartInfo; $s.FileName='{}'; $s.Arguments='--bootstrap-input-remainder'; $s.UseShellExecute=$false;
$p=[Diagnostics.Process]::Start($s); $p.WaitForExit(); exit $p.ExitCode
}} catch {{ $cause=$_.Exception; while ($null -ne $cause.InnerException) {{ $cause=$cause.InnerException }}; if ($cause -is [ComponentModel.Win32Exception]) {{ [Console]::Error.WriteLine('native error '+$cause.NativeErrorCode) }}; [Console]::Error.WriteLine($_); exit 1 }}",
        domyjob_core::wire::MAX_SNAPSHOT_BYTES.saturating_add(1), INPUT_PREFIX.len(), quote(&prefix), quote(&executable)))
}

fn input_case(root: &Path, shell: &str, schedule: InputSchedule) -> Result<(), crate::Failure> {
    let case = InputCase {
        root,
        shell,
        schedule,
    };
    let script = input_script(root, schedule)?;
    let capture = InputCapture {
        output: tempfile::NamedTempFile::new_in(root)
            .context("preparing the binary input output")?,
        errors: tempfile::NamedTempFile::new_in(root)
            .context("preparing the binary input diagnostics")?,
    };
    let marker = root.join("bootstrap-input-pending.txt");
    std::fs::write(&marker, b"").context("resetting the native pending observations")?;
    let prefix = root.join("bootstrap-input-prefix.bin");
    if crate::present(&prefix)? {
        std::fs::remove_file(&prefix).context("resetting the source prefix evidence")?;
    }
    let (input, mut writer) = input_pipe(root).context("creating real overlapped stdin")?;
    let input = if schedule == InputSchedule::NativeError {
        drop(input);
        writer
            .try_clone()
            .context("inheriting the write-only pipe as stdin")?
    } else {
        input
    };
    if schedule == InputSchedule::Buffered {
        write_input(&mut writer, INPUT_PREFIX)?;
    }
    let mut child = Command::new(shell)
        .args(["-NoProfile", "-Command", &script])
        .stdin(Stdio::from(input))
        .stdout(Stdio::from(
            capture
                .output
                .as_file()
                .try_clone()
                .context("cloning fixture stdout")?,
        ))
        .stderr(Stdio::from(
            capture
                .errors
                .as_file()
                .try_clone()
                .context("cloning fixture stderr")?,
        ))
        .spawn()
        .context("starting the actual Framework input reader")?;
    let supplied = supply_input(&marker, &mut child, writer, schedule);
    if let Err(failure) = supplied {
        let _killed = child.kill();
        let _reaped = child.wait();
        return Err(input_failure(&case, &failure, &capture));
    }
    let status = match wait_reader(&mut child) {
        Ok(status) => status,
        Err(failure) => {
            return Err(input_failure(&case, &failure, &capture));
        }
    };
    check_input_result(&case, status, &capture)
}

fn check_input_result(
    case: &InputCase<'_>,
    status: std::process::ExitStatus,
    capture: &InputCapture,
) -> Result<(), crate::Failure> {
    let InputCase {
        root,
        shell,
        schedule,
    } = *case;
    let prefix = root.join("bootstrap-input-prefix.bin");
    let diagnostic =
        diagnostic_prefix(capture.errors.as_file()).context("reading input diagnostics")?;
    if let Some(expected) = schedule.failure() {
        ensure!(
            status.code() == Some(1) && diagnostic.contains(expected),
            "{shell} {schedule:?} lost its exact read failure: {status}; {diagnostic}"
        );
        ensure!(
            !crate::present(&prefix)?,
            "a refused read published a prefix"
        );
        ensure!(
            std::fs::read(capture.output.path())
                .context("reading refused output")?
                .is_empty(),
            "a refused read launched the first node"
        );
    } else {
        ensure!(
            status.code() == Some(i32::from(crate::bootstrap::PRIMARY_EXIT)),
            "{shell} {schedule:?} input reader failed: {status}; {diagnostic}"
        );
        ensure!(
            std::fs::read(prefix).context("reading the consumed prefix")? == INPUT_PREFIX,
            "the source prefix changed"
        );
        ensure!(
            std::fs::read(capture.output.path()).context("reading inherited binary bytes")?
                == crate::bootstrap::INPUT_REMAINDER,
            "the inherited protocol bytes changed"
        );
    }
    Ok(())
}

fn write_input(writer: &mut std::fs::File, prefix: &[u8]) -> Result<(), crate::Failure> {
    writer
        .write_all(prefix)
        .context("writing the exact source prefix")?;
    writer
        .write_all(crate::bootstrap::INPUT_REMAINDER)
        .context("writing the original binary remainder")
}

fn supply_input(
    marker: &Path,
    child: &mut std::process::Child,
    mut writer: std::fs::File,
    schedule: InputSchedule,
) -> Result<(), crate::Failure> {
    let (first, second) = INPUT_PREFIX.split_at(INPUT_SPLIT_BYTES);
    match schedule {
        InputSchedule::Buffered | InputSchedule::NativeError => {}
        InputSchedule::ObserverError => {
            wait_pending(marker, child, 1)?;
            write_input(&mut writer, INPUT_PREFIX)?;
        }
        InputSchedule::Truncated => {
            wait_pending(marker, child, 1)?;
            writer
                .write_all(first)
                .context("injecting truncated native EOF")?;
        }
        InputSchedule::Pending => {
            wait_pending(marker, child, 1)?;
            writer
                .write_all(first)
                .context("releasing the first pending short read")?;
            wait_pending(marker, child, 2)?;
            write_input(&mut writer, second)?;
        }
        InputSchedule::ZeroWrite => {
            wait_pending(marker, child, 1)?;
            write_zero(&writer).context("releasing a successful zero-byte native read")?;
            write_input(&mut writer, INPUT_PREFIX)?;
        }
    }
    Ok(())
}

fn input_failure(
    case: &InputCase<'_>,
    failure: &impl std::fmt::Display,
    capture: &InputCapture,
) -> crate::Failure {
    let InputCase {
        root,
        shell,
        schedule,
    } = *case;
    let diagnostics = diagnostic_prefix(capture.errors.as_file())
        .unwrap_or_else(|error| format!("stderr capture refused: {error}"));
    let pending = std::fs::File::open(root.join("bootstrap-input-pending.txt"))
        .and_then(diagnostic_prefix)
        .unwrap_or_else(|error| format!("pending capture refused: {error}"));
    let report = format!(
        "{shell} {schedule:?}: {failure}\npending observations:\n{pending}\nstderr:\n{diagnostics}"
    );
    let path = root.join("bootstrap-input-failure.log");
    let retained = std::fs::write(&path, &report);
    crate::Failure::new(format!(
        "{report}\nretaining {}: {retained:?}",
        path.display()
    ))
}

fn write_zero(writer: &std::fs::File) -> io::Result<()> {
    let sentinel = [0_u8];
    let mut received = 0;
    let success = unsafe {
        WriteFile(
            writer.as_raw_handle(),
            sentinel.as_ptr(),
            0,
            &raw mut received,
            std::ptr::null_mut(),
        )
    };
    if success == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn wait_pending(
    marker: &Path,
    child: &mut std::process::Child,
    count: usize,
) -> Result<(), crate::Failure> {
    let started = Instant::now();
    loop {
        if std::fs::read_to_string(marker)
            .context("reading native pending observations")?
            .lines()
            .count()
            >= count
        {
            return Ok(());
        }
        if let Some(status) = child
            .try_wait()
            .context("checking the owned input reader")?
        {
            return Err(crate::Failure::new(format!(
                "input reader exited before native pending {count}: {status}"
            )));
        }
        ensure!(
            started.elapsed() < INPUT_DEADLINE,
            "native pending observation {count} exceeded its finite deadline"
        );
        crate::pause(INPUT_POLL_MILLIS);
    }
}

fn quote(path: &Path) -> String {
    path.to_string_lossy().replace('\'', "''")
}

fn wait_reader(child: &mut std::process::Child) -> io::Result<std::process::ExitStatus> {
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if started.elapsed() >= INPUT_DEADLINE {
            child.kill()?;
            child.wait()?;
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "overlapped stdin reader exceeded its finite deadline",
            ));
        }
        crate::pause(INPUT_POLL_MILLIS);
    }
}

fn diagnostic_prefix(mut source: impl io::Read + io::Seek) -> io::Result<String> {
    let length = source.seek(io::SeekFrom::End(0))?;
    source.seek(io::SeekFrom::Start(0))?;
    let count =
        usize::try_from(length.min(MAX_INPUT_DIAGNOSTIC_BYTES)).map_err(io::Error::other)?;
    let mut bytes = vec![0; count];
    source.read_exact(&mut bytes)?;
    let text = String::from_utf8_lossy(&bytes);
    let omitted = length.saturating_sub(MAX_INPUT_DIAGNOSTIC_BYTES);
    Ok(if omitted == 0 {
        text.into_owned()
    } else {
        format!("{text}\n[{omitted} later input diagnostic bytes omitted]")
    })
}

pub(super) fn parent_of(pid: u32) -> io::Result<u32> {
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let size = u32::try_from(size_of::<PROCESSENTRY32W>()).map_err(io::Error::other)?;
    let mut entry = PROCESSENTRY32W {
        dwSize: size,
        ..PROCESSENTRY32W::default()
    };
    let mut parent = None;
    let mut more = unsafe { Process32FirstW(snapshot, &raw mut entry) } != 0;
    while more {
        if entry.th32ProcessID == pid {
            parent = Some(entry.th32ParentProcessID);
            break;
        }
        more = unsafe { Process32NextW(snapshot, &raw mut entry) } != 0;
    }
    unsafe { CloseHandle(snapshot) };
    parent.ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("no process {pid}")))
}
