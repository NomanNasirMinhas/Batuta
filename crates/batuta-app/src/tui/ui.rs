//! Rendering for the TUI.
//!
//! `draw` returns the number of result rows that fit, which the event loop
//! feeds back in as the window size. Layout therefore decides how much data
//! gets fetched, rather than the two having to agree by coincidence.
//!
//! Color is not decoration here, it carries reading order: each mode has an
//! accent that tints the frame so you know where you are from the corner of
//! your eye, sizes worth acting on warm toward red, directories are tinted,
//! and the part of each path your keystrokes picked out is bolded inside the
//! result.

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Cell, Clear, Paragraph, Row as TableRow, Table};

use super::app::{App, DupeEntry, Kind, Mode, PendingDiscard, Sort, DUPES_MIN_SIZE};
use super::explorer::find;
use super::explorer::state::{Explorer, Focus};
use super::layout::{self, Panes};
use super::theme::theme;
use crate::fmt;
use batuta_ipc::Row;

/// Secondary text. A function rather than a constant because the value now
/// depends on what the terminal can render; see `theme.rs`.
fn dim() -> Color {
    theme().dim()
}

/// The screen behind everything.
///
/// Foreground and background are always set together: a painted background
/// with an inherited foreground puts a light scheme's black text on our
/// charcoal, so the theme pairs them and this never splits them apart.
fn base_style() -> Style {
    let style = Style::default().fg(theme().text());
    match theme().bg() {
        Some(bg) => style.bg(bg),
        None => style,
    }
}

/// A panel's fill: one step up from the screen behind it.
fn surface_style() -> Style {
    let style = Style::default().fg(theme().text());
    match theme().surface() {
        Some(bg) => style.bg(bg),
        None => style,
    }
}

/// Each mode carries its own accent: the frame's border, its title, the mode
/// pill, and the selected row all tint to it, so the mode you are in is legible
/// before you read anything.
fn mode_accent(mode: Mode) -> Color {
    theme().accent(mode)
}

/// Warm colors for sizes worth acting on.
fn size_color(size: u64) -> Option<Color> {
    theme().heat(size)
}

/// A framed panel in the current theme, so border style is decided in one
/// place rather than at every call site.
fn panel(border: Color) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(theme().border_type())
        .border_style(Style::default().fg(border))
        .style(surface_style())
}

fn size_style(size: u64) -> Style {
    size_color(size).map_or_else(Style::default, |c| Style::default().fg(c))
}

/// The style of the highlighted row: one uniform bar, the strongest signal on
/// the screen. Nothing on a selected row competes with it.
fn selected_style(i: usize, cursor: Option<usize>, accent: Color) -> Style {
    if cursor == Some(i) {
        Style::default().bg(accent).fg(theme().on_accent())
    } else {
        Style::default()
    }
}

/// Draw a frame; returns how many result rows are visible.
pub fn draw(f: &mut Frame, app: &App) -> usize {
    // The explorer is a different screen, not a different table, so it is
    // routed before the query/results layout runs at all.
    if app.mode == Mode::Explore {
        return draw_explorer(f, app);
    }
    if app.mode == Mode::Terminal {
        return draw_terminal(f, app);
    }

    let panes = layout::compute(f.area(), app.rail);

    // Painted first so every pane sits on a known ground rather than on
    // whatever the terminal happened to have behind it.
    f.render_widget(Block::default().style(base_style()), f.area());

    if let Some(rail) = panes.rail {
        draw_rail(f, rail, app);
    }
    draw_query(f, panes.query, app, &panes);
    let visible = draw_results(f, panes.results, app);
    draw_status(f, panes.status, app);

    // Overlays last, so they sit above the list rather than being drawn over.
    // `dupes_pending` starts true so the first visit to that view scans, so it
    // has to be paired with the mode or the panel appears over every screen.
    if app.mode == Mode::Dupes && app.dupes_pending {
        draw_scanning(f, panes.results);
    }
    if let Some(pending) = &app.confirm_delete {
        draw_delete_confirm(f, f.area(), pending);
    }
    visible
}

/// The explorer: a tree, an editor, and a path bar.
///
/// Returns how many editor lines fit, which the event loop feeds back in the
/// same way the result list's row count is.
fn draw_explorer(f: &mut Frame, app: &App) -> usize {
    let accent = mode_accent(Mode::Explore);
    let Some(x) = &app.explorer else {
        // Nothing to lay out against; the frame still needs its background.
        f.render_widget(Block::default().style(base_style()), f.area());
        return 0;
    };
    let panes = layout::explorer(f.area(), true, x.find.open);
    f.render_widget(Block::default().style(base_style()), f.area());

    let title = x
        .open_path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| x.tree.root().display().to_string());
    draw_title_bar(f, panes.title, &title, accent);

    if let Some(area) = panes.tree {
        draw_tree(f, area, x, accent);
    }
    let visible = draw_editor(f, panes.editor, x, accent);
    if let Some(area) = panes.find {
        draw_find_bar(f, area, x, accent);
    }
    draw_path_bar(f, panes.path, x, accent, panes.compact);
    draw_explore_status(f, panes.status, app, x);

    if let Some(pending) = &app.confirm_discard {
        draw_discard_confirm(f, f.area(), pending);
    }
    visible
}

/// A pane's frame: the mode accent when it has the keyboard, dim when it does
/// not. Focus has to be visible without reading anything.
fn pane(focused: bool, accent: Color, title: &str) -> Block<'static> {
    let border = if focused { accent } else { theme().border() };
    panel(border).title_top(Span::styled(
        format!(" {title} "),
        Style::default().fg(border).add_modifier(Modifier::BOLD),
    ))
}

fn draw_tree(f: &mut Frame, area: Rect, x: &Explorer, accent: Color) {
    let focused = x.focus == Focus::Tree;
    let inner = area.height.saturating_sub(2) as usize;
    let start = x.tree.window_start.min(x.tree.selected);
    let start = if x.tree.selected >= start + inner.max(1) {
        x.tree.selected + 1 - inner.max(1)
    } else {
        start
    };

    let lines: Vec<Line> = x
        .rows
        .iter()
        .enumerate()
        .skip(start)
        .take(inner)
        .map(|(i, row)| {
            let selected = i == x.tree.selected;
            let style = if selected && focused {
                Style::default().bg(accent).fg(theme().on_accent())
            } else if row.is_dir {
                Style::default().fg(accent)
            } else {
                Style::default().fg(theme().text())
            };

            if let Some(why) = &row.error {
                // An unreadable directory drawn as an empty one sends people
                // hunting for files that are right there.
                return Line::from(Span::styled(
                    format!("{}  <{why}>", "  ".repeat(row.depth)),
                    Style::default().fg(theme().warn()),
                ));
            }
            let marker = if !row.is_dir {
                "  "
            } else if row.expanded {
                "\u{25be} "
            } else {
                "\u{25b8} "
            };
            Line::from(Span::styled(
                format!("{}{marker}{}", "  ".repeat(row.depth), row.name),
                style,
            ))
        })
        .collect();

    f.render_widget(
        Paragraph::new(lines).block(pane(focused, accent, "tree")),
        area,
    );
}

fn draw_editor(f: &mut Frame, area: Rect, x: &Explorer, accent: Color) -> usize {
    let focused = x.focus == Focus::Editor;
    let visible = area.height.saturating_sub(2) as usize;

    let title = match (&x.doc, &x.notice) {
        (Some(doc), _) => {
            let name = doc.path.display().to_string();
            if doc.buffer.modified() {
                format!("{name}  \u{2022} modified")
            } else {
                name
            }
        }
        (None, Some(_)) => "cannot edit".to_string(),
        (None, None) => "no file open".to_string(),
    };
    let block = pane(focused, accent, &title);

    let Some(doc) = &x.doc else {
        let msg = x
            .notice
            .clone()
            .unwrap_or_else(|| "Pick a file in the tree, or type a path below.".to_string());
        f.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(msg, Style::default().fg(theme().warn())))
                    .alignment(Alignment::Center),
                Line::raw(""),
                Line::from(Span::styled(
                    "Shift+Enter opens it in its default application",
                    Style::default().fg(dim()),
                ))
                .alignment(Alignment::Center),
            ])
            .block(block),
            area,
        );
        return visible;
    };

    // Line numbers are width-matched to the file, so the text does not shift
    // sideways as you scroll past line 100.
    let width = doc.buffer.len().to_string().len();
    let caret = doc.buffer.cursor();

    let lines: Vec<Line> = doc
        .buffer
        .lines()
        .iter()
        .enumerate()
        .skip(x.top_line)
        .take(visible)
        .map(|(n, text)| {
            let here = n == caret.line;
            let mut spans = vec![Span::styled(
                format!("{:>width$} ", n + 1),
                Style::default().fg(if here { accent } else { theme().dim() }),
            )];
            spans.extend(highlight_hits(text, n, x, accent));
            Line::from(spans)
        })
        .collect();

    f.render_widget(Paragraph::new(lines).block(block), area);

    // The real terminal caret, not a drawn glyph: it survives the terminal's
    // own selection rendering and is what assistive tooling follows.
    if focused {
        let row = caret.line.saturating_sub(x.top_line) as u16;
        let col = (width as u16 + 1) + caret.col as u16;
        if row < visible as u16 && col < area.width.saturating_sub(2) {
            f.set_cursor_position((area.x + 1 + col, area.y + 1 + row));
        }
    }
    visible
}

