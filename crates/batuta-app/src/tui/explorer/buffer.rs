//! The editor's document: lines, their terminators, a cursor, and undo.
//!
//! No I/O and no drawing, so the part that can silently corrupt someone's file
//! is testable on its own.
//!
//! ## Why every edit is one `replace`
//!
//! Typing, Enter, Backspace and Delete are all expressed as "replace the range
//! `from..to` with this text". Insert is a replace over an empty range; a
//! newline is a replace whose text contains one. That means there is exactly
//! one function that mutates the document, one place that can get the
//! arithmetic wrong, and undo is the same operation with the arguments
//! swapped — an inverse that cannot drift away from the thing it inverts.
//!
//! ## Why line endings live here and not in the loader
//!
//! Files with mixed CRLF and LF are real: a merge artefact, or a file touched
//! by both WSL and Notepad. Normalising them on load and re-emitting one
//! convention on save rewrites *every line the user did not touch* — a
//! one-character fix produces a whole-file diff, and in a repository, a
//! whole-file conflict. Every byte is still valid, and the change is still
//! damage.
//!
//! So each line keeps its own terminator, and terminators move with the lines
//! they belong to. That only works if they are spliced by the same code that
//! splices the text, which is why they are a field here rather than metadata
//! held alongside.
//!
//! ## Bytes and characters
//!
//! The cursor counts **characters**; `String` indexes **bytes**. Mixing them
//! panics, and the release profile is `panic = "abort"`: no unwinding, no
//! recovery, and the unsaved buffer dies with the process. Every conversion
//! therefore goes through [`byte_of`], which saturates at the end of the line
//! instead of slicing past it, and nothing here indexes a `String` any other
//! way.

use std::cmp::Ordering;

/// A line terminator. Each line carries its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ending {
    Lf,
    Crlf,
}

impl Ending {
    pub fn as_str(self) -> &'static str {
        match self {
            Ending::Lf => "\n",
            Ending::Crlf => "\r\n",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Ending::Lf => "LF",
            Ending::Crlf => "CRLF",
        }
    }
}

/// A position in the document. `col` counts characters, never bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Cursor {
    pub line: usize,
    pub col: usize,
}

impl Cursor {
    pub fn new(line: usize, col: usize) -> Self {
        Cursor { line, col }
    }
}

impl PartialOrd for Cursor {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Cursor {
    fn cmp(&self, other: &Self) -> Ordering {
        self.line.cmp(&other.line).then(self.col.cmp(&other.col))
    }
}

/// The byte offset of character `col`, saturating at the end of the line.
///
/// Saturating rather than panicking is the point: a cursor that has drifted
/// past the end of a line is a bug, but killing the process over it loses the
/// user's unsaved work.
fn byte_of(line: &str, col: usize) -> usize {
    line.char_indices()
        .nth(col)
        .map(|(i, _)| i)
        .unwrap_or(line.len())
}

fn char_len(line: &str) -> usize {
    line.chars().count()
}

/// One undoable change: a range replaced by some text.
///
/// Both cursors are recorded, not just the text. Restoring `before` is what
/// puts you back where you were typing rather than wherever the change ended.
/// The removed terminators are recorded for the same reason the text is: a
/// multi-line delete consumes them, and an undo that guessed would quietly
/// convert those lines.
#[derive(Debug, Clone)]
struct Edit {
    from: Cursor,
    removed: String,
    removed_eols: Vec<Ending>,
    inserted: String,
    before: Cursor,
    after: Cursor,
}

#[derive(Debug, Clone)]
pub struct Buffer {
    /// Never empty: an empty document is one empty line, so `lines[line]` is
    /// always valid.
    lines: Vec<String>,
    /// One per line, same length as `lines`. `eols[i]` terminated line `i` in
    /// the original file; the last is only written when `trailing_newline`.
    eols: Vec<Ending>,
    /// The file ended with a terminator. A file that did not must not gain
    /// one, and a file that did must not lose it.
    trailing_newline: bool,
    /// The file began with a UTF-8 byte-order mark. Written back only if it
    /// was there: adding one breaks shell scripts and JSON parsers, removing
    /// one breaks whatever Windows tool insisted on it.
    bom: bool,
    /// What a newly typed Enter produces.
    dominant: Ending,

