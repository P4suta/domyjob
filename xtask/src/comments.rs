use std::iter::Peekable;
use std::path::Path;
use std::str::CharIndices;

const LICENSE: &str = "// SPDX-";

const GENERATED: [&str; 2] = ["supply-chain/audits.toml", "supply-chain/config.toml"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Code,
    Text,
    Raw(usize),
    Line,
    Block(usize),
}

struct Lexer<'source> {
    source: &'source str,
    chars: Peekable<CharIndices<'source>>,
    line: usize,
    previous: [char; 2],
    mode: Mode,
    found: Vec<usize>,
}

fn identifier(character: char) -> bool {
    character.is_alphanumeric() || character == '_'
}

fn raw_hashes(rest: &str) -> Option<usize> {
    let after = rest.strip_prefix('r')?;
    let hashes = after
        .len()
        .saturating_sub(after.trim_start_matches('#').len());
    after.get(hashes..)?.starts_with('"').then_some(hashes)
}

fn char_literal(rest: &str) -> bool {
    let mut characters = rest.chars().skip(1);
    match characters.next() {
        Some('\\') => true,
        Some(_) => characters.next() == Some('\''),
        None => false,
    }
}

impl<'source> Lexer<'source> {
    fn new(source: &'source str) -> Self {
        Self {
            source,
            chars: source.char_indices().peekable(),
            line: 1,
            previous: [' ', ' '],
            mode: Mode::Code,
            found: Vec::new(),
        }
    }

    fn advance(&mut self) -> Option<char> {
        let (_, character) = self.chars.next()?;
        if character == '\n' {
            self.line = self.line.saturating_add(1);
        }
        let [_, last] = self.previous;
        self.previous = [last, character];
        Some(character)
    }

    fn skip(&mut self, count: usize) {
        for _ in 0..count {
            self.advance();
        }
    }

    fn peek_is(&mut self, wanted: char) -> bool {
        self.chars
            .peek()
            .is_some_and(|&(_, character)| character == wanted)
    }

    fn rest(&self, at: usize) -> &'source str {
        self.source.get(at..).unwrap_or_default()
    }

    fn opens_raw(&self, at: usize) -> Option<usize> {
        let [before, last] = self.previous;
        let boundary = !identifier(last) || (matches!(last, 'b' | 'c') && !identifier(before));
        if boundary {
            raw_hashes(self.rest(at))
        } else {
            None
        }
    }

    fn code(&mut self, at: usize, character: char) {
        let rest = self.rest(at);
        if rest.starts_with("//") {
            if !rest.starts_with(LICENSE) {
                self.found.push(self.line);
            }
            self.mode = Mode::Line;
        } else if rest.starts_with("/*") {
            self.found.push(self.line);
            self.skip(1);
            self.mode = Mode::Block(1);
        } else if character == '"' {
            self.mode = Mode::Text;
        } else if character == '\'' && char_literal(rest) {
            self.skip(1);
            while let Some(inner) = self.advance() {
                if inner == '\\' {
                    self.skip(1);
                } else if inner == '\'' {
                    return;
                }
            }
            return;
        } else if let Some(hashes) = (character == 'r').then(|| self.opens_raw(at)).flatten() {
            self.skip(hashes.saturating_add(1));
            self.mode = Mode::Raw(hashes);
        }
        self.skip(1);
    }

    fn text(&mut self, character: char) {
        self.skip(1);
        if character == '\\' {
            self.skip(1);
        } else if character == '"' {
            self.mode = Mode::Code;
        }
    }

    fn raw(&mut self, at: usize, character: char, hashes: usize) {
        self.skip(1);
        let closing = self
            .rest(at.saturating_add(1))
            .chars()
            .take(hashes)
            .filter(|&next| next == '#')
            .count();
        if character == '"' && closing == hashes {
            self.skip(hashes);
            self.mode = Mode::Code;
        }
    }

    fn block(&mut self, character: char, depth: usize) {
        self.skip(1);
        if character == '/' && self.peek_is('*') {
            self.skip(1);
            self.mode = Mode::Block(depth.saturating_add(1));
        } else if character == '*' && self.peek_is('/') {
            self.skip(1);
            self.mode = match depth.saturating_sub(1) {
                0 => Mode::Code,
                outer => Mode::Block(outer),
            };
        }
    }

    fn run(mut self) -> Vec<usize> {
        while let Some((at, character)) = self.chars.peek().copied() {
            match self.mode {
                Mode::Code => self.code(at, character),
                Mode::Text => self.text(character),
                Mode::Raw(hashes) => self.raw(at, character, hashes),
                Mode::Line => {
                    if character == '\n' {
                        self.mode = Mode::Code;
                    }
                    self.skip(1);
                }
                Mode::Block(depth) => self.block(character, depth),
            }
        }
        self.found.dedup();
        self.found
    }
}

