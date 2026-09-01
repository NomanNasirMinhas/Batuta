//! The ratatui front-end for Quick Setup.
//!
//! [`flow::run`] asks its questions through the [`Ask`] trait and never touches
//! a terminal itself; this module is the other implementation of that trait,
//! answering with widgets instead of a line protocol. The step machine cannot
//! tell the two apart, so its branching stays testable as data.
//!
//! Every dialog is drawn by a plain function over a frame, and the interactive
//! state (checkboxes) is a struct with no terminal attached — both are checked
//! against ratatui's `TestBackend`, with no console and no elevation.

use std::io::{self};
use std::ops::RangeInclusive;

use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Wrap};
use ratatui::Frame;

use super::prompt::{number_answer, Ask};

/// The setup accent, matching the search mode the hotkey opens.
const ACCENT: Color = Color::Cyan;
/// Quiet text: hints, defaults, context shown before a question.
const DIM: Color = Color::DarkGray;

/// A dialog question this front-end can answer with widgets. Anything else —
/// a `line` prompt — has no widget here, and the flow never needs one.
pub struct Wizard {
    terminal: Terminal,
    /// Lines said since the last question was answered; shown above it as
    /// context. Cleared when a question completes, so notes meant for the next
    /// question survive and old ones do not pile up.
    context: Vec<String>,
    /// Apply progress, one line per step as it happens.
    log: Vec<String>,
}

type Terminal = ratatui::Terminal<CrosstermBackend<io::Stdout>>;

impl Wizard {
    /// Take over the terminal. Failing halfway must not leave the user's
    /// console in raw mode, so each failure undoes the steps before it.
    pub fn new() -> anyhow::Result<Self> {
        enable_raw_mode().map_err(anyhow::Error::from)?;
        let mut stdout = io::stdout();
        if let Err(e) = execute!(stdout, EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(e.into());
        }
        match ratatui::Terminal::new(CrosstermBackend::new(stdout)) {
            Ok(terminal) => Ok(Wizard {
                terminal,
                context: Vec::new(),
                log: Vec::new(),
            }),
            Err(e) => {
                let _ = disable_raw_mode();
                let _ = execute!(io::stdout(), LeaveAlternateScreen);
                Err(e.into())
            }
        }
    }

    /// One dialog frame: the accumulated context, the question, the body
    /// widget, and the key hints.
    fn draw(&mut self, question: &str, body: Vec<Line<'static>>, hints: &str) -> io::Result<()> {
        let context = self.context.clone();
        self.terminal
            .draw(|f| draw_dialog(f, &context, question, body, hints))?;
        Ok(())
    }

    /// The question is over; its context is not interesting anymore.
    fn answered(&mut self) {
        self.context.clear();
    }
}

impl Drop for Wizard {
    fn drop(&mut self) {
        // Restore the user's terminal even when setup fails or is cancelled.
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}

impl Ask for Wizard {
    fn line(&mut self, _prompt: &str) -> io::Result<Option<String>> {
        // Every question the flow asks goes through a typed method with a
        // widget. A line prompt reaching here means a future step added one
        // without teaching the wizard to draw it — refuse rather than pretend.
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "the setup UI has no widget for a plain line prompt",
        ))
    }

    fn say(&mut self, text: &str) {
        if !text.trim().is_empty() {
            self.context.push(text.to_string());
        }
    }

    fn confirm(&mut self, question: &str, default: bool) -> io::Result<bool> {
        let out = self.confirm_dialog(question, default);
        self.answered();
        out
    }

    fn number(
        &mut self,
        question: &str,
        range: RangeInclusive<u32>,
        default: u32,
    ) -> io::Result<u32> {
        let out = self.number_dialog(question, range, default);
        self.answered();
        out
    }

    fn text(&mut self, question: &str, default: &str) -> io::Result<String> {
        let out = self.text_dialog(question, default);
        self.answered();
        out
    }

    fn choose(
        &mut self,
        question: &str,
        options: &[&str],
        default_index: usize,
    ) -> io::Result<usize> {
        let out = self.choose_dialog(question, options, default_index);
        self.answered();
        out
    }

    fn multi_select(
        &mut self,
        question: &str,
        items: &[String],
        selectable: &[bool],
        default: &[usize],
    ) -> io::Result<Vec<usize>> {
        let out = self.multi_select_dialog(question, items, selectable, default);
        self.answered();
        out
    }

    fn progress(&mut self, line: &str) {
        self.log.push(line.to_string());
        // Redrawing on every line is fine here: apply emits one per step, and
        // the long pause (index building) happens between lines, not during.
        let log = self.log.clone();
        let _ = self.terminal.draw(|f| draw_log(f, " applying ", &log, ""));
    }

    fn finish(&mut self, lines: &[String]) {
        let shown = lines.to_vec();
        if self
            .terminal
            .draw(|f| draw_log(f, " setup ", &shown, "press any key to close"))
            .is_err()
        {
            // Not on the alternate screen anymore; the caller's plain prints
            // will have to do.
            return;
        }
        let _ = key();
    }
}

