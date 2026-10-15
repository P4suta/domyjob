use std::io;

use super::{Paint, Renderer};
use crate::tui::frame::{Cell, Grid};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Effect {
    Clear,
    Row(u16),
    Finish,
}

#[derive(Default)]
struct Painter {
    observed: Vec<Effect>,
    failure: Option<Effect>,
}

impl Painter {
    fn record(&mut self, effect: Effect) -> io::Result<()> {
        self.observed.push(effect);
        if self.failure == Some(effect) {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "owned paint fault",
            ))
        } else {
            Ok(())
        }
    }
}

impl Paint for Painter {
    fn clear(&mut self) -> io::Result<()> {
        self.record(Effect::Clear)
    }
    fn row(&mut self, y: u16, _cells: &[Cell]) -> io::Result<()> {
        self.record(Effect::Row(y))
    }
    fn finish(&mut self) -> io::Result<()> {
        self.record(Effect::Finish)
    }
}

#[test]
fn unchanged_frames_emit_no_effect_and_resize_clears_the_old_extent() {
    let mut renderer = Renderer::new(Painter::default());
    let frame = Grid::new(3, 2).unwrap();
    renderer.draw(frame.clone()).unwrap();
    assert_eq!(
        renderer.painter.observed,
        [
            Effect::Clear,
            Effect::Row(0),
            Effect::Row(1),
            Effect::Finish
        ]
    );
    renderer.painter.observed.clear();
    renderer.draw(frame).unwrap();
    assert_eq!(renderer.painter.observed, []);
    renderer.draw(Grid::new(2, 1).unwrap()).unwrap();
    assert_eq!(
        renderer.painter.observed,
        [Effect::Clear, Effect::Row(0), Effect::Finish]
    );
}

#[test]
fn clear_row_and_flush_failures_preserve_the_cause_and_cannot_commit_a_frame() {
    for effect in [
        Effect::Clear,
        Effect::Row(0),
        Effect::Row(1),
        Effect::Finish,
    ] {
        let mut renderer = Renderer::new(Painter {
            failure: Some(effect),
            ..Painter::default()
        });
        let error = renderer.draw(Grid::new(3, 2).unwrap()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(error.to_string(), "owned paint fault");
        assert_eq!(renderer.painter.observed.last(), Some(&effect));
        assert!(renderer.previous.is_none());
        renderer.painter.failure = None;
        renderer.painter.observed.clear();
        renderer.draw(Grid::new(3, 2).unwrap()).unwrap();
        assert_eq!(renderer.painter.observed.first(), Some(&Effect::Clear));
    }
}
