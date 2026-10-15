use std::io;

use domyjob_core::dashboard::{self, Movement, Section};
use domyjob_core::terminal_input::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use super::model::{Model, RowId};
use super::worker::Worker;
use super::{TuiError, frame::Grid, output, render};
use crate::chat::store::observer::HistoryScan;
use crate::platform::{clock, terminal};

const POLL_MILLIS: u64 = 50;
const REFRESH_MILLIS: u64 = 1000;
const SCROLL_LINES: u16 = 5;

#[derive(Debug, Clone, Copy)]
enum Input {
    Key(KeyEvent),
    Refresh,
    Idle,
}

#[derive(Debug)]
struct NativeInput<'a> {
    refresh: clock::Deadline,
    input: &'a mut terminal::Input,
}

impl NativeInput<'_> {
    fn next(&mut self) -> io::Result<Input> {
        if self.refresh.expired() {
            self.refresh = clock::Deadline::after_millis(REFRESH_MILLIS);
            return Ok(Input::Refresh);
        }
        if let Some(key) = self.input.next(POLL_MILLIS)? {
            return Ok(Input::Key(key));
        }
        Ok(Input::Idle)
    }
}

pub(super) fn run(actor: Option<&str>, mut output: io::Stdout) -> Result<(), TuiError> {
    if !terminal::interactive(&output) {
        return Err(TuiError::NotTerminal);
    }
    let worker = Worker::start()?;
    let mut model = Model::default();
    model.request.initial_actor = actor.map(str::to_owned);
    let mut native = terminal::Native::new(&mut output)?;
    terminal::session(&mut native, |native| {
        let mut renderer = output::Renderer::new(output::Native {
            output: &mut *native.output,
        });
        let mut input = NativeInput {
            refresh: clock::Deadline::after_millis(REFRESH_MILLIS),
            input: &mut native.input,
        };
        event_loop(
            &worker,
            &mut model,
            |model| {
                let (width, height) = native.display.frame_size()?;
                let mut frame = Grid::new(width, height).map_err(io::Error::other)?;
                render::draw(&mut frame, model, clock::observed_millis());
                renderer.draw(frame)
            },
            || input.next(),
        )
    })?;
    Ok(())
}

fn event_loop(
    worker: &Worker,
    model: &mut Model,
    mut draw: impl FnMut(&Model) -> io::Result<()>,
    mut input: impl FnMut() -> io::Result<Input>,
) -> io::Result<()> {
    worker.refresh(model);
    loop {
        worker.drain(model);
        draw(model)?;
        match input()? {
            Input::Refresh => worker.refresh(model),
            Input::Key(key) if key.kind != KeyEventKind::Release => {
                if !key_event(model, key) {
                    break;
                }
                if key.code == KeyCode::Char('r')
                    || model.request.conversation.is_some()
                    || model.request.actor.is_some()
                {
                    worker.refresh(model);
                }
            }
            Input::Key(_) | Input::Idle => {}
        }
    }
    Ok(())
}

fn key_event(model: &mut Model, key: KeyEvent) -> bool {
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        return false;
    }
    if model.searching {
        search(model, key.code);
        return true;
    }
    if key.code == KeyCode::Char('q') {
        return false;
    }
    if model.help {
        model.help = false;
        return true;
    }
    if let Some(position) = model.picker {
        picker(model, position, key.code);
        return true;
    }
    if key.code == KeyCode::Tab {
        model.section = model.section.next();
        model.selected = None;
        model.intended = None;
        model.detail = false;
        model.request.conversation = None;
        model.reconcile();
        if model.section == Section::Chats && model.request.actor.is_none() {
            model.picker = Some(0);
        }
    } else if key.code == KeyCode::Down || key.code == KeyCode::Char('j') {
        model.navigate(Movement::Next);
    } else if key.code == KeyCode::Up || key.code == KeyCode::Char('k') {
        model.navigate(Movement::Previous);
    } else if key.code == KeyCode::Enter {
        model.detail = true;
        model.scroll = 0;
        if let Some(RowId::Chat(conversation)) = &model.selected {
            model.request.conversation = Some(conversation.clone());
            model.snapshot.messages.clear();
            model.snapshot.history_scan = HistoryScan::default();
        }
    } else {
        controls(model, key.code);
    }
    true
}

fn controls(model: &mut Model, key: KeyCode) {
    if key == KeyCode::Esc {
        model.detail = false;
        model.query.clear();
        model.request.conversation = None;
        model.reconcile();
    } else if key == KeyCode::Char('/') {
        model.searching = true;
    } else if key == KeyCode::Char('?') {
        model.help = true;
    } else if key == KeyCode::Char('a') {
        model.picker = Some(0);
    } else if key == KeyCode::PageDown {
        model.scroll = model.scroll.saturating_add(SCROLL_LINES);
    } else if key == KeyCode::PageUp {
        model.scroll = model.scroll.saturating_sub(SCROLL_LINES);
    }
}

fn search(model: &mut Model, key: KeyCode) {
    if key == KeyCode::Esc {
        model.query.clear();
        model.searching = false;
    } else if key == KeyCode::Enter {
        model.searching = false;
    } else if key == KeyCode::Backspace {
        model.query.pop();
    } else if let KeyCode::Char(character) = key {
        dashboard::query_push(&mut model.query, character);
    }
    model.reconcile();
}

fn picker(model: &mut Model, position: usize, key: KeyCode) {
    if key == KeyCode::Esc {
        model.picker = None;
    } else if key == KeyCode::Enter {
        model.choose_actor(position);
    } else if key == KeyCode::Down || key == KeyCode::Char('j') {
        model.picker = dashboard::navigate(
            Some(position),
            model.snapshot.local_agents.len(),
            Movement::Next,
        )
        .or(Some(0));
    } else if key == KeyCode::Up || key == KeyCode::Char('k') {
        model.picker = dashboard::navigate(
            Some(position),
            model.snapshot.local_agents.len(),
            Movement::Previous,
        )
        .or(Some(0));
    }
}

#[cfg(test)]
mod tests;