impl Wizard {
    fn confirm_dialog(&mut self, question: &str, default: bool) -> io::Result<bool> {
        let mut on = default;
        loop {
            self.draw(question, confirm_body(on), &confirm_hints(default))?;
            match key()? {
                Key::Cancel => return Err(cancelled()),
                Key::Left | Key::Right => on = !on,
                Key::Char('y') => return Ok(true),
                Key::Char('n') => return Ok(false),
                Key::Esc => return Ok(default),
                Key::Enter => return Ok(on),
                _ => {}
            }
        }
    }

    fn number_dialog(
        &mut self,
        question: &str,
        range: RangeInclusive<u32>,
        default: u32,
    ) -> io::Result<u32> {
        let mut draft = String::new();
        let mut complaint = String::new();
        loop {
            self.draw(
                question,
                number_body(&draft, &range, default, &complaint),
                "type digits · ↑ ↓ adjust · Enter accept · Esc default · Ctrl+C cancel",
            )?;
            match key()? {
                Key::Cancel => return Err(cancelled()),
                Key::Char(c) if c.is_ascii_digit() && draft.len() < 3 => {
                    complaint.clear();
                    draft.push(c);
                }
                Key::Backspace => {
                    complaint.clear();
                    draft.pop();
                }
                // Adjust from the current value, clamped into the range so the
                // keys can never produce an answer that would be refused.
                Key::Up => {
                    complaint.clear();
                    let base = draft.parse().unwrap_or(default);
                    draft = base.saturating_sub(1).max(*range.start()).to_string();
                }
                Key::Down => {
                    complaint.clear();
                    let base = draft.parse().unwrap_or(default);
                    draft = base.saturating_add(1).min(*range.end()).to_string();
                }
                Key::Esc => return Ok(default),
                Key::Enter => {
                    if draft.is_empty() {
                        return Ok(default);
                    }
                    match number_answer(&draft, &range) {
                        Ok(Some(v)) => return Ok(v),
                        Ok(None) => return Ok(default),
                        Err(c) => complaint = c,
                    }
                }
                _ => {}
            }
        }
    }

    fn text_dialog(&mut self, question: &str, default: &str) -> io::Result<String> {
        let mut draft = String::new();
        loop {
            self.draw(
                question,
                text_body(&draft, default),
                "type a path · Enter accept · Esc default · Ctrl+C cancel",
            )?;
            match key()? {
                Key::Cancel => return Err(cancelled()),
                Key::Char(c) if !c.is_control() && draft.len() < 260 => draft.push(c),
                Key::Backspace => {
                    draft.pop();
                }
                Key::Esc => return Ok(default.to_string()),
                Key::Enter => {
                    if draft.is_empty() {
                        return Ok(default.to_string());
                    }
                    return Ok(draft);
                }
                _ => {}
            }
        }
    }

