use std::fs::File;
use std::io::{self, Read as _, Write as _};
use std::path::{Path, PathBuf};

use windows_spawn::{Child, Command, Job, SpawnOptions};

use super::input_controller::{self, Controller, Identity, Outcome, Ready};

use super::{
    INPUT_POLL_MILLIS, INPUT_PREFIX, INPUT_SPLIT_BYTES, InputCapture, InputSchedule,
    MAX_INPUT_DIAGNOSTIC_BYTES,
};

#[derive(Debug, thiserror::Error)]
#[error("Begin publication failed: {primary}; removing its owned stage failed: {cleanup}")]
struct PublicationFailure {
    primary: io::Error,
    cleanup: io::Error,
}

fn publish_begin(directory: &Path, identity: Identity) -> io::Result<()> {
    let stage = directory.join("begin.tmp");
    let mut output = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&stage)?;
    let prepared = output
        .write_all(&identity.bytes())
        .and_then(|()| output.sync_all());
    drop(output);
    let primary = prepared.and_then(|()| std::fs::hard_link(&stage, directory.join("begin.bin")));
    let cleanup = std::fs::remove_file(&stage);
    match (primary, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(primary), Ok(())) => Err(primary),
        (Ok(()), Err(cleanup)) => Err(cleanup),
        (Err(primary), Err(cleanup)) => Err(io::Error::new(
            primary.kind(),
            PublicationFailure { primary, cleanup },
        )),
    }
}

pub(super) struct Control {
    directory: tempfile::TempDir,
    case: [u8; input_controller::CASE_BYTES],
}

impl Control {
    pub(super) fn new(root: &Path) -> io::Result<Self> {
        let directory = tempfile::TempDir::new_in(root)?;
        let mut case = [0; input_controller::CASE_BYTES];
        getrandom::fill(&mut case).map_err(|error| io::Error::other(error.to_string()))?;
        Ok(Self { directory, case })
    }

    fn ready(&self) -> PathBuf {
        self.directory.path().join("ready.bin")
    }
    fn begin(&self) -> PathBuf {
        self.directory.path().join("begin.bin")
    }

    pub(super) fn script(&self) -> String {
        let bytes = self
            .case
            .iter()
            .map(u8::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let ready = super::quote(&self.ready());
        let begin = super::quote(&self.begin());
        format!(
            "$identity=[byte[]](@({bytes})+[BitConverter]::GetBytes([uint32]$PID));\n[IO.File]::WriteAllBytes('{ready}.tmp',$identity); [IO.File]::Move('{ready}.tmp','{ready}');\n[Console]::Error.WriteLine('bootstrap input phase: ready');\nwhile (-not [IO.File]::Exists('{begin}')) {{ [Threading.Thread]::Sleep({INPUT_POLL_MILLIS}) }}\nif ([Convert]::ToBase64String([IO.File]::ReadAllBytes('{begin}')) -cne [Convert]::ToBase64String($identity)) {{ throw 'foreign bootstrap Begin' }}\n[Console]::Error.WriteLine('bootstrap input phase: begun');"
        )
    }
}

pub(super) struct Execution {
    child: Child,
    job: Job,
    diagnostics: File,
}

impl Execution {
    pub(super) fn spawn(
        shell: &str,
        script: &str,
        input: File,
        capture: &InputCapture,
    ) -> io::Result<Self> {
        let job = Job::create()?;
        job.set_kill_on_close(true)?;
        let child = Command::new(shell)
            .args(["-NoProfile", "-Command", script])
            .stdin(input)
            .stdout(capture.output.as_file().try_clone()?)
            .stderr(capture.errors.as_file().try_clone()?)
            .spawn_with(SpawnOptions::new().job(&job))?;
        Ok(Self {
            child,
            job,
            diagnostics: capture.errors.reopen()?,
        })
    }
}

pub(super) struct Context<'a> {
    pub(super) control: &'a Control,
    pub(super) marker: &'a Path,
    pub(super) schedule: InputSchedule,
    pub(super) descendant: Option<&'a Path>,
}

#[derive(Clone, Copy)]
enum Transfer {
    Initial,
    ShortWritten,
    Closed,
}

pub(super) struct Reader<'a> {
    execution: Execution,
    input: Option<File>,
    context: Context<'a>,
    transfer: Transfer,
    contained: bool,
    waiting: crate::peer::Wait,
}

impl<'a> Reader<'a> {
    pub(super) fn new(execution: Execution, input: File, context: Context<'a>) -> Self {
        let waiting = crate::peer::Wait::new(
            format!(
                "PowerShell bootstrap input reader (pid {})",
                execution.child.id()
            ),
            format!(
                "binary source prefix and inherited remainder ({:?})",
                context.schedule
            ),
        );
        Self {
            execution,
            input: Some(input),
            context,
            transfer: Transfer::Initial,
            contained: false,
            waiting,
        }
    }