#[must_use]
pub fn in_rust(source: &str) -> Vec<usize> {
    Lexer::new(source).run()
}

fn pinned_action(line: &str) -> bool {
    line.split_once('@').is_some_and(|(_, reference)| {
        let (digest, note) = reference.split_at_checked(40).unwrap_or_default();
        digest.len() == 40
            && digest
                .chars()
                .all(|character| character.is_ascii_hexdigit())
            && note
                .trim_start()
                .strip_prefix('#')
                .is_some_and(|version| version.trim_start().starts_with('v'))
    })
}

fn comment_at(line: &str) -> bool {
    let mut quote = None;
    let mut escaped = false;
    let mut spaced = true;
    for character in line.chars() {
        if let Some(open) = quote {
            if escaped {
                escaped = false;
            } else if character == '\\' && open == '"' {
                escaped = true;
            } else if character == open {
                quote = None;
            }
        } else if matches!(character, '"' | '\'') {
            quote = Some(character);
        } else if character == '#' && spaced {
            return true;
        }
        spaced = character.is_whitespace();
    }
    false
}

#[must_use]
pub fn in_config(path: &str, source: &str) -> Vec<usize> {
    if GENERATED.contains(&path) {
        return Vec::new();
    }
    let workflow = Path::new(path).extension().is_some_and(|extension| {
        extension.eq_ignore_ascii_case("yml") || extension.eq_ignore_ascii_case("yaml")
    });
    source
        .lines()
        .enumerate()
        .filter(|(_, line)| comment_at(line) && !(workflow && pinned_action(line)))
        .map(|(index, _)| index.saturating_add(1))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{in_config, in_rust};

    #[test]
    fn every_rust_comment_is_found_and_nothing_else() {
        assert_eq!(in_rust("fn f() {}\n// a\n/// b\n//! c\n"), [2, 3, 4]);
        assert_eq!(in_rust("let a = 1; /* x /* y */ z */ let b = 2;\n"), [1]);
        assert_eq!(
            in_rust("// SPDX-License-Identifier: MIT\nfn f() {}\n").len(),
            0
        );
        let quiet = [
            "let url = \"https://example.com/a\";\n",
            "let raw = r#\"// not \"a\" comment\"#;\n",
            "let bytes = br\"/* no */\";\n",
            "let slash = '/'; let quote = '\\''; let star = '*';\n",
            "fn f<'a>(x: &'a str) -> &'a str { x }\n",
            "let escaped = \"\\\" // still text\";\n",
            "let r#type = 1; let br = 2 / 1;\n",
        ];
        for source in quiet {
            assert!(in_rust(source).is_empty(), "{source}");
        }
        assert_eq!(in_rust("let a = '/'; // b\n"), [1]);
        assert_eq!(in_rust("let a = \"x\";\n\n/* c */\n"), [3]);
    }

    #[test]
    fn literals_and_nested_blocks_end_before_the_next_comment() {
        for literal in [
            r#"r"plain""#,
            r##"r#"a " // still raw"#"##,
            r##"br#"a " // still raw"#"##,
            r##"cr#"a " // still raw"#"##,
            r###"r##"a "# // still raw"##"###,
            r##"r#"a # // still raw"#"##,
            r####"r###"a "## /* still raw */"###"####,
            r"'/'",
            r"'\''",
            r"'\\'",
            r#"'"'"#,
            r#"'\"'"#,
            r"'\n'",
            r"'\u{2f}'",
            r#""escaped \" // text""#,
        ] {
            assert_eq!(
                in_rust(&format!("let value = {literal};\n// after\n/* last */")),
                [2, 3],
                "{literal}"
            );
        }
        assert_eq!(
            in_rust("/* outer\n / x * y\n /* nested */\n // inside\n */\n// after\n"),
            [1, 6]
        );
        assert_eq!(in_rust("// text /* block\n// next\n"), [1, 2]);
        assert_eq!(in_rust("fn f<'a>() {}\n// after\n"), [2]);
        assert_eq!(in_rust("let r#type = '\\\\';\n// after\n"), [2]);
        for source in ["'", "'\\", "\"\\", "r#\"open", "/* open"] {
            let expected = if source.starts_with("/*") {
                vec![1]
            } else {
                Vec::new()
            };
            assert_eq!(in_rust(source), expected, "{source}");
        }
        assert_eq!(in_rust("'\\unfinished // still quoted'\n// after"), [2]);
        assert_eq!(in_rust("'\\' // still quoted'\n// after"), [2]);
        assert_eq!(in_rust("'"), Vec::<usize>::new());
    }

    #[test]
    fn raw_prefixes_respect_adjacent_identifier_tokens() {
        for prefix in ["ar", "_r", "abr", "acr", "ébr", "_br"] {
            let source = format!("tokens!({prefix}#\"text\" // comment\n);\n// after");
            assert_eq!(in_rust(&source), [1, 3], "{prefix}");
        }
        for prefix in ["r", "br", "cr"] {
            let source = format!("tokens!({prefix}#\"text\" // raw\"#);\n// after");
            assert_eq!(in_rust(&source), [2], "{prefix}");
        }
    }

    #[test]
    fn configuration_comments_are_found_except_version_pins_and_generated_files() {
        assert_eq!(
            in_config("mise.toml", "# a\nb = \"#c\"\nd = 1 # e\n"),
            [1, 3]
        );
        assert_eq!(in_config("x.toml", "a = 'x#y'\n").len(), 0);
        let pin =
            "      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1\n";
        assert_eq!(in_config(".github/workflows/ci.yml", pin).len(), 0);
        assert_eq!(in_config(".github/workflows/ci.yml", "  # note\n"), [1]);
        assert_eq!(
            in_config("supply-chain/audits.toml", "# cargo-vet audits file\n").len(),
            0
        );
    }

    #[test]
    fn configuration_quotes_close_and_only_complete_workflow_pins_are_exempt() {
        let quoted = r#"a = "escaped \" # quoted" # first
b = 'backslash \' # second
c = "single ' quote" # third
d = 'double " quote' # fourth
e = "backslash \\" # fifth
f = plain#value
"#;
        assert_eq!(in_config("x.toml", quoted), [1, 2, 3, 4, 5]);
        assert_eq!(in_config("x.toml", "a = \"\" # empty\n"), [1]);
        assert_eq!(in_config("x.toml", r#"a = "escaped \" # quoted""#).len(), 0);
        assert_eq!(in_config("x.toml", r#"a = "text # quoted""#).len(), 0);
        assert_eq!(in_config("x.toml", "a = 'text # quoted'").len(), 0);
        let digest = "3d3c42e5aac5ba805825da76410c181273ba90b1";
        let pin = format!("uses: actions/checkout@{digest} # v7.0.1\n");
        for path in ["ci.yml", "ci.yaml", "CI.YML", "CI.YAML"] {
            assert!(in_config(path, &pin).is_empty(), "{path}");
        }
        assert_eq!(in_config("x.toml", &pin), [1]);
        for reference in [
            "short # v1",
            "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz # v1",
            "3d3c42e5aac5ba805825da76410c181273ba90b1 # note",
            "3d3c42e5aac5ba805825da76410c181273ba90b1 extra # v1",
        ] {
            let source = format!("uses: owner/action@{reference}");
            assert_eq!(in_config("ci.yml", &source), [1], "{reference}");
        }
        assert_eq!(
            in_config("supply-chain/config.toml", "# generated").len(),
            0
        );
    }
}