    fn choose_dialog(
        &mut self,
        question: &str,
        options: &[&str],
        default_index: usize,
    ) -> io::Result<usize> {
        let mut cursor = default_index.min(options.len().saturating_sub(1));
        loop {
            self.draw(
                question,
                choose_body(options, cursor, default_index),
                "↑ ↓ choose · Enter accept · Esc default · Ctrl+C cancel",
            )?;
            match key()? {
                Key::Cancel => return Err(cancelled()),
                Key::Up => cursor = cursor.max(1) - 1,
                Key::Down => cursor = (cursor + 1).min(options.len() - 1),
                Key::Esc => return Ok(default_index),
                Key::Enter => return Ok(cursor),
                _ => {}
            }
        }
    }

    fn multi_select_dialog(
        &mut self,
        question: &str,
        items: &[String],
        selectable: &[bool],
        default: &[usize],
    ) -> io::Result<Vec<usize>> {
        let mut t = Toggles::new(selectable, default);
        let mut complaint = String::new();
        loop {
            self.draw(
                question,
                multi_select_body(items, &t, &complaint),
                "↑ ↓ move · Space toggle · a all · Enter accept · Esc default · Ctrl+C cancel",
            )?;
            match key()? {
                Key::Cancel => return Err(cancelled()),
                Key::Up => t.up(),
                Key::Down => t.down(),
                Key::Char(' ') => {
                    if !t.toggle() {
                        complaint = format!("  {} cannot be indexed.", items[t.cursor]);
                    } else {
                        complaint.clear();
                    }
                }
                Key::Char('a') => {
                    complaint.clear();
                    t.all();
                }
                Key::Esc => return Ok(default.to_vec()),
                Key::Enter => {
                    if t.none_on() {
                        complaint = "  choose at least one.".into();
                    } else {
                        return Ok(t.chosen());
                    }
                }
                _ => {}
            }
        }
    }
}

/// Ctrl+C anywhere means stop; nothing has been applied yet, which is the
/// point of asking everything first.
fn cancelled() -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, "setup cancelled")
}

// ---- input -------------------------------------------------------------

/// The keys a dialog understands. `Other` covers everything ignored (and
/// resize, which just causes the next loop turn to redraw).
enum Key {
    Up,
    Down,
    Left,
    Right,
    Enter,
    Esc,
    Backspace,
    Char(char),
    Cancel,
    Other,
}

/// Wait for the next key press. Release events and non-key events are skipped.
fn key() -> io::Result<Key> {
    loop {
        match event::read()? {
            Event::Key(k) if k.kind == KeyEventKind::Press => return Ok(map_key(k)),
            Event::Resize(_, _) => return Ok(Key::Other),
            _ => {}
        }
    }
}

fn map_key(k: KeyEvent) -> Key {
    if k.modifiers.contains(KeyModifiers::CONTROL) {
        return match k.code {
            KeyCode::Char('c') => Key::Cancel,
            _ => Key::Other,
        };
    }
    match k.code {
        KeyCode::Up => Key::Up,
        KeyCode::Down => Key::Down,
        KeyCode::Left => Key::Left,
        KeyCode::Right => Key::Right,
        KeyCode::Enter => Key::Enter,
        KeyCode::Esc => Key::Esc,
        KeyCode::Backspace => Key::Backspace,
        KeyCode::Char(c) => Key::Char(c),
        _ => Key::Other,
    }
}

// ---- checkbox state ----------------------------------------------------

/// Checkbox state for a multi-select question, with no terminal attached.
struct Toggles {
    on: Vec<bool>,
    selectable: Vec<bool>,
    cursor: usize,
}

impl Toggles {
    fn new(selectable: &[bool], default: &[usize]) -> Self {
        let mut on = vec![false; selectable.len()];
        for &i in default {
            if i < on.len() {
                on[i] = true;
            }
        }
        let cursor = default
            .first()
            .copied()
            .unwrap_or(0)
            .min(selectable.len().saturating_sub(1));
        Toggles {
            on,
            selectable: selectable.to_vec(),
            cursor,
        }
    }

    fn up(&mut self) {
        self.cursor = self.cursor.max(1) - 1;
    }

    fn down(&mut self) {
        self.cursor = (self.cursor + 1).min(self.on.len().saturating_sub(1));
    }