/// One line of the editor, with any search hits picked out.
///
/// The same treatment the result list gives matched terms: showing *why* a line
/// is interesting beats making the reader find it again by eye. The hit the
/// cursor is on is drawn differently from the rest, so stepping with `F3` is
/// visible rather than inferred from the cursor alone.
fn highlight_hits<'a>(text: &'a str, line: usize, x: &Explorer, accent: Color) -> Vec<Span<'a>> {
    let plain = Style::default().fg(theme().text());
    let hits = find::on_line(&x.find.hits, line);
    if hits.is_empty() || x.find.query.is_empty() {
        return vec![Span::styled(text.to_string(), plain)];
    }

    let current = x.find.cursor();
    let len = x.find.query.chars().count();
    let chars: Vec<char> = text.chars().collect();

    let mut spans = Vec::new();
    let mut at = 0usize;
    for hit in hits {
        // Guard rather than slice blindly: the buffer can have been edited
        // since the hits were found, and `panic = "abort"` in release makes a
        // stale index fatal rather than merely wrong.
        let start = hit.col.min(chars.len());
        let end = (hit.col + len).min(chars.len());
        if start < at {
            continue;
        }
        if start > at {
            spans.push(Span::styled(
                chars[at..start].iter().collect::<String>(),
                plain,
            ));
        }
        let style = if current == Some(*hit) {
            Style::default().bg(accent).fg(theme().on_accent())
        } else {
            Style::default()
                .fg(accent)
                .add_modifier(Modifier::UNDERLINED)
        };
        spans.push(Span::styled(
            chars[start..end].iter().collect::<String>(),
            style,
        ));
        at = end;
    }
    if at < chars.len() {
        spans.push(Span::styled(chars[at..].iter().collect::<String>(), plain));
    }
    spans
}

/// The find prompt: what is being searched for, and how it is going.
fn draw_find_bar(f: &mut Frame, area: Rect, x: &Explorer, accent: Color) {
    let tally = x.find.tally();
    let line = Line::from(vec![
        Span::styled(
            " find ",
            Style::default().bg(accent).fg(theme().on_accent()),
        ),
        Span::raw(" "),
        Span::styled(x.find.query.clone(), Style::default().fg(theme().text())),
        Span::raw("  "),
        Span::styled(
            tally,
            Style::default().fg(if x.find.hits.is_empty() {
                theme().warn()
            } else {
                dim()
            }),
        ),
    ]);
    f.render_widget(Paragraph::new(line), area);

    // The caret belongs in the prompt while it has the keyboard, not in the
    // text being searched.
    let col = 7 + x.find.caret as u16;
    if col < area.width {
        f.set_cursor_position((area.x + col, area.y));
    }
}

fn draw_path_bar(f: &mut Frame, area: Rect, x: &Explorer, accent: Color, compact: bool) {
    let focused = x.focus == Focus::Path;
    let text = Line::from(Span::styled(
        x.path_text.clone(),
        Style::default().fg(theme().text()),
    ));

    if compact {
        f.render_widget(Paragraph::new(text), area);
    } else {
        f.render_widget(
            Paragraph::new(text).block(pane(focused, accent, "path")),
            area,
        );
    }

    if focused {
        let col = x.path_caret as u16;
        let (bx, by) = if compact {
            (area.x, area.y)
        } else {
            (area.x + 1, area.y + 1)
        };
        if col < area.width.saturating_sub(2) {
            f.set_cursor_position((bx + col, by));
        }
    }
}

fn draw_explore_status(f: &mut Frame, area: Rect, app: &App, x: &Explorer) {
    let mut left = Vec::new();
    if !app.status.is_empty() {
        left.push(Span::styled(
            app.status.clone(),
            Style::default().fg(theme().warn()),
        ));
    } else if let Some(doc) = &x.doc {
        let caret = doc.buffer.cursor();
        left.push(Span::styled(
            format!("line {} col {}", caret.line + 1, caret.col + 1),
            Style::default().add_modifier(Modifier::BOLD),
        ));
        left.push(Span::styled(
            format!("  ·  {} lines", fmt::count(doc.buffer.len() as u64)),
            Style::default().fg(dim()),
        ));
        left.push(Span::styled(
            format!("  ·  {}", doc.buffer.dominant().label()),
            Style::default().fg(dim()),
        ));
        if doc.buffer.mixed_endings() {
            // Worth saying out loud: it changes what a save looks like in a
            // diff, and it is invisible otherwise.
            left.push(Span::styled(
                "  ·  mixed endings",
                Style::default().fg(theme().warn()),
            ));
        }
        if doc.buffer.modified() {
            left.push(Span::styled(
                "  ·  unsaved",
                Style::default().fg(theme().danger()),
            ));
        }
    } else {
        left.push(Span::styled(
            x.tree.root().display().to_string(),
            Style::default().fg(dim()),
        ));
    }

    // Naming a key that would do nothing teaches the wrong thing, so undo
    // and redo appear only when there is something to undo or redo.
    let undo = x.doc.as_ref().is_some_and(|d| d.buffer.can_undo());
    let redo = x.doc.as_ref().is_some_and(|d| d.buffer.can_redo());
    // The prompt has its own small vocabulary, and the editor's keys do not
    // apply while it is showing.
    if x.find.open {
        let keys = "Enter keep  Esc cancel  \u{2191}\u{2193} between matches";
        f.render_widget(Paragraph::new(Line::from(left)), area);
        if area.width >= keys.chars().count() as u16 + 30 {
            f.render_widget(
                Paragraph::new(Span::styled(keys, Style::default().fg(dim())))
                    .alignment(Alignment::Right),
                area,
            );
        }
        return;
    }

    let mut full = String::from("Ctrl+S save");
    if undo {
        full.push_str("  Ctrl+Z undo");
    }
    if redo {
        full.push_str("  Ctrl+Y redo");
    }
    // Not "Esc back": Esc deliberately does nothing here, and a hint naming a
    // key that is ignored is worse than no hint.
    full.push_str("  Ctrl+F find  Shift+Enter open  F5 refresh  Ctrl+E back");
    let some = "Ctrl+S save  Ctrl+F find  Ctrl+E back";

    // Enough for "line 120 col 40  ·  9,999 lines  ·  CRLF  ·  unsaved".
    // The hints give way before the state does: a key you can guess is worth
    // less than being told the file has unsaved changes.
    const STATE_MIN: u16 = 55;

    let keys = [full.as_str(), some, "Ctrl+E back"]
        .into_iter()
        .find(|k| area.width >= k.chars().count() as u16 + STATE_MIN);

    let Some(keys) = keys else {
        f.render_widget(Paragraph::new(Line::from(left)), area);
        return;
    };
    let chunks = Layout::horizontal([
        Constraint::Min(STATE_MIN),
        Constraint::Length(keys.chars().count() as u16),
    ])
    .split(area);

    f.render_widget(Paragraph::new(Line::from(left)), chunks[0]);
    f.render_widget(
        Paragraph::new(Span::styled(keys, Style::default().fg(dim()))).alignment(Alignment::Right),
        chunks[1],
    );
}

/// The unsaved-changes prompt.
///
/// Three outcomes, not two, and — like the delete prompt — `Enter` does
/// nothing at all: it is the key most likely to be hit from habit, and one of
/// these branches throws away work.
fn draw_discard_confirm(f: &mut Frame, area: Rect, pending: &PendingDiscard) {
    let box_area = centered(area, 72, 9);
    f.render_widget(Clear, box_area);

    let text = vec![
        Line::raw(""),
        Line::from(Span::styled(
            "This file has unsaved changes.",
            Style::default()
                .fg(theme().danger())
                .add_modifier(Modifier::BOLD),
        ))
        .alignment(Alignment::Center),
        Line::raw(""),
        Line::from(Span::raw(pending.path.clone())).alignment(Alignment::Center),
        Line::raw(""),
        Line::from(Span::styled(
            "S save and continue    D discard them    Esc stay here",
            Style::default().fg(dim()),
        ))
        .alignment(Alignment::Center),
    ];

    f.render_widget(
        Paragraph::new(text).block(
            panel(theme().danger()).title_top(Span::styled(
                " unsaved changes ",
                Style::default()
                    .fg(theme().danger())
                    .add_modifier(Modifier::BOLD),
            )),
        ),
        box_area,
    );
}

/// The title bar the borderless window no longer has.
///
/// Making the launcher window frameless took away minimise, maximise and
/// close along with the frame. Drawing them back is not decoration: without a
/// title bar there is otherwise no way to get the window out of the way
/// without ending the program.
fn draw_title_bar(f: &mut Frame, area: Rect, title: &str, accent: Color) {
    let base = Style::default().fg(theme().on_accent()).bg(accent);
    f.render_widget(Block::default().style(base), area);

    let left = format!(" {title}");
    f.render_widget(
        Paragraph::new(Span::styled(left, base.add_modifier(Modifier::BOLD))),
        area,
    );

    // Drawn from the same origin the click test uses, so what is pressed is
    // always what was drawn.
    let origin = layout::title_buttons_origin(area);
    if origin > area.x {
        let buttons = Rect {
            x: origin,
            y: area.y,
            width: area.width - (origin - area.x),
            height: 1,
        };
        f.render_widget(
            Paragraph::new(Span::styled("  \u{2500}    \u{25a1}    \u{2715}  ", base)),
            buttons,
        );
    }
}

