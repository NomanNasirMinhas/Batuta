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

use super::app::{App, DupeEntry, Mode, DUPES_MIN_SIZE};
use crate::fmt;
use batuta_ipc::Row;

const DIM: Color = Color::DarkGray;

/// The bytes-per-second question is really "which of these are worth acting
/// on", so sizes answer it with color as well as columns.
const GB: u64 = 1024 * 1024 * 1024;

/// Each mode carries its own accent: the frame's border, its title, the mode
/// pill, and the selected row all tint to it, so the mode you are in is legible
/// before you read anything.
fn mode_accent(mode: Mode) -> Color {
    match mode {
        Mode::Search => Color::Cyan,
        Mode::Bloat => Color::LightMagenta,
        Mode::Dupes => Color::Yellow,
    }
}

/// Warm colors for sizes worth acting on: red from 1 GiB, yellow from 100 MiB.
fn size_color(size: u64) -> Option<Color> {
    if size >= GB {
        Some(Color::LightRed)
    } else if size >= GB / 10 {
        Some(Color::LightYellow)
    } else {
        None
    }
}

fn size_style(size: u64) -> Style {
    size_color(size).map_or_else(Style::default, |c| Style::default().fg(c))
}

/// The style of the highlighted row: one uniform bar, the strongest signal on
/// the screen. Nothing on a selected row competes with it.
fn selected_style(i: usize, cursor: Option<usize>, accent: Color) -> Style {
    if cursor == Some(i) {
        Style::default().bg(accent).fg(Color::Black)
    } else {
        Style::default()
    }
}

/// Draw a frame; returns how many result rows are visible.
pub fn draw(f: &mut Frame, app: &App) -> usize {
    let chunks = Layout::vertical([
        Constraint::Length(3), // query box
        Constraint::Min(1),    // results
        Constraint::Length(1), // status
    ])
    .split(f.area());

    draw_query(f, chunks[0], app);
    let visible = draw_results(f, chunks[1], app);
    draw_status(f, chunks[2], app);

    // Overlays last, so they sit above the list rather than being drawn over.
    // `dupes_pending` starts true so the first visit to that view scans, so it
    // has to be paired with the mode or the panel appears over every screen.
    if app.mode == Mode::Dupes && app.dupes_pending {
        draw_scanning(f, chunks[1]);
    }
    if let Some(pending) = &app.confirm_delete {
        draw_delete_confirm(f, f.area(), pending);
    }
    visible
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
            Style::default().fg(DIM),
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
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
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
                Style::default().fg(Color::Yellow),
            ))
            .alignment(Alignment::Center),
        );
    } else {
        text.push(
            Line::from(Span::styled(
                fmt::bytes(pending.size),
                Style::default().fg(DIM),
            ))
            .alignment(Alignment::Center),
        );
    }

    text.push(Line::from(""));
    text.push(
        Line::from(Span::styled(
            "This does not go to the Recycle Bin.   Y delete    N cancel",
            Style::default().fg(DIM),
        ))
        .alignment(Alignment::Center),
    );

    f.render_widget(
        Paragraph::new(text).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Red))
                .title(Span::styled(
                    " confirm ",
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                )),
        ),
        box_area,
    );
}

fn draw_query(f: &mut Frame, area: Rect, app: &App) {
    let accent = mode_accent(app.mode);

    let title = match app.mode {
        Mode::Search => " batuta · search ",
        Mode::Bloat => " batuta · bloat (largest directories) ",
        Mode::Dupes => " batuta · duplicates (byte-identical files) ",
    };

    let prompt = match app.mode {
        Mode::Search => Line::from(query_spans(app, accent)),
        Mode::Bloat => Line::from(vec![Span::styled(
            "ranked by rolled-up size — the bar shows each directory's share of the largest",
            Style::default().fg(DIM),
        )]),
        Mode::Dupes => Line::from(vec![Span::styled(
            "each file with the paths holding identical copies — F5 rescan, Shift+Tab modes",
            Style::default().fg(DIM),
        )]),
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(accent))
        .title_top(Span::styled(
            title,
            Style::default().fg(accent).add_modifier(Modifier::BOLD),
        ))
        .title_top(mode_pills(app.mode).alignment(Alignment::Right));

    f.render_widget(Paragraph::new(prompt).block(block), area);
}

