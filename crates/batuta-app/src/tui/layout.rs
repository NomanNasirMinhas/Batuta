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

/// Columns the directory tree occupies when shown.
pub const TREE_WIDTH: u16 = 30;

/// Below this width the tree is dropped. The editor needs the columns more:
/// a tree can be reached from the path bar, but wrapped code cannot be read.
pub const TREE_MIN_COLS: u16 = 90;

/// Rows the path bar takes with and without its frame.
const PATH_FRAMED: u16 = 3;
const PATH_BARE: u16 = 1;

/// Where the explorer's panes go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExplorerPanes {
    /// The custom title bar, carrying the window controls.
    pub title: Rect,
    /// The directory tree, or `None` when the terminal is too narrow.
    pub tree: Option<Rect>,
    pub editor: Rect,
    /// The find prompt, only while it is showing. It takes its row from the
    /// editor rather than from the path bar: losing a line of text for the
    /// duration of a search is cheaper than hiding the path you are editing.
    pub find: Option<Rect>,
    pub path: Rect,
    pub status: Rect,
    pub compact: bool,
}

/// Divide `area` into the explorer's panes.
///
/// Same degradation rule as [`compute`]: drop the least important thing first.
/// Here that is the tree, then the path bar's frame.
pub fn explorer(area: Rect, tree_wanted: bool, finding: bool) -> ExplorerPanes {
    let compact = area.height <= COMPACT_MAX_ROWS;
    let path_h = if compact { PATH_BARE } else { PATH_FRAMED };

    // Status first and full width, then the path bar above it: both span the
    // whole terminal, because a path is the longest string on screen and the
    // one that suffers most from being boxed in.
    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(path_h),
        Constraint::Length(1),
    ])
    .split(area);
    let (title, main, path, status) = (rows[0], rows[1], rows[2], rows[3]);

    let (tree, editor) = if tree_wanted && area.width >= TREE_MIN_COLS {
        let cols =
            Layout::horizontal([Constraint::Length(TREE_WIDTH), Constraint::Min(1)]).split(main);
        (Some(cols[0]), cols[1])
    } else {
        (None, main)
    };

    // The prompt takes the editor's last row, and only when there is a row to
    // spare: at one row high the editor would be left with nothing, and a find
    // bar with no text to search is worse than no find bar.
    let (editor, find) = if finding && editor.height > 1 {
        let split = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(editor);
        (split[0], Some(split[1]))
    } else {
        (editor, None)
    };

    ExplorerPanes {
        title,
        tree,
        editor,
        find,
        path,
        status,
        compact,
    }
}

/// A control on the custom title bar.
///
/// The hotkey window is deliberately borderless, which took the real title bar
/// with it. These put back the three things it was for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TitleButton {
    Minimize,
    Maximize,
    Close,
}

/// Columns each button occupies, and how many there are.
const BUTTON_WIDTH: u16 = 5;
const BUTTONS: u16 = 3;

/// Which button, if any, sits under a click.
///
/// Pure arithmetic so the hit boxes can be tested without a mouse: an
/// off-by-one here means closing the window when you meant to minimise it,
/// which is not a mistake worth discovering by hand.
pub fn title_button_at(bar: Rect, x: u16, y: u16) -> Option<TitleButton> {
    if y != bar.y || bar.width < BUTTON_WIDTH * BUTTONS {
        return None;
    }
    let first = bar.x + bar.width - BUTTON_WIDTH * BUTTONS;
    if x < first {
        return None;
    }
    match (x - first) / BUTTON_WIDTH {
        0 => Some(TitleButton::Minimize),
        1 => Some(TitleButton::Maximize),
        _ => Some(TitleButton::Close),
    }
}

/// Where the buttons start, for drawing them in the same place they are
/// clicked. Sharing this is what stops the two drifting apart.
pub fn title_buttons_origin(bar: Rect) -> u16 {
    bar.x + bar.width.saturating_sub(BUTTON_WIDTH * BUTTONS)
}

/// Panes for the terminal view: a title bar, the screen, and a status line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalPanes {
    pub title: Rect,
    pub screen: Rect,
    pub status: Rect,
}