/// Map a terminal colour onto ratatui's, honouring the theme's palette.
fn ink(c: crate::tui::terminal::grid::Ink, fallback: Color) -> Color {
    use crate::tui::terminal::grid::Ink;
    match c {
        Ink::Default => fallback,
        Ink::Indexed(i) => Color::Indexed(i),
        Ink::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

fn draw_terminal(f: &mut Frame, app: &App) -> usize {
    let panes = layout::terminal(f.area());
    f.render_widget(Block::default().style(base_style()), f.area());

    let accent = mode_accent(Mode::Terminal);
    let Some(term) = &app.terminal else {
        return 0;
    };

    // The shell's own title when it says something, the folder otherwise.
    // PowerShell's default title is the full path to `powershell.exe`, which
    // is the least useful string available and would sit there permanently.
    let title = match term.title() {
        Some(t) if !t.to_ascii_lowercase().ends_with(".exe") => t.to_string(),
        _ => term.cwd.display().to_string(),
    };
    draw_title_bar(f, panes.title, &title, accent);

    // The grid is already a screen: every cell carries its own colour, so
    // this is a transcription rather than a layout.
    let lines: Vec<Line> = term
        .grid
        .visible()
        .iter()
        .take(panes.screen.height as usize)
        .map(|row| {
            let mut spans: Vec<Span> = Vec::new();
            let mut run = String::new();
            let mut style: Option<crate::tui::terminal::grid::Style> = None;

            // Cells are merged into runs of the same style: a span per cell
            // would be tens of thousands of allocations per frame.
            for cell in row.iter().take(panes.screen.width as usize) {
                if style != Some(cell.style) {
                    if let Some(prev) = style {
                        spans.push(Span::styled(std::mem::take(&mut run), to_style(prev)));
                    }
                    style = Some(cell.style);
                }
                run.push(cell.ch);
            }
            if let Some(last) = style {
                spans.push(Span::styled(run, to_style(last)));
            }
            Line::from(spans)
        })
        .collect();

    f.render_widget(Paragraph::new(lines), panes.screen);

    // The real caret, positioned where the program put it.
    if term.grid.cursor_visible && term.grid.view_offset == 0 {
        let (row, col) = term.grid.cursor();
        if (row as u16) < panes.screen.height && (col as u16) < panes.screen.width {
            f.set_cursor_position((panes.screen.x + col as u16, panes.screen.y + row as u16));
        }
    }

    draw_terminal_status(f, panes.status, app, term);
    panes.screen.height as usize
}

fn to_style(s: crate::tui::terminal::grid::Style) -> Style {
    let mut out = Style::default()
        .fg(ink(s.fg, theme().text()))
        .bg(ink(s.bg, theme().bg().unwrap_or(Color::Reset)));
    if s.bold {
        out = out.add_modifier(Modifier::BOLD);
    }
    if s.dim {
        out = out.add_modifier(Modifier::DIM);
    }
    if s.italic {
        out = out.add_modifier(Modifier::ITALIC);
    }
    if s.underline {
        out = out.add_modifier(Modifier::UNDERLINED);
    }
    if s.reverse {
        out = out.add_modifier(Modifier::REVERSED);
    }
    out
}

fn draw_terminal_status(
    f: &mut Frame,
    area: Rect,
    app: &App,
    term: &crate::tui::terminal::session::Session,
) {
    let mut left = Vec::new();
    if !app.status.is_empty() {
        left.push(Span::styled(
            app.status.clone(),
            Style::default().fg(theme().warn()),
        ));
    } else if term.ended {
        // Otherwise a dead shell just looks like a frozen one.
        left.push(Span::styled(
            "the shell has exited - Esc to leave",
            Style::default().fg(theme().warn()),
        ));
    } else {
        left.push(Span::styled(
            term.cwd.display().to_string(),
            Style::default().fg(dim()),
        ));
        if term.grid.view_offset > 0 {
            left.push(Span::styled(
                format!(
                    "  ·  scrolled back {} of {}",
                    term.grid.view_offset,
                    term.grid.scrollback_len()
                ),
                Style::default().fg(theme().warn()),
            ));
        }
    }

    let keys = "keys go to the shell  ·  Ctrl+E explorer  ·  Ctrl+Q quit";
    let chunks = Layout::horizontal([
        Constraint::Min(10),
        Constraint::Length(keys.chars().count() as u16 + 1),
    ])
    .split(area);
    f.render_widget(Paragraph::new(Line::from(left)), chunks[0]);
    f.render_widget(
        Paragraph::new(Span::styled(keys, Style::default().fg(dim()))).alignment(Alignment::Right),
        chunks[1],
    );
}

/// Centre a box of the given size inside `area`.
fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width.saturating_sub(2));
    let h = height.min(area.height.saturating_sub(2));
    Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    }
}

/// A prominent panel while duplicates are being found.
///
/// The scan reads file contents, so it takes seconds rather than the
/// microseconds every other view costs. A status-line note was too easy to
/// miss, and the screen otherwise looks frozen.
fn draw_scanning(f: &mut Frame, area: Rect) {
    // The duplicates view's own colour, so the panel reads as part of it.
    let accent = mode_accent(Mode::Dupes);
    let box_area = centered(area, 56, 7);
    f.render_widget(Clear, box_area);

    let text = vec![
        Line::from(""),
        Line::from(Span::styled(
            "Finding duplicates...",
            Style::default().fg(accent).add_modifier(Modifier::BOLD),
        ))
        .alignment(Alignment::Center),
        Line::from(Span::styled(
            "reading file contents, this takes a few seconds",
            Style::default().fg(dim()),
        ))
        .alignment(Alignment::Center),
    ];

    f.render_widget(
        Paragraph::new(text).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(accent)),
        ),
        box_area,
    );
}

/// The delete confirmation.
///
/// States the full path and, for a directory, how many files go with it. The
/// default is deliberately no: this is the one action in the UI that destroys
/// data, and Enter does nothing at all.
fn draw_delete_confirm(f: &mut Frame, area: Rect, pending: &super::app::PendingDelete) {
    let box_area = centered(area, 76, 11);
    f.render_widget(Clear, box_area);

    let what = if pending.is_dir { "folder" } else { "file" };
    let mut text = vec![
        Line::from(""),
        Line::from(Span::styled(
            format!("Delete this {what}?"),
            Style::default()
                .fg(theme().danger())
                .add_modifier(Modifier::BOLD),
        ))
        .alignment(Alignment::Center),
        Line::from(""),
        Line::from(Span::raw(pending.path.clone())).alignment(Alignment::Center),
    ];

    if pending.is_dir {
        text.push(
            Line::from(Span::styled(
                format!(
                    "{} and everything in it - {} files",
                    fmt::bytes(pending.size),
                    fmt::count(pending.files as u64)
                ),
                Style::default().fg(theme().warn()),
            ))
            .alignment(Alignment::Center),
        );
    } else {
        text.push(
            Line::from(Span::styled(
                fmt::bytes(pending.size),
                Style::default().fg(dim()),
            ))
            .alignment(Alignment::Center),
        );
    }

    text.push(Line::from(""));
    text.push(
        Line::from(Span::styled(
            "This does not go to the Recycle Bin.   Y delete    N cancel",
            Style::default().fg(dim()),
        ))
        .alignment(Alignment::Center),
    );

    f.render_widget(
        Paragraph::new(text).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme().danger()))
                .title(Span::styled(
                    " confirm ",
                    Style::default()
                        .fg(theme().danger())
                        .add_modifier(Modifier::BOLD),
                )),
        ),
        box_area,
    );
}

/// The rail: mode, filter and sort, all three visible at once.
///
/// These were previously reachable only through a chord and reported only as
/// text in the status line, so the state you were in had to be remembered
/// rather than seen. Nothing here is focusable: the query keeps the keyboard
/// at all times, because typing is the primary loop and a focus ring that
/// swallowed keystrokes would be a regression, not a feature.
fn draw_rail(f: &mut Frame, area: Rect, app: &App) {
    let accent = mode_accent(app.mode);
    let mut lines: Vec<Line> = Vec::new();

    // Inside the border. The active row is filled edge to edge, so it reads
    // as a selected item rather than as differently-coloured text.
    let inner = area.width.saturating_sub(2) as usize;

    let section = |lines: &mut Vec<Line>, title: &str, items: Vec<(bool, &'static str)>| {
        if !lines.is_empty() {
            lines.push(Line::raw(""));
        }
        lines.push(Line::from(Span::styled(
            format!(" {title}"),
            Style::default().fg(dim()).add_modifier(Modifier::BOLD),
        )));
        for (active, label) in items {
            // The marker carries the state as well as the colour does, so the
            // rail still reads correctly with no colour at all.
            let text = format!("{} {label}", if active { "\u{25b8}" } else { " " });
            let style = if active {
                Style::default()
                    .bg(accent)
                    .fg(theme().on_accent())
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(dim())
            };
            // Padded so the fill spans the pane; a bar that stopped at the end
            // of the word would look like a highlight, not a selection.
            let padded = format!("{text:<inner$}");
            lines.push(Line::from(Span::styled(padded, style)));
        }
    };

    section(
        &mut lines,
        "MODE",
        vec![
            (app.mode == Mode::Search, "SEARCH"),
            (app.mode == Mode::Bloat, "BLOAT"),
            (app.mode == Mode::Dupes, "DUPES"),
        ],
    );
    section(
        &mut lines,
        "SHOW",
        vec![
            (app.kind == Kind::All, "ALL"),
            (app.kind == Kind::DirsOnly, "DIRS"),
            (app.kind == Kind::FilesOnly, "FILES"),
        ],
    );
    section(
        &mut lines,
        "SORT",
        vec![
            (app.sort == Sort::Name, "NAME"),
            (app.sort == Sort::Size, "SIZE"),
            (app.sort == Sort::Modified, "MODIFIED"),
        ],
    );

    f.render_widget(Paragraph::new(lines).block(panel(theme().border())), area);
}

