use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum Invalid {
    #[error("machine name must be a single SSH destination of at most 128 ASCII bytes")]
    Machine,
    #[error("job identifier must be 32 lowercase hexadecimal digits")]
    JobId,
    #[error("submission identifier must be 32 lowercase hexadecimal digits")]
    SubmissionId,
    #[error("job reference must be MACHINE:JOB")]
    JobReference,
    #[error("path must be a portable relative path within 4096 bytes and 64 components")]
    RelativePath,
    #[error("command must have 1 to 256 arguments and use at most 64 KiB")]
    Command,
    #[error("remote text must fit within 64 KiB")]
    RemoteText,
}

fn check_machine(value: &str) -> Result<(), Invalid> {
    let bytes = value.as_bytes();
    let Some(first) = bytes.first() else {
        return Err(Invalid::Machine);
    };
    if bytes.len() > 128
        || !first.is_ascii_alphanumeric()
        || !bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_' | b'@'))
    {
        return Err(Invalid::Machine);
    }
    Ok(())
}

validated_string!(MachineName, Invalid, check_machine);
validated_string!(JobId, Invalid, |value| if valid_identifier(value) {
    Ok(())
} else {
    Err(Invalid::JobId)
});
validated_string!(SubmissionId, Invalid, |value| if valid_identifier(value) {
    Ok(())
} else {
    Err(Invalid::SubmissionId)
});

