use std::collections::BTreeSet;
use std::io::{BufRead, ErrorKind, Read, Write};
use std::num::NonZeroUsize;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc;

pub const REQUEST_LINE: u64 = 1 << 20;
pub const REPLY_LINE: u64 = 64 << 20;
pub const BLOB: u64 = 8 << 30;
pub const TAIL_WINDOW: u64 = 4 << 20;
pub const IN_MEMORY_FILE: u64 = 64 << 20;
pub const CAPTURE: u64 = 4 << 20;
pub const CONTROL_LINE: u64 = 1024;
pub const CONFIG_TEXT: u64 = 4 << 20;
pub const BOOT_ID: u64 = 128;
pub const SIGNED_METADATA: u64 = 4 << 20;
pub const SOURCE_ARCHIVE: u64 = 64 << 20;
pub const DIRECTORY_BATCH: NonZeroUsize = NonZeroUsize::MIN.saturating_add(255);

#[derive(Debug)]
pub struct SortedScan<K> {
    after: Option<K>,
}

#[derive(Debug)]
struct SortedBatch<K> {
    values: BTreeSet<K>,
    final_batch: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanFlow {
    Continue,
    Stop,
}

impl<K: Ord + Clone> SortedScan<K> {
    #[must_use]
    pub const fn new() -> Self {
        Self { after: None }
    }

    fn next<E>(
        &mut self,
        scan: impl FnOnce(&mut dyn FnMut(K)) -> Result<(), E>,
    ) -> Result<Option<SortedBatch<K>>, E> {
        let mut values = BTreeSet::new();
        scan(&mut |key| {
            if self.after.as_ref().is_some_and(|previous| &key <= previous) {
                return;
            }
            values.insert(key);
            if values.len() > DIRECTORY_BATCH.get() {
                values.pop_last();
            }
        })?;
        let Some(last) = values.last() else {
            return Ok(None);
        };
        self.after = Some(last.clone());
        Ok(Some(SortedBatch {
            final_batch: values.len() < DIRECTORY_BATCH.get(),
            values,
        }))
    }

    pub fn walk<E>(
        mut self,
        mut scan: impl FnMut(&mut dyn FnMut(K)) -> Result<(), E>,
        mut visit: impl FnMut(K) -> Result<ScanFlow, E>,
    ) -> Result<ScanFlow, E> {
        loop {
            let Some(batch) = self.next(|offer| scan(offer))? else {
                return Ok(ScanFlow::Continue);
            };
            for key in batch.values {
                if visit(key)? == ScanFlow::Stop {
                    return Ok(ScanFlow::Stop);
                }
            }
            if batch.final_batch {
                return Ok(ScanFlow::Continue);
            }
        }
    }
}

impl<K: Ord + Clone> Default for SortedScan<K> {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Copy)]
pub enum Capture {
    BootIdentity,
    SshConfig,
    SourceListing,
    Notifier,
    WmiPid,
}

impl Capture {
    const fn limits(self) -> (u64, u64) {
        match self {
            Self::BootIdentity | Self::WmiPid => (4 << 10, 4 << 10),
            Self::SshConfig => (1 << 20, 64 << 10),
            Self::SourceListing => (64 << 20, 64 << 10),
            Self::Notifier => (0, 64 << 10),
        }
    }
}

enum Captured {
    Stdout(std::io::Result<Vec<u8>>),
    Stderr(std::io::Result<Vec<u8>>),
    Input(std::io::Result<()>),
}

fn read_pipe<R: Read + Send + 'static>(
    mut reader: R,
    limit: u64,
    sender: mpsc::Sender<Captured>,
    wrap: fn(std::io::Result<Vec<u8>>) -> Captured,
) {
    std::thread::spawn(move || {
        let result = to_end(&mut reader, limit);
        drop(sender.send(wrap(result)));
    });
}

pub fn command_output(command: &mut Command, capture: Capture) -> std::io::Result<Output> {
    let child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child_output(child, None, capture)
}