    const fn identity(&self) -> Identity {
        Identity {
            case: self.context.control.case,
            process: self.execution.child.id(),
        }
    }

    pub(super) fn run(mut self) -> (Outcome<std::process::ExitStatus, io::Error>, bool) {
        let controller = Controller::new(self.identity());
        let outcome = input_controller::wait(&mut self, controller);
        (outcome, self.contained)
    }

    fn pending(&self) -> io::Result<usize> {
        let mut bytes = Vec::new();
        File::open(self.context.marker)?
            .take(MAX_INPUT_DIAGNOSTIC_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if u64::try_from(bytes.len()).map_err(io::Error::other)? > MAX_INPUT_DIAGNOSTIC_BYTES {
            return Err(io::Error::other(
                "native pending observations exceeded their bounded capture",
            ));
        }
        Ok(bytes.split(|byte| *byte == b'\n').skip(1).count())
    }

    fn transfer(&mut self, pending: usize) -> io::Result<()> {
        let Some(writer) = self.input.as_mut() else {
            return Ok(());
        };
        let (first, second) = INPUT_PREFIX.split_at(INPUT_SPLIT_BYTES);
        match (self.context.schedule, self.transfer) {
            (InputSchedule::Buffered | InputSchedule::NativeError, _) => {}
            (InputSchedule::Pending, Transfer::Initial) if pending >= 1 => {
                writer.write_all(first)?;
                self.transfer = Transfer::ShortWritten;
                return Ok(());
            }
            (InputSchedule::Pending, Transfer::ShortWritten) if pending >= 2 => {
                super::write_input(writer, second)
                    .map_err(|error| io::Error::other(error.to_string()))?;
            }
            (InputSchedule::ObserverError, _) if pending >= 1 => {
                super::write_input(writer, INPUT_PREFIX)
                    .map_err(|error| io::Error::other(error.to_string()))?;
            }
            (InputSchedule::Truncated, _) if pending >= 1 => writer.write_all(first)?,
            (InputSchedule::ZeroWrite, _) if pending >= 1 => {
                super::write_zero(writer)?;
                super::write_input(writer, INPUT_PREFIX)
                    .map_err(|error| io::Error::other(error.to_string()))?;
            }
            _ => return Ok(()),
        }
        self.transfer = Transfer::Closed;
        drop(self.input.take());
        Ok(())
    }
}

impl input_controller::Reader for Reader<'_> {
    type Exit = std::process::ExitStatus;
    type Error = io::Error;

    fn try_wait(&mut self) -> io::Result<Option<Self::Exit>> {
        self.execution.child.try_wait()
    }

    fn ready(&mut self) -> io::Result<Ready> {
        let source = match File::open(self.context.control.ready()) {
            Ok(source) => source,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Ready::Missing),
            Err(error) => return Err(error),
        };
        let mut bytes = Vec::new();
        source
            .take(u64::try_from(input_controller::IDENTITY_BYTES + 1).map_err(io::Error::other)?)
            .read_to_end(&mut bytes)?;
        Ok(match bytes.try_into() {
            Ok(token) => Ready::Published(Identity::from_bytes(token)),
            Err(_) => Ready::Invalid,
        })
    }

    fn begin(&mut self) -> io::Result<()> {
        publish_begin(self.context.control.directory.path(), self.identity())
    }

    fn progress(&mut self) -> io::Result<()> {
        if let Some(marker) = self.context.descendant
            && marker.is_file()
        {
            let mut marker_bytes = Vec::new();
            File::open(marker)?.take(8).read_to_end(&mut marker_bytes)?;
            if marker_bytes != b"holding" {
                return Err(io::Error::other(
                    "the owned descendant published invalid readiness",
                ));
            }
            if super::owned_job::active(&self.execution.job)? < 2 {
                return Err(io::Error::other(
                    "the held descendant was not observed in its creation-time Job",
                ));
            }
            self.contained = true;
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "controlled cancellation after owned descendant readiness",
            ));
        }
        self.transfer(self.pending()?)
    }
    fn pause(&mut self) {
        let received = super::diagnostic_prefix(&self.execution.diagnostics)
            .unwrap_or_else(|error| format!("stderr capture: {error}"));
        self.waiting.pause(received);
    }
    fn close_input(&mut self) -> io::Result<()> {
        drop(self.input.take());
        Ok(())
    }
    fn stop_tree(&mut self) -> io::Result<()> {
        self.execution.job.terminate(1)
    }

    fn reap(&mut self) -> io::Result<()> {
        let mut waiting = crate::peer::Wait::new(
            format!(
                "owned bootstrap Job (root pid {})",
                self.execution.child.id()
            ),
            "root exit and zero active descendants after termination",
        );
        loop {
            let exited = self.execution.child.try_wait()?.is_some();
            let active = super::owned_job::active(&self.execution.job)?;
            if exited && active == 0 {
                self.execution.child.wait()?;
                return Ok(());
            }
            waiting.pause(format!("root exited {exited}; active {active}"));
        }
    }
}

