#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Comment {
    pub line: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Code,
    Line,
    Block(usize),
    Str,
    RawStr(usize),
    Char,
}

fn raw_start(rest: &[u8]) -> Option<(usize, usize)> {
    let after_prefix = match rest {
        [b'b' | b'c', b'r', ..] => 2,
        [b'r', ..] => 1,
        _ => return None,
    };
    let hashes = rest
        .iter()
        .skip(after_prefix)
        .take_while(|b| **b == b'#')
        .count();
    match rest.get(after_prefix.saturating_add(hashes)) {
        Some(b'"') => Some((
            hashes,
            after_prefix.saturating_add(hashes).saturating_add(1),
        )),
        _ => None,
    }
}

fn closes_raw(rest: &[u8], hashes: usize) -> bool {
    rest.first() == Some(&b'"')
        && rest
            .iter()
            .skip(1)
            .take(hashes)
            .filter(|b| **b == b'#')
            .count()
            == hashes
}

fn is_char_literal(rest: &[u8]) -> bool {
    match rest {
        [b'\'', b'\\', ..] | [b'\'', _, b'\'', ..] => true,
        [b'\'', first, ..] => {
            let width: usize = match first {
                0xC0..=0xDF => 2,
                0xE0..=0xEF => 3,
                0xF0..=0xF7 => 4,
                _ => return false,
            };
            rest.get(width.saturating_add(1)) == Some(&b'\'')
        }
        _ => false,
    }
}

fn code(rest: &[u8], previous_ident: bool) -> (Mode, usize) {
    match rest {
        [b'/', b'/', ..] => (Mode::Line, 1),
        [b'/', b'*', ..] => (Mode::Block(1), 2),
        [b'"', ..] => (Mode::Str, 1),
        [b'\'', ..] if is_char_literal(rest) => (Mode::Char, 1),
        _ => match raw_start(rest) {
            Some((hashes, width)) if !previous_ident => (Mode::RawStr(hashes), width),
            Some(_) | None => (Mode::Code, 1),
        },
    }
}

#[must_use]
pub fn find(source: &str) -> Vec<Comment> {
    let bytes = source.as_bytes();
    let mut found = Vec::new();
    let mut mode = Mode::Code;
    let mut line = 1usize;
    let mut index = 0usize;
    let mut previous_ident = false;
    while let Some(rest) = bytes.get(index..) {
        let Some(&byte) = rest.first() else { break };
        let mut step = 1usize;
        mode = match mode {
            Mode::Code => {
                let (next, width) = code(rest, previous_ident);
                if matches!(next, Mode::Line | Mode::Block(_)) {
                    found.push(Comment { line });
                }
                step = width;
                previous_ident = byte.is_ascii_alphanumeric() || byte == b'_';
                next
            }
            Mode::Line => {
                if byte == b'\n' {
                    Mode::Code
                } else {
                    Mode::Line
                }
            }
            Mode::Block(depth) => match rest {
                [b'*', b'/', ..] => {
                    step = 2;
                    if depth <= 1 {
                        Mode::Code
                    } else {
                        Mode::Block(depth.saturating_sub(1))
                    }
                }
                [b'/', b'*', ..] => {
                    step = 2;
                    Mode::Block(depth.saturating_add(1))
                }
                _ => Mode::Block(depth),
            },
            Mode::Str => match rest {
                [b'\\', _, ..] => {
                    step = 2;
                    Mode::Str
                }
                [b'"', ..] => Mode::Code,
                _ => Mode::Str,
            },
            Mode::RawStr(hashes) => {
                if closes_raw(rest, hashes) {
                    step = hashes.saturating_add(1);
                    Mode::Code
                } else {
                    Mode::RawStr(hashes)
                }
            }
            Mode::Char => match rest {
                [b'\\', _, ..] => {
                    step = 2;
                    Mode::Char
                }
                [b'\'', ..] => Mode::Code,
                _ => Mode::Char,
            },
        };
        for skipped in bytes.iter().skip(index).take(step) {
            if *skipped == b'\n' {
                line = line.saturating_add(1);
            }
        }
        index = index.saturating_add(step);
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(source: &str) -> Vec<usize> {
        find(source).into_iter().map(|c| c.line).collect()
    }

    #[test]
    fn comments_are_found_outside_literals() {
        assert_eq!(lines("let a = 1; // note\nlet b = 2;"), [1]);
        assert_eq!(lines("/// doc\nfn f() {}"), [1]);
        assert_eq!(lines("fn f() {}\n/* a /* nested */ b */\n"), [2]);
        assert!(lines("let url = \"https://example.com\";").is_empty());
        assert!(lines("let raw = r#\"// not a comment\"#;").is_empty());
        assert!(lines("let slash = '/'; let quote = '\\''; let s = \"\\\"//\";").is_empty());
        assert_eq!(
            lines("fn f<'a>(x: &'a str) -> &'a str { x } // tail").len(),
            1
        );
        assert!(lines("let t = br\"//\"; let u = 'é';").is_empty());
    }
}