    /// Toggle the entry under the cursor. Unselectable entries refuse, so the
    /// cursor must move on — silently hiding a drive the user can see would
    /// look like a bug.
    fn toggle(&mut self) -> bool {
        if self.selectable[self.cursor] {
            self.on[self.cursor] = !self.on[self.cursor];
            true
        } else {
            false
        }
    }

    /// Every selectable entry on.
    fn all(&mut self) {
        for (on, s) in self.on.iter_mut().zip(&self.selectable) {
            if *s {
                *on = true;
            }
        }
    }

    /// The chosen indices, in list order.
    fn chosen(&self) -> Vec<usize> {
        self.on
            .iter()
            .enumerate()
            .filter(|(_, &on)| on)
            .map(|(i, _)| i)
            .collect()
    }

    fn none_on(&self) -> bool {
        self.on.iter().all(|&on| !on)
    }
}

// ---- dialog rendering --------------------------------------------------

/// A centered box, wide enough for a path and tall enough for the summary
/// confirm, without taking over the whole screen.
fn dialog_rect(area: Rect) -> Rect {
    let w = 76.min(area.width.saturating_sub(2));
    let h = (area.height.saturating_sub(4)).clamp(8, 32);
    Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    }
}

fn draw_dialog(
    f: &mut Frame<'_>,
    context: &[String],
    question: &str,
    body: Vec<Line<'static>>,
    hints: &str,
) {
    let block = Block::bordered()
        .title_top(" batuta · quick setup ")
        .title_top(Line::from("Esc = default").alignment(Alignment::Right))
        .border_style(Style::new().fg(ACCENT));

    let mut lines = Vec::new();
    for c in context {
        lines.push(Line::from(c.clone()).style(Style::new().fg(DIM)));
    }
    if !context.is_empty() {
        lines.push(Line::from(""));
    }
    lines.push(
        Line::from(question.to_string())
            .style(Style::new().fg(Color::White).add_modifier(Modifier::BOLD)),
    );
    lines.push(Line::from(""));
    lines.extend(body);
    lines.push(Line::from(""));
    lines.push(Line::from(format!(" {hints}")).style(Style::new().fg(DIM)));

    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(block),
        dialog_rect(f.area()),
    );
}

/// The apply log and the closing report: one colored line per step.
fn draw_log(f: &mut Frame<'_>, title: &str, lines: &[String], footer: &str) {
    let block = Block::bordered()
        .title_top(format!(" batuta{title} "))
        .border_style(Style::new().fg(ACCENT));
    let mut rows: Vec<Line> = lines.iter().map(|l| log_line(l)).collect();
    if !footer.is_empty() {
        rows.push(Line::from(""));
        rows.push(Line::from(format!(" {footer}")).style(Style::new().fg(DIM)));
    }
    f.render_widget(
        Paragraph::new(rows).wrap(Wrap { trim: false }).block(block),
        Rect {
            x: f.area().x,
            y: f.area().y,
            width: f.area().width,
            height: f.area().height,
        },
    );
}

/// Apply lines carry their own status marker; color by it rather than parsing
/// the sentence.
fn log_line(line: &str) -> Line<'static> {
    let style = if line.starts_with("FAIL") {
        Style::new().fg(Color::LightRed)
    } else if line.starts_with("  ok") {
        Style::new().fg(Color::Green)
    } else if line.starts_with("  --") || line.starts_with("  note") {
        Style::new().fg(DIM)
    } else {
        // The one long-running step ("building the index...").
        Style::new().fg(Color::LightYellow)
    };
    Line::from(line.to_string()).style(style)
}

fn confirm_body(on: bool) -> Vec<Line<'static>> {
    vec![Line::from(vec![
        Span::raw("   "),
        pill("Yes", on),
        Span::raw("   "),
        pill("No", !on),
    ])]
}

fn pill(label: &str, selected: bool) -> Span<'static> {
    if selected {
        Span::styled(
            format!("‹ {label} ›"),
            Style::new().fg(ACCENT).add_modifier(Modifier::BOLD),
        )
    } else {
        Span::styled(format!("  {label}  "), Style::new().fg(Color::Gray))
    }
}