pub(super) fn descendant_contract(root: &Path) -> Result<(), crate::Failure> {
    use crate::Context as _;

    let control = Control::new(root).context("owning the descendant control")?;
    let marker = control.directory.path().join("descendant.bin");
    let pending = control.directory.path().join("pending.txt");
    std::fs::write(&pending, b"").context("owning the descendant progress channel")?;
    let executable = std::env::current_exe().context("locating the owned descendant fixture")?;
    let handshake = control.script();
    let script = format!(
        "$ErrorActionPreference='Stop'; {handshake}\n$s=New-Object Diagnostics.ProcessStartInfo; $s.FileName='{}'; $s.Arguments='--bootstrap-owned-hold \"{}\"'; $s.UseShellExecute=$false; $p=[Diagnostics.Process]::Start($s); $p.WaitForExit(); exit $p.ExitCode",
        super::quote(&executable),
        super::quote(&marker)
    );
    let capture = InputCapture {
        output: tempfile::NamedTempFile::new_in(root).context("owning descendant stdout")?,
        errors: tempfile::NamedTempFile::new_in(root).context("owning descendant stderr")?,
    };
    let input = File::open("NUL").context("opening descendant stdin")?;
    let writer = tempfile::NamedTempFile::new_in(root)
        .context("owning descendant input release")?
        .reopen()
        .context("retaining descendant input release")?;
    let context = Context {
        control: &control,
        marker: &pending,
        schedule: InputSchedule::Buffered,
        descendant: Some(&marker),
    };
    let execution = Execution::spawn("powershell.exe", &script, input, &capture)
        .context("admitting the held tree before execution")?;
    let (outcome, contained) = Reader::new(execution, writer, context).run();
    ensure!(
        contained,
        "the known held child never entered the owned Job: {outcome:?}"
    );
    ensure!(
        matches!(
            outcome.primary,
            Err(input_controller::Problem::Port(ref error)) if error.kind() == io::ErrorKind::Interrupted
        ),
        "the held child lost its controlled cancellation: {outcome:?}"
    );
    ensure!(
        outcome.cleanup.iter().all(Result::is_ok),
        "owned descendant cleanup did not establish quiescence: {outcome:?}"
    );
    println!("bootstrap owned descendant: contained; controlled cancellation; quiescent");
    Ok(())
}

pub(super) fn owned_input_descendant() -> std::process::ExitCode {
    let Some(marker) = std::env::args_os().nth(2) else {
        return std::process::ExitCode::FAILURE;
    };
    if let Err(error) = std::fs::write(marker, b"holding") {
        eprintln!("owned input descendant readiness failed: {error}");
        return std::process::ExitCode::FAILURE;
    }
    loop {
        std::thread::park();
    }
}

pub(super) fn publication_contract(root: &Path) -> Result<(), crate::Failure> {
    use crate::Context as _;

    let identity = Identity {
        case: [1; input_controller::CASE_BYTES],
        process: u32::MAX,
    };
    let fresh = Control::new(root).context("owning the fresh Begin control")?;
    publish_begin(fresh.directory.path(), identity)
        .context("publishing fresh Begin without replacement")?;
    ensure!(
        std::fs::read(fresh.begin()).context("reading the exact Begin token")? == identity.bytes(),
        "Begin token changed"
    );
    ensure!(
        !fresh.directory.path().join("begin.tmp").exists(),
        "successful Begin left its owned stage"
    );
    let occupied = Control::new(root).context("owning the occupied Begin control")?;
    std::fs::write(occupied.begin(), b"existing Begin").context("injecting preexisting Begin")?;
    ensure!(
        publish_begin(occupied.directory.path(), identity).is_err(),
        "occupied Begin was overwritten"
    );
    ensure!(
        std::fs::read(occupied.begin()).context("reading occupied Begin after refusal")?
            == b"existing Begin",
        "refused Begin changed existing bytes"
    );
    ensure!(
        !occupied.directory.path().join("begin.tmp").exists(),
        "refused Begin left its owned stage"
    );
    let preexisting_stage = Control::new(root).context("owning the occupied stage control")?;
    let stage = preexisting_stage.directory.path().join("begin.tmp");
    std::fs::write(&stage, b"existing stage").context("injecting preexisting stage")?;
    ensure!(
        publish_begin(preexisting_stage.directory.path(), identity).is_err(),
        "occupied stage was replaced"
    );
    ensure!(
        std::fs::read(&stage).context("reading occupied stage after refusal")? == b"existing stage",
        "unowned stage bytes changed"
    );
    ensure!(
        !preexisting_stage.begin().exists(),
        "occupied stage created a Begin"
    );
    println!(
        "bootstrap Begin publication: exact bytes; occupied Begin preserved; occupied stage preserved"
    );
    Ok(())
}
