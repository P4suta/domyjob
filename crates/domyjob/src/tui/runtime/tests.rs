use super::{Input, Worker, event_loop};
use super::{Model, key_event};
use crate::chat::store::observer::Snapshot;
use domyjob_core::dashboard::Section;
use domyjob_core::terminal_input::{KeyCode, KeyEvent, KeyModifiers};

#[test]
fn chats_prompt_for_an_explicit_local_viewing_identity() {
    let mut model = Model {
        section: Section::Agents,
        ..Model::default()
    };
    assert!(key_event(
        &mut model,
        KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)
    ));
    assert_eq!(model.section, Section::Chats);
    assert_eq!(model.picker, Some(0));
    assert!(model.request.actor.is_none());
}

#[test]
fn nonterminal_startup_refuses_before_a_worker_or_terminal_mode_is_entered() {
    let output = crate::raw::output().into_stream();
    assert!(matches!(
        super::run(None, output),
        Err(crate::tui::TuiError::NotTerminal)
    ));
}

#[test]
fn search_accepts_unicode_and_esc_restores_navigation() {
    let mut model = Model::default();
    for key in [
        KeyCode::Char('/'),
        KeyCode::Char('界'),
        KeyCode::Char('\u{1b}'),
    ] {
        assert!(key_event(
            &mut model,
            KeyEvent::new(key, KeyModifiers::NONE)
        ));
    }
    assert_eq!(model.query, "界");
    assert!(key_event(
        &mut model,
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)
    ));
    assert!(!model.searching);
    assert_eq!(model.query, "");
    assert!(!key_event(
        &mut model,
        KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)
    ));
}

#[test]
fn the_exit_key_remains_available_while_choosing_a_viewing_identity() {
    let mut model = Model {
        picker: Some(0),
        ..Model::default()
    };
    assert!(!key_event(
        &mut model,
        KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)
    ));
}

#[test]
fn actual_loop_stops_at_the_first_draw_or_input_error() {
    let worker = Worker::with_observer(|_| Ok(Snapshot::default())).unwrap();
    let mut model = Model::default();
    let mut input_calls = 0;
    let draw_error = event_loop(
        &worker,
        &mut model,
        |_| Err(std::io::Error::from_raw_os_error(7)),
        || {
            input_calls += 1;
            Ok(Input::Idle)
        },
    )
    .unwrap_err();
    assert_eq!(draw_error.raw_os_error(), Some(7));
    assert_eq!(input_calls, 0);
    let input_error = event_loop(
        &worker,
        &mut model,
        |_| Ok(()),
        || Err(std::io::Error::from_raw_os_error(8)),
    )
    .unwrap_err();
    assert_eq!(input_error.raw_os_error(), Some(8));
}

#[test]
fn keyboard_exit_is_not_blocked_by_an_inflight_observation() {
    let (entered, started) = std::sync::mpsc::sync_channel(1);
    let (release, blocked) = std::sync::mpsc::sync_channel(1);
    let worker = Worker::with_observer(move |_| {
        entered.send(()).unwrap();
        blocked.recv().unwrap();
        Ok(Snapshot::default())
    })
    .unwrap();
    let mut model = Model::default();
    worker.refresh(&mut model);
    assert!(matches!(
        crate::platform::clock::receive(
            &started,
            crate::platform::clock::Deadline::after_seconds(5)
        ),
        crate::platform::clock::Waited::Received(())
    ));
    event_loop(
        &worker,
        &mut model,
        |_| Ok(()),
        || {
            Ok(Input::Key(KeyEvent::new(
                KeyCode::Char('q'),
                KeyModifiers::NONE,
            )))
        },
    )
    .unwrap();
    drop(worker);
    release.send(()).unwrap();
}
