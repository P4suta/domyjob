use std::io;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};

pub(crate) const MAX_WORKER_DIAGNOSTIC_BYTES: u64 = 4096;
pub(crate) const MAX_SAMPLE_DIAGNOSTIC_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SampleRefusal {
    Spawn(io::ErrorKind),
    Wait(io::ErrorKind),
    Exit(Option<i32>),
    Capture(io::ErrorKind),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Sampling {
    Captured,
    Refused(SampleRefusal),
    Unsupported,
    ServiceExited,
}

pub(crate) struct SampleEvidence {
    pub(crate) outcome: Sampling,
    pub(crate) text: String,
}

impl SampleEvidence {
    pub(crate) fn collecting_failed(error: &io::Error) -> Self {
        Self {
            outcome: Sampling::Refused(SampleRefusal::Capture(error.kind())),
            text: format!("collecting the owned service sample failed: {error}"),
        }
    }
}

pub(crate) fn sample_arguments(pid: u32, report: &Path) -> [std::ffi::OsString; 6] {
    const SAMPLE_SECONDS: &str = "1";
    const SAMPLE_INTERVAL_MILLIS: &str = "10";
    [
        pid.to_string().into(),
        SAMPLE_SECONDS.into(),
        SAMPLE_INTERVAL_MILLIS.into(),
        "-mayDie".into(),
        "-file".into(),
        report.as_os_str().to_owned(),
    ]
}

pub(crate) fn cleanup_with_evidence<E>(
    outcome: &Result<(), E>,
    capture: impl FnOnce() -> String,
    cleanup: impl FnOnce() -> Result<(), E>,
) -> (Result<(), E>, Option<String>) {
    let evidence = outcome.is_err().then(capture);
    let cleaned = cleanup();
    (cleaned, evidence)
}

pub(crate) fn sample_prefix(mut source: impl io::Read + io::Seek) -> io::Result<String> {
    let length = source.seek(io::SeekFrom::End(0))?;
    source.seek(io::SeekFrom::Start(0))?;
    let count =
        usize::try_from(length.min(MAX_SAMPLE_DIAGNOSTIC_BYTES)).map_err(io::Error::other)?;
    let mut bytes = vec![0; count];
    source.read_exact(&mut bytes)?;
    let text = String::from_utf8_lossy(&bytes);
    let omitted = length.saturating_sub(MAX_SAMPLE_DIAGNOSTIC_BYTES);
    Ok(if omitted == 0 {
        text.into_owned()
    } else {
        format!("{text}\n[{omitted} later sample bytes omitted]")
    })
}

pub(crate) fn sample_service(
    owned: &mut Child,
    directory: &Path,
    command: impl FnOnce(u32, &Path) -> Option<Command>,
    wait: impl FnOnce(&mut Child) -> io::Result<ExitStatus>,
) -> io::Result<SampleEvidence> {
    if let Some(status) = owned.try_wait()? {
        return Ok(SampleEvidence {
            outcome: Sampling::ServiceExited,
            text: format!("owned service already exited: {status}"),
        });
    }
    let report = tempfile::NamedTempFile::new_in(directory)?;
    let output = tempfile::NamedTempFile::new_in(directory)?;
    let errors = tempfile::NamedTempFile::new_in(directory)?;
    let Some(mut command) = command(owned.id(), report.path()) else {
        return Ok(SampleEvidence {
            outcome: Sampling::Unsupported,
            text: "stack sampling is unsupported on this platform".to_owned(),
        });
    };
    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(output.as_file().try_clone()?))
        .stderr(Stdio::from(errors.as_file().try_clone()?));
    let (outcome, status) = match command.spawn() {
        Ok(mut sampler) => {
            let observed = wait(&mut sampler);
            match observed {
                Ok(status) => (
                    if status.success() {
                        Sampling::Captured
                    } else {
                        Sampling::Refused(SampleRefusal::Exit(status.code()))
                    },
                    format!("sampler exited: {status}"),
                ),
                Err(error) => {
                    let killed = sampler.kill();
                    std::thread::spawn(move || {
                        let _reaped = sampler.wait();
                    });
                    (
                        Sampling::Refused(SampleRefusal::Wait(error.kind())),
                        format!("sampling failed: {error}; sampler termination: {killed:?}"),
                    )
                }
            }
        }
        Err(error) => (
            Sampling::Refused(SampleRefusal::Spawn(error.kind())),
            format!("starting the sampler failed: {error}"),
        ),
    };
    let text = format!(
        "{status}\nstdout:\n{}\nstderr:\n{}\nreport:\n{}",
        worker_tail(output.as_file())?,
        worker_tail(errors.as_file())?,
        sample_prefix(report.as_file())?,
    );
    Ok(SampleEvidence { outcome, text })
}

pub(crate) fn worker_tail(mut source: impl io::Read + io::Seek) -> io::Result<String> {
    let length = source.seek(io::SeekFrom::End(0))?;
    let omitted = length.saturating_sub(MAX_WORKER_DIAGNOSTIC_BYTES);
    source.seek(io::SeekFrom::Start(omitted))?;
    let count =
        usize::try_from(length.min(MAX_WORKER_DIAGNOSTIC_BYTES)).map_err(io::Error::other)?;
    let mut bytes = vec![0; count];
    source.read_exact(&mut bytes)?;
    let text = String::from_utf8_lossy(&bytes);
    Ok(if omitted == 0 {
        text.into_owned()
    } else {
        format!("[{omitted} earlier diagnostic bytes omitted]\n{text}")
    })
}

pub(crate) fn worker_log(path: &Path) -> io::Result<Option<String>> {
    let source = match std::fs::File::open(path) {
        Ok(source) => source,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    worker_tail(source).map(Some)
}
