use super::{Extent, Glyph, MAX_COLUMNS, MAX_GLYPH_BYTES, MAX_LINES, Rect, Refusal};

#[kani::proof]
fn redraws_follow_the_actual_frame_and_extent_equality() {
    let frame_same: bool = kani::any();
    let extent_same: bool = kani::any();
    let plan = super::paint_plan(
        super::Equality::new(frame_same),
        super::Equality::new(extent_same),
    );
    assert_eq!(
        plan == super::PaintPlan::Unchanged,
        frame_same && extent_same
    );
    assert_eq!(plan == super::PaintPlan::Resize, !extent_same);
    assert_eq!(
        plan == super::PaintPlan::ChangedRows,
        !frame_same && extent_same
    );
    kani::cover!(plan == super::PaintPlan::Unchanged);
    kani::cover!(plan == super::PaintPlan::Resize);
    kani::cover!(plan == super::PaintPlan::ChangedRows);
}

#[kani::proof]
fn frame_allocation_requires_bounded_terminal_dimensions() {
    let width: u16 = kani::any();
    let height: u16 = kani::any();
    match Extent::new(width, height) {
        Ok(extent) => {
            assert!(width <= MAX_COLUMNS && height <= MAX_LINES);
            assert_eq!(extent.cells(), usize::from(width) * usize::from(height));
            assert!(extent.cells() <= usize::from(MAX_COLUMNS) * usize::from(MAX_LINES));
            assert_eq!(extent.area(), Rect::new(0, 0, width, height));
        }
        Err(refusal) => {
            assert_eq!(refusal, Refusal::Capacity);
            assert!(width > MAX_COLUMNS || height > MAX_LINES);
        }
    }
    kani::cover!(width == 0 && height == 0);
    kani::cover!(width == MAX_COLUMNS && height == MAX_LINES);
    kani::cover!(width > MAX_COLUMNS);
    kani::cover!(height > MAX_LINES);
}

fn contained(rect: Rect, extent: Extent) {
    let area = extent.area();
    assert!(u32::from(rect.x) + u32::from(rect.width) <= u32::from(area.width));
    assert!(u32::from(rect.y) + u32::from(rect.height) <= u32::from(area.height));
}

#[kani::proof]
fn clipped_layouts_remain_inside_the_admitted_frame() {
    let width: u16 = kani::any();
    let height: u16 = kani::any();
    let Ok(extent) = Extent::new(width, height) else {
        return;
    };
    let original = Rect::new(kani::any(), kani::any(), kani::any(), kani::any());
    let clipped = original.clip(extent);
    contained(clipped, extent);
    assert!(clipped.width <= original.width && clipped.height <= original.height);
    assert_eq!(clipped.clip(extent), clipped);
    let part: u16 = kani::any();
    let (left, right) = clipped.columns(part);
    contained(left, extent);
    contained(right, extent);
    assert_eq!(left.width + right.width, clipped.width);
    assert_eq!(left.x, clipped.x);
    assert_eq!(right.x, clipped.x + left.width);
    let (top, bottom) = clipped.rows(part);
    contained(top, extent);
    contained(bottom, extent);
    assert_eq!(top.height + bottom.height, clipped.height);
    assert_eq!(bottom.y, clipped.y + top.height);
    contained(clipped.inset(), extent);
    let width_percent: u8 = kani::any();
    let height_percent: u8 = kani::any();
    match clipped.centered(width_percent, height_percent) {
        Some(centered) => {
            assert!(width_percent <= 100 && height_percent <= 100);
            contained(centered, extent);
            assert!(centered.width <= clipped.width && centered.height <= clipped.height);
        }
        None => assert!(width_percent > 100 || height_percent > 100),
    }
    kani::cover!(clipped.width == 0);
    kani::cover!(clipped == extent.area() && width > 0 && height > 0);
    kani::cover!(part == 0 && clipped.width > 0);
    kani::cover!(part >= clipped.width && clipped.width > 0);
    kani::cover!(width_percent == 100 && height_percent == 100);
    kani::cover!(width_percent > 100);
}

#[kani::proof]
fn glyph_placement_requires_a_complete_cell_inside_its_row() {
    let columns: usize = kani::any();
    let bytes: usize = kani::any();
    let glyph = Glyph::new(columns, bytes);
    assert_eq!(
        glyph.is_ok(),
        matches!(columns, 1 | 2) && bytes > 0 && bytes <= MAX_GLYPH_BYTES
    );
    let Ok(glyph) = glyph else { return };
    assert_eq!(usize::from(glyph.columns()), columns);
    let width: u16 = kani::any();
    let height: u16 = kani::any();
    let Ok(extent) = Extent::new(width, height) else {
        return;
    };
    let x: u16 = kani::any();
    let y: u16 = kani::any();
    let expected = y < height && u32::from(x) + u32::from(glyph.columns()) <= u32::from(width);
    let placed = extent.place(x, y, glyph);
    assert_eq!(placed.is_ok(), expected);
    if let Ok(placement) = placed {
        assert_eq!(
            placement.index,
            usize::from(y) * usize::from(width) + usize::from(x)
        );
        assert_eq!(placement.next, x + glyph.columns());
        assert!(placement.index + columns <= extent.cells());
    }
    kani::cover!(columns == 1 && placed.is_ok());
    kani::cover!(columns == 2 && placed.is_ok());
    kani::cover!(y >= height);
    kani::cover!(y < height && x == width && width > 0);
}

#[kani::proof]
fn rejects_oversized_frame_allocation() {
    kani::assert(
        Extent::new(MAX_COLUMNS + 1, 1).is_ok(),
        "NEGATIVE_DASHBOARD_FRAME_CONTROL",
    );
}

#[kani::proof]
fn rejects_an_unclipped_foreign_region() {
    let Ok(extent) = Extent::new(5, 5) else {
        return;
    };
    let foreign = Rect::new(10, 0, 10, 1);
    kani::assert(
        foreign.clip(extent) == foreign,
        "NEGATIVE_DASHBOARD_REGION_CONTROL",
    );
}

#[kani::proof]
fn rejects_wide_glyphs_crossing_the_last_column() {
    let Ok(extent) = Extent::new(3, 2) else {
        return;
    };
    let Ok(glyph) = Glyph::new(2, 3) else { return };
    kani::assert(
        extent.place(2, 0, glyph).is_ok(),
        "NEGATIVE_DASHBOARD_CELL_CONTROL",
    );
}

#[kani::proof]
fn rejects_skipping_a_changed_frame() {
    kani::assert(
        super::paint_plan(super::Equality::Different, super::Equality::Same)
            == super::PaintPlan::Unchanged,
        "NEGATIVE_DASHBOARD_PAINT_CONTROL",
    );
}