fn draw_query(f: &mut Frame, area: Rect, app: &App, panes: &Panes) {
    let accent = mode_accent(app.mode);

    let title = match app.mode {
        Mode::Search => " batuta · search ",
        Mode::Bloat => " batuta · bloat (largest directories) ",
        Mode::Dupes => " batuta · duplicates (byte-identical files) ",
        Mode::Explore => " batuta · explore ",
        Mode::Terminal => " batuta · terminal ",
    };

    let prompt = match app.mode {
        Mode::Search => Line::from(query_spans(app, accent)),
        Mode::Bloat => Line::from(vec![Span::styled(
            "ranked by rolled-up size — the bar shows each directory's share of the largest",
            Style::default().fg(dim()),
        )]),
        Mode::Dupes => Line::from(vec![Span::styled(
            "each file with the paths holding identical copies — F5 rescan, Shift+Tab modes",
            Style::default().fg(dim()),
        )]),
        Mode::Explore | Mode::Terminal => Line::from(Span::raw("")),
    };

    // A short terminal cannot afford two rows of frame around one row of
    // text, and the title only labels something already obvious.
    if panes.compact {
        f.render_widget(Paragraph::new(prompt), area);
        return;
    }

    let mut block = panel(accent).title_top(Span::styled(
        title,
        Style::default().fg(accent).add_modifier(Modifier::BOLD),
    ));
    // The rail names the mode in full; pills would be the same fact twice.
    if panes.rail.is_none() {
        block = block.title_top(mode_pills(app.mode).alignment(Alignment::Right));
    }

    f.render_widget(Paragraph::new(prompt).block(block), area);
}

/// The typed query: the part of a path already settled dims, the component
/// still being typed carries the eye, and the caret block marks the end.
fn query_spans(app: &App, accent: Color) -> Vec<Span<'static>> {
    let mut spans = vec![Span::styled("› ", Style::default().fg(accent))];
    match app.query.rsplit_once(['\\', '/']) {
        Some((dir, tail)) => {
            spans.push(Span::styled(format!("{dir}\\"), Style::default().fg(dim())));
            spans.push(Span::styled(
                tail.to_owned(),
                Style::default().add_modifier(Modifier::BOLD),
            ));
        }
        None => spans.push(Span::raw(app.query.clone())),
    }
    spans.push(Span::styled("█", Style::default().fg(accent)));
    spans
}

/// The right-hand title: every mode as a pill, the one you are in lit in its
/// own accent. The mode is visible in the tint of everything else already;
/// this is the explicit answer when you look for it.
fn mode_pills(mode: Mode) -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];
    for (i, (m, label)) in [
        (Mode::Search, "SEARCH"),
        (Mode::Bloat, "BLOAT"),
        (Mode::Dupes, "DUPES"),
    ]
    .into_iter()
    .enumerate()
    {
        if i > 0 {
            spans.push(Span::styled(" │ ", Style::default().fg(dim())));
        }
        let style = if m == mode {
            Style::default()
                .fg(mode_accent(mode))
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(dim())
        };
        spans.push(Span::styled(format!(" {label} "), style));
    }
    spans.push(Span::raw(" "));
    Line::from(spans)
}

fn draw_results(f: &mut Frame, area: Rect, app: &App) -> usize {
    // Two border rows and one header row are not available for results.
    let visible = area.height.saturating_sub(3) as usize;
    let accent = mode_accent(app.mode);
    let cursor = app.cursor_in_window();

    // The bloat view earns a fourth column — a share bar — so the header,
    // widths, and rows all come per mode rather than from one shared table.
    let (header, widths, rows): (Vec<&str>, Vec<Constraint>, Vec<TableRow<'static>>) =
        match app.mode {
            Mode::Search => (
                vec!["SIZE", "MODIFIED", "PATH"],
                vec![
                    Constraint::Length(10),
                    Constraint::Length(16),
                    Constraint::Min(20),
                ],
                app.rows
                    .iter()
                    .enumerate()
                    .map(|(i, r)| search_row(i, r, cursor, accent, app.needle()))
                    .collect(),
            ),
            Mode::Bloat => (
                vec!["TOTAL", "OWN", "SHARE", "PATH"],
                vec![
                    Constraint::Length(10),
                    Constraint::Length(10),
                    Constraint::Length(12),
                    Constraint::Min(20),
                ],
                {
                    // The bar is relative to the largest directory on screen, so
                    // it stays meaningful as you scroll.
                    let max = app.rows.iter().map(|r| r.size).max().unwrap_or(1).max(1);
                    app.rows
                        .iter()
                        .enumerate()
                        .map(|(i, r)| bloat_row(i, r, cursor, accent, max))
                        .collect()
                },
            ),
            // Never drawn: `draw` sends Explore to its own screen. Present
            // so the match stays total.
            Mode::Explore | Mode::Terminal | Mode::Dupes => (
                vec!["SIZE", "COPIES", "PATH"],
                vec![
                    Constraint::Length(10),
                    Constraint::Length(8),
                    Constraint::Min(20),
                ],
                app.dupes_window
                    .iter()
                    .enumerate()
                    .map(|(i, e)| dupe_row(i, e, cursor, accent))
                    .collect(),
            ),
        };

    let loaded = match app.mode {
        Mode::Dupes => app.dupes_window.len(),
        _ => app.rows.len(),
    };
    if loaded == 0 {
        let block = panel(theme().border());
        let msg = match app.mode {
            Mode::Search if app.query.is_empty() => "type to search".to_string(),
            Mode::Dupes if !app.status.is_empty() => String::new(),
            Mode::Dupes => {
                format!(
                    "no duplicate files at or above {}",
                    fmt::bytes(DUPES_MIN_SIZE)
                )
            }
            _ => "no matches".to_string(),
        };
        f.render_widget(
            Paragraph::new(Span::styled(msg, Style::default().fg(dim())))
                .alignment(Alignment::Center)
                .block(block),
            area,
        );
        return visible;
    }

    // Position belongs next to the list, not twenty rows away in the status
    // line where you have to go looking for it.
    let block = panel(theme().border()).title_top(
        Line::from(Span::styled(
            format!(" {} of {} ", app.selected + 1, fmt::count(app.total)),
            Style::default().fg(dim()),
        ))
        .alignment(Alignment::Right),
    );

    let table = Table::new(rows, widths)
        .header(
            TableRow::new(header.into_iter().map(Cell::from))
                .style(Style::default().fg(accent).add_modifier(Modifier::BOLD)),
        )
        .block(block)
        .column_spacing(2);

    f.render_widget(table, area);
    visible
}

/// A search result. The query's match is bolded inside the path; a selected
/// row is a single uniform bar instead.
fn search_row(
    i: usize,
    r: &Row,
    cursor: Option<usize>,
    accent: Color,
    needle: &str,
) -> TableRow<'static> {
    let selected = cursor == Some(i);
    let base = selected_style(i, cursor, accent);
    TableRow::new(vec![
        Cell::from(Text::from(fmt::bytes(r.size)).alignment(Alignment::Right)).style(if selected {
            base
        } else {
            size_style(r.size)
        }),
        Cell::from(fmt::timestamp(r.mtime)).style(if selected {
            base
        } else {
            Style::default().fg(dim())
        }),
        Cell::from(Text::from(Line::from(path_spans(
            &r.path, needle, selected, accent, r.is_dir,
        )))),
    ])
}

/// A path cell split around the query's match: what the keystrokes picked out
/// is bolded in the accent color, inside the result, where it actually is.
/// Directories stay tinted throughout; a selected row keeps its plain bar,
/// because the selection is already the loudest thing on the line.
fn path_spans(
    path: &str,
    needle: &str,
    selected: bool,
    accent: Color,
    is_dir: bool,
) -> Vec<Span<'static>> {
    let base = if selected {
        Style::default().bg(accent).fg(theme().on_accent())
    } else if is_dir {
        Style::default().fg(accent)
    } else {
        Style::default()
    };
    if selected || needle.is_empty() {
        return vec![Span::styled(path.to_owned(), base)];
    }

    let highlighted = Style::default().fg(accent).add_modifier(Modifier::BOLD);
    let plain = || vec![Span::styled(path.to_owned(), base)];

    // Every occurrence of every term. A query of several terms matches across
    // the whole path rather than any one name, so picking out only the first
    // hit would leave most of the reason a row matched invisible.
    let mut ranges = occurrences(path.as_bytes(), needle);
    if ranges.is_empty() {
        return plain();
    }
    ranges.sort_unstable();

    // Terms overlap freely: one can sit inside another's hit, and two can
    // share a prefix. Merging first leaves the spans ordered and disjoint,
    // which is what rendering them requires.
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(ranges.len());
    for (start, end) in ranges {
        match merged.last_mut() {
            Some(last) if start <= last.1 => last.1 = last.1.max(end),
            _ => merged.push((start, end)),
        }
    }

    // ASCII comparison cannot land inside a multi-byte character, but slicing
    // is the one place a bug here would panic. If a boundary is not a
    // character boundary the whole path is drawn unhighlighted rather than
    // dropping the piece that failed: losing text from a path is a worse
    // outcome than losing its emphasis.
    let mut spans = Vec::with_capacity(merged.len() * 2 + 1);
    let mut cursor = 0usize;
    for (start, end) in merged {
        let (Some(before), Some(hit)) = (path.get(cursor..start), path.get(start..end)) else {
            return plain();
        };
        if !before.is_empty() {
            spans.push(Span::styled(before.to_owned(), base));
        }
        spans.push(Span::styled(hit.to_owned(), highlighted));
        cursor = end;
    }
    match path.get(cursor..) {
        Some(rest) => {
            if !rest.is_empty() {
                spans.push(Span::styled(rest.to_owned(), base));
            }
            spans
        }
        None => plain(),
    }
}

/// Byte ranges where any term of `needle` appears in `path`, ignoring case.
///
/// Occurrences of a single term never overlap each other — a hit advances past
/// itself — but different terms may, which the caller merges.
fn occurrences(path: &[u8], needle: &str) -> Vec<(usize, usize)> {
    let mut found = Vec::new();
    for term in needle.split_whitespace() {
        let n = term.as_bytes();
        // A term longer than the path cannot match, and must not be searched
        // for: deriving the range with a saturating subtraction collapses it
        // to `0..=0` and then reads `path[0..n.len()]` past the end. That was
        // a real crash, and it is not a corner case — while browsing a path
        // the rows are matched by directory, so a row shorter than what has
        // been typed is ordinary.
        if n.is_empty() || n.len() > path.len() {
            continue;
        }
        let mut i = 0;
        while i + n.len() <= path.len() {
            if path[i..i + n.len()].eq_ignore_ascii_case(n) {
                found.push((i, i + n.len()));
                i += n.len();
            } else {
                i += 1;
            }
        }
    }
    found
}

