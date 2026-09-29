use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::iter::{once, repeat_n};

pub const SCHTASKS: &str = "schtasks";

pub const SCHTASKS_COMMAND_MAX: usize = 261;

const XML: &[(char, &str)] = &[
    ('&', "&amp;"),
    ('<', "&lt;"),
    ('>', "&gt;"),
    ('"', "&quot;"),
    ('\'', "&apos;"),
];

const EXEC_START: &[(char, &str)] = &[('\\', "\\\\"), ('"', "\\\""), ('%', "%%"), ('$', "$$")];

const SPECIFIERS: &[(char, &str)] = &[('%', "%%")];

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ServiceError {
    #[error("service definitions cannot contain control characters such as NUL or line breaks")]
    ControlCharacter,
    #[error("service labels, programs, and log paths cannot be empty")]
    Empty,
    #[error("systemd needs an absolute program path without `$`")]
    SystemdProgram,
    #[error("a systemd description cannot end with a backslash")]
    SystemdDescription,
    #[error("a Windows task command cannot contain `%`, nor its program `\"`")]
    WindowsCommand,
    #[error(
        "schtasks accepts at most {} characters after /TR",
        SCHTASKS_COMMAND_MAX
    )]
    CommandTooLong,
}

pub fn launchd_plist(
    label: &str,
    program: &str,
    args: &[&str],
    log: &str,
) -> Result<String, ServiceError> {
    check(&[label, program, log], args.iter().copied())?;
    let words: Vec<String> = once(program)
        .chain(args.iter().copied())
        .map(|word| escape(word, XML))
        .collect();
    let arguments = words.join("</string>\n    <string>");
    let (label, log) = (escape(label, XML), escape(log, XML));
    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{label}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{arguments}</string>
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>StandardOutPath</key>
  <string>{log}</string>
  <key>StandardErrorPath</key>
  <string>{log}</string>
</dict>
</plist>
"#
    ))
}

pub fn systemd_unit(
    description: &str,
    program: &str,
    args: &[&str],
) -> Result<String, ServiceError> {
    check(&[program], once(description).chain(args.iter().copied()))?;
    if !program.starts_with('/') || program.contains('$') {
        return Err(ServiceError::SystemdProgram);
    }
    if description.ends_with('\\') {
        return Err(ServiceError::SystemdDescription);
    }
    let words: Vec<String> = once(program)
        .chain(args.iter().copied())
        .map(exec_start_word)
        .collect();
    Ok(format!(
        "[Unit]
Description={description}

[Service]
ExecStart={command}
Restart=on-failure
KillMode=process

[Install]
WantedBy=default.target
",
        description = escape(description, SPECIFIERS),
        command = words.join(" "),
    ))
}

pub fn windows_task_command(program: &str, args: &[&str]) -> Result<String, ServiceError> {
    check(&[program], args.iter().copied())?;
    if program.contains('"')
        || once(program)
            .chain(args.iter().copied())
            .any(|word| word.contains('%'))
    {
        return Err(ServiceError::WindowsCommand);
    }
    let mut command = format!("\"{program}\"");
    for argument in args {
        command.push(' ');
        push_windows_argument(&mut command, argument);
    }
    if command.encode_utf16().count() > SCHTASKS_COMMAND_MAX {
        return Err(ServiceError::CommandTooLong);
    }
    Ok(command)
}

#[must_use]
pub fn schtasks_create(task: &str, command: &str) -> Vec<String> {
    [
        "/Create", "/F", "/SC", "ONLOGON", "/RL", "LIMITED", "/TN", task, "/TR", command,
    ]
    .map(String::from)
    .into()
}

#[must_use]
pub fn schtasks_run(task: &str) -> Vec<String> {
    ["/Run", "/TN", task].map(String::from).into()
}

#[must_use]
pub fn schtasks_end(task: &str) -> Vec<String> {
    ["/End", "/TN", task].map(String::from).into()
}

#[must_use]
pub fn schtasks_delete(task: &str) -> Vec<String> {
    ["/Delete", "/F", "/TN", task].map(String::from).into()
}

#[must_use]
pub fn schtasks_query(task: &str) -> Vec<String> {
    ["/Query", "/TN", task].map(String::from).into()
}

fn check<'a>(
    required: &[&'a str],
    optional: impl IntoIterator<Item = &'a str>,
) -> Result<(), ServiceError> {
    if required.iter().any(|text| text.is_empty()) {
        return Err(ServiceError::Empty);
    }
    if required
        .iter()
        .copied()
        .chain(optional)
        .flat_map(str::chars)
        .any(|character| character.is_control() || matches!(character, '\u{fffe}' | '\u{ffff}'))
    {
        return Err(ServiceError::ControlCharacter);
    }
    Ok(())
}

