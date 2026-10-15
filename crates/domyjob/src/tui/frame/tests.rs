use domyjob_core::dashboard::display;
use domyjob_core::dashboard::geometry::{Rect, Refusal};

use super::{Grid, Line, Span, Style};

#[test]
fn clipping_a_styled_line_stops_at_the_first_nonfitting_grapheme() {
    let mut grid = Grid::new(7, 1).unwrap();
    let line = Line::spans(vec![
        Span::styled("abcdef界", Style::default()),
        Span::styled("X", Style::default()),
    ]);
    grid.line(grid.area(), &line, Style::default());
    assert_eq!(grid.cell(6, 0).unwrap().symbol(), " ");
    assert!(!grid.content().iter().any(|cell| cell.symbol() == "X"));
}

#[test]
fn complete_graphemes_and_continuation_cells_never_cross_the_row() {
    let mut grid = Grid::new(3, 2).unwrap();
    assert_eq!(grid.put((1, 0), &display("界"), Style::default()), Ok(3));
    assert_eq!(grid.cell(1, 0).unwrap().symbol(), "界");
    assert!(grid.cell(2, 0).unwrap().continuation);
    let before = grid.clone();
    assert_eq!(
        grid.put((2, 1), &display("界"), Style::default()),
        Err(Refusal::Outside)
    );
    assert_eq!(grid, before);
    assert_eq!(
        grid.put((0, 1), &display("e\u{301}"), Style::default()),
        Ok(1)
    );
    assert_eq!(grid.cell(0, 1).unwrap().symbol(), "e\u{301}");
}

#[test]
fn replacing_either_half_of_a_wide_glyph_removes_its_old_cells() {
    let mut grid = Grid::new(4, 1).unwrap();
    grid.put((0, 0), &display("界"), Style::default()).unwrap();
    grid.put((1, 0), &display("x"), Style::default()).unwrap();
    assert_eq!(grid.cell(0, 0).unwrap().symbol(), " ");
    assert_eq!(grid.cell(1, 0).unwrap().symbol(), "x");
    assert!(!grid.cell(1, 0).unwrap().continuation);
    grid.put((0, 0), &display("界"), Style::default()).unwrap();
    grid.put((0, 0), &display("x"), Style::default()).unwrap();
    assert_eq!(grid.cell(1, 0).unwrap().symbol(), " ");
    assert!(!grid.cell(1, 0).unwrap().continuation);
}

#[test]
fn foreign_regions_and_hostile_text_cannot_escape_the_owned_frame() {
    let mut grid = Grid::new(16, 2).unwrap();
    grid.line(
        Rect::new(0, 0, 16, 1),
        &Line::from("x\u{1b}[31m\u{202e}"),
        Style::default(),
    );
    assert!(
        grid.content()
            .iter()
            .all(|cell| !cell.symbol().contains('\u{1b}') && !cell.symbol().contains('\u{202e}'))
    );
    let before = grid.clone();
    grid.fill(
        Rect::new(u16::MAX, u16::MAX, u16::MAX, u16::MAX),
        Style::default(),
    );
    assert_eq!(grid, before);
    assert_eq!(
        grid.put((0, 0), &display(&"a".repeat(129)), Style::default()),
        Err(Refusal::Glyph)
    );
    assert_eq!(grid, before);
}

#[test]
fn leading_combining_and_ordinary_font_variants_have_defined_cells() {
    let mut grid = Grid::new(16, 1).unwrap();
    grid.line(
        grid.area(),
        &Line::from("\u{301} A界 e\u{301}🙂"),
        Style::default(),
    );
    assert_eq!(grid.cell(0, 0).unwrap().symbol(), "◌\u{301}");
    assert!(grid.content().iter().any(|cell| cell.symbol() == "界"));
    assert!(
        grid.content()
            .iter()
            .any(|cell| cell.symbol() == "e\u{301}")
    );
    assert!(grid.content().iter().any(|cell| cell.symbol() == "🙂"));
}

#[test]
fn clipped_paragraphs_cannot_write_into_the_next_panel() {
    let mut grid = Grid::new(6, 2).unwrap();
    grid.line(Rect::new(0, 0, 3, 1), &Line::from("a界b"), Style::default());
    assert_eq!(grid.cell(0, 0).unwrap().symbol(), "a");
    assert_eq!(grid.cell(1, 0).unwrap().symbol(), "界");
    assert!(grid.cell(2, 0).unwrap().continuation);
    assert_eq!(grid.cell(3, 0).unwrap().symbol(), " ");
}
