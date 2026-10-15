use domyjob_core::dashboard::geometry::{Equality, PaintPlan, paint_plan};
use std::io;

use super::frame::{Cell, Grid};
use crate::platform::terminal;

pub(super) trait Paint {
    fn clear(&mut self) -> io::Result<()>;
    fn row(&mut self, y: u16, cells: &[Cell]) -> io::Result<()>;
    fn finish(&mut self) -> io::Result<()>;
}

pub(super) struct Native<'a, W> {
    pub output: &'a mut W,
}

impl<W: io::Write> Paint for Native<'_, W> {
    fn clear(&mut self) -> io::Result<()> {
        terminal::clear_frame(self.output)
    }
    fn row(&mut self, y: u16, cells: &[Cell]) -> io::Result<()> {
        terminal::paint_row(self.output, y, cells)
    }
    fn finish(&mut self) -> io::Result<()> {
        terminal::finish_frame(self.output)
    }
}

pub(super) struct Renderer<P> {
    painter: P,
    previous: Option<Grid>,
}

impl<P: Paint> Renderer<P> {
    pub(super) const fn new(painter: P) -> Self {
        Self {
            painter,
            previous: None,
        }
    }

    pub(super) fn draw(&mut self, next: Grid) -> io::Result<()> {
        let plan = paint_plan(
            Equality::new(self.previous.as_ref() == Some(&next)),
            Equality::new(
                self.previous
                    .as_ref()
                    .is_some_and(|previous| previous.area() == next.area()),
            ),
        );
        if plan == PaintPlan::Unchanged {
            return Ok(());
        }
        let resized = plan == PaintPlan::Resize;
        if resized {
            self.painter.clear()?;
        }
        for y in 0..next.area().height {
            let row = next
                .row(y)
                .ok_or_else(|| io::Error::other("an admitted frame row is absent"))?;
            if !resized && self.previous.as_ref().and_then(|previous| previous.row(y)) == Some(row)
            {
                continue;
            }
            self.painter.row(y, row)?;
        }
        self.painter.finish()?;
        self.previous = Some(next);
        Ok(())
    }
}

#[cfg(test)]
mod tests;