fn escape(text: &str, table: &[(char, &str)]) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        match table.iter().find(|(special, _)| *special == character) {
            Some((_, replacement)) => escaped.push_str(replacement),
            None => escaped.push(character),
        }
    }
    escaped
}

fn exec_start_word(word: &str) -> String {
    let escaped = escape(word, EXEC_START);
    if word.is_empty() || word.contains([' ', '"', '\'', '\\', ';']) {
        format!("\"{escaped}\"")
    } else {
        escaped
    }
}

fn push_windows_argument(command: &mut String, argument: &str) {
    let quoted = argument.is_empty() || argument.contains([' ', '\t']);
    if quoted {
        command.push('"');
    }
    let mut backslashes = 0_usize;
    for character in argument.chars() {
        match character {
            '\\' => backslashes = backslashes.saturating_add(1),
            '"' => {
                command.extend(repeat_n('\\', backslashes.saturating_add(1)));
                backslashes = 0;
            }
            _ => backslashes = 0,
        }
        command.push(character);
    }
    if quoted {
        command.extend(repeat_n('\\', backslashes));
        command.push('"');
    }
}

#[cfg(test)]
mod tests {
    use super::{
        SCHTASKS_COMMAND_MAX, ServiceError, launchd_plist, schtasks_create, schtasks_delete,
        schtasks_end, schtasks_query, schtasks_run, systemd_unit, windows_task_command,
    };
    use alloc::format;
    use alloc::string::String;
    use alloc::vec::Vec;
    use core::iter::repeat_n;

    fn windows_argv(line: &str) -> Vec<String> {
        let mut rest = line.chars().peekable();
        let mut argv = Vec::new();
        loop {
            while rest
                .next_if(|character| matches!(character, ' ' | '\t'))
                .is_some()
            {}
            if rest.peek().is_none() {
                return argv;
            }
            let mut argument = String::new();
            let mut quoted = false;
            loop {
                let mut backslashes = 0_usize;
                while rest.next_if_eq(&'\\').is_some() {
                    backslashes = backslashes.saturating_add(1);
                }
                if rest.next_if_eq(&'"').is_some() {
                    argument.extend(repeat_n('\\', backslashes / 2));
                    if backslashes % 2 == 1 {
                        argument.push('"');
                    } else {
                        quoted = !quoted;
                    }
                } else {
                    argument.extend(repeat_n('\\', backslashes));
                    match rest.next_if(|character| quoted || !matches!(character, ' ' | '\t')) {
                        Some(character) => argument.push(character),
                        None => break,
                    }
                }
            }
            argv.push(argument);
        }
    }

    #[test]
    fn launchd_agent_starts_the_program_at_login_and_escapes_xml() {
        let plist = launchd_plist(
            "dev.domyjob.chat",
            "/Users/a&b/<bin>/domyjob",
            &["chat", "serve", "'q\"'"],
            "/Users/a&b/Library/Logs/domyjob.log",
        )
        .unwrap();
        let log = "  <string>/Users/a&amp;b/Library/Logs/domyjob.log</string>";
        assert!(plist.ends_with("</plist>\n"));
        assert_eq!(
            plist.lines().collect::<Vec<_>>(),
            [
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>",
                "<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">",
                "<plist version=\"1.0\">",
                "<dict>",
                "  <key>Label</key>",
                "  <string>dev.domyjob.chat</string>",
                "  <key>ProgramArguments</key>",
                "  <array>",
                "    <string>/Users/a&amp;b/&lt;bin&gt;/domyjob</string>",
                "    <string>chat</string>",
                "    <string>serve</string>",
                "    <string>&apos;q&quot;&apos;</string>",
                "  </array>",
                "  <key>RunAtLoad</key>",
                "  <true/>",
                "  <key>KeepAlive</key>",
                "  <true/>",
                "  <key>StandardOutPath</key>",
                log,
                "  <key>StandardErrorPath</key>",
                log,
                "</dict>",
                "</plist>",
            ]
        );
    }

