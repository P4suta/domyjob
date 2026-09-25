use std::fmt;
use std::io::Write;

use serde::{Deserialize, Serialize};

const REPLACEMENT: char = '\u{FFFD}';

#[must_use]
pub const fn dangerous(c: char) -> bool {
    matches!(c,
        '\u{0}'..='\u{8}' | '\u{b}' | '\u{c}' | '\u{e}'..='\u{1f}' | '\u{7f}'..='\u{9f}'
        | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
        | '\u{feff}')
}

#[must_use]
pub fn neutralize(text: &str) -> String {
    text.chars()
        .map(|c| if dangerous(c) { REPLACEMENT } else { c })
        .collect()
}

#[derive(Debug, Clone, Copy)]
pub struct Display<'a>(&'a str);

impl<'a> Display<'a> {
    #[must_use]
    pub const fn of(text: &'a str) -> Self {
        Self(text)
    }
}

impl fmt::Display for Display<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&neutralize(self.0))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(transparent)]
pub struct RemoteText(String);

impl RemoteText {
    #[must_use]
    pub const fn new(text: String) -> Self {
        Self(text)
    }

    #[must_use]
    pub fn as_raw_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RemoteText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&neutralize(&self.0))
    }
}

const SEQUENCE_LIMIT: usize = 4096;

#[derive(Debug, Default)]
struct Shown {
    text: String,
    swallowed: usize,
}

impl vte::Perform for Shown {
    fn print(&mut self, c: char) {
        self.swallowed = 0;
        self.text.push(if dangerous(c) { REPLACEMENT } else { c });
    }

    fn execute(&mut self, byte: u8) {
        self.swallowed = 0;
        match byte {
            b'\n' | b'\t' | b'\r' => self.text.push(char::from(byte)),
            _ => self.text.push(REPLACEMENT),
        }
    }
}

struct Scanner {
    parser: vte::Parser,
    shown: Shown,
}

impl fmt::Debug for Scanner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Scanner").finish_non_exhaustive()
    }
}

impl Scanner {
    fn new() -> Self {
        Self {
            parser: vte::Parser::new(),
            shown: Shown::default(),
        }
    }

    fn feed(&mut self, bytes: &[u8]) -> String {
        for byte in bytes {
            self.parser
                .advance(&mut self.shown, std::slice::from_ref(byte));
            self.shown.swallowed = self.shown.swallowed.saturating_add(1);
            if self.shown.swallowed > SEQUENCE_LIMIT {
                self.parser = vte::Parser::new();
                self.shown.swallowed = 0;
                self.shown.text.push(REPLACEMENT);
            }
        }
        std::mem::take(&mut self.shown.text)
    }
}

#[derive(Debug)]
pub struct SanitizingWriter<W: Write> {
    inner: W,
    pending: Vec<u8>,
    scanner: Box<Scanner>,
}

impl<W: Write> SanitizingWriter<W> {
    pub fn new(inner: W) -> Self {
        Self {
            inner,
            pending: Vec::new(),
            scanner: Box::new(Scanner::new()),
        }
    }

    pub fn into_inner(self) -> W {
        self.inner
    }
}

#[must_use]
pub fn clean(text: &str) -> String {
    Scanner::new().feed(text.as_bytes())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Destination {
    Terminal,
    Elsewhere,
}

impl Destination {
    #[must_use]
    pub fn of_stdout() -> Self {
        use std::io::IsTerminal;
        if std::io::stdout().is_terminal() {
            Self::Terminal
        } else {
            Self::Elsewhere
        }
    }
}

#[derive(Debug)]
pub enum RemoteSink<W: Write> {
    Terminal(SanitizingWriter<W>),
    Elsewhere(W),
}

impl<W: Write> RemoteSink<W> {
    pub fn new(inner: W, destination: Destination) -> Self {
        match destination {
            Destination::Terminal => Self::Terminal(SanitizingWriter::new(inner)),
            Destination::Elsewhere => Self::Elsewhere(inner),
        }
    }

    pub fn into_inner(self) -> W {
        match self {
            Self::Terminal(sanitized) => sanitized.into_inner(),
            Self::Elsewhere(raw) => raw,
        }
    }
}

impl<W: Write> Write for RemoteSink<W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::Terminal(sanitized) => sanitized.write(bytes),
            Self::Elsewhere(raw) => raw.write(bytes),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Self::Terminal(sanitized) => sanitized.flush(),
            Self::Elsewhere(raw) => raw.flush(),
        }
    }
}