pub fn child_output(
    mut child: Child,
    input: Option<Vec<u8>>,
    capture: Capture,
) -> std::io::Result<Output> {
    let (stdout_limit, stderr_limit) = capture.limits();
    let (sender, receiver) = mpsc::channel();
    let mut waiting: usize = 0;
    if let Some(stdout) = child.stdout.take() {
        read_pipe(stdout, stdout_limit, sender.clone(), Captured::Stdout);
        waiting = waiting.saturating_add(1);
    }
    if let Some(stderr) = child.stderr.take() {
        read_pipe(stderr, stderr_limit, sender.clone(), Captured::Stderr);
        waiting = waiting.saturating_add(1);
    }
    if let Some(input) = input {
        let Some(mut stdin) = child.stdin.take() else {
            drop(child.kill());
            drop(child.wait());
            return Err(std::io::Error::new(
                ErrorKind::InvalidInput,
                "the child has no input pipe",
            ));
        };
        let sender = sender.clone();
        std::thread::spawn(move || {
            let result = match stdin.write_all(&input) {
                Err(error) if error.kind() == ErrorKind::BrokenPipe => Ok(()),
                result => result,
            };
            drop(sender.send(Captured::Input(result)));
        });
        waiting = waiting.saturating_add(1);
    }
    drop(sender);
    let (mut stdout, mut stderr, mut failure) = (Vec::new(), Vec::new(), None);
    for _ in 0..waiting {
        let result = receiver.recv().map_err(|_disconnected| {
            std::io::Error::other("a child output reader ended without reporting its result")
        });
        match result {
            Ok(Captured::Stdout(Ok(bytes))) => stdout = bytes,
            Ok(Captured::Stderr(Ok(bytes))) => stderr = bytes,
            Ok(Captured::Input(Ok(()))) => {}
            Ok(
                Captured::Stdout(Err(error))
                | Captured::Stderr(Err(error))
                | Captured::Input(Err(error)),
            )
            | Err(error) => {
                if failure.is_none() {
                    drop(child.kill());
                    failure = Some(error);
                }
            }
        }
    }
    let status = child.wait()?;
    if let Some(error) = failure {
        return Err(error);
    }
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

fn too_long(limit: u64) -> std::io::Error {
    std::io::Error::new(
        ErrorKind::InvalidData,
        format!("input longer than {limit} bytes was refused"),
    )
}

pub fn to_end(reader: &mut dyn Read, limit: u64) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::new();
    reader.take(limit.saturating_add(1)).read_to_end(&mut out)?;
    if crate::domain::len_u64(out.len()) > limit {
        return Err(too_long(limit));
    }
    Ok(out)
}

pub fn file_bytes(path: &Path, limit: u64) -> std::io::Result<Vec<u8>> {
    let mut file = std::fs::File::open(path)?;
    to_end(&mut file, limit)
}

pub fn text_file(path: &Path, limit: u64) -> std::io::Result<String> {
    let bytes = file_bytes(path, limit)?;
    String::from_utf8(bytes).map_err(|error| std::io::Error::new(ErrorKind::InvalidData, error))
}

#[derive(Debug)]
pub struct CappedVec {
    bytes: Vec<u8>,
    limit: u64,
}

impl CappedVec {
    #[must_use]
    pub const fn new(limit: u64) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
        }
    }

    #[must_use]
    pub fn into_vec(self) -> Vec<u8> {
        self.bytes
    }
}

impl Write for CappedVec {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if crate::domain::len_u64(self.bytes.len().saturating_add(bytes.len())) > self.limit {
            return Err(too_long(self.limit));
        }
        self.bytes
            .try_reserve(bytes.len())
            .map_err(std::io::Error::other)?;
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub fn line(reader: &mut dyn BufRead, limit: u64) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(out);
        }
        let (take, done) = match available.iter().position(|b| *b == b'\n') {
            Some(at) => (at.saturating_add(1), true),
            None => (available.len(), false),
        };
        let chunk = available.get(..take).unwrap_or(&[]);
        if crate::domain::len_u64(out.len().saturating_add(chunk.len())) > limit {
            return Err(too_long(limit));
        }
        out.extend_from_slice(chunk);
        reader.consume(take);
        if done {
            return Ok(out);
        }
    }
}