    cursor: Cursor,
    undo: Vec<Edit>,
    redo: Vec<Edit>,
    /// Undo depth at the last save, so undoing back to it clears the modified
    /// marker instead of leaving it stuck on. `None` means no reachable saved
    /// state.
    saved_at: Option<usize>,
    /// The last edit may still absorb more typing.
    coalescing: bool,
}

impl Default for Buffer {
    fn default() -> Self {
        Buffer::from_str("")
    }
}

impl Buffer {
    /// Build from already-split lines and their terminators.
    pub fn from_parts(
        lines: Vec<String>,
        eols: Vec<Ending>,
        trailing_newline: bool,
        bom: bool,
        dominant: Ending,
    ) -> Self {
        debug_assert_eq!(lines.len(), eols.len());
        Buffer {
            lines,
            eols,
            trailing_newline,
            bom,
            dominant,
            cursor: Cursor::default(),
            undo: Vec::new(),
            redo: Vec::new(),
            saved_at: Some(0),
            coalescing: false,
        }
    }

    /// Convenience for tests and scratch buffers: everything LF, no BOM.
    pub fn from_str(text: &str) -> Self {
        let trailing_newline = text.ends_with('\n');
        let body = if trailing_newline {
            &text[..text.len() - 1]
        } else {
            text
        };
        let lines: Vec<String> = body.split('\n').map(str::to_string).collect();
        let eols = vec![Ending::Lf; lines.len()];
        Buffer::from_parts(lines, eols, trailing_newline, false, Ending::Lf)
    }

    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    pub fn line(&self, n: usize) -> &str {
        self.lines.get(n).map(String::as_str).unwrap_or("")
    }

    pub fn len(&self) -> usize {
        self.lines.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lines.len() == 1 && self.lines[0].is_empty()
    }

    pub fn cursor(&self) -> Cursor {
        self.cursor
    }

    pub fn dominant(&self) -> Ending {
        self.dominant
    }

    /// True when the file uses more than one kind of terminator.
    pub fn mixed_endings(&self) -> bool {
        let mut seen = self.eols.iter();
        let Some(&first) = seen.next() else {
            return false;
        };
        seen.any(|&e| e != first)
    }

    pub fn modified(&self) -> bool {
        self.saved_at != Some(self.undo.len())
    }

    /// The document as one string, lines joined with `\n`, for display and
    /// searching. Not what gets written — see [`Buffer::to_bytes`].
    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    /// The document as the bytes it should occupy on disk.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.lines.iter().map(|l| l.len() + 2).sum::<usize>() + 3);
        if self.bom {
            out.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
        }
        let last = self.lines.len().saturating_sub(1);
        for (i, line) in self.lines.iter().enumerate() {
            out.extend_from_slice(line.as_bytes());
            // The final terminator is written only if the file had one, which
            // is what stops a save from appending a newline nobody asked for.
            if i < last || self.trailing_newline {
                out.extend_from_slice(self.eols[i].as_str().as_bytes());
            }
        }
        out
    }

    /// Mark the current contents as what is on disk.
    pub fn mark_saved(&mut self) {
        self.saved_at = Some(self.undo.len());
        // A save is a boundary: typing after it starts a new undo unit, so one
        // undo cannot reach back through the save point into a state the file
        // on disk never had.
        self.coalescing = false;
    }

    // ---- reading a range --------------------------------------------------

    fn slice(&self, from: Cursor, to: Cursor) -> String {
        if from.line == to.line {
            let l = self.line(from.line);
            let (a, b) = (byte_of(l, from.col), byte_of(l, to.col));
            return l[a.min(b)..a.max(b)].to_string();
        }
        let first = self.line(from.line);
        let mut out = first[byte_of(first, from.col)..].to_string();
        for n in (from.line + 1)..to.line {
            out.push('\n');
            out.push_str(self.line(n));
        }
        out.push('\n');
        let last = self.line(to.line);
        out.push_str(&last[..byte_of(last, to.col)]);
        out
    }

    /// Where the cursor ends up after `text` is inserted at `at`.
    fn advance(at: Cursor, text: &str) -> Cursor {
        match text.rfind('\n') {
            None => Cursor::new(at.line, at.col + char_len(text)),
            Some(i) => Cursor::new(
                at.line + text.matches('\n').count(),
                char_len(&text[i + 1..]),
            ),
        }
    }

