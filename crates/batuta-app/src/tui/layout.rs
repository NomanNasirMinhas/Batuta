//! Where each pane goes, as arithmetic rather than as drawing.
//!
//! Kept apart from `ui.rs` for the same reason the scroll window is: the parts
//! that can be wrong in a way you cannot see — a pane that overlaps another, a
//! results area that collapses to nothing on a small terminal — are then
//! testable without a terminal at all.
//!
//! The layout adapts by dropping the least important thing first. A narrow
//! terminal loses the rail, because a file path needs the width more than the
//! filter list does. A genuinely short one loses the query box's frame, because
//! two rows of border stop explaining themselves once the screen is that
//! cramped.

use ratatui::layout::{Constraint, Layout, Rect};

/// Columns the rail occupies when shown.
pub const RAIL_WIDTH: u16 = 12;

/// Below this width the rail is dropped so paths keep the room.
pub const RAIL_MIN_COLS: u16 = 100;

/// Rows the rail needs to tell the truth: three sections of a header plus
/// three options, a blank line between them, its own border, and the status
/// line below it.
///
/// Withholding it below this is not cosmetic. A clipped rail shows a `SORT`
/// heading with its options cut off, which states that a section exists while
/// hiding which of its entries is active — worse than not drawing it at all.
pub const RAIL_MIN_ROWS: u16 = 3 * 4 + 2 + 2 + 1;

/// At or below this height the query box loses its border.
///
/// Deliberately well under the classic 24-row terminal: at twenty rows the
/// frame still costs only a seventh of the screen and carries the mode title,
/// which is worth more than the two rows. Only when the terminal is genuinely
/// cramped does chrome start losing its place.
pub const COMPACT_MAX_ROWS: u16 = 15;

/// Rows the query box takes with and without its frame.
const QUERY_FRAMED: u16 = 3;
const QUERY_BARE: u16 = 1;

/// Where everything goes for one frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Panes {
    /// The mode/filter/sort rail, or `None` when the terminal is too narrow.
    pub rail: Option<Rect>,
    pub query: Rect,
    pub results: Rect,
    pub status: Rect,
    /// Chrome is being economised because the terminal is short.
    pub compact: bool,
}