/// A bloat row: the rolled-up TOTAL in the size colors, OWN dim beside it, and
/// a bar showing each directory's share of the largest one on screen — the
/// relative weight is legible without reading a single number.
fn bloat_row(
    i: usize,
    r: &Row,
    cursor: Option<usize>,
    accent: Color,
    max: u64,
) -> TableRow<'static> {
    let selected = cursor == Some(i);
    let base = selected_style(i, cursor, accent);
    TableRow::new(vec![
        Cell::from(Text::from(fmt::bytes(r.size)).alignment(Alignment::Right)).style(if selected {
            base
        } else {
            size_style(r.size)
        }),
        Cell::from(Text::from(fmt::bytes(r.own)).alignment(Alignment::Right)).style(if selected {
            base
        } else {
            Style::default().fg(dim())
        }),
        Cell::from(Text::from(Line::from(share_bar(
            r.size, max, selected, accent,
        )))),
        Cell::from(r.path.clone()).style(if selected {
            base
        } else {
            Style::default().fg(accent)
        }),
    ])
}

/// Filled blocks in the size's color against dim dots. Small directories use
/// the mode accent rather than the default foreground, so the bar stays
/// visible however cold the color scheme of the terminal is.
fn share_bar(size: u64, max: u64, selected: bool, accent: Color) -> Vec<Span<'static>> {
    const WIDTH: usize = 10;
    let filled = ((size as f64 / max as f64) * WIDTH as f64).round() as usize;
    let filled = filled.min(WIDTH);
    let bar = if selected {
        Style::default().bg(accent).fg(theme().on_accent())
    } else {
        Style::default().fg(size_color(size).unwrap_or(accent))
    };
    let rest = if selected {
        Style::default().bg(accent).fg(theme().on_accent())
    } else {
        Style::default().fg(dim())
    };
    vec![
        Span::styled("█".repeat(filled), bar),
        Span::styled("·".repeat(WIDTH - filled), rest),
    ]
}

/// One line of the duplicates view.
///
/// The group divider carries the mapping the mode exists for — what the files
/// share and what deleting the extras would reclaim — drawn as a dim rule with
/// the reclaim figure in red: it is the number you came here for.
fn dupe_row(
    i: usize,
    entry: &DupeEntry,
    cursor: Option<usize>,
    accent: Color,
) -> TableRow<'static> {
    let selected = cursor == Some(i);
    let base = selected_style(i, cursor, accent);
    let dim = if selected {
        base
    } else {
        Style::default().fg(dim())
    };
    match entry {
        DupeEntry::Group {
            size,
            copies,
            wasted,
        } => TableRow::new(vec![
            Cell::from(Text::from(fmt::bytes(*size)).alignment(Alignment::Right)).style(dim),
            Cell::from("").style(base),
            Cell::from(Text::from(Line::from(vec![
                Span::styled(
                    format!("── {copies} copies · each {} ·", fmt::bytes(*size)),
                    dim,
                ),
                Span::styled(
                    format!(" reclaim {} ", fmt::bytes(*wasted)),
                    if selected {
                        base
                    } else {
                        Style::default().fg(theme().danger())
                    },
                ),
                Span::styled("──", dim),
            ]))),
        ]),
        DupeEntry::Copy { row, copies } => TableRow::new(vec![
            Cell::from(Text::from(fmt::bytes(row.size)).alignment(Alignment::Right))
                .style(if selected { base } else { size_style(row.size) }),
            Cell::from(Text::from(format!("{copies}")).alignment(Alignment::Right)).style(dim),
            Cell::from(row.path.clone()).style(base),
        ]),
    }
}

