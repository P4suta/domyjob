use crate::protocol::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Posix,
    PowerShell,
    Cmd,
}

#[must_use]
pub fn posix_quote(text: &str) -> String {
    let without_nul = text.replace('\0', "");
    match shlex::try_quote(&without_nul) {
        Ok(quoted) => quoted.into_owned(),
        Err(_nul_was_removed) => String::from("''"),
    }
}

#[must_use]
pub fn powershell_quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len().saturating_add(2));
    out.push('\'');
    for c in text.chars() {
        match c {
            '\'' | '\u{2018}' | '\u{2019}' | '\u{201a}' | '\u{201b}' => {
                out.push(c);
                out.push(c);
            }
            other => out.push(other),
        }
    }
    out.push('\'');
    out
}

#[must_use]
pub fn cmd_quote(text: &str) -> String {
    let plain = !text.is_empty()
        && !text.contains([' ', '\t', '"', '&', '|', '<', '>', '^', '(', ')', '%', '!']);
    if plain {
        text.to_owned()
    } else {
        format!("\"{}\"", text.replace('"', "\"\""))
    }
}

#[must_use]
pub fn msvc_quote(text: &str) -> String {
    let plain = !text.is_empty() && !text.contains([' ', '\t', '\n', '\u{b}', '"']);
    if plain {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len().saturating_add(2));
    out.push('"');
    let mut backslashes = 0usize;
    for c in text.chars() {
        match c {
            '\\' => backslashes = backslashes.saturating_add(1),
            '"' => {
                out.extend(std::iter::repeat_n(
                    '\\',
                    backslashes.saturating_mul(2).saturating_add(1),
                ));
                out.push('"');
                backslashes = 0;
            }
            other => {
                out.extend(std::iter::repeat_n('\\', backslashes));
                out.push(other);
                backslashes = 0;
            }
        }
    }
    out.extend(std::iter::repeat_n('\\', backslashes.saturating_mul(2)));
    out.push('"');
    out
}

#[must_use]
pub fn msvc_program(path: &str) -> Option<String> {
    if path.contains('"') {
        None
    } else {
        Some(format!("\"{path}\""))
    }
}

#[must_use]
pub fn kind_of(shell: &str) -> Kind {
    let base = shell
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(shell)
        .to_ascii_lowercase();
    match base.strip_suffix(".exe").unwrap_or(&base) {
        "pwsh" | "powershell" => Kind::PowerShell,
        "cmd" => Kind::Cmd,
        _ => Kind::Posix,
    }
}

#[must_use]
pub fn script(command: &Command, kind: Kind) -> String {
    match command {
        Command::Script(text) => text.clone(),
        Command::Argv(argv) => {
            let quote = match kind {
                Kind::Posix => posix_quote,
                Kind::PowerShell => powershell_quote,
                Kind::Cmd => cmd_quote,
            };
            let words: Vec<String> = argv.iter().map(|arg| quote(arg)).collect();
            match kind {
                Kind::PowerShell => format!("& {}", words.join(" ")),
                Kind::Posix | Kind::Cmd => words.join(" "),
            }
        }
    }
}

fn on_path(name: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        [name.to_owned(), format!("{name}.exe")]
            .iter()
            .any(|file| std::fs::metadata(dir.join(file)).is_ok_and(|meta| meta.is_file()))
    })
}

#[must_use]
pub fn default_shell() -> String {
    if cfg!(windows) {
        if on_path("pwsh") {
            "pwsh".to_owned()
        } else {
            "powershell".to_owned()
        }
    } else {
        match std::env::var("SHELL") {
            Ok(shell) if !shell.is_empty() => shell,
            Ok(_) | Err(std::env::VarError::NotPresent | std::env::VarError::NotUnicode(_)) => {
                "/bin/sh".to_owned()
            }
        }
    }
}

const POWERSHELL_PLAIN_OUTPUT: &str = "if ($PSStyle) { $PSStyle.OutputRendering = 'PlainText' }; ";