    /// The one mutating operation.
    ///
    /// `keep_eols` supplies terminators for the rebuilt lines when undoing;
    /// otherwise new lines take the file's dominant ending. The **last**
    /// rebuilt line always inherits the terminator of the line that survives
    /// at `to`, which is what makes splitting and joining preserve the
    /// original terminator rather than converting the file a line at a time.
    fn replace(
        &mut self,
        from: Cursor,
        to: Cursor,
        text: &str,
        keep_eols: Option<&[Ending]>,
    ) -> (String, Vec<Ending>) {
        let (from, to) = if from <= to { (from, to) } else { (to, from) };
        let from = self.clamp(from);
        let to = self.clamp(to);

        let removed = self.slice(from, to);
        // The terminators of the lines this range consumes. The line at `to`
        // survives, so its terminator is not removed.
        let removed_eols: Vec<Ending> = self.eols[from.line..to.line].to_vec();

        let head = {
            let l = self.line(from.line);
            l[..byte_of(l, from.col)].to_string()
        };
        let tail = {
            let l = self.line(to.line);
            l[byte_of(l, to.col)..].to_string()
        };

        let merged = format!("{head}{text}{tail}");
        let rebuilt: Vec<String> = merged.split('\n').map(str::to_string).collect();

        let surviving = self.eols[to.line];
        let mut new_eols: Vec<Ending> = Vec::with_capacity(rebuilt.len());
        for i in 0..rebuilt.len().saturating_sub(1) {
            new_eols.push(
                keep_eols
                    .and_then(|k| k.get(i).copied())
                    .unwrap_or(self.dominant),
            );
        }
        new_eols.push(surviving);

        self.lines.splice(from.line..=to.line, rebuilt);
        self.eols.splice(from.line..=to.line, new_eols);
        debug_assert_eq!(self.lines.len(), self.eols.len());

        (removed, removed_eols)
    }

    fn clamp(&self, at: Cursor) -> Cursor {
        let line = at.line.min(self.lines.len().saturating_sub(1));
        Cursor::new(line, at.col.min(char_len(self.line(line))))
    }

    // ---- editing ----------------------------------------------------------

    fn apply(&mut self, from: Cursor, to: Cursor, text: &str, coalescable: bool) {
        let before = self.cursor;
        let (removed, removed_eols) = self.replace(from, to, text, None);
        let after = Self::advance(self.clamp(from), text);
        self.cursor = after;
        self.redo.clear();

        // Extend the previous entry when this is more of the same typing,
        // picking up exactly where it left off, so undo takes back a word
        // rather than a letter.
        if coalescable && self.coalescing {
            if let Some(last) = self.undo.last_mut() {
                if last.after == before && last.removed.is_empty() && removed.is_empty() {
                    last.inserted.push_str(text);
                    last.after = after;
                    return;
                }
            }
        }

        // Truncating the redo history can strand the saved point on a branch
        // that is no longer reachable; saying "modified" is the safe answer.
        if self.saved_at.is_some_and(|at| at > self.undo.len()) {
            self.saved_at = None;
        }

        self.undo.push(Edit {
            from,
            removed,
            removed_eols,
            inserted: text.to_string(),
            before,
            after,
        });
        self.coalescing = coalescable;
    }

    pub fn insert_char(&mut self, c: char) {
        let at = self.cursor;
        // A newline ends the run: undoing back through one is disorienting.
        self.apply(at, at, &c.to_string(), c != '\n');
    }

    pub fn insert_str(&mut self, text: &str) {
        let at = self.cursor;
        self.apply(at, at, text, false);
    }

    pub fn insert_newline(&mut self) {
        self.insert_char('\n');
    }

    /// Delete the character before the cursor, joining lines at column 0.
    pub fn backspace(&mut self) {
        let to = self.cursor;
        let from = if to.col > 0 {
            Cursor::new(to.line, to.col - 1)
        } else if to.line > 0 {
            Cursor::new(to.line - 1, char_len(self.line(to.line - 1)))
        } else {
            return;
        };
        // Deletions never join a typing run: backspacing then typing should
        // undo as two steps, not one.
        self.apply(from, to, "", false);
    }

