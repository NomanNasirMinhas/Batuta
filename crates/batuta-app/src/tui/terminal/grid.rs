//! The terminal's screen: a grid of cells, and the VT interpreter that fills it.
//!
//! ## Why the parser is not written here
//!
//! The escape-sequence state machine is the part of a terminal emulator most
//! likely to be subtly wrong — it has to handle truncated sequences, parameters
//! split across reads, and a long tail of forms nobody remembers. `vte` is
//! Alacritty's, it is exhaustively tested, and it is a state machine rather
//! than an opinion about what a terminal should do. What is written here is the
//! part that *is* an opinion: what each sequence does to the screen.
//!
//! ## Scope
//!
//! This covers what shells and command-line tools actually emit: cursor
//! movement, erasing, colours and attributes, scroll regions, insert and delete
//! of lines and characters, the alternate screen, and the title. Sixel, mouse
//! reporting, double-width lines and character sets are not handled — they are
//! parsed and ignored rather than printed as garbage, which is the important
//! part.

use std::collections::VecDeque;

/// A colour as the terminal expresses it, kept independent of ratatui so this
/// module stays pure and testable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Ink {
    #[default]
    Default,
    /// One of the 256 palette entries.
    Indexed(u8),
    Rgb(u8, u8, u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Style {
    pub fg: Ink,
    pub bg: Ink,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub reverse: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    pub ch: char,
    pub style: Style,
}

impl Default for Cell {
    fn default() -> Self {
        Cell {
            ch: ' ',
            style: Style::default(),
        }
    }
}

/// How many lines of history are kept above the screen.
const SCROLLBACK: usize = 5_000;

pub struct Grid {
    cols: usize,
    rows: usize,
    cells: Vec<Cell>,
    /// Lines that have scrolled off the top.
    scrollback: VecDeque<Vec<Cell>>,
    row: usize,
    col: usize,
    /// Set once the cursor has been pushed past the last column. The next
    /// printable character wraps. Deferring it is what stops a character
    /// written in the last column from scrolling the screen on its own.
    pending_wrap: bool,
    style: Style,
    saved: Option<(usize, usize)>,
    /// Inclusive scroll region, as rows.
    scroll_top: usize,
    scroll_bottom: usize,
    pub cursor_visible: bool,
    /// The screen the alternate buffer replaced, if it is active.
    alternate: Option<Vec<Cell>>,
    pub title: Option<String>,
    /// How far up the scrollback the viewport is scrolled.
    pub view_offset: usize,
    /// Answers the terminal owes the program, waiting to be written back.
    ///
    /// A terminal is not only a display: programs *ask it questions* and block
    /// until they are answered. ConPTY asks for the cursor position the moment
    /// it starts and emits nothing further until it gets a reply, so a
    /// terminal that only draws and never answers hangs before the shell ever
    /// prints a prompt.
    replies: Vec<u8>,
}

impl Grid {
    pub fn new(cols: usize, rows: usize) -> Self {
        let (cols, rows) = (cols.max(1), rows.max(1));
        Grid {
            cols,
            rows,
            cells: vec![Cell::default(); cols * rows],
            scrollback: VecDeque::new(),
            row: 0,
            col: 0,
            pending_wrap: false,
            style: Style::default(),
            saved: None,
            scroll_top: 0,
            scroll_bottom: rows - 1,
            cursor_visible: true,
            alternate: None,
            title: None,
            view_offset: 0,
            replies: Vec::new(),
        }
    }

    /// Take whatever the terminal owes the program, to be written to its input.
    pub fn take_replies(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.replies)
    }

    pub fn size(&self) -> (usize, usize) {
        (self.cols, self.rows)
    }

    pub fn cursor(&self) -> (usize, usize) {
        (self.row, self.col)
    }

    pub fn scrollback_len(&self) -> usize {
        self.scrollback.len()
    }

    pub fn line(&self, row: usize) -> &[Cell] {
        let start = row * self.cols;
        &self.cells[start..start + self.cols]
    }

    /// The rows to draw, oldest first, honouring how far back the view is
    /// scrolled.
    pub fn visible(&self) -> Vec<&[Cell]> {
        let back = self.view_offset.min(self.scrollback.len());
        let from_history = back.min(self.rows);
        let start = self.scrollback.len() - back;

        let mut out: Vec<&[Cell]> = Vec::with_capacity(self.rows);
        for line in self.scrollback.iter().skip(start).take(from_history) {
            out.push(line.as_slice());
        }
        for row in 0..self.rows - from_history {
            out.push(self.line(row));
        }
        out
    }

    pub fn scroll_view(&mut self, delta: isize) {
        let max = self.scrollback.len();
        self.view_offset = (self.view_offset as isize + delta).clamp(0, max as isize) as usize;
    }

    /// Jump back to the live screen. Anything the program prints should do
    /// this, or output would arrive somewhere the user cannot see.
    pub fn follow(&mut self) {
        self.view_offset = 0;
    }

    /// Resize, keeping the cursor on screen.
    ///
    /// Reflowing wrapped lines is deliberately not attempted: doing it badly
    /// mangles output worse than not doing it, and the shell redraws its own
    /// prompt on resize anyway.
    pub fn resize(&mut self, cols: usize, rows: usize) {
        let (cols, rows) = (cols.max(1), rows.max(1));
        if (cols, rows) == (self.cols, self.rows) {
            return;
        }
        let mut next = vec![Cell::default(); cols * rows];
        for r in 0..rows.min(self.rows) {
            for c in 0..cols.min(self.cols) {
                next[r * cols + c] = self.cells[r * self.cols + c];
            }
        }
        self.cells = next;
        self.cols = cols;
        self.rows = rows;
        self.row = self.row.min(rows - 1);
        self.col = self.col.min(cols - 1);
        self.scroll_top = 0;
        self.scroll_bottom = rows - 1;
        self.alternate = None;
    }

    fn at(&mut self, row: usize, col: usize) -> &mut Cell {
        let i = row * self.cols + col;
        &mut self.cells[i]
    }

    fn blank_row(&mut self, row: usize) {
        let style = Style {
            // Erasing keeps the current background, which is how a program
            // paints a coloured region.
            bg: self.style.bg,
            ..Style::default()
        };
        for c in 0..self.cols {
            *self.at(row, c) = Cell { ch: ' ', style };
        }
    }

    /// Move everything in the scroll region up one, pushing the top line into
    /// history when the region is the whole screen.
    fn scroll_up(&mut self, n: usize) {
        for _ in 0..n {
            if self.scroll_top == 0 && self.scroll_bottom == self.rows - 1 {
                let line = self.line(0).to_vec();
                self.scrollback.push_back(line);
                if self.scrollback.len() > SCROLLBACK {
                    self.scrollback.pop_front();
                }
            }
            for r in self.scroll_top..self.scroll_bottom {
                let (from, to) = ((r + 1) * self.cols, r * self.cols);
                self.cells.copy_within(from..from + self.cols, to);
            }
            let bottom = self.scroll_bottom;
            self.blank_row(bottom);
        }
    }

    fn scroll_down(&mut self, n: usize) {
        for _ in 0..n {
            let mut r = self.scroll_bottom;
            while r > self.scroll_top {
                let (from, to) = ((r - 1) * self.cols, r * self.cols);
                self.cells.copy_within(from..from + self.cols, to);
                r -= 1;
            }
            let top = self.scroll_top;
            self.blank_row(top);
        }
    }

    fn newline(&mut self) {
        if self.row == self.scroll_bottom {
            self.scroll_up(1);
        } else if self.row + 1 < self.rows {
            self.row += 1;
        }
    }
}

