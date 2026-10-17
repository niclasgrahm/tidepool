//! The dashboard grid as the editor sees it: cells, snapping to them, and
//! the rule that visuals never overlap (the compiler's rule too: a saved
//! layout that broke it wouldn't apply). No DOM here, so `cargo test`
//! reaches all of it; the editor only turns pointer pixels into cells.

use tidepool_config::dashboard::GridPosition as Rect;

/// The tallest a visual can be made, in rows.
pub const MAX_ROWS: u16 = 40;

/// Whether two rectangles share a cell.
#[must_use]
pub fn overlaps(a: Rect, b: Rect) -> bool {
    a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h
}

/// Whether `rect` has a size, is inside `columns`, and overlaps none of
/// `others`.
#[must_use]
pub fn free(rect: Rect, others: &[Rect], columns: u16) -> bool {
    rect.w > 0
        && rect.h > 0
        && rect.x + rect.w <= columns
        && !others.iter().any(|o| overlaps(rect, *o))
}

/// Whole cells a pointer moved by `px`, a cell being `cell` pixels
/// (gap included): to the nearest one, so a drag snaps as it passes the
/// middle of a cell.
#[must_use]
pub fn cells(px: f64, cell: f64) -> i32 {
    if cell <= 0.0 || !px.is_finite() {
        return 0;
    }
    #[allow(clippy::cast_possible_truncation)]
    let n = (px / cell).round().clamp(-1000.0, 1000.0) as i32;
    n
}

fn shift(at: u16, by: i32, max: u16) -> u16 {
    let moved = (i32::from(at) + by).clamp(0, i32::from(max));
    u16::try_from(moved).unwrap_or(0)
}

/// `start` moved by whole cells, kept inside the grid: never left of the
/// first column, past the last, or above the first row.
#[must_use]
pub fn moved(start: Rect, dx: i32, dy: i32, columns: u16) -> Rect {
    Rect {
        x: shift(start.x, dx, columns.saturating_sub(start.w)),
        y: shift(start.y, dy, u16::MAX - MAX_ROWS),
        ..start
    }
}

/// `start` with its bottom-right corner moved by whole cells: at least one
/// cell, at most to the grid's last column and [`MAX_ROWS`] tall.
#[must_use]
pub fn resized(start: Rect, dw: i32, dh: i32, columns: u16) -> Rect {
    let wide =
        (i32::from(start.w) + dw).clamp(1, i32::from(columns.saturating_sub(start.x).max(1)));
    let tall = (i32::from(start.h) + dh).clamp(1, i32::from(MAX_ROWS));
    Rect {
        w: u16::try_from(wide).unwrap_or(1),
        h: u16::try_from(tall).unwrap_or(1),
        ..start
    }
}

/// The first free place for a `w` × `h` visual, reading the grid like a
/// page: top row first, left to right. Narrowed to the grid if wider.
#[must_use]
pub fn place(w: u16, h: u16, others: &[Rect], columns: u16) -> Rect {
    let w = w.clamp(1, columns.max(1));
    let h = h.max(1);
    let bottom = depth(others);
    for y in 0..=bottom {
        for x in 0..=columns.saturating_sub(w) {
            let rect = Rect { x, y, w, h };
            if free(rect, others, columns) {
                return rect;
            }
        }
    }
    Rect {
        x: 0,
        y: bottom,
        w,
        h,
    }
}

/// The rows the visuals take: the bottom edge of the lowest.
#[must_use]
pub fn depth(rects: &[Rect]) -> u16 {
    rects.iter().map(|r| r.y + r.h).max().unwrap_or(0)
}