fn confirm_hints(default: bool) -> String {
    format!(
        "← → choose · Enter accept · Esc default ({}) · Ctrl+C cancel",
        if default { "Yes" } else { "No" }
    )
}

fn number_body(
    draft: &str,
    range: &RangeInclusive<u32>,
    default: u32,
    complaint: &str,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if draft.is_empty() {
        lines.push(Line::from(Span::styled(
            format!(" {default}▏"),
            Style::new().fg(DIM),
        )));
    } else {
        lines.push(Line::from(Span::styled(
            format!(" {draft}▏"),
            Style::new().fg(ACCENT).add_modifier(Modifier::BOLD),
        )));
    }
    lines.push(Line::from(Span::styled(
        format!(
            " range {}-{} · default {}",
            range.start(),
            range.end(),
            default
        ),
        Style::new().fg(DIM),
    )));
    if !complaint.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            complaint.to_string(),
            Style::new().fg(Color::LightRed),
        )));
    }
    lines
}

fn text_body(draft: &str, default: &str) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if draft.is_empty() {
        lines.push(Line::from(Span::styled(
            format!(" {default}▏"),
            Style::new().fg(DIM),
        )));
    } else {
        lines.push(Line::from(Span::styled(
            format!(" {draft}▏"),
            Style::new().fg(ACCENT).add_modifier(Modifier::BOLD),
        )));
    }
    lines
}

fn choose_body(options: &[&str], cursor: usize, default_index: usize) -> Vec<Line<'static>> {
    options
        .iter()
        .enumerate()
        .map(|(i, opt)| {
            let marker = if i == cursor { "▸ " } else { "  " };
            let tag = if i == default_index {
                "   (default)"
            } else {
                ""
            };
            let style = if i == cursor {
                Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(Color::Gray)
            };
            Line::from(Span::styled(format!("{marker}{opt}{tag}"), style))
        })
        .collect()
}

