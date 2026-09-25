use std::path::{Path, PathBuf};
use std::process::Command;

use crate::template::Arg;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    program: Arg,
    args: Vec<Arg>,
    dir: Option<PathBuf>,
}

impl Invocation {
    #[must_use]
    pub const fn new(program: Arg, args: Vec<Arg>) -> Self {
        Self {
            program,
            args,
            dir: None,
        }
    }

    #[must_use]
    pub fn from_words(words: Vec<Arg>) -> Option<Self> {
        let mut words = words.into_iter();
        let program = words.next()?;
        Some(Self::new(program, words.collect()))
    }

    #[must_use]
    pub fn arg(mut self, arg: Arg) -> Self {
        self.args.push(arg);
        self
    }

    #[must_use]
    pub fn in_dir(mut self, dir: &Path) -> Self {
        self.dir = Some(dir.to_path_buf());
        self
    }

    #[must_use]
    pub const fn program(&self) -> &Arg {
        &self.program
    }

    #[must_use]
    pub fn args(&self) -> &[Arg] {
        &self.args
    }

    #[must_use]
    pub fn dir(&self) -> Option<&Path> {
        self.dir.as_deref()
    }

    #[must_use]
    pub fn display(&self) -> String {
        std::iter::once(&self.program)
            .chain(&self.args)
            .map(Arg::as_arg_str)
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[must_use]
    #[expect(
        clippy::disallowed_methods,
        reason = "the one place a process is created, and it only accepts typed arguments"
    )]
    pub fn command(&self) -> Command {
        let mut command = Command::new(self.program.as_arg_str());
        let cmd = crate::shell::kind_of(self.program.as_arg_str()) == crate::shell::Kind::Cmd;
        for arg in &self.args {
            verbatim_or_quoted(&mut command, arg.as_arg_str(), cmd);
        }
        if let Some(dir) = &self.dir {
            command.current_dir(dir);
        }
        command
    }
}

impl Invocation {
    #[cfg(windows)]
    #[must_use]
    #[expect(
        clippy::disallowed_methods,
        reason = "the one place a process is created with an explicit handle list, and it only accepts typed arguments"
    )]
    pub fn windows_command(&self) -> windows_spawn::Command {
        let mut command = windows_spawn::Command::new(self.program.as_arg_str());
        let cmd = crate::shell::kind_of(self.program.as_arg_str()) == crate::shell::Kind::Cmd;
        for arg in &self.args {
            if cmd {
                command.raw_arg(arg.as_arg_str());
            } else {
                command.arg(arg.as_arg_str());
            }
        }
        if let Some(dir) = &self.dir {
            command.current_dir(dir);
        }
        command
    }
}

#[cfg(windows)]
fn verbatim_or_quoted(command: &mut Command, word: &str, cmd: bool) {
    use std::os::windows::process::CommandExt;
    if cmd {
        command.raw_arg(word);
    } else {
        command.arg(word);
    }
}

#[cfg(not(windows))]
fn verbatim_or_quoted(command: &mut Command, word: &str, _cmd: bool) {
    command.arg(word);
}