    /// Delete the character under the cursor, joining lines at end of line.
    pub fn delete(&mut self) {
        let from = self.cursor;
        let line_len = char_len(self.line(from.line));
        let to = if from.col < line_len {
            Cursor::new(from.line, from.col + 1)
        } else if from.line + 1 < self.lines.len() {
            Cursor::new(from.line + 1, 0)
        } else {
            return;
        };
        self.apply(from, to, "", false);
    }

    // ---- undo -------------------------------------------------------------

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    pub fn undo(&mut self) -> bool {
        let Some(edit) = self.undo.pop() else {
            return false;
        };
        let end = Self::advance(edit.from, &edit.inserted);
        self.replace(edit.from, end, &edit.removed, Some(&edit.removed_eols));
        self.cursor = self.clamp(edit.before);
        self.coalescing = false;
        self.redo.push(edit);
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(edit) = self.redo.pop() else {
            return false;
        };
        let end = Self::advance(edit.from, &edit.removed);
        self.replace(edit.from, end, &edit.inserted, None);
        self.cursor = self.clamp(edit.after);
        self.coalescing = false;
        self.undo.push(edit);
        true
    }

    // ---- moving -----------------------------------------------------------

    /// Any deliberate move ends a typing run, so undo splits where the user
    /// paused rather than swallowing everything since the file opened.
    fn moved(&mut self) {
        self.coalescing = false;
    }

    /// Move left. `false` means there was nowhere left to go, which is what
    /// the focus rules read to decide whether to leave the pane.
    pub fn left(&mut self) -> bool {
        self.moved();
        if self.cursor.col > 0 {
            self.cursor.col -= 1;
        } else if self.cursor.line > 0 {
            // End of the previous line, as every editor does.
            self.cursor.line -= 1;
            self.cursor.col = char_len(self.line(self.cursor.line));
        } else {
            return false;
        }
        true
    }

    pub fn right(&mut self) -> bool {
        self.moved();
        let len = char_len(self.line(self.cursor.line));
        if self.cursor.col < len {
            self.cursor.col += 1;
        } else if self.cursor.line + 1 < self.lines.len() {
            self.cursor.line += 1;
            self.cursor.col = 0;
        } else {
            return false;
        }
        true
    }

    pub fn up(&mut self) -> bool {
        self.moved();
        if self.cursor.line == 0 {
            return false;
        }
        self.cursor.line -= 1;
        self.cursor = self.clamp(self.cursor);
        true
    }

    pub fn down(&mut self) -> bool {
        self.moved();
        if self.cursor.line + 1 >= self.lines.len() {
            return false;
        }
        self.cursor.line += 1;
        self.cursor = self.clamp(self.cursor);
        true
    }

    pub fn home(&mut self) {
        self.moved();
        self.cursor.col = 0;
    }

    pub fn end(&mut self) {
        self.moved();
        self.cursor.col = char_len(self.line(self.cursor.line));
    }

