use domyjob_core::dashboard::geometry::{Extent, Glyph, Rect, Refusal};
use domyjob_core::dashboard::{DisplayText, display};
use unicode_segmentation::UnicodeSegmentation as _;
use unicode_width::UnicodeWidthStr as _;

const MARKER_COLUMNS: u16 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Wrap {
    Clip,
    Fold,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct Flow {
    pub style: Style,
    pub scroll: u16,
    pub wrap: Wrap,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct PanelStyle {
    pub normal: Style,
    pub border: Style,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct ListStyle {
    pub selected: Option<usize>,
    pub normal: Style,
    pub highlight: Style,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Color {
    Rgb(u8, u8, u8),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Style {
    pub foreground: Option<Color>,
    pub background: Option<Color>,
    pub bold: bool,
}

impl Style {
    pub(crate) const fn fg(mut self, color: Color) -> Self {
        self.foreground = Some(color);
        self
    }

    pub(crate) const fn bg(mut self, color: Color) -> Self {
        self.background = Some(color);
        self
    }

    pub(crate) const fn bold(mut self) -> Self {
        self.bold = true;
        self
    }

    const fn overlay(self, other: Self) -> Self {
        Self {
            foreground: match other.foreground {
                Some(color) => Some(color),
                None => self.foreground,
            },
            background: match other.background {
                Some(color) => Some(color),
                None => self.background,
            },
            bold: self.bold || other.bold,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Span {
    text: DisplayText,
    style: Style,
}

impl Span {
    pub(super) fn styled(text: impl AsRef<str>, style: Style) -> Self {
        Self {
            text: display(text.as_ref()),
            style,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Line {
    spans: Vec<Span>,
}

impl Line {
    pub(super) fn styled(text: impl AsRef<str>, style: Style) -> Self {
        Self {
            spans: vec![Span::styled(text, style)],
        }
    }

    pub(super) const fn spans(spans: Vec<Span>) -> Self {
        Self { spans }
    }
}

impl From<&str> for Line {
    fn from(text: &str) -> Self {
        Self::styled(text, Style::default())
    }
}

impl From<String> for Line {
    fn from(text: String) -> Self {
        Self::from(text.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Cell {
    pub symbol: DisplayText,
    pub style: Style,
    pub continuation: bool,
}

impl Cell {
    fn blank(style: Style) -> Self {
        Self {
            symbol: display(" "),
            style,
            continuation: false,
        }
    }

    #[cfg(test)]
    pub(crate) fn symbol(&self) -> &str {
        self.symbol.as_str()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Grid {
    extent: Extent,
    cells: Vec<Cell>,
}

impl Grid {
    pub(super) fn new(width: u16, height: u16) -> Result<Self, Refusal> {
        let extent = Extent::new(width, height)?;
        Ok(Self {
            extent,
            cells: vec![Cell::blank(Style::default()); extent.cells()],
        })
    }

    pub(super) const fn area(&self) -> Rect {
        self.extent.area()
    }

    #[cfg(test)]
    pub(super) fn content(&self) -> &[Cell] {
        &self.cells
    }

    pub(super) fn row(&self, y: u16) -> Option<&[Cell]> {
        let width = usize::from(self.area().width);
        let start = usize::from(y).checked_mul(width)?;
        let end = start.checked_add(width)?;
        if y < self.area().height {
            self.cells.get(start..end)
        } else {
            None
        }
    }

    #[cfg(test)]
    pub(super) fn cell(&self, x: u16, y: u16) -> Option<&Cell> {
        let Ok(glyph) = Glyph::new(1, 1) else {
            return None;
        };
        let Ok(placement) = self.extent.place(x, y, glyph) else {
            return None;
        };
        self.cells.get(placement.index)
    }

    fn erase(&mut self, index: usize) {
        let Some(original) = self.cells.get(index) else {
            return;
        };
        let style = original.style;
        let continuation = original.continuation;
        if continuation
            && let Some(previous) = index
                .checked_sub(1)
                .and_then(|index| self.cells.get_mut(index))
        {
            *previous = Cell::blank(style);
        }
        if let Some(cell) = self.cells.get_mut(index) {
            *cell = Cell::blank(style);
        }
        if let Some(next) = index
            .checked_add(1)
            .and_then(|index| self.cells.get_mut(index))
            && next.continuation
        {
            *next = Cell::blank(style);
        }
    }

    pub(super) fn put(
        &mut self,
        (x, y): (u16, u16),
        text: &DisplayText,
        style: Style,
    ) -> Result<u16, Refusal> {
        let glyph = Glyph::new(text.as_str().width(), text.as_str().len())?;
        let placement = self.extent.place(x, y, glyph)?;
        self.erase(placement.index);
        if glyph.columns() > 1
            && let Some(next) = placement.index.checked_add(1)
        {
            self.erase(next);
        }
        let cell = self
            .cells
            .get_mut(placement.index)
            .ok_or(Refusal::Outside)?;
        *cell = Cell {
            symbol: text.clone(),
            style,
            continuation: false,
        };
        if glyph.columns() > 1 {
            let next = placement.index.checked_add(1).ok_or(Refusal::Outside)?;
            let following = self.cells.get_mut(next).ok_or(Refusal::Outside)?;
            *following = Cell {
                symbol: display(" "),
                style,
                continuation: true,
            };
        }
        Ok(placement.next)
    }

    pub(super) fn fill(&mut self, area: Rect, style: Style) {
        let area = area.clip(self.extent);
        let blank = display(" ");
        for y in area.y..area.y.saturating_add(area.height) {
            for x in area.x..area.x.saturating_add(area.width) {
                let _clipped = self.put((x, y), &blank, style);
            }
        }
    }

    pub(super) fn line(&mut self, area: Rect, line: &Line, base: Style) {
        self.paragraph(
            area,
            core::slice::from_ref(line),
            Flow {
                style: base,
                scroll: 0,
                wrap: Wrap::Clip,
            },
        );
    }

    pub(super) fn paragraph(&mut self, area: Rect, lines: &[Line], flow: Flow) {
        let area = area.clip(self.extent);
        if area.width == 0 || area.height == 0 {
            return;
        }
        let end = area.x.saturating_add(area.width);
        let first = usize::from(flow.scroll);
        let last = first.saturating_add(usize::from(area.height));
        let mut row = 0_usize;
        for line in lines {
            let mut x = area.x;
            'spans: for span in &line.spans {
                for grapheme in span.text.as_str().graphemes(true) {
                    let (text, columns) = glyph_text(grapheme);
                    if x.saturating_add(columns) > end {
                        if flow.wrap == Wrap::Clip {
                            break 'spans;
                        }
                        row = row.saturating_add(1);
                        x = area.x;
                    }
                    if row >= last {
                        return;
                    }
                    if row >= first
                        && let Ok(offset) = u16::try_from(row.saturating_sub(first))
                    {
                        let _clipped = self.put(
                            (x, area.y.saturating_add(offset)),
                            &text,
                            flow.style.overlay(span.style),
                        );
                    }
                    x = x.saturating_add(columns);
                }
            }
            row = row.saturating_add(1);
            if row >= last {
                return;
            }
        }
    }

    pub(super) fn panel(&mut self, area: Rect, title: &str, palette: PanelStyle) -> Rect {
        let area = area.clip(self.extent);
        self.fill(area, palette.normal);
        if area.width == 0 || area.height == 0 {
            return area.inset();
        }
        let right = area.x.saturating_add(area.width).saturating_sub(1);
        let bottom = area.y.saturating_add(area.height).saturating_sub(1);
        let horizontal = display("─");
        let vertical = display("│");
        for x in area.x..=right {
            let _top = self.put((x, area.y), &horizontal, palette.border);
            let _bottom = self.put((x, bottom), &horizontal, palette.border);
        }
        for y in area.y..=bottom {
            let _left = self.put((area.x, y), &vertical, palette.border);
            let _right = self.put((right, y), &vertical, palette.border);
        }
        for (x, y, glyph) in [
            (area.x, area.y, "╭"),
            (right, area.y, "╮"),
            (area.x, bottom, "╰"),
            (right, bottom, "╯"),
        ] {
            let _clipped = self.put((x, y), &display(glyph), palette.border);
        }
        let inner = area.inset();
        self.line(
            Rect::new(inner.x, area.y, inner.width, 1),
            &Line::from(title),
            palette.border,
        );
        inner
    }

    pub(super) fn list(&mut self, area: Rect, items: &[Vec<Line>], palette: ListStyle) {
        let area = area.clip(self.extent);
        let mut start = 0;
        if let Some(selected) = palette.selected.filter(|index| *index < items.len()) {
            let mut height = items
                .iter()
                .take(selected.saturating_add(1))
                .map(Vec::len)
                .sum::<usize>();
            while height > usize::from(area.height) && start < selected {
                height = height.saturating_sub(items.get(start).map_or(0, Vec::len));
                start = start.saturating_add(1);
            }
        }
        let mut row = 0_u16;
        for (index, item) in items.iter().enumerate().skip(start) {
            let active = palette.selected == Some(index);
            let style = if active {
                palette.normal.overlay(palette.highlight)
            } else {
                palette.normal
            };
            for (offset, line) in item.iter().enumerate() {
                if row >= area.height {
                    return;
                }
                let row_area = Rect::new(area.x, area.y.saturating_add(row), area.width, 1);
                self.fill(row_area, style);
                let (marker, text) = row_area.columns(MARKER_COLUMNS);
                if active && offset == 0 {
                    self.line(marker, &Line::from("› "), style);
                }
                self.line(text, line, style);
                row = row.saturating_add(1);
            }
        }
    }
}

fn glyph_text(text: &str) -> (DisplayText, u16) {
    let text = if text.width() == 0 {
        format!("◌{text}")
    } else {
        text.to_owned()
    };
    let escaped = display(&text);
    match Glyph::new(escaped.as_str().width(), escaped.as_str().len()) {
        Ok(glyph) => (escaped, glyph.columns()),
        Err(_) => (display("?"), 1),
    }
}

#[cfg(test)]
#[path = "frame/tests.rs"]
mod tests;
