pub const MAX_COLUMNS: u16 = 512;
pub const MAX_LINES: u16 = 256;
pub const MAX_GLYPH_BYTES: usize = 128;
pub const PERCENT: u32 = 100;
const NARROW_COLUMNS: usize = 1;
const WIDE_COLUMNS: usize = 2;
const WIDE_CELL_COLUMNS: u16 = 2;
const CENTER_PARTS: u16 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Equality {
    Same,
    Different,
}

impl Equality {
    #[must_use]
    pub const fn new(same: bool) -> Self {
        if same { Self::Same } else { Self::Different }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaintPlan {
    Unchanged,
    ChangedRows,
    Resize,
}

#[must_use]
pub const fn paint_plan(frame_same: Equality, extent_same: Equality) -> PaintPlan {
    if matches!(extent_same, Equality::Different) {
        PaintPlan::Resize
    } else if matches!(frame_same, Equality::Same) {
        PaintPlan::Unchanged
    } else {
        PaintPlan::ChangedRows
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    #[error("the terminal exceeds the owned frame budget")]
    Capacity,
    #[error("a glyph exceeds its byte or display-column budget")]
    Glyph,
    #[error("a cell is outside the admitted frame")]
    Outside,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Extent {
    width: u16,
    height: u16,
}

impl Extent {
    pub const fn new(width: u16, height: u16) -> Result<Self, Refusal> {
        if width > MAX_COLUMNS || height > MAX_LINES {
            Err(Refusal::Capacity)
        } else {
            Ok(Self { width, height })
        }
    }

    #[must_use]
    pub const fn area(self) -> Rect {
        Rect::new(0, 0, self.width, self.height)
    }

    #[must_use]
    pub fn cells(self) -> usize {
        usize::from(self.width).saturating_mul(usize::from(self.height))
    }

    pub fn place(self, x: u16, y: u16, glyph: Glyph) -> Result<Placement, Refusal> {
        let Some(next) = x.checked_add(glyph.columns) else {
            return Err(Refusal::Outside);
        };
        if y >= self.height || next > self.width {
            return Err(Refusal::Outside);
        }
        let index = usize::from(y)
            .saturating_mul(usize::from(self.width))
            .saturating_add(usize::from(x));
        Ok(Placement { index, next })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placement {
    pub index: usize,
    pub next: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Glyph {
    columns: u16,
}

impl Glyph {
    pub const fn new(columns: usize, bytes: usize) -> Result<Self, Refusal> {
        if bytes == 0 || bytes > MAX_GLYPH_BYTES {
            return Err(Refusal::Glyph);
        }
        let columns = match columns {
            NARROW_COLUMNS => 1,
            WIDE_COLUMNS => WIDE_CELL_COLUMNS,
            _ => return Err(Refusal::Glyph),
        };
        Ok(Self { columns })
    }

    #[must_use]
    pub const fn columns(self) -> u16 {
        self.columns
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

impl Rect {
    #[must_use]
    pub const fn new(x: u16, y: u16, width: u16, height: u16) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    #[must_use]
    pub const fn clip(self, extent: Extent) -> Self {
        let x = if self.x < extent.width {
            self.x
        } else {
            extent.width
        };
        let y = if self.y < extent.height {
            self.y
        } else {
            extent.height
        };
        let width = extent.width.saturating_sub(x);
        let height = extent.height.saturating_sub(y);
        Self {
            x,
            y,
            width: if self.width < width {
                self.width
            } else {
                width
            },
            height: if self.height < height {
                self.height
            } else {
                height
            },
        }
    }

    #[must_use]
    pub const fn columns(self, first: u16) -> (Self, Self) {
        let first = if first < self.width {
            first
        } else {
            self.width
        };
        (
            Self {
                width: first,
                ..self
            },
            Self {
                x: self.x.saturating_add(first),
                width: self.width.saturating_sub(first),
                ..self
            },
        )
    }

    #[must_use]
    pub const fn rows(self, first: u16) -> (Self, Self) {
        let first = if first < self.height {
            first
        } else {
            self.height
        };
        (
            Self {
                height: first,
                ..self
            },
            Self {
                y: self.y.saturating_add(first),
                height: self.height.saturating_sub(first),
                ..self
            },
        )
    }

    #[must_use]
    pub const fn inset(self) -> Self {
        let x = if self.width == 0 {
            self.x
        } else {
            self.x.saturating_add(1)
        };
        let y = if self.height == 0 {
            self.y
        } else {
            self.y.saturating_add(1)
        };
        Self {
            x,
            y,
            width: self.width.saturating_sub(1).saturating_sub(1),
            height: self.height.saturating_sub(1).saturating_sub(1),
        }
    }

    #[must_use]
    pub fn centered(self, width_percent: u8, height_percent: u8) -> Option<Self> {
        let width = share(self.width, width_percent)?;
        let height = share(self.height, height_percent)?;
        Some(Self {
            x: self
                .x
                .saturating_add(self.width.saturating_sub(width) / CENTER_PARTS),
            y: self
                .y
                .saturating_add(self.height.saturating_sub(height) / CENTER_PARTS),
            width,
            height,
        })
    }
}

#[must_use]
pub fn share(total: u16, percent: u8) -> Option<u16> {
    if u32::from(percent) > PERCENT {
        return None;
    }
    let scaled = u32::from(total).saturating_mul(u32::from(percent)) / PERCENT;
    match u16::try_from(scaled) {
        Ok(share) => Some(share),
        Err(_) => None,
    }
}

#[cfg(kani)]
mod proofs;