/// Divide `area` into panes.
///
/// `rail_wanted` is the user's toggle; a rail can still be withheld when there
/// is not the width for it.
pub fn compute(area: Rect, rail_wanted: bool) -> Panes {
    let compact = area.height <= COMPACT_MAX_ROWS;

    // The status line spans the full width: it is the one thing that should
    // never be squeezed by a side pane.
    let outer = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(area);
    let (main, status) = (outer[0], outer[1]);

    let room_for_rail = area.width >= RAIL_MIN_COLS && area.height >= RAIL_MIN_ROWS;
    let (rail, body) = if rail_wanted && room_for_rail {
        let cols =
            Layout::horizontal([Constraint::Length(RAIL_WIDTH), Constraint::Min(1)]).split(main);
        (Some(cols[0]), cols[1])
    } else {
        (None, main)
    };

    let query_h = if compact { QUERY_BARE } else { QUERY_FRAMED };
    let rows = Layout::vertical([Constraint::Length(query_h), Constraint::Min(0)]).split(body);

    Panes {
        rail,
        query: rows[0],
        results: rows[1],
        status,
        compact,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area(w: u16, h: u16) -> Rect {
        Rect {
            x: 0,
            y: 0,
            width: w,
            height: h,
        }
    }

    /// Every pane has to sit inside the frame. A pane extending past the edge
    /// is not a visual glitch, it is a panic in ratatui's buffer.
    fn assert_inside(p: &Panes, a: Rect) {
        let mut all = vec![p.query, p.results, p.status];
        all.extend(p.rail);
        for r in all {
            assert!(
                r.x >= a.x
                    && r.y >= a.y
                    && r.x + r.width <= a.x + a.width
                    && r.y + r.height <= a.y + a.height,
                "{r:?} escapes {a:?}"
            );
        }
    }

    fn overlaps(a: Rect, b: Rect) -> bool {
        a.x < b.x + b.width && b.x < a.x + a.width && a.y < b.y + b.height && b.y < a.y + a.height
    }

    #[test]
    fn a_wide_tall_terminal_gets_everything() {
        let a = area(140, 40);
        let p = compute(a, true);
        assert!(p.rail.is_some());
        assert_eq!(p.rail.unwrap().width, RAIL_WIDTH);
        assert!(!p.compact);
        assert_eq!(p.query.height, QUERY_FRAMED);
        assert_inside(&p, a);
    }

    #[test]
    fn the_rail_gives_way_before_the_paths_do() {
        // A file path needs the columns more than a filter list does.
        let a = area(RAIL_MIN_COLS - 1, 40);
        let p = compute(a, true);
        assert!(p.rail.is_none());
        assert_eq!(p.results.width, a.width, "results should take the width");
        assert_inside(&p, a);
    }

    #[test]
    fn a_rail_that_would_be_clipped_is_not_drawn_at_all() {
        // A half-drawn rail names a section whose options are off-screen, so
        // it claims state it is not showing. Better to drop it entirely.
        let short = compute(area(140, RAIL_MIN_ROWS - 1), true);
        assert!(short.rail.is_none());

        let exact = compute(area(140, RAIL_MIN_ROWS), true);
        let rail = exact.rail.expect("rail should fit at its stated minimum");
        // Its whole content must fit inside the border, or the minimum is a
        // lie and the clipping is back.
        assert!(
            rail.height >= 3 * 4 + 2 + 2,
            "rail is {} rows, too short for its sections",
            rail.height
        );
    }

    #[test]
    fn asking_for_no_rail_is_honoured_at_any_width() {
        let p = compute(area(200, 50), false);
        assert!(p.rail.is_none());
    }

    #[test]
    fn a_short_terminal_spends_its_rows_on_results_not_chrome() {
        let a = area(120, COMPACT_MAX_ROWS);
        let p = compute(a, true);
        assert!(p.compact);
        assert_eq!(p.query.height, QUERY_BARE);
        // The two rows saved must actually reach the results.
        let tall = compute(area(120, COMPACT_MAX_ROWS + 1), true);
        assert!(!tall.compact);
        assert_eq!(p.results.height, tall.results.height + 1);
        assert_inside(&p, a);
    }

    #[test]
    fn panes_never_overlap() {
        for (w, h) in [(140, 40), (99, 40), (140, 18), (80, 24), (200, 60)] {
            let a = area(w, h);
            let p = compute(a, true);
            let mut all = vec![
                ("query", p.query),
                ("results", p.results),
                ("status", p.status),
            ];
            if let Some(r) = p.rail {
                all.push(("rail", r));
            }
            for (i, (an, ar)) in all.iter().enumerate() {
                for (bn, br) in all.iter().skip(i + 1) {
                    assert!(!overlaps(*ar, *br), "{an} overlaps {bn} at {w}x{h}");
                }
            }
            assert_inside(&p, a);
        }
    }

    #[test]
    fn the_status_line_always_spans_the_full_width() {
        // It is the one row that must not be pushed around by a side pane.
        for (w, h) in [(140, 40), (99, 12), (60, 8)] {
            let p = compute(area(w, h), true);
            assert_eq!(p.status.width, w, "at {w}x{h}");
            assert_eq!(p.status.height, 1);
        }
    }

    #[test]
    fn a_terminal_too_small_for_anything_still_produces_valid_panes() {
        // Degrading has to stay inside the buffer; ratatui panics otherwise,
        // and a resize can make the terminal arbitrarily small for one frame.
        for (w, h) in [(1, 1), (2, 2), (10, 3), (200, 1)] {
            let a = area(w, h);
            let p = compute(a, true);
            assert_inside(&p, a);
        }
    }
}