fn draw_status(f: &mut Frame, area: Rect, app: &App) {
    let accent = mode_accent(app.mode);
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let subtle = Style::default().fg(dim());

    let mut left = Vec::new();

    if !app.status.is_empty() {
        left.push(Span::styled(
            app.status.clone(),
            Style::default().fg(theme().warn()),
        ));
    } else if app.mode == Mode::Dupes {
        let groups = app.dupe_group_count();
        left.push(Span::styled(fmt::count(groups as u64), bold));
        left.push(Span::raw(format!(
            " group{}",
            if groups == 1 { "" } else { "s" }
        )));
        left.push(Span::styled(
            format!("  ·  {} reclaimable", fmt::bytes(app.dupes_wasted)),
            Style::default().fg(theme().danger()),
        ));
        if app.total > 0 {
            left.push(Span::styled(
                format!("  ·  {} of {}", app.selected + 1, fmt::count(app.total)),
                subtle,
            ));
        }
        if app.dupes_elapsed_us > 0 {
            left.push(Span::styled(
                format!("  ·  {:.2}s", app.dupes_elapsed_us as f64 / 1_000_000.0),
                subtle,
            ));
        }
        left.push(Span::styled(
            if app.live {
                "  ·  live"
            } else {
                "  ·  cached"
            },
            Style::default().fg(if app.live {
                theme().ok()
            } else {
                theme().warn()
            }),
        ));
    } else {
        left.push(Span::styled(fmt::count(app.total), bold));
        left.push(Span::raw(format!(
            " match{}",
            if app.total == 1 { "" } else { "es" }
        )));
        if app.total > 0 {
            left.push(Span::styled(
                format!("  ·  {} of {}", app.selected + 1, fmt::count(app.total)),
                subtle,
            ));
        }
        if app.elapsed_us > 0 {
            left.push(Span::styled(
                format!("  ·  {:.2}ms", app.elapsed_us as f64 / 1000.0),
                subtle,
            ));
        }
        left.push(Span::styled(
            if app.live {
                "  ·  live"
            } else {
                "  ·  cached"
            },
            Style::default().fg(if app.live {
                theme().ok()
            } else {
                theme().warn()
            }),
        ));
    }

    // Appended after whichever branch above ran, so it shows in every mode and
    // alongside a status message rather than being replaced by one. Worth
    // saying, not worth interrupting anyone over: it is the same weight as the
    // counters beside it, and it names the version so the reader can decide
    // whether they care.
    if let Some(version) = app.update {
        left.push(Span::styled(
            format!("  ·  {version} available"),
            Style::default().fg(theme().warn()),
        ));
    }

    let state = format!("sort:{}  show:{}", app.sort.label(), app.kind.label());
    let full = format!("{state}  │ Tab complete  Shift+Tab modes  Ctrl+S sort  Ctrl+T filter  Ctrl+B rail  Ctrl+D dupes  Ctrl+E explore  Enter reveal  Shift+Enter open  Esc close");
    // Two middle tiers, so the hints thin out a rung at a time rather than
    // falling off a cliff from everything to nothing. The rungs drop what is
    // most guessable first: the rail and the duplicates shortcut before the
    // sort and filter keys, and those before the two ways to act on a row.
    //
    // Enter and Shift+Enter always appear together. Showing one without the
    // other reads as though it were the only way to act on a row, which is
    // exactly the wrong thing to teach.
    let some = format!("{state}  │ Tab complete  Shift+Tab modes  Ctrl+S sort  Ctrl+T filter  Enter reveal  Shift+Enter open  Esc close");
    let core =
        format!("{state}  │ Ctrl+S sort  Ctrl+T filter  Enter reveal  Shift+Enter open  Esc close");
    let short = format!("{state}  │ Esc close");

    // The status text is what the user needs; hints are a courtesy. Rather
    // than dropping every hint the moment the full set stops fitting, step
    // down a rung at a time, then to the one key that gets them out, then to
    // nothing.
    //
    // The room reserved for the counters is measured rather than guessed. A
    // fixed reserve was too small for a line like "4,204 matches · 1 of 4,204
    // · 76.00ms · cached", so the counters were cut off mid-word and ran
    // straight into the hints with no gap.
    //
    // Width is counted in characters, not bytes: these strings contain
    // multi-byte glyphs, and `len()` would over-reserve the right-hand column
    // and push the counters off screen.
    let left_width: usize = left.iter().map(|s| s.content.chars().count()).sum();
    /// Columns between the counters and the hints, so the two never touch.
    const GAP: usize = 2;

    let keys = [full, some, core, short]
        .into_iter()
        .find(|k| area.width as usize >= left_width + GAP + k.chars().count());

    let Some(keys) = keys else {
        f.render_widget(Paragraph::new(Line::from(left)), area);
        return;
    };
    let keys_width = keys.chars().count() as u16;

    let chunks = Layout::horizontal([
        Constraint::Min((left_width + GAP) as u16),
        Constraint::Length(keys_width),
    ])
    .split(area);

    f.render_widget(
        Paragraph::new(Line::from(left)).style(Style::default().fg(accent)),
        chunks[0],
    );
    f.render_widget(
        Paragraph::new(Span::styled(keys, Style::default().fg(dim()))).alignment(Alignment::Right),
        chunks[1],
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::{App, Kind, Sort};
    use batuta_ipc::Row;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn rows(n: usize) -> Vec<Row> {
        (0..n)
            .map(|i| Row {
                path: format!(r"C:\Users\Hacker\Downloads\item_{i:03}.bin"),
                size: (i as u64 + 1) * 1024 * 1024,
                mtime: 1_788_134_400,
                is_dir: i % 4 == 0,
                files: 12,
                own: 4096,
            })
            .collect()
    }

    /// Render one frame and return the screen as text plus the visible count.
    fn render(app: &App, w: u16, h: u16) -> (String, usize) {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        let mut visible = 0;
        terminal.draw(|f| visible = draw(f, app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let text = (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        (text, visible)
    }

    #[test]
    fn the_visible_count_matches_the_rows_actually_drawn() {
        // The event loop uses this number to decide how many rows to fetch, so
        // if it disagreed with the layout the list would under- or over-fill.
        let mut app = App::default();
        app.apply_rows(rows(40), 40, 0, 40);

        for height in [10u16, 24, 50] {
            let (_, visible) = render(&app, 100, height);
            // Derived from the layout rather than restated, because the query
            // box is not a fixed height any more: a short terminal drops its
            // frame and hands those rows back to the results.
            let panes = layout::compute(
                Rect {
                    x: 0,
                    y: 0,
                    width: 100,
                    height,
                },
                app.rail,
            );
            // The results pane spends two rows on its border and one on the
            // column header.
            let expected = panes.results.height.saturating_sub(3) as usize;
            assert_eq!(visible, expected, "wrong visible count at height {height}");
        }
    }

    #[test]
    fn an_available_update_is_named_in_the_status_line() {
        let mut app = App {
            total: 3,
            ..Default::default()
        };
        app.update = Some(crate::update::Version {
            major: 0,
            minor: 2,
            patch: 0,
        });
        let (screen, _) = render(&app, 160, 20);
        assert!(
            screen.contains("v0.2.0 available"),
            "the version has to be named, not just hinted at:\n{screen}"
        );
    }

    #[test]
    fn nothing_is_said_when_there_is_no_update() {
        // The check failing, or finding nothing, must look exactly like the
        // feature not existing. A "checking..." or "up to date" line would be
        // noise on every launch forever.
        let app = App {
            total: 3,
            ..Default::default()
        };
        let (screen, _) = render(&app, 160, 20);
        assert!(!screen.contains("available"), "{screen}");
    }

    #[test]
    fn the_update_survives_a_status_message_and_every_mode() {
        // It is appended after the branch that builds the counters, so a
        // status message must not replace it and Dupes must not lose it.
        for mode in [Mode::Search, Mode::Bloat, Mode::Dupes] {
            let mut app = App {
                mode,
                total: 3,
                status: "connected to daemon (live index)".into(),
                ..Default::default()
            };
            app.update = Some(crate::update::Version {
                major: 9,
                minor: 9,
                patch: 9,
            });
            let (screen, _) = render(&app, 160, 20);
            assert!(screen.contains("v9.9.9 available"), "{mode:?}:\n{screen}");
        }
    }

    fn explorer_fixture() -> App {
        use crate::tui::explorer::buffer::Buffer;
        use crate::tui::explorer::state::{Doc, Explorer};
        use crate::tui::explorer::tree::Row as TreeRow;
        use std::path::PathBuf;

        let mut x = Explorer::new(PathBuf::from(r"D:\Code\Batuta"));
        let mk = |p: &str, name: &str, depth: usize, is_dir: bool, expanded: bool| TreeRow {
            path: PathBuf::from(p),
            name: name.into(),
            depth,
            is_dir,
            expanded,
            error: None,
        };
        x.rows = vec![
            mk(r"D:\Code\Batuta\crates", "crates", 0, true, true),
            mk(r"D:\Code\Batuta\crates\app", "app", 1, true, false),
            mk(r"D:\Code\Batuta\crates\core", "core", 1, true, false),
            mk(r"D:\Code\Batuta\src", "src", 0, true, true),
            mk(r"D:\Code\Batuta\src\main.rs", "main.rs", 1, false, false),
            mk(r"D:\Code\Batuta\README.md", "README.md", 0, false, false),
            mk(r"D:\Code\Batuta\Cargo.toml", "Cargo.toml", 0, false, false),
        ];
        x.tree.selected = 4;
        x.path_text = r"D:\Code\Batuta\src\main.rs".into();

        let mut buffer = Buffer::from_str(
            "use std::fs;\n\nfn main() {\n    let path = \"notes.txt\";\n    \
             println!(\"{path}\");\n}\n",
        );
        buffer.goto(crate::tui::explorer::buffer::Cursor::new(3, 8));
        buffer.insert_char('!');
        x.doc = Some(Doc {
            path: PathBuf::from(r"D:\Code\Batuta\src\main.rs"),
            buffer,
            stamp: None,
        });
        x.focus = crate::tui::explorer::state::Focus::Editor;

        App {
            mode: Mode::Explore,
            explorer: Some(x),
            ..Default::default()
        }
    }

    #[test]
    fn the_title_bar_carries_the_window_controls() {
        // The launcher window is borderless, so these are the only way to
        // minimise or close it without ending the program from the keyboard.
        let app = explorer_fixture();
        let (screen, _) = render(&app, 100, 20);
        let bar = screen.lines().next().expect("a title row");

        assert!(bar.contains('\u{2500}'), "minimise missing: {bar}");
        assert!(bar.contains('\u{25a1}'), "maximise missing: {bar}");
        assert!(bar.contains('\u{2715}'), "close missing: {bar}");
        assert!(bar.contains("main.rs"), "the title itself is missing");
    }

    #[test]
    fn the_explorer_draws_its_three_panes() {
        let app = explorer_fixture();
        let (screen, visible) = render(&app, 118, 22);

        assert!(screen.contains("tree"), "tree pane missing");
        assert!(screen.contains("path"), "path bar missing");
        assert!(screen.contains("main.rs"), "the open file should be named");
        assert!(screen.contains("use std::fs;"), "file contents missing");
        assert!(screen.contains("crates"), "tree contents missing");
        assert!(
            screen.contains("modified"),
            "an unsaved buffer has to say so"
        );
        assert!(visible > 0, "the editor must report how many lines fit");
    }

    #[test]
    fn the_find_bar_shows_the_query_and_how_many_it_matched() {
        let mut app = explorer_fixture();
        {
            let x = app.explorer.as_mut().unwrap();
            let lines = x.doc.as_ref().unwrap().buffer.lines().to_vec();
            x.find
                .open(crate::tui::explorer::buffer::Cursor::new(0, 0), &lines);
            for c in "path".chars() {
                x.find.insert(c, &lines);
            }
        }
        let (screen, _) = render(&app, 118, 22);

        assert!(screen.contains("find"), "the prompt should name itself");
        assert!(screen.contains("path"), "the query should be shown");
        // Two occurrences of "path" in the fixture, so the tally is a count
        // rather than a bare marker.
        assert!(
            screen.contains("/2"),
            "the tally should say how many were found:\n{screen}"
        );
    }

    #[test]
    fn a_search_that_matches_nothing_says_so() {
        let mut app = explorer_fixture();
        {
            let x = app.explorer.as_mut().unwrap();
            let lines = x.doc.as_ref().unwrap().buffer.lines().to_vec();
            x.find
                .open(crate::tui::explorer::buffer::Cursor::new(0, 0), &lines);
            for c in "zzzz".chars() {
                x.find.insert(c, &lines);
            }
        }
        let (screen, _) = render(&app, 118, 22);
        assert!(screen.contains("no matches"), "{screen}");
    }

    #[test]
    fn a_directory_shows_whether_it_is_open() {
        // The marker carries it as well as the colour, so it survives a
        // terminal with no colour at all.
        let app = explorer_fixture();
        let (screen, _) = render(&app, 118, 22);
        assert!(screen.contains("\u{25be} crates"), "an open directory");
        assert!(screen.contains("\u{25b8} app"), "a closed one");
    }

    #[test]
    fn the_editor_keeps_the_columns_when_the_tree_cannot_have_them() {
        let app = explorer_fixture();
        let (screen, _) = render(&app, 80, 22);
        assert!(
            !screen.contains("\u{25be} crates"),
            "the tree should be gone"
        );
        assert!(
            screen.contains("use std::fs;"),
            "the file still has to be readable"
        );
    }

    #[test]
    fn the_status_line_reports_where_the_caret_is_and_what_the_file_uses() {
        let app = explorer_fixture();
        let (screen, _) = render(&app, 118, 22);
        assert!(screen.contains("line 4 col 10"), "caret position missing");
        assert!(screen.contains("LF"), "line ending missing");
        assert!(screen.contains("unsaved"));
    }

    #[test]
    fn the_discard_prompt_offers_three_outcomes_and_never_enter() {
        // Enter is the key most likely to be hit from habit, and one of these
        // branches throws away work.
        let mut app = explorer_fixture();
        app.confirm_discard = Some(crate::tui::app::PendingDiscard {
            path: r"D:\Code\Batuta\src\main.rs".into(),
            then: crate::tui::app::Exit::LeaveExplorer,
        });
        let (screen, _) = render(&app, 118, 22);

        assert!(screen.contains("unsaved changes"));
        assert!(screen.contains("S save"), "saving must be offered");
        assert!(screen.contains("D discard"), "discarding must be offered");
        assert!(screen.contains("Esc stay here"), "so must backing out");
        // Scoped to the dialog's own line: the status bar behind it still
        // says "Shift+Enter open", which is not the prompt offering Enter.
        let options = screen
            .lines()
            .find(|l| l.contains("S save and continue"))
            .expect("the options line");
        assert!(
            !options.contains("Enter"),
            "offering Enter on a destructive prompt is the bug this avoids: {options}"
        );
    }

    #[test]
    fn a_file_the_editor_will_not_open_says_why_and_offers_the_way_out() {
        let mut app = explorer_fixture();
        if let Some(x) = &mut app.explorer {
            x.doc = None;
            x.notice = Some("This file contains NUL bytes, so it is not text.".into());
        }
        let (screen, _) = render(&app, 118, 22);

        assert!(screen.contains("NUL bytes"), "the reason has to be shown");
        assert!(
            screen.contains("Shift+Enter"),
            "and the way to open it anyway"
        );
    }

    #[test]
    fn the_rail_shows_which_mode_filter_and_sort_are_active() {
        // The whole point of the rail: state you can see instead of state you
        // have to remember having pressed a chord for.
        let app = App {
            mode: Mode::Bloat,
            sort: Sort::Size,
            kind: Kind::DirsOnly,
            ..Default::default()
        };
        let (screen, _) = render(&app, 120, 30);

        for section in ["MODE", "SHOW", "SORT"] {
            assert!(screen.contains(section), "{section} section missing");
        }
        // Every option stays listed, so the alternatives are discoverable
        // rather than only the current one being named.
        for label in [
            "SEARCH", "BLOAT", "DUPES", "ALL", "DIRS", "FILES", "NAME", "SIZE",
        ] {
            assert!(screen.contains(label), "{label} missing from the rail");
        }

        // The marker, not just the colour, says which one is live: it has to
        // read correctly with no colour at all.
        let marked: Vec<&str> = screen
            .lines()
            .filter(|l| l.contains('\u{25b8}'))
            .map(|l| l.trim())
            .collect();
        assert_eq!(marked.len(), 3, "one marker per section: {marked:?}");
        assert!(marked.iter().any(|l| l.contains("BLOAT")), "{marked:?}");
        assert!(marked.iter().any(|l| l.contains("DIRS")), "{marked:?}");
        assert!(marked.iter().any(|l| l.contains("SIZE")), "{marked:?}");
    }

    #[test]
    fn every_term_is_picked_out_not_just_the_first() {
        // A multi-term query matches across the path, so showing only one hit
        // would hide most of the reason the row is on screen.
        let spans = path_spans(
            r"D:\SSO Updates\a.zip",
            "sso updates",
            false,
            Color::Cyan,
            false,
        );
        let hits: Vec<String> = spans
            .iter()
            .filter(|s| s.style.add_modifier.contains(Modifier::BOLD))
            .map(|s| s.content.to_string())
            .collect();
        assert_eq!(hits, vec!["SSO", "Updates"]);
    }

    #[test]
    fn a_term_is_picked_out_everywhere_it_appears() {
        let spans = path_spans(r"D:\SSO\SSO 1.zip", "sso", false, Color::Cyan, false);
        let hits = spans
            .iter()
            .filter(|s| s.style.add_modifier.contains(Modifier::BOLD))
            .count();
        assert_eq!(hits, 2, "both occurrences should be marked");
    }

    #[test]
    fn overlapping_terms_merge_into_one_run() {
        // "sso" sits inside "ssou". Rendered as two spans they would overlap
        // and the text would be duplicated on screen.
        let spans = path_spans("SSOU", "sso ssou", false, Color::Cyan, false);
        let hits: Vec<String> = spans
            .iter()
            .filter(|s| s.style.add_modifier.contains(Modifier::BOLD))
            .map(|s| s.content.to_string())
            .collect();
        assert_eq!(hits, vec!["SSOU"]);
    }

    #[test]
    fn the_spans_always_reassemble_into_the_original_path() {
        // The invariant that matters more than any emphasis: however the row
        // is split up, the user must still be shown their exact path.
        for (path, needle) in [
            (r"D:\SSO Updates\SSO 0.1.0.zip", "sso updates"),
            (r"D:\SSO\SSO 1.zip", "sso"),
            ("SSOU", "sso ssou"),
            (r"C:\a\b.txt", "zzz"),
            (r"C:\a\b.txt", ""),
            (r"C:\a\b.txt", "   "),
            (r"D:\x", "a-much-longer-needle-than-the-path"),
        ] {
            let spans = path_spans(path, needle, false, Color::Cyan, false);
            let joined: String = spans.iter().map(|s| s.content.as_ref()).collect();
            assert_eq!(joined, path, "rebuilding failed for needle {needle:?}");
        }
    }

    #[test]
    fn a_needle_longer_than_the_path_does_not_panic() {
        // The crash this replaced: "range end index 43 out of range for slice
        // of length 39", reached by typing a path segment longer than one of
        // the rows being drawn.
        let spans = path_spans(
            r"D:\Android\SDK",
            "a-much-longer-thing-than-the-path-itself",
            false,
            Color::Cyan,
            false,
        );
        let whole: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(whole, r"D:\Android\SDK", "the path must survive intact");
        assert_eq!(spans.len(), 1, "nothing matched, so nothing is split");
    }

    #[test]
    fn a_needle_the_exact_length_of_the_path_still_matches() {
        // The off-by-one either side of the fix: equal lengths must still be
        // searched, and must still highlight.
        let spans = path_spans("abc", "ABC", false, Color::Cyan, false);
        let whole: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(whole, "abc");
        assert!(
            spans
                .iter()
                .any(|s| s.content.as_ref() == "abc"
                    && s.style.add_modifier.contains(Modifier::BOLD)),
            "an exact-length match should still be picked out"
        );
    }

    #[test]
    fn the_counters_are_never_cut_into_by_the_hints() {
        // The failure this replaced rendered "... 76.00ms ·  ca" immediately
        // followed by "sort:size", with the two runs of text touching.
        let mut app = App::new(false);
        app.apply_rows(rows(5), 4204, 76_000, 20);

        for width in [90u16, 120, 150, 170, 200] {
            let (screen, _) = render(&app, width, 12);
            let status = screen.lines().last().unwrap();
            // Whatever tier was chosen, the counters must appear whole.
            assert!(
                status.contains("4,204 matches"),
                "counters cut off at {width}: {status}"
            );
            if let Some(at) = status.find("sort:") {
                assert!(
                    status[..at].ends_with("  "),
                    "no gap before the hints at {width}: {status}"
                );
            }
        }
    }

    #[test]
    fn the_rails_active_entry_is_filled_edge_to_edge() {
        // A bar that stopped at the end of the word would read as coloured
        // text, not as the selected item.
        let app = App {
            mode: Mode::Bloat,
            ..Default::default()
        };
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        terminal
            .draw(|f| {
                draw(f, &app);
            })
            .unwrap();
        let buf = terminal.backend().buffer().clone();

        // Find the row holding the active mode.
        let row = (0..buf.area.height)
            .find(|&y| {
                (0..layout::RAIL_WIDTH)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
                    .contains("BLOAT")
            })
            .expect("active rail entry");

        let accent = mode_accent(Mode::Bloat);
        // Inside the border, every cell carries the fill — including the
        // padding past the end of the label.
        for x in 1..layout::RAIL_WIDTH - 1 {
            assert_eq!(
                buf[(x, row)].style().bg,
                Some(accent),
                "column {x} of the active entry is not filled"
            );
        }
    }

    #[test]
    fn the_rail_yields_its_columns_when_asked_to_and_when_it_must() {
        let mut app = App::default();
        app.apply_rows(rows(3), 3, 0, 20);

        // Turned off by the user, at a width that would otherwise allow it.
        app.rail = false;
        let (off, _) = render(&app, 120, 30);
        assert!(!off.contains("SHOW"), "rail drawn after being turned off");
        // The mode is still reported, just in the border instead.
        assert!(off.contains("SEARCH"), "mode pills should return");

        // Wanted, but the terminal is too narrow to spare the columns.
        app.rail = true;
        let (narrow, _) = render(&app, layout::RAIL_MIN_COLS - 1, 30);
        assert!(!narrow.contains("SHOW"), "rail drawn on a narrow terminal");
    }

    #[test]
    fn the_result_count_is_reported_next_to_the_list() {
        let mut app = App::default();
        app.apply_rows(rows(5), 4204, 0, 20);
        let (screen, _) = render(&app, 120, 30);
        assert!(
            screen.contains("1 of 4,204"),
            "position missing from the results frame"
        );
    }

    #[test]
    fn search_mode_shows_the_query_and_results() {
        let mut app = App {
            query: "item_0".into(),
            ..Default::default()
        };
        app.apply_rows(rows(5), 5, 1500, 20);

        let (screen, _) = render(&app, 100, 20);
        assert!(screen.contains("batuta"), "title missing");
        assert!(screen.contains("item_0"), "query text missing");
        assert!(
            screen.contains("SIZE") && screen.contains("PATH"),
            "headers missing"
        );
        assert!(screen.contains("item_000.bin"), "first row missing");
        assert!(screen.contains("5 matches"), "match count missing");
        assert!(screen.contains("1.50ms"), "timing missing");
        assert!(screen.contains("cached"), "index source missing");
    }

    #[test]
    fn the_mode_pills_are_always_visible() {
        // The tint answers "where am I" only if you already know the code;
        // the pills spell it out, in every mode.
        let mut app = App::default();
        app.apply_rows(rows(3), 3, 0, 20);
        let (screen, _) = render(&app, 100, 20);
        assert!(screen.contains("SEARCH"), "pills missing: {screen}");
        assert!(screen.contains("BLOAT"));
        assert!(screen.contains("DUPES"));

        let mut app = App {
            mode: Mode::Bloat,
            ..Default::default()
        };
        app.apply_rows(rows(3), 3, 0, 20);
        let (screen, _) = render(&app, 100, 20);
        assert!(screen.contains("BLOAT") && screen.contains("DUPES"));
    }

    #[test]
    fn the_query_match_is_highlighted_inside_the_path() {
        // Three spans: before, the match in the accent, after. The match is
        // found without regard to case, because neither the query nor the
        // index respects it.
        let spans = path_spans(
            r"C:\Users\Hacker\ITEM_000.bin",
            "item_0",
            false,
            Color::Cyan,
            false,
        );
        assert_eq!(spans.len(), 3, "{spans:?}");
        assert_eq!(spans[0].content.to_string(), r"C:\Users\Hacker\");
        assert_eq!(spans[1].content.to_string(), "ITEM_0");
        assert_eq!(spans[2].content.to_string(), "00.bin");
        assert_eq!(spans[1].style.fg, Some(Color::Cyan));
        assert!(spans[1].style.add_modifier.contains(Modifier::BOLD));

        // No needle, no split.
        let spans = path_spans(r"C:\a.bin", "", false, Color::Cyan, false);
        assert_eq!(spans.len(), 1);

        // A needle that does not occur leaves the path whole.
        let spans = path_spans(r"C:\a.bin", "zzz", false, Color::Cyan, false);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content.to_string(), r"C:\a.bin");

        // A selected row is one uniform bar: the selection is the signal.
        let spans = path_spans(r"C:\a\item.bin", "item", true, Color::Cyan, false);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].style.fg, Some(theme().on_accent()));
        assert_eq!(spans[0].style.bg, Some(Color::Cyan));
    }

    #[test]
    fn bloat_mode_switches_the_columns() {
        let mut app = App {
            mode: Mode::Bloat,
            ..Default::default()
        };
        app.apply_rows(rows(3), 3, 0, 20);

        let (screen, _) = render(&app, 100, 20);
        assert!(screen.contains("TOTAL") && screen.contains("OWN") && screen.contains("SHARE"));
        // Scoped to the header row rather than the whole screen: the rail
        // lists MODIFIED as a sort option, which is not a leaked column.
        let header = screen
            .lines()
            .find(|l| l.contains("TOTAL"))
            .expect("bloat header row");
        assert!(
            !header.contains("MODIFIED"),
            "search columns must not leak in: {header}"
        );
        assert!(screen.contains("largest directories"));
        // The largest row on screen fills the whole bar; smaller ones do not.
        assert!(
            screen.contains("██████████"),
            "the largest directory fills its bar"
        );
        assert!(
            screen.contains("·"),
            "smaller directories leave the bar unfilled"
        );
    }

    #[test]
    fn an_empty_index_says_so_instead_of_rendering_a_blank_box() {
        let app = App::default();
        let (screen, _) = render(&app, 80, 15);
        assert!(screen.contains("type to search"));

        let mut app = App {
            query: "nothing".into(),
            ..Default::default()
        };
        app.apply_rows(Vec::new(), 0, 0, 10);
        let (screen, _) = render(&app, 80, 15);
        assert!(screen.contains("no matches"));
        assert!(screen.contains("0 matches"));
    }

    #[test]
    fn the_status_line_reports_position_and_liveness() {
        let mut app = App::new(true);
        app.apply_rows(rows(10), 5000, 0, 10);
        app.move_selection(3, 10);

        let (screen, _) = render(&app, 110, 20);
        assert!(screen.contains("5,000 matches"), "counts should be grouped");
        assert!(screen.contains("4 of 5,000"), "cursor position missing");
        assert!(
            screen.contains("live"),
            "daemon-backed index should say live"
        );
    }

    #[test]
    fn a_status_message_replaces_the_counters() {
        let app = App {
            status: r"no indexed directory matches C:\Nope".into(),
            ..Default::default()
        };
        let (screen, _) = render(&app, 100, 12);
        assert!(screen.contains("no indexed directory matches"));
    }

    #[test]
    fn hints_thin_out_gradually_rather_than_vanishing_at_once() {
        let app = App::default();

        // Too narrow for every hint, wide enough for the ones that matter.
        let (mid, _) = render(&app, 140, 12);
        assert!(mid.contains("Ctrl+S sort"), "core hints dropped too early");
        assert!(mid.contains("Esc close"));
        assert!(
            !mid.contains("Ctrl+B rail"),
            "the full set should not have fitted"
        );

        // Narrow enough that only the way out is worth the space.
        let (narrow, _) = render(&app, 90, 12);
        assert!(narrow.contains("Esc close"), "the exit hint must survive");
        assert!(!narrow.contains("Ctrl+S sort"));

        // Narrower still: the hints go entirely rather than crowding out the
        // counts, which are what the status line is actually for.
        let (tiny, _) = render(&app, 40, 12);
        assert!(!tiny.contains("Esc close"));
        assert!(
            tiny.contains("match"),
            "the counts must not be squeezed out"
        );
    }

    #[test]
    fn sort_and_filter_state_is_visible() {
        let mut app = App::default();
        app.cycle_sort();
        app.cycle_kind();
        // Wide enough for the whole hint set; the tiers below are covered
        // by their own test.
        let (screen, _) = render(&app, 180, 12);
        assert!(screen.contains(Sort::Size.label()));
        assert!(screen.contains(Kind::FilesOnly.label()));
        assert!(screen.contains("Tab complete"), "completion hint missing");
        assert!(screen.contains("Shift+Tab modes"), "mode hint missing");
        assert!(screen.contains("Ctrl+S sort"), "sort key hint missing");
        assert!(screen.contains("Ctrl+T filter"), "filter key hint missing");
        assert!(
            screen.contains("Shift+Enter open"),
            "launch key hint missing"
        );
        assert!(screen.contains("Esc close"), "close hint missing");
        assert!(
            !screen.contains("F2"),
            "the old function-key hints must be gone"
        );
        // Spelled out, not the caret shorthand a non-technical user has to decode.
        assert!(!screen.contains("^S"), "modifiers must be spelled out");
        assert!(!screen.contains("^C"), "modifiers must be spelled out");
    }

    #[test]
    fn the_hints_degrade_before_the_counters_do() {
        // A narrow terminal must keep the match count and drop hints, not the
        // other way round — and the key that closes the window survives
        // longest, because a user who cannot read it is stuck.
        let mut app = App::new(true);
        app.apply_rows(rows(5), 4321, 0, 5);

        let (wide, _) = render(&app, 170, 12);
        assert!(wide.contains("Ctrl+S sort"), "full hints should fit at 170");
        assert!(wide.contains("Tab complete"));
        assert!(wide.contains("4,321 matches"));

        let (mid, _) = render(&app, 100, 12);
        assert!(
            mid.contains("Esc close"),
            "the close hint should survive: {mid}"
        );
        assert!(!mid.contains("Ctrl+S sort"), "full hints do not fit at 100");
        assert!(
            mid.contains("4,321 matches"),
            "counters must not be dropped"
        );

        let (narrow, _) = render(&app, 60, 12);
        assert!(!narrow.contains("Esc close"), "no room for hints at 60");
        assert!(
            narrow.contains("4,321 matches"),
            "counters must still be there"
        );
    }

    #[test]
    fn rendering_survives_a_tiny_terminal() {
        // Layout arithmetic must not underflow when there is no room at all.
        let mut app = App::default();
        app.apply_rows(rows(3), 3, 0, 1);
        for (w, h) in [(20u16, 5u16), (10, 4), (40, 6), (5, 3)] {
            let (_, visible) = render(&app, w, h);
            assert!(visible <= h as usize);
        }
    }

    #[test]
    fn long_paths_do_not_break_the_layout() {
        let mut app = App::default();
        app.apply_rows(
            vec![Row {
                path: format!(r"C:\{}\deep.bin", vec!["verylongsegment"; 30].join("\\")),
                size: 1024,
                mtime: 0,
                is_dir: false,
                files: 0,
                own: 0,
            }],
            1,
            0,
            10,
        );
        let (screen, _) = render(&app, 60, 12);
        // Every line must respect the terminal width.
        for line in screen.lines() {
            assert_eq!(line.chars().count(), 60);
        }
    }

    #[test]
    fn dupes_mode_draws_group_dividers_above_their_copies() {
        let mut app = App {
            mode: Mode::Dupes,
            ..Default::default()
        };
        app.apply_dupes(
            vec![batuta_ipc::DupeGroupRows {
                size: 4096,
                wasted: 4096,
                rows: vec![
                    Row {
                        path: r"C:\a\copy.bin".into(),
                        size: 4096,
                        mtime: 0,
                        is_dir: false,
                        files: 0,
                        own: 0,
                    },
                    Row {
                        path: r"C:\b\copy.bin".into(),
                        size: 4096,
                        mtime: 0,
                        is_dir: false,
                        files: 0,
                        own: 0,
                    },
                ],
            }],
            4096,
            2_500_000,
            10,
        );

        let (screen, _) = render(&app, 100, 20);
        assert!(screen.contains("duplicates"), "title missing");
        assert!(screen.contains("COPIES"), "dupes header missing");
        assert!(screen.contains("── 2 copies"), "group divider missing");
        assert!(screen.contains("reclaim 4.00 KB"), "reclaim total missing");
        assert!(screen.contains(r"C:\a\copy.bin"), "first copy missing");
        assert!(screen.contains(r"C:\b\copy.bin"), "second copy missing");
        assert!(screen.contains("1 group"), "group count missing");
        assert!(screen.contains("reclaimable"), "wasted total missing");
        assert!(screen.contains("2.50s"), "timing missing");
    }

    #[test]
    fn an_empty_dupes_view_names_the_threshold() {
        let mut app = App {
            mode: Mode::Dupes,
            ..Default::default()
        };
        app.apply_dupes(Vec::new(), 0, 0, 10);

        let (screen, _) = render(&app, 100, 20);
        assert!(screen.contains("no duplicate files at or above 1.00 MB"));
    }

    #[test]
    fn a_pending_dupes_scan_announces_itself_rather_than_showing_stale_numbers() {
        // While the scan runs, the status replaces the counters and the empty
        // box stays blank — "no duplicates" would be a lie mid-scan.
        let mut app = App {
            mode: Mode::Dupes,
            ..Default::default()
        };
        app.status = "scanning for duplicates; hashing file contents...".into();

        let (screen, _) = render(&app, 100, 20);
        assert!(screen.contains("scanning for duplicates"));
        assert!(
            !screen.contains("0 groups"),
            "mid-scan must not claim zero groups"
        );
        assert!(
            !screen.contains("no duplicate files"),
            "mid-scan must not claim none"
        );
    }
}