/// One CSI parameter, defaulting when absent or zero.
fn param(params: &vte::Params, i: usize, default: u16) -> u16 {
    params
        .iter()
        .nth(i)
        .and_then(|p| p.first().copied())
        .filter(|v| *v != 0)
        .unwrap_or(default)
}

fn param_raw(params: &vte::Params, i: usize) -> u16 {
    params
        .iter()
        .nth(i)
        .and_then(|p| p.first().copied())
        .unwrap_or(0)
}

impl vte::Perform for Grid {
    fn print(&mut self, c: char) {
        self.follow();
        if self.pending_wrap {
            self.col = 0;
            self.newline();
            self.pending_wrap = false;
        }
        let (row, col, style) = (self.row, self.col, self.style);
        *self.at(row, col) = Cell { ch: c, style };

        if self.col + 1 >= self.cols {
            // Deferred: a character in the last column must not scroll the
            // screen until something more is actually written.
            self.pending_wrap = true;
        } else {
            self.col += 1;
        }
    }

    fn execute(&mut self, byte: u8) {
        self.follow();
        match byte {
            b'\n' => {
                self.pending_wrap = false;
                self.newline();
            }
            b'\r' => {
                self.pending_wrap = false;
                self.col = 0;
            }
            b'\t' => {
                self.pending_wrap = false;
                self.col = ((self.col / 8) + 1) * 8;
                self.col = self.col.min(self.cols - 1);
            }
            0x08 => {
                self.pending_wrap = false;
                self.col = self.col.saturating_sub(1);
            }
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &vte::Params, intermediates: &[u8], _ignore: bool, c: char) {
        self.follow();
        self.pending_wrap = false;
        let private = intermediates.first() == Some(&b'?');

        match c {
            'A' => self.row = self.row.saturating_sub(param(params, 0, 1) as usize),
            'B' => self.row = (self.row + param(params, 0, 1) as usize).min(self.rows - 1),
            'C' => self.col = (self.col + param(params, 0, 1) as usize).min(self.cols - 1),
            'D' => self.col = self.col.saturating_sub(param(params, 0, 1) as usize),
            'E' => {
                self.row = (self.row + param(params, 0, 1) as usize).min(self.rows - 1);
                self.col = 0;
            }
            'F' => {
                self.row = self.row.saturating_sub(param(params, 0, 1) as usize);
                self.col = 0;
            }
            'G' => self.col = (param(params, 0, 1) as usize - 1).min(self.cols - 1),
            'H' | 'f' => {
                self.row = (param(params, 0, 1) as usize - 1).min(self.rows - 1);
                self.col = (param(params, 1, 1) as usize - 1).min(self.cols - 1);
            }
            'J' => {
                let (row, col) = (self.row, self.col);
                match param_raw(params, 0) {
                    0 => {
                        for c in col..self.cols {
                            *self.at(row, c) = Cell::default();
                        }
                        for r in row + 1..self.rows {
                            self.blank_row(r);
                        }
                    }
                    1 => {
                        for r in 0..row {
                            self.blank_row(r);
                        }
                        for c in 0..=col.min(self.cols - 1) {
                            *self.at(row, c) = Cell::default();
                        }
                    }
                    _ => {
                        for r in 0..self.rows {
                            self.blank_row(r);
                        }
                    }
                }
            }
            'K' => {
                let (row, col) = (self.row, self.col);
                let range = match param_raw(params, 0) {
                    0 => col..self.cols,
                    1 => 0..col.min(self.cols - 1) + 1,
                    _ => 0..self.cols,
                };
                let style = Style {
                    bg: self.style.bg,
                    ..Style::default()
                };
                for c in range {
                    *self.at(row, c) = Cell { ch: ' ', style };
                }
            }
            'L' => {
                // Insert lines at the cursor, within the scroll region.
                let n = param(params, 0, 1) as usize;
                if self.row >= self.scroll_top && self.row <= self.scroll_bottom {
                    let saved = self.scroll_top;
                    self.scroll_top = self.row;
                    self.scroll_down(n);
                    self.scroll_top = saved;
                }
            }
            'M' => {
                let n = param(params, 0, 1) as usize;
                if self.row >= self.scroll_top && self.row <= self.scroll_bottom {
                    let saved = self.scroll_top;
                    self.scroll_top = self.row;
                    self.scroll_up(n);
                    self.scroll_top = saved;
                }
            }
            '@' => {
                // Insert blanks, pushing the rest of the line right.
                let n = (param(params, 0, 1) as usize).min(self.cols - self.col);
                let (row, col) = (self.row, self.col);
                for c in (col + n..self.cols).rev() {
                    let moved = *self.at(row, c - n);
                    *self.at(row, c) = moved;
                }
                for c in col..col + n {
                    *self.at(row, c) = Cell::default();
                }
            }
            'P' => {
                let n = (param(params, 0, 1) as usize).min(self.cols - self.col);
                let (row, col) = (self.row, self.col);
                for c in col..self.cols - n {
                    let moved = *self.at(row, c + n);
                    *self.at(row, c) = moved;
                }
                for c in self.cols - n..self.cols {
                    *self.at(row, c) = Cell::default();
                }
            }
            'X' => {
                let n = (param(params, 0, 1) as usize).min(self.cols - self.col);
                let (row, col) = (self.row, self.col);
                for c in col..col + n {
                    *self.at(row, c) = Cell::default();
                }
            }
            'S' => {
                let n = param(params, 0, 1) as usize;
                self.scroll_up(n);
            }
            'T' => {
                let n = param(params, 0, 1) as usize;
                self.scroll_down(n);
            }
            'd' => self.row = (param(params, 0, 1) as usize - 1).min(self.rows - 1),
            'r' => {
                let top = param(params, 0, 1) as usize - 1;
                let bottom = param(params, 1, self.rows as u16) as usize - 1;
                if top < bottom && bottom < self.rows {
                    self.scroll_top = top;
                    self.scroll_bottom = bottom;
                    self.row = top;
                    self.col = 0;
                }
            }
            's' => self.saved = Some((self.row, self.col)),
            'u' => {
                if let Some((r, c)) = self.saved {
                    self.row = r.min(self.rows - 1);
                    self.col = c.min(self.cols - 1);
                }
            }
            'h' | 'l' => {
                let set = c == 'h';
                if private {
                    match param_raw(params, 0) {
                        25 => self.cursor_visible = set,
                        1049 | 47 | 1047 => {
                            if set {
                                if self.alternate.is_none() {
                                    self.alternate = Some(self.cells.clone());
                                    self.cells = vec![Cell::default(); self.cols * self.rows];
                                    self.row = 0;
                                    self.col = 0;
                                }
                            } else if let Some(saved) = self.alternate.take() {
                                self.cells = saved;
                            }
                        }
                        _ => {}
                    }
                }
            }
            'm' => self.sgr(params),
            'n' => {
                // Device Status Report. 6 is "where is the cursor", which is
                // the question ConPTY blocks on at startup.
                if param_raw(params, 0) == 6 {
                    let reply = format!("\x1b[{};{}R", self.row + 1, self.col + 1);
                    self.replies.extend_from_slice(reply.as_bytes());
                }
            }
            'c' => {
                // Primary Device Attributes: "what kind of terminal are you".
                // Claiming VT100 with no options is both true enough and the
                // least likely to invite sequences we do not implement.
                self.replies.extend_from_slice(b"\x1b[?1;0c");
            }
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, _intermediates: &[u8], _ignore: bool, byte: u8) {
        match byte {
            // Index / reverse index / next line.
            b'D' => self.newline(),
            b'M' => {
                if self.row == self.scroll_top {
                    self.scroll_down(1);
                } else {
                    self.row = self.row.saturating_sub(1);
                }
            }
            b'E' => {
                self.col = 0;
                self.newline();
            }
            b'c' => *self = Grid::new(self.cols, self.rows),
            _ => {}
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        // Window title, which the shell sets to the current directory.
        if matches!(params.first().copied(), Some(b"0") | Some(b"2")) {
            if let Some(title) = params.get(1) {
                self.title = Some(String::from_utf8_lossy(title).into_owned());
            }
        }
    }
}

impl Grid {
    /// Select Graphic Rendition: colours and attributes.
    fn sgr(&mut self, params: &vte::Params) {
        let flat: Vec<u16> = params
            .iter()
            .map(|p| p.first().copied().unwrap_or(0))
            .collect();
        if flat.is_empty() {
            self.style = Style::default();
            return;
        }

        let mut i = 0;
        while i < flat.len() {
            match flat[i] {
                0 => self.style = Style::default(),
                1 => self.style.bold = true,
                2 => self.style.dim = true,
                3 => self.style.italic = true,
                4 => self.style.underline = true,
                7 => self.style.reverse = true,
                22 => {
                    self.style.bold = false;
                    self.style.dim = false;
                }
                23 => self.style.italic = false,
                24 => self.style.underline = false,
                27 => self.style.reverse = false,
                30..=37 => self.style.fg = Ink::Indexed((flat[i] - 30) as u8),
                39 => self.style.fg = Ink::Default,
                40..=47 => self.style.bg = Ink::Indexed((flat[i] - 40) as u8),
                49 => self.style.bg = Ink::Default,
                90..=97 => self.style.fg = Ink::Indexed((flat[i] - 90 + 8) as u8),
                100..=107 => self.style.bg = Ink::Indexed((flat[i] - 100 + 8) as u8),
                // Extended colour: 38/48 then either 5;n or 2;r;g;b.
                38 | 48 => {
                    let target_fg = flat[i] == 38;
                    let ink = match flat.get(i + 1) {
                        Some(5) => {
                            let n = flat.get(i + 2).copied().unwrap_or(0) as u8;
                            i += 2;
                            Some(Ink::Indexed(n))
                        }
                        Some(2) => {
                            let r = flat.get(i + 2).copied().unwrap_or(0) as u8;
                            let g = flat.get(i + 3).copied().unwrap_or(0) as u8;
                            let b = flat.get(i + 4).copied().unwrap_or(0) as u8;
                            i += 4;
                            Some(Ink::Rgb(r, g, b))
                        }
                        _ => None,
                    };
                    if let Some(ink) = ink {
                        if target_fg {
                            self.style.fg = ink;
                        } else {
                            self.style.bg = ink;
                        }
                    }
                }
                _ => {}
            }
            i += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(grid: &mut Grid, bytes: &str) {
        let mut parser = vte::Parser::new();
        parser.advance(grid, bytes.as_bytes());
    }

    fn row_text(grid: &Grid, row: usize) -> String {
        grid.line(row).iter().map(|c| c.ch).collect::<String>()
    }

    fn trimmed(grid: &Grid, row: usize) -> String {
        row_text(grid, row).trim_end().to_string()
    }

    #[test]
    fn a_cursor_position_request_is_answered() {
        // The failure this prevents is total: ConPTY sends this before
        // anything else and waits for the reply, so without it the shell
        // never even prints a prompt and the terminal looks simply broken.
        let mut g = Grid::new(20, 5);
        feed(&mut g, "\x1b[3;7H\x1b[6n");
        assert_eq!(
            String::from_utf8(g.take_replies()).unwrap(),
            "\x1b[3;7R",
            "reported back 1-based, as it was asked"
        );
        assert!(
            g.take_replies().is_empty(),
            "taking an answer must not leave it queued to send twice"
        );
    }

    #[test]
    fn a_device_attributes_request_is_answered() {
        let mut g = Grid::new(20, 5);
        feed(&mut g, "\x1b[c");
        assert!(!g.take_replies().is_empty(), "an unanswered probe hangs");
    }

    #[test]
    fn nothing_is_owed_when_nothing_was_asked() {
        let mut g = Grid::new(20, 5);
        feed(&mut g, "just text\r\n");
        assert!(g.take_replies().is_empty());
    }

    #[test]
    fn plain_text_lands_on_the_screen() {
        let mut g = Grid::new(20, 4);
        feed(&mut g, "hello");
        assert_eq!(trimmed(&g, 0), "hello");
        assert_eq!(g.cursor(), (0, 5));
    }

    #[test]
    fn carriage_return_and_newline_do_different_things() {
        let mut g = Grid::new(20, 4);
        feed(&mut g, "one\r\ntwo");
        assert_eq!(trimmed(&g, 0), "one");
        assert_eq!(trimmed(&g, 1), "two");
    }

    #[test]
    fn a_bare_carriage_return_overwrites_the_same_line() {
        // How every progress bar in existence works.
        let mut g = Grid::new(20, 2);
        feed(&mut g, "50%\r99%");
        assert_eq!(trimmed(&g, 0), "99%");
        assert_eq!(g.cursor().0, 0, "must not have moved down");
    }

    #[test]
    fn the_last_column_does_not_scroll_until_something_more_is_written() {
        // Wrapping eagerly puts the cursor on the next line the moment the
        // last column is filled, which scrolls the screen a line early and
        // makes full-width output jump.
        let mut g = Grid::new(3, 2);
        feed(&mut g, "abc");
        assert_eq!(g.cursor(), (0, 2), "still on the first line");
        feed(&mut g, "d");
        assert_eq!(trimmed(&g, 0), "abc");
        assert_eq!(trimmed(&g, 1), "d");
    }

    #[test]
    fn output_past_the_bottom_scrolls_into_history() {
        let mut g = Grid::new(10, 2);
        feed(&mut g, "one\r\ntwo\r\nthree");
        assert_eq!(trimmed(&g, 0), "two");
        assert_eq!(trimmed(&g, 1), "three");
        assert_eq!(g.scrollback_len(), 1, "the first line is not lost");
    }

    #[test]
    fn scrollback_is_bounded() {
        let mut g = Grid::new(4, 1);
        for _ in 0..SCROLLBACK + 50 {
            feed(&mut g, "x\r\n");
        }
        assert_eq!(g.scrollback_len(), SCROLLBACK);
    }

    #[test]
    fn the_cursor_can_be_placed_absolutely() {
        let mut g = Grid::new(10, 5);
        feed(&mut g, "\x1b[3;4H");
        assert_eq!(
            g.cursor(),
            (2, 3),
            "rows and columns are 1-based on the wire"
        );
        feed(&mut g, "\x1b[H");
        assert_eq!(g.cursor(), (0, 0), "no parameters means home");
    }

    #[test]
    fn erasing_clears_what_it_says_it_does() {
        let mut g = Grid::new(6, 2);
        feed(&mut g, "abcdef\x1b[1;3H\x1b[K");
        assert_eq!(trimmed(&g, 0), "ab", "erase to end of line");

        let mut g = Grid::new(6, 2);
        feed(&mut g, "abcdef\r\nghijkl\x1b[2J");
        assert_eq!(trimmed(&g, 0), "");
        assert_eq!(trimmed(&g, 1), "");
    }

    #[test]
    fn colours_and_attributes_are_carried_on_the_cells() {
        let mut g = Grid::new(10, 2);
        feed(&mut g, "\x1b[1;31mred\x1b[0mplain");
        let cells = g.line(0);
        assert!(cells[0].style.bold);
        assert_eq!(cells[0].style.fg, Ink::Indexed(1));
        assert!(!cells[3].style.bold, "reset must actually reset");
        assert_eq!(cells[3].style.fg, Ink::Default);
    }

    #[test]
    fn twenty_four_bit_and_indexed_colour_both_work() {
        let mut g = Grid::new(10, 2);
        feed(&mut g, "\x1b[38;2;10;20;30mx");
        assert_eq!(g.line(0)[0].style.fg, Ink::Rgb(10, 20, 30));

        let mut g = Grid::new(10, 2);
        feed(&mut g, "\x1b[38;5;200my");
        assert_eq!(g.line(0)[0].style.fg, Ink::Indexed(200));
    }

    #[test]
    fn the_alternate_screen_is_restored_when_it_is_left() {
        // What every full-screen program does on exit. Losing the shell's
        // output here would be very noticeable.
        let mut g = Grid::new(20, 2);
        feed(&mut g, "shell output");
        feed(&mut g, "\x1b[?1049h");
        assert_eq!(trimmed(&g, 0), "", "the alternate screen starts blank");
        feed(&mut g, "full screen app");
        feed(&mut g, "\x1b[?1049l");
        assert_eq!(trimmed(&g, 0), "shell output", "and the shell comes back");
    }

    #[test]
    fn a_scroll_region_confines_scrolling_to_itself() {
        let mut g = Grid::new(6, 4);
        feed(&mut g, "\x1b[2;3r");
        feed(&mut g, "\x1b[1;1Hkeep");
        feed(&mut g, "\x1b[2;1Ha\r\nb\r\nc");
        assert_eq!(trimmed(&g, 0), "keep", "outside the region, untouched");
    }

    #[test]
    fn inserting_and_deleting_characters_shifts_the_line() {
        let mut g = Grid::new(8, 2);
        feed(&mut g, "abcdef\x1b[1;2H\x1b[2@");
        assert_eq!(trimmed(&g, 0), "a  bcdef");

        let mut g = Grid::new(8, 2);
        feed(&mut g, "abcdef\x1b[1;2H\x1b[2P");
        assert_eq!(trimmed(&g, 0), "adef");
    }

    #[test]
    fn the_title_is_picked_up_from_the_shell() {
        let mut g = Grid::new(10, 2);
        feed(&mut g, "\x1b]0;C:\\Users\\Dev\x07");
        assert_eq!(g.title.as_deref(), Some(r"C:\Users\Dev"));
    }

    #[test]
    fn the_cursor_can_be_hidden_and_shown() {
        let mut g = Grid::new(10, 2);
        feed(&mut g, "\x1b[?25l");
        assert!(!g.cursor_visible);
        feed(&mut g, "\x1b[?25h");
        assert!(g.cursor_visible);
    }

    #[test]
    fn a_sequence_split_across_reads_is_still_understood() {
        // Output arrives in whatever chunks the pipe hands over, and a escape
        // sequence cut in half is the normal case, not an edge one.
        let mut g = Grid::new(10, 2);
        let mut parser = vte::Parser::new();
        parser.advance(&mut g, b"\x1b[1;3");
        parser.advance(&mut g, b"H");
        assert_eq!(g.cursor(), (0, 2));
    }

    #[test]
    fn unknown_sequences_are_swallowed_rather_than_printed() {
        let mut g = Grid::new(20, 2);
        feed(&mut g, "\x1b[?2004hstart\x1b[?2004l");
        assert_eq!(
            trimmed(&g, 0),
            "start",
            "bracketed paste is not text the user should see"
        );
    }

    #[test]
    fn resizing_keeps_the_content_and_the_cursor_on_screen() {
        let mut g = Grid::new(20, 5);
        feed(&mut g, "hello\r\nworld");
        g.resize(10, 2);
        assert_eq!(trimmed(&g, 0), "hello");
        let (row, col) = g.cursor();
        assert!(row < 2 && col < 10, "cursor left the screen");
    }

    #[test]
    fn resizing_to_nothing_does_not_panic() {
        // A terminal can be dragged to zero, and one frame at that size must
        // not take the process down.
        let mut g = Grid::new(10, 3);
        g.resize(0, 0);
        assert_eq!(g.size(), (1, 1));
        feed(&mut g, "x\r\ny");
        assert_eq!(g.size(), (1, 1));
    }

    #[test]
    fn scrolling_back_shows_history_and_output_snaps_to_the_bottom() {
        let mut g = Grid::new(10, 2);
        feed(&mut g, "one\r\ntwo\r\nthree\r\nfour");
        g.scroll_view(2);
        let seen: Vec<String> = g
            .visible()
            .iter()
            .map(|l| {
                l.iter()
                    .map(|c| c.ch)
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect();
        assert_eq!(seen[0], "one", "scrolled back into history");

        // Anything printed has to bring the view back, or output would land
        // somewhere the user is not looking.
        feed(&mut g, "five");
        assert_eq!(g.view_offset, 0);
    }
}
