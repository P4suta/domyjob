use std::collections::VecDeque;
use std::io::BufRead;

pub const MOST_TAIL: u32 = 1000;
pub const MOST_CONTEXT: u32 = 20;
pub const MOST_HITS: u32 = 1000;
pub const MOST_PATTERN: usize = 1024;
const MOST_LINE: usize = 4096;
const MOST_RAW_LINE: usize = 64 << 10;
const MOST_RETURNED_LINES: usize = 1024;

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

fn tidy(raw: &[u8], clipped: bool) -> String {
    let text = String::from_utf8_lossy(raw);
    let trimmed = text.trim_end_matches(['\n', '\r']);
    let cleaned = crate::terminal::clean(trimmed);
    let mut shown = match cleaned.char_indices().nth(MOST_LINE) {
        Some((cut, _)) => format!("{}…", cleaned.get(..cut).unwrap_or(&cleaned)),
        None => cleaned,
    };
    if clipped && !shown.ends_with('…') {
        shown.push('…');
    }
    shown
}

pub fn summarize(reader: &mut dyn BufRead, tail: u32) -> Result<Summary, ScanError> {
    let keep = crate::domain::to_usize(tail.min(MOST_TAIL));
    let mut last: VecDeque<String> = VecDeque::with_capacity(keep);
    let mut bytes = 0u64;
    let lines = crate::bounded::scan_lines(reader, MOST_RAW_LINE, |_, line_bytes, raw| {
        bytes = bytes.saturating_add(line_bytes);
        let line = tidy(raw, line_bytes > crate::domain::len_u64(raw.len()));
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

fn keep(hits: &mut Vec<Hit>, hit: Hit, truncated: &mut bool) {
    if hits.len() < MOST_RETURNED_LINES {
        hits.push(hit);
    } else {
        *truncated = true;
    }
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
    crate::bounded::scan_lines(reader, MOST_RAW_LINE, |number, line_bytes, raw| {
        let text = tidy(raw, line_bytes > crate::domain::len_u64(raw.len()));
        if wanted.is_match(&text) {
            matched = matched.saturating_add(1);
            if matched > limit {
                truncated = true;
                return;
            }
            for (line, earlier) in std::mem::take(&mut before) {
                keep(
                    &mut hits,
                    Hit {
                        line,
                        text: earlier,
                        matched: false,
                    },
                    &mut truncated,
                );
            }
            keep(
                &mut hits,
                Hit {
                    line: number,
                    text,
                    matched: true,
                },
                &mut truncated,
            );
            after = context;
        } else if after > 0 && !truncated {
            after = after.saturating_sub(1);
            keep(
                &mut hits,
                Hit {
                    line: number,
                    text,
                    matched: false,
                },
                &mut truncated,
            );
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
    fn a_long_log_line_has_bounded_storage_but_accurate_counts() {
        let mut log = vec![b'a'; MOST_RAW_LINE * 3];
        log.push(b'\n');
        log.extend_from_slice(b"last");
        let mut seen = Vec::new();
        let count =
            crate::bounded::scan_lines(&mut log.as_slice(), MOST_RAW_LINE, |number, bytes, raw| {
                seen.push((number, bytes, raw.len()));
            })
            .unwrap();
        assert_eq!(count, 2);
        assert_eq!(
            seen,
            [
                (
                    1,
                    crate::domain::len_u64(MOST_RAW_LINE * 3 + 1),
                    MOST_RAW_LINE
                ),
                (2, 4, 4),
            ]
        );
        let summary = summarize(&mut log.as_slice(), 2).unwrap();
        assert_eq!(summary.bytes, crate::domain::len_u64(log.len()));
        assert_eq!(summary.tail.len(), 2);
        assert!(summary.tail.first().unwrap().chars().count() <= MOST_LINE + 1);
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
    fn returned_log_lines_have_a_total_budget() {
        let log = "hit\ncontext\n".repeat(crate::domain::to_usize(MOST_HITS));
        let found = search(
            &mut log.as_bytes(),
            &pattern("^hit$").unwrap(),
            Window {
                context: 1,
                limit: MOST_HITS,
            },
        )
        .unwrap();
        assert_eq!(found.matched, u64::from(MOST_HITS));
        assert_eq!(found.hits.len(), MOST_RETURNED_LINES);
        assert!(found.truncated);
    }

    #[test]
    fn hostile_patterns_are_refused_up_front() {
        pattern("(").unwrap_err();
        pattern(&"a".repeat(MOST_PATTERN + 1)).unwrap_err();
        pattern("(a+)+$").unwrap();
    }
}