pub fn scan_lines(
    reader: &mut dyn BufRead,
    prefix_limit: usize,
    mut each: impl FnMut(u64, u64, &[u8]),
) -> std::io::Result<u64> {
    let mut number = 0u64;
    let mut raw = Vec::new();
    let mut bytes = 0u64;
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            if bytes > 0 {
                number = number.saturating_add(1);
                each(number, bytes, &raw);
            }
            return Ok(number);
        }
        let ending = available.iter().position(|byte| *byte == b'\n');
        let take = ending.map_or(available.len(), |at| at.saturating_add(1));
        let chunk = available.get(..take).unwrap_or(&[]);
        bytes = bytes.saturating_add(crate::domain::len_u64(chunk.len()));
        let prefix = prefix_limit.saturating_sub(raw.len()).min(chunk.len());
        raw.extend_from_slice(chunk.get(..prefix).unwrap_or(&[]));
        reader.consume(take);
        if ending.is_some() {
            number = number.saturating_add(1);
            each(number, bytes, &raw);
            raw.clear();
            bytes = 0;
        }
    }
}

pub fn line_unread_past(reader: &mut dyn Read, limit: u64) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        if reader.read(&mut byte)? == 0 {
            return Ok(out);
        }
        if crate::domain::len_u64(out.len()) >= limit {
            return Err(too_long(limit));
        }
        out.extend_from_slice(&byte);
        if byte == *b"\n" {
            return Ok(out);
        }
    }
}