/// The typed query: the part of a path already settled dims, the component
/// still being typed carries the eye, and the caret block marks the end.
fn query_spans(app: &App, accent: Color) -> Vec<Span<'static>> {
    let mut spans = vec![Span::styled("› ", Style::default().fg(accent))];
    match app.query.rsplit_once(['\\', '/']) {
        Some((dir, tail)) => {
            spans.push(Span::styled(format!("{dir}\\"), Style::default().fg(DIM)));
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
            spans.push(Span::styled(" │ ", Style::default().fg(DIM)));
        }
        let style = if m == mode {
            Style::default()
                .fg(mode_accent(mode))
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(DIM)
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
            Mode::Dupes => (
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
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(DIM));
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
            Paragraph::new(Span::styled(msg, Style::default().fg(DIM)))
                .alignment(Alignment::Center)
                .block(block),
            area,
        );
        return visible;
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(DIM));

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
            Style::default().fg(DIM)
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
        Style::default().bg(accent).fg(Color::Black)
    } else if is_dir {
        Style::default().fg(accent)
    } else {
        Style::default()
    };
    if selected || needle.is_empty() {
        return vec![Span::styled(path.to_owned(), base)];
    }

    let (p, n) = (path.as_bytes(), needle.as_bytes());
    let at = (0..=p.len().saturating_sub(n.len()))
        .find(|&i| p[i..i + n.len()].eq_ignore_ascii_case(n))
        .filter(|&i| i + n.len() <= p.len());
    // ASCII comparison cannot land inside a multi-byte character, but slicing
    // is the one place a bug here would panic, so stay defensive.
    let split = at.and_then(|i| {
        match (
            path.get(..i),
            path.get(i..i + n.len()),
            path.get(i + n.len()..),
        ) {
            (Some(a), Some(m), Some(z)) => Some((a, m, z)),
            _ => None,
        }
    });

    match split {
        Some((a, m, z)) => vec![
            Span::styled(a.to_owned(), base),
            Span::styled(
                m.to_owned(),
                Style::default().fg(accent).add_modifier(Modifier::BOLD),
            ),
            Span::styled(z.to_owned(), base),
        ],
        None => vec![Span::styled(path.to_owned(), base)],
    }
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
            Style::default().fg(DIM)
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
        Style::default().bg(accent).fg(Color::Black)
    } else {
        Style::default().fg(size_color(size).unwrap_or(accent))
    };
    let rest = if selected {
        Style::default().bg(accent).fg(Color::Black)
    } else {
        Style::default().fg(DIM)
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
        Style::default().fg(DIM)
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
                        Style::default().fg(Color::LightRed)
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
    let dim = Style::default().fg(DIM);

    let mut left = Vec::new();

    if !app.status.is_empty() {
        left.push(Span::styled(
            app.status.clone(),
            Style::default().fg(Color::Yellow),
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
            Style::default().fg(Color::LightRed),
        ));
        if app.total > 0 {
            left.push(Span::styled(
                format!("  ·  {} of {}", app.selected + 1, fmt::count(app.total)),
                dim,
            ));
        }
        if app.elapsed_us > 0 {
            left.push(Span::styled(
                format!("  ·  {:.2}s", app.elapsed_us as f64 / 1_000_000.0),
                dim,
            ));
        }
        left.push(Span::styled(
            if app.live {
                "  ·  live"
            } else {
                "  ·  cached"
            },
            Style::default().fg(if app.live {
                Color::Green
            } else {
                Color::Yellow
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
                dim,
            ));
        }
        if app.elapsed_us > 0 {
            left.push(Span::styled(
                format!("  ·  {:.2}ms", app.elapsed_us as f64 / 1000.0),
                dim,
            ));
        }
        left.push(Span::styled(
            if app.live {
                "  ·  live"
            } else {
                "  ·  cached"
            },
            Style::default().fg(if app.live {
                Color::Green
            } else {
                Color::Yellow
            }),
        ));
    }

    let state = format!("sort:{}  show:{}", app.sort.label(), app.kind.label());
    let full = format!("{state}  │ Tab complete  Shift+Tab modes  Ctrl+S sort  Ctrl+T filter  Ctrl+D dupes  Enter open  Esc close");
    let short = format!("{state}  │ Esc close");

    // The status text is what the user needs; hints are a courtesy. Rather
    // than dropping every hint the moment the full set stops fitting, fall
    // back to the one key that gets them out, then to nothing.
    //
    // Width is counted in characters, not bytes: these strings contain
    // multi-byte glyphs, and `len()` would over-reserve the right-hand column
    // and push the counters off screen.
    const MIN_STATUS: u16 = 40;
    let keys = [full, short]
        .into_iter()
        .find(|k| area.width >= k.chars().count() as u16 + MIN_STATUS);

    let Some(keys) = keys else {
        f.render_widget(Paragraph::new(Line::from(left)), area);
        return;
    };
    let keys_width = keys.chars().count() as u16;

    let chunks = Layout::horizontal([Constraint::Min(MIN_STATUS), Constraint::Length(keys_width)])
        .split(area);

    f.render_widget(
        Paragraph::new(Line::from(left)).style(Style::default().fg(accent)),
        chunks[0],
    );
    f.render_widget(
        Paragraph::new(Span::styled(keys, Style::default().fg(DIM))).alignment(Alignment::Right),
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
            // 3 rows for the query box, 1 for status, 2 borders, 1 header.
            let expected = height.saturating_sub(3 + 1 + 3) as usize;
            assert_eq!(visible, expected, "wrong visible count at height {height}");
        }
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
        assert_eq!(spans[0].style.fg, Some(Color::Black));
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
        assert!(
            !screen.contains("MODIFIED"),
            "search columns must not leak in"
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
    fn sort_and_filter_state_is_visible() {
        let mut app = App::default();
        app.cycle_sort();
        app.cycle_kind();
        let (screen, _) = render(&app, 170, 12);
        assert!(screen.contains(Sort::Size.label()));
        assert!(screen.contains(Kind::FilesOnly.label()));
        assert!(screen.contains("Tab complete"), "completion hint missing");
        assert!(screen.contains("Shift+Tab modes"), "mode hint missing");
        assert!(screen.contains("Ctrl+S sort"), "sort key hint missing");
        assert!(screen.contains("Ctrl+T filter"), "filter key hint missing");
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