fn utf8_boundary(bytes: &[u8]) -> usize {
    let tail = bytes.len().saturating_sub(3);
    for (index, &byte) in bytes.iter().enumerate().skip(tail).rev() {
        let width = match byte {
            0x00..=0x7f => 1,
            0xc0..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf7 => 4,
            _ => continue,
        };
        return if index.saturating_add(width) > bytes.len() {
            index
        } else {
            bytes.len()
        };
    }
    bytes.len()
}

impl<W: Write> Write for SanitizingWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.pending.extend_from_slice(bytes);
        let cut = utf8_boundary(&self.pending);
        let complete: Vec<u8> = self.pending.drain(..cut).collect();
        let shown = self.scanner.feed(&complete);
        self.inner.write_all(shown.as_bytes())?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let rest = std::mem::take(&mut self.pending);
        if !rest.is_empty() {
            let lossy = String::from_utf8_lossy(&rest).into_owned();
            let shown = self.scanner.feed(lossy.as_bytes());
            self.inner.write_all(shown.as_bytes())?;
        }
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_sequences_and_bidi_overrides_cannot_reach_a_terminal() {
        let hostile = "ok\u{1b}]52;c;cGFzc3dk\u{7}\u{9b}31m\u{202e}txt.exe\ttab\nline";
        let shown = neutralize(hostile);
        assert!(!shown.contains('\u{1b}'));
        assert!(!shown.contains('\u{9b}'));
        assert!(!shown.contains('\u{202e}'));
        assert!(shown.contains("\ttab\nline"));
        assert_eq!(
            format!("{}", RemoteText::new("a\u{1b}[2J".into())),
            "a\u{FFFD}[2J"
        );
        let mut out = SanitizingWriter::new(Vec::new());
        let bytes = "é\u{1b}x".as_bytes();
        for byte in bytes {
            out.write_all(std::slice::from_ref(byte)).unwrap();
        }
        out.flush().unwrap();
        assert_eq!(String::from_utf8(out.inner).unwrap(), "é");
    }

    fn shown(chunks: &[&str]) -> String {
        let mut out = SanitizingWriter::new(Vec::new());
        for chunk in chunks {
            out.write_all(chunk.as_bytes()).unwrap();
        }
        out.flush().unwrap();
        String::from_utf8(out.inner).unwrap()
    }

    #[test]
    fn whole_sequences_are_removed_even_across_writes() {
        assert_eq!(
            shown(&["\u{1b}[7mtest result\u{1b}[0m: ok"]),
            "test result: ok"
        );
        assert_eq!(shown(&["a\u{1b}]52;c;cGFzc3dk\u{7}b"]), "ab");
        assert_eq!(
            shown(&["a\u{1b}]8;;http://x\u{1b}\\link\u{1b}]8;;\u{1b}\\b"]),
            "alinkb"
        );
        assert_eq!(shown(&["x\u{1b}", "[3", "1m", "red\u{1b}[", "0m"]), "xred");
        assert!(
            !shown(&["\u{9b}2Jc\u{9d}0;title\u{9c}d"])
                .chars()
                .any(|c| c != '\t' && dangerous(c))
        );
        assert_eq!(shown(&["\u{1b}(Bplain\u{1b}Pq#0\u{1b}\\end"]), "plainend");
        assert_eq!(
            shown(&["tab\there\r\nnext\u{202e}"]),
            "tab\there\r\nnext\u{FFFD}"
        );
        let endless = format!("\u{1b}]{}", "x".repeat(SEQUENCE_LIMIT + 10));
        assert!(shown(&[&endless, "after"]).ends_with("after"));
    }

    fn shown_bytes(chunks: &[&[u8]]) -> String {
        let mut out = SanitizingWriter::new(Vec::new());
        for chunk in chunks {
            out.write_all(chunk).unwrap();
        }
        out.flush().unwrap();
        String::from_utf8(out.into_inner()).unwrap()
    }

    #[derive(Debug, Default)]
    struct Recorder {
        bytes: Vec<u8>,
        flushed: usize,
    }

    impl Write for Recorder {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            self.flushed = self.flushed.saturating_add(1);
            Ok(())
        }
    }

    #[test]
    fn a_terminal_gets_sanitized_output_and_anything_else_the_raw_bytes() {
        let raw = b"a\x1b[31mb\xe3\x81";
        let mut terminal = RemoteSink::new(Recorder::default(), Destination::Terminal);
        terminal.write_all(raw).unwrap();
        terminal.flush().unwrap();
        let RemoteSink::Terminal(sanitized) = terminal else {
            panic!("a terminal destination must sanitize");
        };
        let recorded = sanitized.into_inner();
        assert_eq!(recorded.bytes, "ab\u{FFFD}".as_bytes());
        assert_eq!(recorded.flushed, 1);

        let mut elsewhere = RemoteSink::new(Recorder::default(), Destination::Elsewhere);
        elsewhere.write_all(raw).unwrap();
        elsewhere.flush().unwrap();
        let RemoteSink::Elsewhere(passed) = elsewhere else {
            panic!("any other destination must pass bytes through");
        };
        assert_eq!(passed.bytes, raw);
        assert_eq!(passed.flushed, 1);
    }

    #[derive(Debug)]
    struct Broken;

    impl Write for Broken {
        fn write(&mut self, _bytes: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("the terminal went away"))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::Error::other("the terminal went away"))
        }
    }

    #[test]
    fn complete_characters_stream_out_before_any_flush() {
        let mut out = SanitizingWriter::new(Vec::new());
        out.write_all("tick \u{3042}".as_bytes()).unwrap();
        out.write_all("\u{1F680}".as_bytes().get(..2).unwrap())
            .unwrap();
        assert_eq!(out.inner, "tick \u{3042}".as_bytes());
        out.write_all("\u{1F680}".as_bytes().get(2..).unwrap())
            .unwrap();
        assert_eq!(out.inner, "tick \u{3042}\u{1F680}".as_bytes());
        let mut held = "ab\u{1F680}".as_bytes().to_vec();
        held.truncate(4);
        out.write_all(&held).unwrap();
        assert_eq!(out.into_inner(), "tick \u{3042}\u{1F680}ab".as_bytes());
    }

    #[derive(Debug)]
    struct Unwritable;

    impl Write for Unwritable {
        fn write(&mut self, _bytes: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("the disk is full"))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn text_after_broken_bytes_streams_out_at_once() {
        let mut out = SanitizingWriter::new(Vec::new());
        out.write_all(&[0xf0, 0x9f, b'a']).unwrap();
        assert_eq!(out.inner, "\u{FFFD}a".as_bytes());
    }

    #[test]
    fn a_held_character_that_cannot_be_written_at_flush_is_reported() {
        let mut out = SanitizingWriter::new(Unwritable);
        out.write_all("\u{1F680}".as_bytes().get(..2).unwrap())
            .unwrap();
        out.flush().unwrap_err();
    }

    #[test]
    fn a_failing_destination_is_reported_not_swallowed() {
        let mut sanitized = SanitizingWriter::new(Broken);
        sanitized.write_all(b"x").unwrap_err();
        sanitized.flush().unwrap_err();
        for destination in [Destination::Terminal, Destination::Elsewhere] {
            let mut sink = RemoteSink::new(Broken, destination);
            sink.write_all(b"x").unwrap_err();
            sink.flush().unwrap_err();
        }
    }

    #[test]
    fn a_character_split_across_writes_arrives_whole() {
        for split in 0..=4 {
            let bytes = "x\u{1F680}y".as_bytes();
            let halves: [&[u8]; 2] = bytes.split_at(split).into();
            assert_eq!(shown_bytes(&halves), "x\u{1F680}y");
        }
        let three = "\u{3042}".as_bytes();
        for split in 0..=3 {
            let halves: [&[u8]; 2] = three.split_at(split).into();
            assert_eq!(shown_bytes(&halves), "\u{3042}");
        }
    }

    proptest::proptest! {
        #[test]
        fn nothing_that_steers_a_terminal_survives(text in ".*", cut in 0usize..64) {
            let split = text.char_indices().nth(cut).map_or(text.len(), |(at, _)| at);
            let halves: [&str; 2] = text.split_at(split).into();
            let whole = shown(&[&text]);
            proptest::prop_assert_eq!(&shown(&halves), &whole);
            for c in whole.chars() {
                proptest::prop_assert!(!dangerous(c) || c == '\t', "{:?} reached the terminal", c);
                proptest::prop_assert!(c != '\x1b');
            }
        }

        #[test]
        fn splitting_bytes_anywhere_changes_nothing(text in ".*", cut in 0usize..256) {
            let bytes = text.as_bytes();
            let at = cut.min(bytes.len());
            let halves: [&[u8]; 2] = bytes.split_at(at).into();
            proptest::prop_assert_eq!(shown_bytes(&halves), shown(&[&text]));
        }

        #[test]
        fn ordinary_text_passes_unchanged(text in "[a-zA-Z0-9 .,:;!?()/\\\n\t\u{3042}-\u{3093}]*") {
            proptest::prop_assert_eq!(shown(&[&text]), text);
        }
    }
}