fn valid_identifier(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

impl From<&SubmissionId> for JobId {
    fn from(submission: &SubmissionId) -> Self {
        Self(submission.0.clone())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobReference {
    machine: MachineName,
    job: JobId,
}

impl TryFrom<String> for JobReference {
    type Error = Invalid;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        let (machine, job) = value.split_once(':').ok_or(Invalid::JobReference)?;
        Ok(Self {
            machine: MachineName::try_from(String::from(machine))?,
            job: JobId::try_from(String::from(job))?,
        })
    }
}

impl JobReference {
    #[must_use]
    pub const fn new(machine: MachineName, job: JobId) -> Self {
        Self { machine, job }
    }

    #[must_use]
    pub const fn machine(&self) -> &MachineName {
        &self.machine
    }

    #[must_use]
    pub const fn job(&self) -> &JobId {
        &self.job
    }
}

fn check_relative_path(value: &str) -> Result<(), Invalid> {
    if value.is_empty() || value.len() > 4096 {
        return Err(Invalid::RelativePath);
    }
    let mut count = 0_usize;
    for component in value.split('/') {
        count = count.saturating_add(1);
        if count > 64 || !safe_component(component) {
            return Err(Invalid::RelativePath);
        }
    }
    Ok(())
}

validated_string!(RelativePath, Invalid, check_relative_path);

fn safe_component(component: &str) -> bool {
    if component.is_empty()
        || matches!(component, "." | "..")
        || component.ends_with(['.', ' '])
        || component
            .chars()
            .any(|character| character.is_control() || "\\:*?\"<>|".contains(character))
    {
        return false;
    }
    let stem = component
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end_matches(' ');
    let upper = stem.to_ascii_uppercase();
    !matches!(
        upper.as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    )
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Vec<String>", into = "Vec<String>")]
pub struct Command {
    program: String,
    arguments: Vec<String>,
}

impl TryFrom<Vec<String>> for Command {
    type Error = Invalid;

    fn try_from(value: Vec<String>) -> Result<Self, Self::Error> {
        if value.is_empty() || value.len() > 256 {
            return Err(Invalid::Command);
        }
        let mut total = 0_usize;
        for word in &value {
            total = total.checked_add(word.len()).ok_or(Invalid::Command)?;
            if word.len() > 8192 || word.contains('\0') {
                return Err(Invalid::Command);
            }
        }
        if value.first().is_none_or(String::is_empty) || total > 65536 {
            return Err(Invalid::Command);
        }
        let mut words = value.into_iter();
        let program = words.next().ok_or(Invalid::Command)?;
        Ok(Self {
            program,
            arguments: words.collect(),
        })
    }
}

impl From<Command> for Vec<String> {
    fn from(value: Command) -> Self {
        core::iter::once(value.program)
            .chain(value.arguments)
            .collect()
    }
}

impl Command {
    #[must_use]
    pub fn program(&self) -> &str {
        &self.program
    }

    #[must_use]
    pub fn arguments(&self) -> &[String] {
        &self.arguments
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String")]
pub struct RemoteText(String);

impl TryFrom<String> for RemoteText {
    type Error = Invalid;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.len() > 65536 {
            return Err(Invalid::RemoteText);
        }
        Ok(Self(value))
    }
}

impl fmt::Debug for RemoteText {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("RemoteText").finish_non_exhaustive()
    }
}

impl RemoteText {
    #[must_use]
    pub fn for_terminal(&self) -> String {
        terminal_text(&self.0)
    }
}

#[must_use]
pub fn terminal_text(text: &str) -> String {
    let mut safe = String::with_capacity(text.len());
    for character in text.chars() {
        if (character.is_control() && character != '\n')
            || matches!(
                character,
                '\u{61c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}'
                    | '\u{2066}'..='\u{2069}' | '\u{feff}'
            )
        {
            safe.extend(character.escape_default());
        } else {
            safe.push(character);
        }
    }
    safe
}

#[cfg(test)]
mod tests {
    use alloc::borrow::ToOwned;
    use alloc::format;
    use alloc::string::String;
    use alloc::vec;
    use alloc::vec::Vec;

    use super::{
        Command, JobId, JobReference, MachineName, RelativePath, RemoteText, SubmissionId,
    };

    #[test]
    fn machine_names_cannot_be_ssh_options_or_shell_fragments() {
        for invalid in [
            "",
            "-oProxyCommand=x",
            "box;id",
            "box name",
            "box\nname",
            "box/path",
        ] {
            MachineName::try_from(invalid.to_owned()).unwrap_err();
        }
        MachineName::try_from("user@box.example".to_owned()).unwrap();
    }

    #[test]
    fn paths_are_portable_and_cannot_traverse() {
        for invalid in [
            "",
            "/absolute",
            "a//b",
            "a/../b",
            "a/./b",
            "a\\b",
            "CON.txt",
            "CON .txt",
            "foo. ",
        ] {
            RelativePath::try_from(invalid.to_owned()).unwrap_err();
        }
        RelativePath::try_from("src/main.rs".to_owned()).unwrap();
    }

    #[test]
    fn identifiers_and_commands_are_bounded() {
        JobId::try_from("0".repeat(32)).unwrap();
        JobId::try_from("g".repeat(32)).unwrap_err();
        SubmissionId::try_from("1".repeat(32)).unwrap();
        SubmissionId::try_from("G".repeat(32)).unwrap_err();
        JobReference::try_from(format!("linux:{}", "a".repeat(32))).unwrap();
        JobReference::try_from("linux:../bad".to_owned()).unwrap_err();
        Command::try_from(Vec::new()).unwrap_err();
        Command::try_from(vec!["cargo".to_owned(), "test".to_owned()]).unwrap();
        Command::try_from(vec!["cargo".to_owned(), "x".repeat(8193)]).unwrap_err();
    }

    fn boundary<T, E: core::fmt::Debug>(limit: usize, make: impl Fn(usize) -> Result<T, E>) {
        assert!(make(limit).is_ok(), "the limit {limit} itself is allowed");
        assert!(
            make(limit.saturating_add(1)).is_err(),
            "one past the limit {limit} is refused"
        );
    }

    #[test]
    fn every_limit_holds_exactly_at_its_boundary() {
        boundary(128, |length| MachineName::try_from("m".repeat(length)));
        boundary(4096, |length| {
            RelativePath::try_from(format!("{}/b", "a".repeat(length.saturating_sub(2))))
        });
        boundary(64, |count| {
            RelativePath::try_from(vec!["a"; count].join("/"))
        });
        boundary(256, |count| Command::try_from(vec!["w".to_owned(); count]));
        boundary(8192, |length| {
            Command::try_from(vec!["cargo".to_owned(), "x".repeat(length)])
        });
        boundary(65536, |total| {
            let mut words = vec!["x".repeat(8192); 7];
            words.push("x".repeat(total.saturating_sub(7 * 8192)));
            Command::try_from(words)
        });
        boundary(65536, |length| RemoteText::try_from("x".repeat(length)));
    }

    #[test]
    fn commands_need_a_program_and_keep_their_words_in_order() {
        Command::try_from(vec![String::new(), "argument".to_owned()]).unwrap_err();
        Command::try_from(vec!["cargo".to_owned(), "a\0b".to_owned()]).unwrap_err();
        let command = Command::try_from(vec!["cargo".to_owned(), "test".to_owned()]).unwrap();
        assert_eq!(command.program(), "cargo");
        assert_eq!(command.arguments(), ["test"]);
        assert_eq!(Vec::<String>::from(command), ["cargo", "test"]);
    }

    #[test]
    fn job_references_need_a_machine_and_a_job() {
        let job = "a".repeat(32);
        JobReference::try_from(job.clone()).unwrap_err();
        JobReference::try_from(format!("-oProxy:{job}")).unwrap_err();
        let reference = JobReference::try_from(format!("linux:{job}")).unwrap();
        assert_eq!(reference.machine().as_str(), "linux");
        assert_eq!(reference.job().as_str(), job);
    }

    #[test]
    fn terminal_text_preserves_unicode_and_lines_but_escapes_controls() {
        let text = RemoteText::try_from("結果\n\u{1b}[31m\u{202e}".to_owned()).unwrap();
        assert_eq!(text.for_terminal(), "結果\n\\u{1b}[31m\\u{202e}");
    }
}