fn multi_select_body(items: &[String], t: &Toggles, complaint: &str) -> Vec<Line<'static>> {
    let mut lines: Vec<Line> = items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let at = t.cursor == i;
            let (mark, mut style) = if !t.selectable[i] {
                ("[–]", Style::new().fg(DIM))
            } else if t.on[i] {
                ("[×]", Style::new().fg(ACCENT))
            } else {
                ("[ ]", Style::new().fg(Color::Gray))
            };
            if at {
                style = style.add_modifier(Modifier::BOLD);
            }
            let marker = if at { "▸ " } else { "  " };
            Line::from(Span::styled(format!("{marker}{mark} {item}"), style))
        })
        .collect();
    if !complaint.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            complaint.to_string(),
            Style::new().fg(Color::LightRed),
        )));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    use ratatui::backend::TestBackend;

    /// What a render actually put on screen, as one string.
    fn rendered<F>(w: u16, h: u16, draw: F) -> String
    where
        F: FnOnce(&mut Frame<'_>),
    {
        let mut terminal = ratatui::Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal.draw(|f| draw(f)).unwrap();
        let buf = terminal.backend().buffer().clone();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn toggles_track_defaults_refuse_unselectable_and_gather_choices() {
        let sel = [true, true, false];
        let mut t = Toggles::new(&sel, &[1]);
        assert_eq!(t.chosen(), vec![1]);

        t.down(); // cursor 2, the exFAT stick
        assert!(!t.toggle(), "an unselectable entry must refuse");
        assert_eq!(t.chosen(), vec![1], "and change nothing");

        t.all();
        assert_eq!(
            t.chosen(),
            vec![0, 1],
            "'all' must skip unselectable entries"
        );

        t.up(); // cursor 1
        t.toggle();
        assert_eq!(t.chosen(), vec![0]);
    }

    #[test]
    fn toggles_report_when_nothing_is_chosen() {
        let mut t = Toggles::new(&[true, true], &[]);
        assert!(t.none_on());
        t.toggle();
        assert!(!t.none_on());
    }

    #[test]
    fn the_confirm_dialog_marks_the_selected_pill() {
        let yes = rendered(80, 24, |f| {
            draw_dialog(f, &[], "Install the daemon?", confirm_body(true), "hints")
        });
        assert!(yes.contains("‹ Yes ›"));
        assert!(yes.contains("  No  "));
        assert!(yes.contains("batuta · quick setup"));

        let no = rendered(80, 24, |f| {
            draw_dialog(f, &[], "Install the daemon?", confirm_body(false), "hints")
        });
        assert!(no.contains("‹ No ›"));
    }

    #[test]
    fn the_number_dialog_shows_the_draft_the_range_and_complaints() {
        let body = number_body("15", &(1..=60), 15, "");
        let text = rendered(80, 24, |f| draw_dialog(f, &[], "How often?", body, "h"));
        assert!(text.contains("15▏"));
        assert!(text.contains("range 1-60"));

        let empty = number_body("", &(1..=60), 15, "");
        let text = rendered(80, 24, |f| draw_dialog(f, &[], "q", empty, "h"));
        assert!(text.contains("default 15"));

        let bad = number_body("0", &(1..=60), 15, "  0 is outside 1-60.");
        let text = rendered(80, 24, |f| draw_dialog(f, &[], "q", bad, "h"));
        assert!(text.contains("outside 1-60"));
    }

    #[test]
    fn the_choose_dialog_marks_the_cursor_and_the_default() {
        let opts = ["Ctrl+Space", "Ctrl+Alt+Space", "Alt+Space"];
        let body = choose_body(&opts, 1, 0);
        let text = rendered(80, 24, |f| {
            draw_dialog(f, &[], "Which shortcut?", body, "h")
        });
        assert!(text.contains("▸ Ctrl+Alt+Space"));
        assert!(text.contains("Ctrl+Space   (default)"));
    }

    #[test]
    fn the_drive_dialog_shows_checkboxes_and_refusals() {
        let items: Vec<String> = vec![
            "C: OS (NTFS)".into(),
            "D: Data (NTFS)".into(),
            "E: Stick (exFAT)".into(),
        ];
        let t = Toggles::new(&[true, true, false], &[0]);
        let body = multi_select_body(&items, &t, "");
        let text = rendered(80, 24, |f| draw_dialog(f, &[], "Which drives?", body, "h"));
        assert!(text.contains("[×] C: OS (NTFS)"));
        assert!(text.contains("[ ] D: Data (NTFS)"));
        assert!(text.contains("[–] E: Stick (exFAT)"));

        let refused = multi_select_body(&items, &t, "  E: Stick (exFAT) cannot be indexed.");
        let text = rendered(80, 24, |f| draw_dialog(f, &[], "q", refused, "h"));
        assert!(text.contains("cannot be indexed"));
    }

    #[test]
    fn the_apply_log_is_colored_by_its_marker() {
        let lines = vec![
            "  ok stop: nothing to stop".to_string(),
            "FAIL hotkey: already taken".to_string(),
            "  -- PATH: already present".to_string(),
            "     building the index (new), this takes a few seconds...".to_string(),
        ];
        let text = rendered(80, 24, |f| {
            draw_log(f, " applying ", &lines, "press any key")
        });
        assert!(text.contains("building the index"));
        assert!(text.contains("press any key"));

        // Marker styles, not just text.
        assert_eq!(log_line("FAIL x").style, Style::new().fg(Color::LightRed));
        assert_eq!(log_line("  ok x").style, Style::new().fg(Color::Green));
        assert_eq!(log_line("  -- x").style, Style::new().fg(DIM));
        assert_eq!(
            log_line("     building").style,
            Style::new().fg(Color::LightYellow)
        );
    }

    #[test]
    fn context_lines_render_above_the_question() {
        let ctx = vec!["Batuta Quick Setup".to_string()];
        let body = confirm_body(true);
        let text = rendered(80, 24, |f| {
            draw_dialog(f, &ctx, "Install the daemon?", body, "hints")
        });
        let ctx_at = text.find("Batuta Quick Setup").unwrap();
        let q_at = text.find("Install the daemon?").unwrap();
        assert!(ctx_at < q_at, "context must come before the question");
    }
}