/// What an arrow key does to a selected visual: move it a cell, or with
/// Shift grow or shrink it. `None` for other keys.
#[must_use]
pub fn nudge(rect: Rect, key: &str, resize: bool, columns: u16) -> Option<Rect> {
    let (dx, dy) = match key {
        "ArrowLeft" => (-1, 0),
        "ArrowRight" => (1, 0),
        "ArrowUp" => (0, -1),
        "ArrowDown" => (0, 1),
        _ => return None,
    };
    Some(if resize {
        resized(rect, dx, dy, columns)
    } else {
        moved(rect, dx, dy, columns)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(x: u16, y: u16, w: u16, h: u16) -> Rect {
        Rect { x, y, w, h }
    }

    #[test]
    fn touching_edges_is_not_overlapping() {
        assert!(!overlaps(r(0, 0, 3, 2), r(3, 0, 3, 2)));
        assert!(!overlaps(r(0, 0, 3, 2), r(0, 2, 3, 2)));
        assert!(overlaps(r(0, 0, 3, 2), r(2, 1, 3, 2)));
        assert!(overlaps(r(0, 0, 12, 1), r(5, 0, 1, 1)));
    }

    #[test]
    fn a_place_is_free_inside_the_grid_and_off_the_others() {
        let others = [r(0, 0, 6, 2)];
        assert!(free(r(6, 0, 6, 2), &others, 12));
        assert!(!free(r(5, 0, 6, 2), &others, 12), "overlaps");
        assert!(!free(r(7, 0, 6, 2), &others, 12), "past the last column");
        assert!(!free(r(6, 0, 0, 2), &others, 12), "no size");
    }

    #[test]
    fn a_drag_snaps_to_the_nearest_cell() {
        assert_eq!(cells(0.0, 100.0), 0);
        assert_eq!(cells(49.0, 100.0), 0);
        assert_eq!(cells(51.0, 100.0), 1);
        assert_eq!(cells(-151.0, 100.0), -2);
        assert_eq!(cells(10.0, 0.0), 0, "a grid without size moves nothing");
        assert_eq!(cells(f64::NAN, 10.0), 0);
    }

    #[test]
    fn moving_stays_inside_the_grid() {
        assert_eq!(moved(r(2, 3, 4, 2), 3, -1, 12), r(5, 2, 4, 2));
        assert_eq!(moved(r(2, 3, 4, 2), -9, -9, 12), r(0, 0, 4, 2));
        assert_eq!(moved(r(2, 3, 4, 2), 99, 0, 12), r(8, 3, 4, 2));
    }

    #[test]
    fn resizing_keeps_at_least_a_cell_and_stops_at_the_edge() {
        assert_eq!(resized(r(2, 0, 4, 2), 2, 1, 12), r(2, 0, 6, 3));
        assert_eq!(resized(r(2, 0, 4, 2), -9, -9, 12), r(2, 0, 1, 1));
        assert_eq!(resized(r(8, 0, 2, 2), 9, 0, 12), r(8, 0, 4, 2));
        assert_eq!(resized(r(0, 0, 2, 2), 0, 999, 12).h, MAX_ROWS);
    }

    #[test]
    fn a_new_visual_goes_in_the_first_gap() {
        assert_eq!(place(3, 2, &[], 12), r(0, 0, 3, 2));
        let others = [r(0, 0, 3, 2), r(3, 0, 5, 4)];
        assert_eq!(place(3, 2, &others, 12), r(8, 0, 3, 2));
        // too wide for the gap on the right: under the first one
        assert_eq!(place(6, 2, &others, 12), r(0, 4, 6, 2));
        let full = [r(0, 0, 12, 4)];
        assert_eq!(place(4, 3, &full, 12), r(0, 4, 4, 3));
        // wider than the grid: narrowed
        assert_eq!(place(20, 1, &[], 12), r(0, 0, 12, 1));
        for rect in [place(3, 2, &others, 12), place(6, 2, &others, 12)] {
            assert!(free(rect, &others, 12));
        }
    }

    #[test]
    fn arrows_move_and_shift_arrows_resize() {
        assert_eq!(
            nudge(r(1, 1, 2, 2), "ArrowRight", false, 12),
            Some(r(2, 1, 2, 2))
        );
        assert_eq!(
            nudge(r(1, 1, 2, 2), "ArrowUp", false, 12),
            Some(r(1, 0, 2, 2))
        );
        assert_eq!(
            nudge(r(1, 1, 2, 2), "ArrowDown", true, 12),
            Some(r(1, 1, 2, 3))
        );
        assert_eq!(
            nudge(r(1, 1, 2, 2), "ArrowLeft", true, 12),
            Some(r(1, 1, 1, 2))
        );
        assert_eq!(nudge(r(1, 1, 2, 2), "Enter", false, 12), None);
    }

    #[test]
    fn the_depth_is_the_lowest_bottom_edge() {
        assert_eq!(depth(&[]), 0);
        assert_eq!(depth(&[r(0, 0, 3, 2), r(0, 4, 12, 4)]), 8);
    }
}