    #[test]
    fn systemd_unit_restarts_the_program_and_quotes_each_word() {
        let unit = systemd_unit(
            "domyjob chat at 100%",
            "/home/me/my apps/domyjob",
            &["chat", "serve"],
        )
        .unwrap();
        assert_eq!(
            unit,
            "[Unit]\nDescription=domyjob chat at 100%%\n\n[Service]\nExecStart=\"/home/me/my apps/domyjob\" chat serve\nRestart=on-failure\nKillMode=process\n\n[Install]\nWantedBy=default.target\n"
        );
        for (word, expected) in [
            ("plain-word_1.0", "plain-word_1.0"),
            ("", r#""""#),
            ("two words", r#""two words""#),
            (r#"say "hi""#, r#""say \"hi\"""#),
            (r"back\slash", r#""back\\slash""#),
            ("it's", r#""it's""#),
            (";", r#"";""#),
            ("50%", "50%%"),
            ("$HOME", "$$HOME"),
            ("${HOME} %h", r#""$${HOME} %%h""#),
        ] {
            let line = format!("\nExecStart=/bin/domyjob {expected}\n");
            let rendered = systemd_unit("d", "/bin/domyjob", &[word]).unwrap();
            assert!(rendered.contains(&line), "{word:?} became {rendered}");
        }
    }

    #[test]
    fn windows_command_reaches_the_program_unchanged() {
        let program = r"C:\Program Files\domyjob\domyjob.exe";
        let arguments = [
            "chat",
            "",
            " ",
            "two words",
            "wide\u{3000}space",
            r#"say "hi""#,
            r"C:\dir\",
            r"C:\my dir\",
            r"\",
            r#"a\\"b"#,
            r#"\""#,
            r"end\\",
            "日本語",
        ];
        let command = windows_task_command(program, &arguments).unwrap();
        let mut expected = Vec::from([String::from(program)]);
        expected.extend(arguments.map(String::from));
        assert_eq!(windows_argv(&command), expected);
        assert_eq!(
            windows_task_command(r"C:\domyjob.exe", &["chat", r"C:\my dir\", r#"a"b"#]).unwrap(),
            r#""C:\domyjob.exe" chat "C:\my dir\\" a\"b"#
        );
    }

    #[test]
    fn schtasks_arguments_manage_one_logon_task() {
        let command = r#""C:\domyjob.exe" chat serve"#;
        assert_eq!(
            schtasks_create("domyjob chat", command),
            [
                "/Create",
                "/F",
                "/SC",
                "ONLOGON",
                "/RL",
                "LIMITED",
                "/TN",
                "domyjob chat",
                "/TR",
                command
            ]
        );
        assert_eq!(schtasks_run("t"), ["/Run", "/TN", "t"]);
        assert_eq!(schtasks_end("t"), ["/End", "/TN", "t"]);
        assert_eq!(schtasks_delete("t"), ["/Delete", "/F", "/TN", "t"]);
        assert_eq!(schtasks_query("t"), ["/Query", "/TN", "t"]);
    }

    #[test]
    fn renderers_refuse_text_their_format_cannot_carry() {
        for bad in [
            "a\0b",
            "a\nb",
            "a\rb",
            "a\tb",
            "a\u{7f}b",
            "a\u{85}b",
            "a\u{ffff}b",
        ] {
            for result in [
                launchd_plist(bad, "/bin/domyjob", &[], "/log"),
                launchd_plist("label", bad, &[], "/log"),
                launchd_plist("label", "/bin/domyjob", &[bad], "/log"),
                launchd_plist("label", "/bin/domyjob", &[], bad),
                systemd_unit(bad, "/bin/domyjob", &[]),
                systemd_unit("d", "/bin/domyjob", &[bad]),
                windows_task_command(r"C:\domyjob.exe", &[bad]),
            ] {
                assert_eq!(result, Err(ServiceError::ControlCharacter), "{bad:?}");
            }
        }
        let fits = format!(r"C:\{}.exe", "日".repeat(252));
        assert_eq!(
            windows_task_command(&fits, &[]).map(|command| command.chars().count()),
            Ok(SCHTASKS_COMMAND_MAX)
        );
        for (result, error) in [
            (
                launchd_plist("", "/bin/domyjob", &[], "/log"),
                ServiceError::Empty,
            ),
            (launchd_plist("label", "", &[], "/log"), ServiceError::Empty),
            (
                launchd_plist("label", "/bin/domyjob", &[], ""),
                ServiceError::Empty,
            ),
            (systemd_unit("d", "", &[]), ServiceError::Empty),
            (windows_task_command("", &[]), ServiceError::Empty),
            (
                systemd_unit("d", "domyjob", &[]),
                ServiceError::SystemdProgram,
            ),
            (
                systemd_unit("d", "-/bin/domyjob", &[]),
                ServiceError::SystemdProgram,
            ),
            (
                systemd_unit("d", "/opt/$x/domyjob", &[]),
                ServiceError::SystemdProgram,
            ),
            (
                systemd_unit(r"ends\", "/bin/domyjob", &[]),
                ServiceError::SystemdDescription,
            ),
            (
                windows_task_command(r#"C:\"x".exe"#, &[]),
                ServiceError::WindowsCommand,
            ),
            (
                windows_task_command(r"C:\%x%.exe", &[]),
                ServiceError::WindowsCommand,
            ),
            (
                windows_task_command(r"C:\domyjob.exe", &["%PATH%"]),
                ServiceError::WindowsCommand,
            ),
            (
                windows_task_command(&format!("{fits}x"), &[]),
                ServiceError::CommandTooLong,
            ),
        ] {
            assert_eq!(result, Err(error));
        }
    }
}
