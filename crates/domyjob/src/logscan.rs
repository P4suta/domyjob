use std::collections::VecDeque;
use std::io::BufRead;

pub const MOST_TAIL: u32 = 1000;
pub const MOST_CONTEXT: u32 = 20;
pub const MOST_HITS: u32 = 1000;
pub const MOST_PATTERN: usize = 1024;
const MOST_LINE: usize = 4096;

#[derive(Debug, thiserror::Error)]
pub enum ScanError {
    #[error("reading the log: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0:?} is not a pattern domyjob can search for: {1}")]
    Pattern(String, String),
    #[error("a pattern may be at most {MOST_PATTERN} bytes long")]
    PatternTooLong,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    pub lines: u64,
    pub bytes: u64,
    pub tail: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub line: u64,
    pub text: String,
    pub matched: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub hits: Vec<Hit>,
    pub matched: u64,
    pub truncated: bool,
}

fn tidy(raw: &[u8]) -> String {
    let text = String::from_utf8_lossy(raw);
    let trimmed = text.trim_end_matches(['\n', '\r']);
    let cleaned = crate::terminal::clean(trimmed);
    match cleaned.char_indices().nth(MOST_LINE) {
        Some((cut, _)) => format!("{}…", cleaned.get(..cut).unwrap_or(&cleaned)),
        None => cleaned,
    }
}

fn lines(reader: &mut dyn BufRead, mut each: impl FnMut(u64, &[u8])) -> Result<u64, ScanError> {
    let mut number = 0u64;
    let mut raw = Vec::new();
    loop {
        raw.clear();
        let read = reader.read_until(b'\n', &mut raw)?;
        if read == 0 {
            return Ok(number);
        }
        number = number.saturating_add(1);
        each(number, &raw);
    }
}

pub fn summarize(reader: &mut dyn BufRead, tail: u32) -> Result<Summary, ScanError> {
    let keep = crate::domain::to_usize(tail.min(MOST_TAIL));
    let mut last: VecDeque<String> = VecDeque::with_capacity(keep);
    let mut bytes = 0u64;
    let lines = lines(reader, |_, raw| {
        bytes = bytes.saturating_add(crate::domain::len_u64(raw.len()));
        let line = tidy(raw);
        if keep == 0 || line.trim().is_empty() {
            return;
        }
        if last.len() == keep {
            last.pop_front();
        }
        last.push_back(line);
    })?;
    Ok(Summary {
        lines,
        bytes,
        tail: last.into_iter().collect(),
    })
}

pub fn pattern(text: &str) -> Result<regex::Regex, ScanError> {
    if text.len() > MOST_PATTERN {
        return Err(ScanError::PatternTooLong);
    }
    regex::RegexBuilder::new(text)
        .size_limit(1 << 20)
        .dfa_size_limit(1 << 20)
        .build()
        .map_err(|error| ScanError::Pattern(text.to_owned(), error.to_string()))
}

#[derive(Debug, Clone, Copy)]
pub struct Window {
    pub context: u32,
    pub limit: u32,
}

pub fn search(
    reader: &mut dyn BufRead,
    wanted: &regex::Regex,
    window: Window,
) -> Result<Found, ScanError> {
    let context = crate::domain::to_usize(window.context.min(MOST_CONTEXT));
    let limit = u64::from(window.limit.min(MOST_HITS));
    let mut before: VecDeque<(u64, String)> = VecDeque::with_capacity(context);
    let mut hits = Vec::new();
    let mut matched = 0u64;
    let mut after = 0usize;
    let mut truncated = false;
    lines(reader, |number, raw| {
        let text = tidy(raw);
        if wanted.is_match(&text) {
            matched = matched.saturating_add(1);
            if matched > limit {
                truncated = true;
                return;
            }
            hits.extend(
                std::mem::take(&mut before)
                    .into_iter()
                    .map(|(line, earlier)| Hit {
                        line,
                        text: earlier,
                        matched: false,
                    }),
            );
            hits.push(Hit {
                line: number,
                text,
                matched: true,
            });
            after = context;
        } else if after > 0 && !truncated {
            after = after.saturating_sub(1);
            hits.push(Hit {
                line: number,
                text,
                matched: false,
            });
        } else if context > 0 {
            if before.len() == context {
                before.pop_front();
            }
            before.push_back((number, text));
        }
    })?;
    Ok(Found {
        hits,
        matched,
        truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOG: &str = "compiling a\ncompiling b\n\u{1b}[31merror\u{1b}[0m: broken\nnote: here\ncompiling c\nerror: again\nfinished\n";

    #[test]
    fn a_summary_counts_everything_and_keeps_a_clean_tail() {
        let summary = summarize(&mut LOG.as_bytes(), 2).unwrap();
        assert_eq!(summary.lines, 7);
        assert_eq!(summary.bytes, crate::domain::len_u64(LOG.len()));
        assert_eq!(summary.tail, ["error: again", "finished"]);
        assert!(summarize(&mut LOG.as_bytes(), 0).unwrap().tail.is_empty());
        let unterminated = summarize(&mut &b"one\ntwo"[..], 5).unwrap();
        assert_eq!((unterminated.lines, unterminated.tail.len()), (2, 2));
        let spaced = summarize(&mut &b"result: ok\n\n   \n"[..], 1).unwrap();
        assert_eq!(
            (spaced.lines, spaced.tail.as_slice()),
            (3, ["result: ok".to_owned()].as_slice())
        );
    }

    #[test]
    fn a_search_returns_matches_with_their_context() {
        let found = search(
            &mut LOG.as_bytes(),
            &pattern("^error").unwrap(),
            Window {
                context: 1,
                limit: 10,
            },
        )
        .unwrap();
        let shown: Vec<(u64, &str, bool)> = found
            .hits
            .iter()
            .map(|h| (h.line, h.text.as_str(), h.matched))
            .collect();
        assert_eq!(
            shown,
            [
                (2, "compiling b", false),
                (3, "error: broken", true),
                (4, "note: here", false),
                (5, "compiling c", false),
                (6, "error: again", true),
                (7, "finished", false),
            ]
        );
        assert_eq!((found.matched, found.truncated), (2, false));
    }

    #[test]
    fn a_search_says_when_it_stopped_early() {
        let found = search(
            &mut LOG.as_bytes(),
            &pattern("compiling").unwrap(),
            Window {
                context: 0,
                limit: 2,
            },
        )
        .unwrap();
        assert_eq!(found.hits.len(), 2);
        assert_eq!((found.matched, found.truncated), (3, true));
    }

    #[test]
    fn hostile_patterns_are_refused_up_front() {
        pattern("(").unwrap_err();
        pattern(&"a".repeat(MOST_PATTERN + 1)).unwrap_err();
        pattern("(a+)+$").unwrap();
    }
}