#[must_use]
pub fn process(command: &Command, shell: Option<&str>) -> crate::spawn::Invocation {
    use crate::template::Arg;
    let shell = shell.map_or_else(default_shell, str::to_owned);
    let kind = kind_of(&shell);
    let body = script(command, kind);
    let text = Arg::authorized_job_text(match kind {
        Kind::PowerShell => format!("{POWERSHELL_PLAIN_OUTPUT}{body}"),
        Kind::Posix | Kind::Cmd => body,
    });
    let program = Arg::authorized_job_text(shell);
    let args = match kind {
        Kind::Posix => vec![Arg::literal("-lc"), text],
        Kind::PowerShell => vec![
            Arg::literal("-NoLogo"),
            Arg::literal("-NoProfile"),
            Arg::literal("-NonInteractive"),
            Arg::literal("-Command"),
            text,
        ],
        Kind::Cmd => vec![
            Arg::literal("/d /s /c"),
            Arg::concat(&[Arg::literal("\""), text, Arg::literal("\"")]),
        ],
    };
    crate::spawn::Invocation::new(program, args)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn powershell_jobs_render_plain_text_and_others_run_as_written() {
        let command = Command::Script("cargo test".into());
        let rendered = |shell: &str| format!("{:?}", process(&command, Some(shell)));
        assert!(rendered("pwsh").contains("OutputRendering = 'PlainText' }; cargo test"));
        assert!(!rendered("bash").contains("OutputRendering"));
    }

    #[test]
    fn posix_quoting() {
        assert_eq!(posix_quote("cargo"), "cargo");
        for word in ["a b", "it's", "", "$HOME", "a\nb"] {
            assert_eq!(
                shlex::split(&posix_quote(word)),
                Some(vec![word.to_owned()])
            );
        }
    }

    #[test]
    fn argv_becomes_a_script_per_shell() {
        let argv = Command::Argv(vec!["echo".into(), "it's here".into()]);
        assert_eq!(script(&argv, Kind::Posix), r#"echo "it's here""#);
        assert_eq!(script(&argv, Kind::PowerShell), "& 'echo' 'it''s here'");
        assert_eq!(
            powershell_quote("O\u{2019}Brien"),
            "'O\u{2019}\u{2019}Brien'"
        );
        assert_eq!(script(&argv, Kind::Cmd), "echo \"it's here\"");
        let raw = Command::Script("make && make test".into());
        assert_eq!(script(&raw, Kind::Posix), "make && make test");
    }

    #[test]
    fn msvc_quoting_survives_backslashes_before_quotes() {
        assert_eq!(msvc_quote("plain"), "plain");
        assert_eq!(msvc_quote(""), "\"\"");
        assert_eq!(msvc_quote(r"C:\my dir\"), r#""C:\my dir\\""#);
        assert_eq!(msvc_quote(r#"say "hi""#), r#""say \"hi\"""#);
        assert_eq!(msvc_quote(r#"a\"b"#), r#""a\\\"b""#);
        assert_eq!(msvc_quote(r"C:\no\space"), r"C:\no\space");
        assert_eq!(
            msvc_program(r"C:\Program Files\x.exe").unwrap(),
            r#""C:\Program Files\x.exe""#
        );
        assert!(msvc_program(r#"bad"name"#).is_none());
    }

    #[test]
    fn shell_kinds() {
        assert_eq!(kind_of("/usr/bin/zsh"), Kind::Posix);
        assert_eq!(
            kind_of(r"C:\Program Files\PowerShell\7\pwsh.exe"),
            Kind::PowerShell
        );
        assert_eq!(kind_of("CMD.EXE"), Kind::Cmd);
    }

    fn msvc_split(line: &str) -> Vec<String> {
        let mut words = Vec::new();
        let mut chars = line.chars().peekable();
        loop {
            while chars.peek().is_some_and(|c| *c == ' ' || *c == '\t') {
                chars.next();
            }
            if chars.peek().is_none() {
                return words;
            }
            let mut word = String::new();
            let mut quoted = false;
            while let Some(&c) = chars.peek() {
                if !quoted && (c == ' ' || c == '\t') {
                    break;
                }
                chars.next();
                match c {
                    '\\' => {
                        let mut run = 1usize;
                        while chars.peek() == Some(&'\\') {
                            chars.next();
                            run = run.saturating_add(1);
                        }
                        if chars.peek() == Some(&'"') {
                            word.extend(std::iter::repeat_n('\\', run / 2));
                            if run % 2 == 1 {
                                chars.next();
                                word.push('"');
                            }
                        } else {
                            word.extend(std::iter::repeat_n('\\', run));
                        }
                    }
                    '"' => {
                        if quoted && chars.peek() == Some(&'"') {
                            chars.next();
                            word.push('"');
                        } else {
                            quoted = !quoted;
                        }
                    }
                    other => word.push(other),
                }
            }
            words.push(word);
        }
    }

    proptest::proptest! {
        #[test]
        fn msvc_quoting_round_trips(words in proptest::collection::vec("[^\u{0}]*", 1..5)) {
            let line = words.iter().map(|w| msvc_quote(w)).collect::<Vec<_>>().join(" ");
            proptest::prop_assert_eq!(msvc_split(&line), words);
        }

        #[test]
        fn posix_quoting_round_trips(word in "[^\u{0}]*") {
            proptest::prop_assert_eq!(shlex::split(&posix_quote(&word)), Some(vec![word]));
        }
    }
}
