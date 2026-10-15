use std::io::{self, IsTerminal as _, Write as _};

use crate::tui::frame::{Cell, Color, Style};
use domyjob_core::dashboard::{Restore, TerminalStage};

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;
#[cfg(unix)]
use unix as native;
#[cfg(windows)]
use windows as native;

pub(crate) use native::Input;

const RESET: &[u8] = b"\x1b[0m";
const BOLD: &[u8] = b"\x1b[1m";
const CLEAR: &[u8] = b"\x1b[2J";
const SCREEN_ENTER: &[u8] = b"\x1b[?1049h";
const SCREEN_LEAVE: &[u8] = b"\x1b[?1049l";
const CURSOR_HIDE: &[u8] = b"\x1b[?25l";
const CURSOR_SHOW: &[u8] = b"\x1b[?25h";
const FOREGROUND: u8 = 38;
const BACKGROUND: u8 = 48;
const ANSI_ORIGIN: u32 = 1;

pub(crate) fn clear_frame(output: &mut impl io::Write) -> io::Result<()> {
    output.write_all(CLEAR)
}

fn color(output: &mut impl io::Write, layer: u8, color: Color) -> io::Result<()> {
    let Color::Rgb(red, green, blue) = color;
    write!(output, "\x1b[{layer};2;{red};{green};{blue}m")
}

fn cell_style(output: &mut impl io::Write, value: Style) -> io::Result<()> {
    output.write_all(RESET)?;
    if let Some(foreground) = value.foreground {
        color(output, FOREGROUND, foreground)?;
    }
    if let Some(background) = value.background {
        color(output, BACKGROUND, background)?;
    }
    if value.bold {
        output.write_all(BOLD)?;
    }
    Ok(())
}

pub(crate) fn paint_row(output: &mut impl io::Write, y: u16, cells: &[Cell]) -> io::Result<()> {
    let row = u32::from(y).saturating_add(ANSI_ORIGIN);
    write!(output, "\x1b[{row};1H")?;
    let mut previous = None;
    for cell in cells {
        if cell.continuation {
            continue;
        }
        if previous != Some(cell.style) {
            cell_style(output, cell.style)?;
            previous = Some(cell.style);
        }
        output.write_all(cell.symbol.as_str().as_bytes())?;
    }
    Ok(())
}

pub(crate) fn finish_frame(output: &mut impl io::Write) -> io::Result<()> {
    output.write_all(RESET)?;
    output.flush()
}

#[derive(Debug, thiserror::Error)]
#[error("terminal session failed: primary={primary:?}; cleanup failures={cleanup:?}")]
pub(crate) struct SessionFailure {
    #[source]
    pub(crate) primary: Option<io::Error>,
    pub(crate) cleanup: Vec<io::Error>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Setup {
    Raw,
    Screen,
    Cursor,
}

pub(crate) trait Modes {
    fn setup(&mut self, setup: Setup) -> io::Result<()>;
    fn restore(&mut self, restore: Restore) -> io::Result<()>;
}

trait DisplayModes {
    fn setup(&self) -> io::Result<()>;
    fn restore(&self) -> io::Result<()>;
}

#[derive(Debug)]
pub(crate) struct Native<'a> {
    pub(crate) output: &'a mut io::Stdout,
    pub(crate) input: Input,
    pub(crate) display: native::Display,
}

impl<'a> Native<'a> {
    pub(crate) fn new(output: &'a mut io::Stdout) -> io::Result<Self> {
        Ok(Self {
            input: Input::new(&io::stdin())?,
            display: native::Display::new(output)?,
            output,
        })
    }

    fn control(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.output.write_all(bytes)?;
        self.output.flush()
    }
}

#[derive(Debug, thiserror::Error)]
#[error("restoring terminal modes failed: input={input:?}; output={output:?}")]
struct ModeFailures {
    input: Option<io::Error>,
    output: Option<io::Error>,
}

fn restore_modes(input: io::Result<()>, output: io::Result<()>) -> io::Result<()> {
    match (input, output) {
        (Ok(()), Ok(())) => Ok(()),
        (input, output) => Err(io::Error::other(ModeFailures {
            input: input.err(),
            output: output.err(),
        })),
    }
}

impl Modes for Native<'_> {
    fn setup(&mut self, setup: Setup) -> io::Result<()> {
        match setup {
            Setup::Raw => {
                self.input.setup()?;
                self.display.setup()
            }
            Setup::Screen => self.control(SCREEN_ENTER),
            Setup::Cursor => self.control(CURSOR_HIDE),
        }
    }

    fn restore(&mut self, restore: Restore) -> io::Result<()> {
        match restore {
            Restore::Cursor => self.control(CURSOR_SHOW),
            Restore::Screen => self.control(SCREEN_LEAVE),
            Restore::Raw => restore_modes(self.input.restore(), self.display.restore()),
        }
    }
}

#[must_use]
pub(crate) fn interactive(output: &io::Stdout) -> bool {
    io::stdin().is_terminal() && output.is_terminal()
}

pub(crate) fn session<T, M: Modes>(
    modes: &mut M,
    work: impl FnOnce(&mut M) -> io::Result<T>,
) -> Result<T, SessionFailure> {
    let mut stage = TerminalStage::Untouched;
    let result = enter(modes, &mut stage).and_then(|()| work(modes));
    let mut cleanup = Vec::new();
    for restore in [Restore::Cursor, Restore::Screen, Restore::Raw] {
        if stage.owns(restore)
            && let Err(error) = modes.restore(restore)
        {
            cleanup.push(error);
        }
    }
    match (result, cleanup.is_empty()) {
        (Ok(value), true) => Ok(value),
        (Ok(_value), false) => Err(SessionFailure {
            primary: None,
            cleanup,
        }),
        (Err(error), _) => Err(SessionFailure {
            primary: Some(error),
            cleanup,
        }),
    }
}

fn enter(modes: &mut impl Modes, stage: &mut TerminalStage) -> io::Result<()> {
    *stage = TerminalStage::RawAttempted;
    modes.setup(Setup::Raw)?;
    *stage = TerminalStage::ScreenAttempted;
    modes.setup(Setup::Screen)?;
    *stage = TerminalStage::CursorAttempted;
    modes.setup(Setup::Cursor)
}

#[cfg(test)]
mod tests;