pub fn exactly(
    reader: &mut dyn Read,
    size: u64,
    sink: &mut dyn FnMut(&[u8]) -> std::io::Result<()>,
) -> std::io::Result<()> {
    let mut left = size;
    let mut buffer = vec![0u8; 64 * 1024];
    while left > 0 {
        let want = match usize::try_from(left) {
            Ok(fits) => fits.min(buffer.len()),
            Err(_beyond_usize) => buffer.len(),
        };
        let slot = buffer.get_mut(..want).unwrap_or(&mut []);
        let read = reader.read(slot)?;
        if read == 0 {
            return Err(std::io::Error::new(
                ErrorKind::UnexpectedEof,
                "the peer stopped before sending everything it announced",
            ));
        }
        sink(slot.get(..read).unwrap_or(&[]))?;
        left = left.saturating_sub(crate::domain::len_u64(read));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sorted_scan_keeps_a_fixed_batch_and_emits_every_key_once() {
        let mut found = Vec::new();
        let mut scans = 0;
        let flow = SortedScan::<usize>::new()
            .walk(
                |offer| {
                    scans += 1;
                    for key in (0..513).rev() {
                        offer(key);
                        offer(key);
                    }
                    Ok::<(), ()>(())
                },
                |key| {
                    found.push(key);
                    Ok(ScanFlow::Continue)
                },
            )
            .unwrap();
        assert_eq!(flow, ScanFlow::Continue);
        assert_eq!(scans, 3);
        assert_eq!(found, (0..513).collect::<Vec<_>>());
    }

    #[test]
    fn sorted_scan_stops_before_a_second_directory_read() {
        let mut scans = 0;
        let mut found = Vec::new();
        let flow = SortedScan::<usize>::new()
            .walk(
                |offer| {
                    scans += 1;
                    for key in (0..513).rev() {
                        offer(key);
                    }
                    Ok::<(), ()>(())
                },
                |key| {
                    found.push(key);
                    Ok(if key == 2 {
                        ScanFlow::Stop
                    } else {
                        ScanFlow::Continue
                    })
                },
            )
            .unwrap();
        assert_eq!(flow, ScanFlow::Stop);
        assert_eq!(scans, 1);
        assert_eq!(found, [0, 1, 2]);
    }

    #[test]
    fn child_output_fixture() {
        match std::env::var("DOMYJOB_CAPTURE_FIXTURE") {
            Ok(mode) if mode == "overflow" => {
                std::io::stdout().write_all(&[b'x'; 8192]).unwrap();
                std::io::stderr().write_all(&[b'y'; 8192]).unwrap();
            }
            Ok(mode) if mode == "input" => {
                let mut input = Vec::new();
                std::io::stdin().read_to_end(&mut input).unwrap();
                eprintln!("received {} bytes", input.len());
            }
            Ok(_) | Err(_) => {}
        }
    }

    fn fixture(mode: &str) -> Command {
        let exe = std::env::current_exe().unwrap();
        let mut command = crate::spawn::Invocation::new(
            crate::template::Arg::for_test(exe.display().to_string()),
            vec![
                crate::template::Arg::literal("--exact"),
                crate::template::Arg::literal("bounded::tests::child_output_fixture"),
                crate::template::Arg::literal("--nocapture"),
            ],
        )
        .command();
        command.env("DOMYJOB_CAPTURE_FIXTURE", mode);
        command
    }

    #[test]
    fn child_output_budget_rejects_a_chatty_process() {
        let error = command_output(&mut fixture("overflow"), Capture::BootIdentity).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidData);
    }

    #[test]
    fn child_output_drains_stderr_while_sending_input() {
        let child = fixture("input")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let output = child_output(child, Some(vec![b'a'; 128 * 1024]), Capture::Notifier).unwrap();
        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("received 131072 bytes"));
    }

    proptest::proptest! {
        #[test]
        fn both_line_readers_agree_and_the_unbuffered_one_reads_nothing_past_its_line(
            input in proptest::collection::vec(proptest::prelude::any::<u8>(), 0..64),
            limit in 0u64..40,
        ) {
            let buffered = line(&mut std::io::Cursor::new(&input), limit);
            let mut raw = std::io::Cursor::new(&input);
            let unbuffered = line_unread_past(&mut raw, limit);
            match (&buffered, &unbuffered) {
                (Ok(a), Ok(b)) => {
                    proptest::prop_assert_eq!(a, b);
                    proptest::prop_assert_eq!(raw.position(), crate::domain::len_u64(b.len()));
                }
                (Err(a), Err(b)) => proptest::prop_assert_eq!(a.kind(), b.kind()),
                (Ok(_), Err(_)) | (Err(_), Ok(_)) => {
                    proptest::prop_assert!(false, "{buffered:?} vs {unbuffered:?}");
                }
            }
        }
    }

    #[test]
    fn lines_stop_at_the_limit() {
        let mut input: &[u8] = b"short\nthis one is far too long\n";
        assert_eq!(line(&mut input, 10).unwrap(), b"short\n");
        assert_eq!(
            line(&mut input, 10).unwrap_err().kind(),
            ErrorKind::InvalidData
        );
        let mut exact: &[u8] = b"abcdef";
        let mut seen = Vec::new();
        exactly(&mut exact, 4, &mut |chunk| {
            seen.extend_from_slice(chunk);
            Ok(())
        })
        .unwrap();
        assert_eq!(seen, b"abcd");
        let mut short: &[u8] = b"ab";
        assert_eq!(
            exactly(&mut short, 4, &mut |_| Ok(())).unwrap_err().kind(),
            ErrorKind::UnexpectedEof
        );
    }

    #[test]
    fn to_end_rejects_a_reader_that_grows_past_its_budget() {
        let mut exact: &[u8] = b"abcd";
        assert_eq!(to_end(&mut exact, 4).unwrap(), b"abcd");
        let mut long: &[u8] = b"abcde";
        assert_eq!(
            to_end(&mut long, 4).unwrap_err().kind(),
            ErrorKind::InvalidData
        );
    }

    #[test]
    fn text_files_refuse_excess_bytes_and_invalid_utf8() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        crate::user_files::write(&path, b"abc").unwrap();
        assert_eq!(text_file(&path, 3).unwrap(), "abc");
        assert_eq!(file_bytes(&path, 3).unwrap(), b"abc");
        assert_eq!(
            file_bytes(&path, 2).unwrap_err().kind(),
            ErrorKind::InvalidData
        );
        assert_eq!(
            text_file(&path, 2).unwrap_err().kind(),
            ErrorKind::InvalidData
        );
        crate::user_files::write(&path, &[0xff]).unwrap();
        assert_eq!(
            text_file(&path, 2).unwrap_err().kind(),
            ErrorKind::InvalidData
        );
        assert_eq!(
            text_file(&dir.path().join("absent"), 2).unwrap_err().kind(),
            ErrorKind::NotFound
        );
    }

    #[test]
    fn capped_writes_refuse_an_archive_before_growing_past_its_budget() {
        let mut buffer = CappedVec::new(3);
        buffer.write_all(b"abc").unwrap();
        assert_eq!(
            buffer.write_all(b"d").unwrap_err().kind(),
            ErrorKind::InvalidData
        );
        assert_eq!(buffer.into_vec(), b"abc");
    }
}
