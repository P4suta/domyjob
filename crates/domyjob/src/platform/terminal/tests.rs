use super::{Modes, Restore, SessionFailure, Setup, session};

#[derive(Debug, Default)]
struct Fixture {
    fail: Option<Setup>,
    cleanup_fails: bool,
    trace: Vec<String>,
}

impl Modes for Fixture {
    fn setup(&mut self, setup: Setup) -> std::io::Result<()> {
        self.trace.push(format!("setup {setup:?}"));
        if self.fail == Some(setup) {
            Err(std::io::Error::other("setup fault"))
        } else {
            Ok(())
        }
    }

    fn restore(&mut self, restore: Restore) -> std::io::Result<()> {
        self.trace.push(format!("restore {restore:?}"));
        if self.cleanup_fails {
            Err(std::io::Error::other(format!("cleanup {restore:?}")))
        } else {
            Ok(())
        }
    }
}

fn primary(failure: &SessionFailure) -> &std::io::Error {
    failure.primary.as_ref().unwrap()
}

#[test]
fn every_partial_setup_failure_restores_all_attempted_modes() {
    for (stage, expected) in [
        (Setup::Raw, vec!["setup Raw", "restore Raw"]),
        (
            Setup::Screen,
            vec!["setup Raw", "setup Screen", "restore Screen", "restore Raw"],
        ),
        (
            Setup::Cursor,
            vec![
                "setup Raw",
                "setup Screen",
                "setup Cursor",
                "restore Cursor",
                "restore Screen",
                "restore Raw",
            ],
        ),
    ] {
        let mut fixture = Fixture {
            fail: Some(stage),
            ..Fixture::default()
        };
        let mut work_calls = 0;
        let failure = session(&mut fixture, |_| {
            work_calls += 1;
            Ok(())
        })
        .unwrap_err();
        assert_eq!(work_calls, 0);
        assert_eq!(primary(&failure).to_string(), "setup fault");
        assert_eq!(fixture.trace, expected);
        assert!(failure.cleanup.is_empty());
    }
}

#[test]
fn rendering_or_input_failure_keeps_primary_and_every_cleanup_failure() {
    let mut fixture = Fixture {
        cleanup_fails: true,
        ..Fixture::default()
    };
    let failure = session(&mut fixture, |_| {
        Err::<(), _>(std::io::Error::from_raw_os_error(7))
    })
    .unwrap_err();
    assert_eq!(primary(&failure).raw_os_error(), Some(7));
    assert_eq!(failure.cleanup.len(), 3);
    assert_eq!(
        fixture.trace,
        [
            "setup Raw",
            "setup Screen",
            "setup Cursor",
            "restore Cursor",
            "restore Screen",
            "restore Raw"
        ]
    );
}

#[test]
fn cleanup_errors_after_a_normal_exit_are_reported() {
    let mut fixture = Fixture {
        cleanup_fails: true,
        ..Fixture::default()
    };
    let failure = session(&mut fixture, |_| Ok(())).unwrap_err();
    assert!(failure.primary.is_none());
    assert_eq!(failure.cleanup.len(), 3);
}

#[test]
fn both_native_restore_errors_are_retained() {
    let failure = super::restore_modes(
        Err(std::io::Error::from_raw_os_error(7)),
        Err(std::io::Error::from_raw_os_error(8)),
    )
    .unwrap_err();
    let modes = failure
        .get_ref()
        .unwrap()
        .downcast_ref::<super::ModeFailures>()
        .unwrap();
    assert_eq!(modes.input.as_ref().unwrap().raw_os_error(), Some(7));
    assert_eq!(modes.output.as_ref().unwrap().raw_os_error(), Some(8));
}

#[test]
fn ansi_rows_preserve_rgb_style_and_skip_wide_continuations() {
    use crate::tui::frame::{Cell, Color, Style};
    use domyjob_core::dashboard::display;
    let style = Style::default()
        .fg(Color::Rgb(1, 2, 3))
        .bg(Color::Rgb(4, 5, 6))
        .bold();
    let mut output = Vec::new();
    super::clear_frame(&mut output).unwrap();
    super::paint_row(
        &mut output,
        2,
        &[
            Cell {
                symbol: display("界"),
                style,
                continuation: false,
            },
            Cell {
                symbol: display(""),
                style,
                continuation: true,
            },
            Cell {
                symbol: display("q"),
                style,
                continuation: false,
            },
        ],
    )
    .unwrap();
    super::finish_frame(&mut output).unwrap();
    assert_eq!(
        output,
        "\u{1b}[2J\u{1b}[3;1H\u{1b}[0m\u{1b}[38;2;1;2;3m\u{1b}[48;2;4;5;6m\u{1b}[1m界q\u{1b}[0m"
            .as_bytes()
    );
}