pub fn terminal(area: Rect) -> TerminalPanes {
    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .split(area);
    TerminalPanes {
        title: rows[0],
        screen: rows[1],
        status: rows[2],
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
    fn the_explorer_gives_every_pane_room_when_there_is_room() {
        let a = area(140, 40);
        let p = explorer(a, true, false);
        assert!(p.tree.is_some());
        assert_eq!(p.tree.unwrap().width, TREE_WIDTH);
        assert_eq!(p.path.width, a.width, "a path deserves the whole width");
        assert_eq!(p.status.height, 1);
        assert!(!p.compact);
    }

    #[test]
    fn the_explorer_tree_gives_way_before_the_editor_does() {
        let a = area(TREE_MIN_COLS - 1, 40);
        let p = explorer(a, true, false);
        assert!(p.tree.is_none());
        assert_eq!(p.editor.width, a.width, "the editor takes the columns");
    }

    #[test]
    fn a_short_terminal_unframes_the_explorer_path_bar() {
        let a = area(120, COMPACT_MAX_ROWS);
        let p = explorer(a, true, false);
        assert!(p.compact);
        assert_eq!(p.path.height, PATH_BARE);

        let tall = explorer(area(120, COMPACT_MAX_ROWS + 1), true, false);
        assert_eq!(
            p.editor.height,
            tall.editor.height + 1,
            "the rows saved must reach the editor"
        );
    }

    #[test]
    fn explorer_panes_never_overlap_and_stay_inside_the_frame() {
        for (w, h) in [(140, 40), (89, 40), (140, 12), (60, 8), (200, 60)] {
            for finding in [false, true] {
                let a = area(w, h);
                let p = explorer(a, true, finding);
                let mut all = vec![
                    ("title", p.title),
                    ("editor", p.editor),
                    ("path", p.path),
                    ("status", p.status),
                ];
                if let Some(t) = p.tree {
                    all.push(("tree", t));
                }
                if let Some(fb) = p.find {
                    all.push(("find", fb));
                }
                for (i, (an, ar)) in all.iter().enumerate() {
                    for (bn, br) in all.iter().skip(i + 1) {
                        assert!(!overlaps(*ar, *br), "{an} overlaps {bn} at {w}x{h}");
                    }
                }
                for (n, r) in &all {
                    assert!(
                        r.x + r.width <= a.x + a.width && r.y + r.height <= a.y + a.height,
                        "{n} escapes the frame at {w}x{h}"
                    );
                }
            }
        }
    }

    #[test]
    fn the_find_bar_takes_its_row_from_the_editor_and_only_when_there_is_one() {
        let a = area(140, 40);
        let without = explorer(a, true, false);
        let with = explorer(a, true, true);
        assert!(without.find.is_none());
        let bar = with.find.expect("a find bar when searching");
        assert_eq!(bar.height, 1);
        assert_eq!(
            with.editor.height + 1,
            without.editor.height,
            "the row comes out of the editor"
        );
        assert_eq!(with.path, without.path, "and never out of the path bar");

        // Too short to spare one: the editor keeps its only row.
        for (w, h) in [(1, 1), (10, 3), (200, 4)] {
            let small = explorer(area(w, h), true, true);
            if small.editor.height <= 1 {
                assert!(small.find.is_none(), "no room, so no bar at {w}x{h}");
            }
        }
    }

    #[test]
    fn an_explorer_in_a_terminal_too_small_for_it_still_produces_valid_panes() {
        // A resize can make the terminal arbitrarily small for one frame, and
        // ratatui panics on a rect that leaves the buffer.
        for (w, h) in [(1, 1), (2, 2), (10, 3), (200, 1)] {
            let a = area(w, h);
            let p = explorer(a, true, false);
            for r in [p.title, p.editor, p.path, p.status] {
                assert!(
                    r.x + r.width <= a.x + a.width && r.y + r.height <= a.y + a.height,
                    "{r:?} escapes {a:?}"
                );
            }
        }
    }

    #[test]
    fn the_title_buttons_are_where_they_are_drawn() {
        let bar = area(80, 1);
        // Rightmost is close, then maximise, then minimise.
        assert_eq!(title_button_at(bar, 79, 0), Some(TitleButton::Close));
        assert_eq!(title_button_at(bar, 75, 0), Some(TitleButton::Close));
        assert_eq!(title_button_at(bar, 74, 0), Some(TitleButton::Maximize));
        assert_eq!(title_button_at(bar, 70, 0), Some(TitleButton::Maximize));
        assert_eq!(title_button_at(bar, 69, 0), Some(TitleButton::Minimize));
        assert_eq!(title_button_at(bar, 65, 0), Some(TitleButton::Minimize));

        // The title itself is not a button.
        assert_eq!(title_button_at(bar, 64, 0), None);
        assert_eq!(title_button_at(bar, 0, 0), None);
        assert_eq!(title_buttons_origin(bar), 65);
    }

    #[test]
    fn a_click_off_the_title_row_is_not_a_button() {
        // Otherwise clicking the first line of output would close the window.
        let bar = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 1,
        };
        assert_eq!(title_button_at(bar, 79, 1), None);
        assert_eq!(title_button_at(bar, 79, 5), None);
    }

    #[test]
    fn a_bar_too_narrow_for_buttons_has_none() {
        // Better nothing than three overlapping hit boxes, one of which
        // closes the window.
        assert_eq!(title_button_at(area(10, 1), 9, 0), None);
    }

    #[test]
    fn the_terminal_view_gives_the_screen_everything_left_over() {
        let a = area(100, 30);
        let p = terminal(a);
        assert_eq!(p.title.height, 1);
        assert_eq!(p.status.height, 1);
        assert_eq!(p.screen.height, 28);
        assert_eq!(p.screen.width, 100);
    }

    #[test]
    fn a_terminal_view_in_a_tiny_area_still_produces_valid_panes() {
        for (w, h) in [(1, 1), (2, 2), (80, 1), (80, 2)] {
            let a = area(w, h);
            let p = terminal(a);
            for r in [p.title, p.screen, p.status] {
                assert!(
                    r.x + r.width <= a.x + a.width && r.y + r.height <= a.y + a.height,
                    "{r:?} escapes {a:?}"
                );
            }
        }
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