    pub fn goto(&mut self, at: Cursor) {
        self.moved();
        self.cursor = self.clamp(at);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buf(text: &str) -> Buffer {
        Buffer::from_str(text)
    }

    /// A document with the endings spelled out, as the loader would build it.
    fn mixed() -> Buffer {
        Buffer::from_parts(
            vec!["a".into(), "b".into(), "c".into()],
            vec![Ending::Crlf, Ending::Lf, Ending::Crlf],
            true,
            false,
            Ending::Crlf,
        )
    }

    #[test]
    fn splitting_text_into_lines_round_trips() {
        for text in ["", "a", "a\nb", "a\n\nb", "\n", "trailing\n"] {
            assert_eq!(buf(text).text(), text.trim_end_matches('\n'), "{text:?}");
            assert_eq!(
                String::from_utf8(buf(text).to_bytes()).unwrap(),
                text,
                "bytes for {text:?}"
            );
        }
    }

    #[test]
    fn each_line_keeps_its_own_terminator() {
        // Normalising here would rewrite every line of a file the user only
        // opened, turning a one-line fix into a whole-file diff.
        let b = mixed();
        assert_eq!(b.to_bytes(), b"a\r\nb\nc\r\n");
        assert!(b.mixed_endings());
    }

    #[test]
    fn editing_one_line_leaves_the_others_terminators_alone() {
        let mut b = mixed();
        b.goto(Cursor::new(1, 1));
        b.insert_char('!');
        assert_eq!(b.to_bytes(), b"a\r\nb!\nc\r\n", "only line 2 changed");
    }

    #[test]
    fn a_new_line_takes_the_dominant_ending_and_the_old_one_stays_put() {
        let mut b = mixed();
        // Split line 1 ("b", LF-terminated). The new first half gets the
        // file's dominant CRLF; the second half keeps the original LF.
        b.goto(Cursor::new(1, 1));
        b.insert_newline();
        assert_eq!(b.to_bytes(), b"a\r\nb\r\n\nc\r\n");
    }

    #[test]
    fn joining_two_lines_keeps_the_survivors_terminator() {
        let mut b = mixed();
        b.goto(Cursor::new(1, 0));
        b.backspace();
        // "a" and "b" merge; the joined line keeps line 1's LF.
        assert_eq!(b.to_bytes(), b"ab\nc\r\n");
    }

    #[test]
    fn undo_restores_the_terminators_a_multi_line_delete_consumed() {
        // The reason each edit records the removed endings: guessing would
        // convert those lines on the way back.
        let mut b = mixed();
        b.goto(Cursor::new(0, 0));
        for _ in 0..4 {
            b.delete();
        }
        assert_eq!(b.to_bytes(), b"c\r\n");

        // Deletions deliberately do not coalesce, so each undoes on its own.
        for _ in 0..4 {
            b.undo();
        }
        assert_eq!(b.to_bytes(), b"a\r\nb\nc\r\n", "endings must come back too");
    }

    #[test]
    fn a_file_without_a_trailing_newline_does_not_gain_one() {
        let b = Buffer::from_parts(vec!["a".into()], vec![Ending::Lf], false, false, Ending::Lf);
        assert_eq!(b.to_bytes(), b"a");
    }

    #[test]
    fn a_bom_is_written_back_only_when_it_was_there() {
        let with = Buffer::from_parts(vec!["a".into()], vec![Ending::Lf], false, true, Ending::Lf);
        assert_eq!(with.to_bytes(), [0xEF, 0xBB, 0xBF, b'a']);
        assert_eq!(buf("a").to_bytes(), b"a");
    }

    #[test]
    fn typing_inserts_at_the_cursor() {
        let mut b = buf("ac");
        b.goto(Cursor::new(0, 1));
        b.insert_char('b');
        assert_eq!(b.text(), "abc");
        assert_eq!(b.cursor(), Cursor::new(0, 2));
    }

    #[test]
    fn enter_splits_a_line_and_backspace_joins_it_again() {
        let mut b = buf("ab");
        b.goto(Cursor::new(0, 1));
        b.insert_newline();
        assert_eq!(b.text(), "a\nb");
        assert_eq!(b.cursor(), Cursor::new(1, 0));

        b.backspace();
        assert_eq!(b.text(), "ab");
        assert_eq!(b.cursor(), Cursor::new(0, 1));
    }

    #[test]
    fn multi_byte_characters_never_split_mid_character() {
        // The failure this guards is not a wrong result, it is a panic — and
        // with `panic = "abort"` that takes the unsaved buffer with it.
        let mut b = buf("héllo → wörld");
        b.end();
        for _ in 0..13 {
            b.backspace();
        }
        assert_eq!(b.text(), "");

        let mut b = buf("日本語");
        b.goto(Cursor::new(0, 1));
        b.insert_char('x');
        assert_eq!(b.text(), "日x本語");
        b.delete();
        assert_eq!(b.text(), "日x語");
    }

    #[test]
    fn backspace_at_the_very_start_does_nothing() {
        let mut b = buf("abc");
        b.backspace();
        assert_eq!(b.text(), "abc");
        assert!(!b.modified(), "a no-op must not mark the file dirty");
    }

    #[test]
    fn delete_at_the_very_end_does_nothing() {
        let mut b = buf("abc");
        b.end();
        b.delete();
        assert_eq!(b.text(), "abc");
        assert!(!b.modified());
    }

    #[test]
    fn a_typed_word_undoes_as_one_unit() {
        let mut b = buf("");
        for c in "hello".chars() {
            b.insert_char(c);
        }
        assert!(b.undo());
        assert_eq!(b.text(), "", "the whole run should go, not one letter");
        assert!(b.redo());
        assert_eq!(b.text(), "hello");
    }

    #[test]
    fn a_newline_breaks_the_typing_run() {
        let mut b = buf("");
        for c in "ab".chars() {
            b.insert_char(c);
        }
        b.insert_newline();
        for c in "cd".chars() {
            b.insert_char(c);
        }
        b.undo();
        assert_eq!(b.text(), "ab\n", "only the second run comes back");
        b.undo();
        assert_eq!(b.text(), "ab");
    }

    #[test]
    fn moving_the_cursor_breaks_the_typing_run() {
        let mut b = buf("xy");
        b.goto(Cursor::new(0, 0));
        b.insert_char('a');
        b.right();
        b.insert_char('b');
        b.undo();
        assert_eq!(b.text(), "axy", "the two inserts must undo separately");
    }

    #[test]
    fn undo_restores_the_cursor_not_just_the_text() {
        let mut b = buf("one\ntwo\nthree");
        b.goto(Cursor::new(1, 3));
        b.insert_str(" more");
        b.goto(Cursor::new(2, 0));

        b.undo();
        assert_eq!(b.text(), "one\ntwo\nthree");
        assert_eq!(b.cursor(), Cursor::new(1, 3), "back where the edit began");
    }

    #[test]
    fn a_new_edit_clears_the_redo_stack() {
        let mut b = buf("");
        b.insert_str("a");
        b.undo();
        assert!(b.can_redo());
        b.insert_str("b");
        assert!(!b.can_redo(), "redo after diverging would be a lie");
    }

    #[test]
    fn undoing_back_to_the_saved_state_clears_the_modified_marker() {
        // A bool would stay stuck on: the text matches the file on disk again,
        // so the marker has to say so.
        let mut b = buf("a");
        b.mark_saved();
        b.end();
        b.insert_char('b');
        assert!(b.modified());
        b.undo();
        assert!(!b.modified(), "back to exactly what was saved");
    }

    #[test]
    fn a_save_ends_the_typing_run() {
        let mut b = buf("");
        b.insert_char('a');
        b.mark_saved();
        b.insert_char('b');
        b.undo();
        assert_eq!(b.text(), "a", "undo must stop at the save");
    }

    #[test]
    fn edges_report_whether_they_moved() {
        // The focus rules read these: false means "nowhere left to go".
        let mut b = buf("ab\ncd");
        assert!(!b.up(), "already on the first line");
        assert!(!b.left(), "already at the very start");

        b.goto(Cursor::new(1, 2));
        assert!(!b.down(), "already on the last line");
        assert!(!b.right(), "already at the very end");

        b.goto(Cursor::new(1, 0));
        assert!(b.left(), "column 0 of a later line goes to the line above");
        assert_eq!(b.cursor(), Cursor::new(0, 2));
    }

    #[test]
    fn a_stale_cursor_is_clamped_rather_than_indexing_past_the_end() {
        let mut b = buf("ab");
        b.goto(Cursor::new(99, 99));
        assert_eq!(b.cursor(), Cursor::new(0, 2));
        b.insert_char('c');
        assert_eq!(b.text(), "abc");
    }

    #[test]
    fn lines_and_endings_stay_the_same_length_through_any_edit() {
        // If these ever diverge, `to_bytes` indexes past the end of one of
        // them, which under `panic = "abort"` is the end of the process.
        let mut b = mixed();
        b.goto(Cursor::new(0, 1));
        b.insert_newline();
        b.insert_str("x\ny\nz");
        for _ in 0..5 {
            b.delete();
        }
        b.backspace();
        b.undo();
        b.undo();
        b.redo();
        assert_eq!(b.lines().len(), b.eols.len());
    }

    #[test]
    fn a_long_single_line_file_still_edits_correctly() {
        let mut b = buf(&"x".repeat(200_000));
        b.end();
        b.insert_char('!');
        assert_eq!(b.len(), 1);
        assert!(b.text().ends_with("x!"));
        b.undo();
        assert_eq!(b.text().len(), 200_000);
    }
}
