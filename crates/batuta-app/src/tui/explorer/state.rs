//! The explorer's state and its keyboard.
//!
//! ## Why this owns its keys instead of extending the main dispatch
//!
//! Almost every existing binding is wrong or dangerous once a text buffer is
//! on screen. `Delete` deletes the highlighted **file from disk**; plain
//! characters type into the search query; `Ctrl+S` cycles the sort order;
//! `Enter` opens Explorer and quits; `Esc` exits the program. Guarding each of
//! those with "unless we are in the explorer" would leave the dangerous ones
//! one missed guard away from firing.
//!
//! So the explorer takes the keyboard outright. Only quit and leave are
//! handled above it, and `ask_delete` is not reachable from here at all —
//! structurally, not conditionally.
//!
//! ## Focus
//!
//! Arrows navigate inside a pane and cross at its edge, with two departures
//! from the literal rule, both because the literal rule is worse:
//!
//! - **The editor crosses left only at the very start of the buffer.** Left at
//!   column 0 of any other line goes to the end of the line above, as every
//!   editor does. Crossing at every line's column 0 would make the commonest
//!   motion in text editing randomly throw focus away.
//! - **The path bar never crosses horizontally.** It is a text field; Left and
//!   Right are how you edit it. Up and Down leave.
//!
//! `Ctrl+Left` and `Ctrl+Right` always cross, for when edge-crossing is more
//! fiddly than useful.

use std::path::{Path, PathBuf};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::buffer::{Buffer, Cursor};
use super::file::{self, Loaded, Stamp};
use super::tree::{Lister, Row, Tree};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Tree,
    Editor,
    Path,
}

/// The file currently open in the editor.
#[derive(Debug)]
pub struct Doc {
    pub path: PathBuf,
    pub buffer: Buffer,
    /// What the file looked like when it was read, so a save can tell that
    /// something else has written to it since.
    pub stamp: Option<Stamp>,
}

/// Something the explorer cannot decide on its own, handed back to the caller.
///
/// Leaving, quitting and opening a different file all have to pass the
/// unsaved-changes guard, and that guard lives with the rest of the app's
/// modal state rather than being duplicated here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    None,
    /// Back to the search view.
    Leave,
    Quit,
    /// Load this file, discarding whatever is open.
    Open(PathBuf),
    Save,
}

pub struct Explorer {
    pub tree: Tree,
    pub focus: Focus,
    pub doc: Option<Doc>,
    /// Shown in place of the editor: a binary file, one too large, or an error.
    pub notice: Option<String>,
    /// The path bar's text and caret, in characters.
    pub path_text: String,
    pub path_caret: usize,
    /// First visible line of the editor.
    pub top_line: usize,
    /// The tree flattened for drawing.
    ///
    /// Rebuilt by [`Explorer::sync`] before each frame rather than by the
    /// renderer, because flattening reads directories and the renderer only
    /// has a shared borrow — the same split the rest of the UI already keeps
    /// between preparing data and drawing it.
    pub rows: Vec<Row>,
    /// The pane focus returns to from the path bar.
    last_pane: Focus,
}

impl Explorer {
    pub fn new(root: PathBuf) -> Self {
        let path_text = root.display().to_string();
        Explorer {
            tree: Tree::new(root),
            focus: Focus::Tree,
            doc: None,
            notice: None,
            path_caret: path_text.chars().count(),
            path_text,
            top_line: 0,
            rows: Vec::new(),
            last_pane: Focus::Tree,
        }
    }

    pub fn modified(&self) -> bool {
        self.doc.as_ref().is_some_and(|d| d.buffer.modified())
    }

    pub fn open_path(&self) -> Option<&Path> {
        self.doc.as_ref().map(|d| d.path.as_path())
    }

    /// Load a file into the editor, replacing whatever was there.
    ///
    /// Callers are responsible for having passed the unsaved-changes guard
    /// first; this discards without asking.
    pub fn load(&mut self, path: &Path) {
        self.top_line = 0;
        self.path_text = path.display().to_string();
        self.path_caret = self.path_text.chars().count();

        match file::load(path) {
            Ok(Loaded::Text(buffer)) => {
                self.notice = None;
                self.doc = Some(Doc {
                    path: path.to_path_buf(),
                    buffer: *buffer,
                    stamp: Stamp::of(path).ok(),
                });
            }
            Ok(Loaded::Rejected(why)) => {
                self.doc = None;
                self.notice = Some(why);
            }
            Err(e) => {
                self.doc = None;
                self.notice = Some(format!("Could not read this file: {e}"));
            }
        }
    }

    /// Write the open file back. Returns what to tell the user.
    pub fn save(&mut self) -> String {
        let Some(doc) = self.doc.as_mut() else {
            return "nothing to save".into();
        };
        match file::save(&doc.path, &doc.buffer.to_bytes(), doc.stamp) {
            Ok(file::Saved::Written) => {
                doc.buffer.mark_saved();
                doc.stamp = Stamp::of(&doc.path).ok();
                format!("saved {}", doc.path.display())
            }
            Ok(file::Saved::ReadOnly) => {
                "this file is read-only; Batuta will not clear that for you".into()
            }
            Ok(file::Saved::ChangedOnDisk) => {
                "this file changed on disk since you opened it; press F5 to reload".into()
            }
            Err(e) => format!("could not save: {e}"),
        }
    }

    /// Rebuild the flattened tree, and keep the selection inside it.
    pub fn sync(&mut self, lister: &dyn Lister) {
        self.rows = self.tree.rows(lister);
        if self.tree.selected >= self.rows.len() {
            self.tree.selected = self.rows.len().saturating_sub(1);
        }
    }

    /// The tree row under the selection.
    pub fn selected_row(&self) -> Option<&Row> {
        self.rows.get(self.tree.selected)
    }

    /// Keep the caret's line inside the visible window.
    pub fn scroll_into_view(&mut self, visible: usize) {
        let Some(doc) = &self.doc else { return };
        if visible == 0 {
            return;
        }
        let line = doc.buffer.cursor().line;
        if line < self.top_line {
            self.top_line = line;
        } else if line >= self.top_line + visible {
            self.top_line = line + 1 - visible;
        }
    }

    // ---- the keyboard -----------------------------------------------------

    pub fn key(&mut self, key: KeyEvent, visible: usize, lister: &dyn Lister) -> Action {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);

        // Always available, whatever has focus.
        match key.code {
            // Both cases: Caps Lock makes these arrive uppercase, and a
            // shortcut that quietly stops working is worse than one that
            // never existed.
            KeyCode::Char('c' | 'C') if ctrl => return Action::Quit,
            KeyCode::Char('s' | 'S') if ctrl => return Action::Save,
            KeyCode::Esc => return Action::Leave,
            KeyCode::F(5) => {
                self.tree.refresh();
                return Action::None;
            }
            // The explicit way across, whatever the caret is doing.
            // Checked before the per-pane arrows so it always wins.
            KeyCode::Left if ctrl => {
                self.cross_left();
                return Action::None;
            }
            KeyCode::Right if ctrl => {
                self.cross_right();
                return Action::None;
            }
            KeyCode::Down if ctrl => {
                self.cross_down();
                return Action::None;
            }
            KeyCode::Up if ctrl => {
                self.cross_up();
                return Action::None;
            }
            _ => {}
        }

        match self.focus {
            Focus::Tree => self.key_tree(key, lister),
            Focus::Editor => self.key_editor(key, visible, shift, ctrl),
            Focus::Path => self.key_path(key),
        }
    }

    fn cross_left(&mut self) {
        self.focus = match self.focus {
            Focus::Editor | Focus::Path => Focus::Tree,
            Focus::Tree => Focus::Tree,
        };
    }

    /// Into the path bar, remembering where to come back to.
    fn cross_down(&mut self) {
        if self.focus != Focus::Path {
            self.last_pane = self.focus;
            self.focus = Focus::Path;
        }
    }

    /// Back out of the path bar. Above the other two panes there is nothing,
    /// so this only does something from there.
    fn cross_up(&mut self) {
        if self.focus == Focus::Path {
            self.focus = self.last_pane;
        }
    }

    fn cross_right(&mut self) {
        self.focus = match self.focus {
            Focus::Tree | Focus::Path => Focus::Editor,
            Focus::Editor => Focus::Editor,
        };
    }

    fn key_tree(&mut self, key: KeyEvent, lister: &dyn Lister) -> Action {
        let rows = self.tree.rows(lister);
        let here = rows.get(self.tree.selected).cloned();

        match key.code {
            KeyCode::Up => {
                self.tree.selected = self.tree.selected.saturating_sub(1);
            }
            KeyCode::Down => {
                if self.tree.selected + 1 < rows.len() {
                    self.tree.selected += 1;
                } else {
                    // Nowhere further down inside the pane.
                    self.last_pane = Focus::Tree;
                    self.focus = Focus::Path;
                }
            }
            KeyCode::Left => {
                if let Some(row) = &here {
                    if row.is_dir && row.expanded {
                        self.tree.collapse(&row.path);
                    } else if let Some(parent) = row.path.parent() {
                        // Walk out to the parent's row rather than leaving:
                        // there is no pane to the tree's left.
                        if let Some(i) = rows.iter().position(|r| r.path == parent) {
                            self.tree.selected = i;
                        }
                    }
                }
            }
            KeyCode::Right => match &here {
                Some(row) if row.is_dir && !row.expanded => self.tree.expand(&row.path),
                _ => self.focus = Focus::Editor,
            },
            KeyCode::Enter => {
                if let Some(row) = &here {
                    if row.is_dir {
                        self.tree.toggle(&row.path);
                    } else {
                        return Action::Open(row.path.clone());
                    }
                }
            }
            KeyCode::Home => self.tree.selected = 0,
            KeyCode::End => self.tree.selected = rows.len().saturating_sub(1),
            _ => {}
        }
        Action::None
    }

    fn key_editor(&mut self, key: KeyEvent, visible: usize, shift: bool, ctrl: bool) -> Action {
        let Some(doc) = self.doc.as_mut() else {
            // No document: the pane is a message, so only leaving it applies.
            if matches!(key.code, KeyCode::Left) {
                self.focus = Focus::Tree;
            }
            return Action::None;
        };

        match key.code {
            KeyCode::Left => {
                // Crossing only at the very start of the buffer. Anywhere else
                // this is the ordinary "wrap to the end of the line above".
                if !doc.buffer.left() {
                    self.focus = Focus::Tree;
                }
            }
            KeyCode::Right => {
                doc.buffer.right();
            }
            KeyCode::Up => {
                doc.buffer.up();
            }
            KeyCode::Down => {
                if !doc.buffer.down() {
                    self.last_pane = Focus::Editor;
                    self.focus = Focus::Path;
                }
            }
            KeyCode::Home => doc.buffer.home(),
            KeyCode::End => doc.buffer.end(),
            KeyCode::PageUp => {
                let to = doc.buffer.cursor().line.saturating_sub(visible.max(1));
                doc.buffer.goto(Cursor::new(to, 0));
            }
            KeyCode::PageDown => {
                let to = doc.buffer.cursor().line + visible.max(1);
                doc.buffer.goto(Cursor::new(to, 0));
            }
            KeyCode::Enter => doc.buffer.insert_newline(),
            KeyCode::Tab => doc.buffer.insert_str("    "),
            KeyCode::Backspace => doc.buffer.backspace(),
            KeyCode::Delete => doc.buffer.delete(),
            KeyCode::Char('z' | 'Z') if ctrl => {
                doc.buffer.undo();
            }
            KeyCode::Char('y' | 'Y') if ctrl => {
                doc.buffer.redo();
            }
            KeyCode::Char(c) if !ctrl => {
                let _ = shift;
                doc.buffer.insert_char(c);
            }
            _ => {}
        }
        self.scroll_into_view(visible);
        Action::None
    }

    fn key_path(&mut self, key: KeyEvent) -> Action {
        let chars = self.path_text.chars().count();
        match key.code {
            // Never crosses: this is a text field, and horizontal motion is
            // the whole point of one.
            KeyCode::Left => self.path_caret = self.path_caret.saturating_sub(1),
            KeyCode::Right => self.path_caret = (self.path_caret + 1).min(chars),
            KeyCode::Home => self.path_caret = 0,
            KeyCode::End => self.path_caret = chars,
            KeyCode::Up | KeyCode::Down => self.focus = self.last_pane,
            KeyCode::Backspace => {
                if self.path_caret > 0 {
                    let at = byte_at(&self.path_text, self.path_caret - 1);
                    self.path_text.remove(at);
                    self.path_caret -= 1;
                }
            }
            KeyCode::Delete => {
                if self.path_caret < chars {
                    let at = byte_at(&self.path_text, self.path_caret);
                    self.path_text.remove(at);
                }
            }
            KeyCode::Char(c) => {
                let at = byte_at(&self.path_text, self.path_caret);
                self.path_text.insert(at, c);
                self.path_caret += 1;
            }
            KeyCode::Enter => {
                let path = PathBuf::from(self.path_text.trim());
                if path.is_dir() {
                    self.tree.set_root(path);
                    self.focus = Focus::Tree;
                } else if path.is_file() {
                    return Action::Open(path);
                } else {
                    self.notice = Some(format!("No such path: {}", self.path_text.trim()));
                }
            }
            _ => {}
        }
        Action::None
    }
}

/// Byte offset of character `n`, saturating at the end.
///
/// Same rule as the buffer's: a caret that has drifted must not slice past the
/// end of the string, because `panic = "abort"` makes that fatal.
fn byte_at(s: &str, n: usize) -> usize {
    s.char_indices().nth(n).map(|(i, _)| i).unwrap_or(s.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::explorer::tree::Entry;
    use std::collections::HashMap;

    struct Fake(HashMap<PathBuf, Vec<Entry>>);

    impl Fake {
        fn new(entries: &[(&str, &[(&str, bool)])]) -> Self {
            let mut m = HashMap::new();
            for (dir, kids) in entries {
                m.insert(
                    PathBuf::from(dir),
                    kids.iter()
                        .map(|(n, d)| Entry {
                            name: (*n).to_string(),
                            is_dir: *d,
                        })
                        .collect(),
                );
            }
            Fake(m)
        }
    }

    impl Lister for Fake {
        fn list(&self, dir: &Path) -> Result<Vec<Entry>, String> {
            Ok(self.0.get(dir).cloned().unwrap_or_default())
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    fn with_doc() -> Explorer {
        let mut x = Explorer::new(r"C:\p".into());
        x.doc = Some(Doc {
            path: r"C:\p\a.txt".into(),
            buffer: Buffer::from_str("one\ntwo"),
            stamp: None,
        });
        x
    }

    #[test]
    fn the_editor_crosses_left_only_at_the_very_start_of_the_buffer() {
        // Crossing at every line's column 0 would throw focus away during the
        // commonest motion in text editing.
        let fs = Fake::new(&[]);
        let mut x = with_doc();
        x.focus = Focus::Editor;

        let doc = x.doc.as_mut().unwrap();
        doc.buffer.goto(Cursor::new(1, 0));
        x.key(key(KeyCode::Left), 10, &fs);
        assert_eq!(x.focus, Focus::Editor, "still editing");
        assert_eq!(
            x.doc.as_ref().unwrap().buffer.cursor(),
            Cursor::new(0, 3),
            "went to the end of the line above"
        );

        // Now genuinely at the start, with nowhere else to go.
        x.key(key(KeyCode::Left), 10, &fs);
        x.key(key(KeyCode::Left), 10, &fs);
        x.key(key(KeyCode::Left), 10, &fs);
        assert_eq!(x.doc.as_ref().unwrap().buffer.cursor(), Cursor::new(0, 0));
        x.key(key(KeyCode::Left), 10, &fs);
        assert_eq!(x.focus, Focus::Tree, "only now does it cross");
    }

    #[test]
    fn the_editor_crosses_down_into_the_path_bar_at_the_last_line() {
        let fs = Fake::new(&[]);
        let mut x = with_doc();
        x.focus = Focus::Editor;

        x.key(key(KeyCode::Down), 10, &fs);
        assert_eq!(x.focus, Focus::Editor, "there was another line");
        x.key(key(KeyCode::Down), 10, &fs);
        assert_eq!(x.focus, Focus::Path);
    }

    #[test]
    fn the_path_bar_never_gives_up_focus_sideways() {
        // It is a text field. Left at column 0 teleporting away would make it
        // unusable.
        let fs = Fake::new(&[]);
        let mut x = with_doc();
        x.focus = Focus::Path;
        x.path_caret = 0;

        x.key(key(KeyCode::Left), 10, &fs);
        assert_eq!(x.focus, Focus::Path);
        x.path_caret = x.path_text.chars().count();
        x.key(key(KeyCode::Right), 10, &fs);
        assert_eq!(x.focus, Focus::Path);
    }

    #[test]
    fn the_path_bar_returns_to_whichever_pane_sent_you_there() {
        let fs = Fake::new(&[]);
        let mut x = with_doc();

        x.focus = Focus::Editor;
        x.key(key(KeyCode::Down), 10, &fs);
        x.key(key(KeyCode::Down), 10, &fs);
        assert_eq!(x.focus, Focus::Path);
        x.key(key(KeyCode::Up), 10, &fs);
        assert_eq!(x.focus, Focus::Editor, "back where it came from");
    }

    #[test]
    fn the_explorers_shortcuts_survive_caps_lock() {
        let fs = Fake::new(&[]);
        let mut x = with_doc();
        assert_eq!(
            x.key(
                KeyEvent::new(KeyCode::Char('S'), KeyModifiers::CONTROL),
                10,
                &fs
            ),
            Action::Save,
            "Ctrl+Shift+S must still save"
        );

        x.focus = Focus::Editor;
        x.key(key(KeyCode::Char('X')), 10, &fs);
        x.key(
            KeyEvent::new(KeyCode::Char('Z'), KeyModifiers::CONTROL),
            10,
            &fs,
        );
        assert_eq!(
            x.doc.as_ref().unwrap().buffer.text(),
            "one\ntwo",
            "Ctrl+Shift+Z must still undo"
        );
    }

    #[test]
    fn a_capital_letter_without_ctrl_is_typed_into_the_buffer() {
        // Capitals must reach the file, not be mistaken for a shortcut.
        let fs = Fake::new(&[]);
        let mut x = with_doc();
        x.focus = Focus::Editor;
        x.doc.as_mut().unwrap().buffer.goto(Cursor::new(0, 0));
        for c in "SZY".chars() {
            x.key(key(KeyCode::Char(c)), 10, &fs);
        }
        assert_eq!(x.doc.as_ref().unwrap().buffer.text(), "SZYone\ntwo");
    }

    #[test]
    fn ctrl_arrows_reach_every_pane_including_the_path_bar() {
        let fs = Fake::new(&[]);
        let mut x = with_doc();
        x.focus = Focus::Editor;

        x.key(ctrl(KeyCode::Down), 10, &fs);
        assert_eq!(x.focus, Focus::Path);
        x.key(ctrl(KeyCode::Up), 10, &fs);
        assert_eq!(x.focus, Focus::Editor, "back where it came from");

        x.key(ctrl(KeyCode::Left), 10, &fs);
        assert_eq!(x.focus, Focus::Tree);
        x.key(ctrl(KeyCode::Down), 10, &fs);
        assert_eq!(x.focus, Focus::Path);
        x.key(ctrl(KeyCode::Up), 10, &fs);
        assert_eq!(x.focus, Focus::Tree, "and back to the tree this time");
    }

    #[test]
    fn ctrl_arrows_cross_regardless_of_position() {
        let fs = Fake::new(&[]);
        let mut x = with_doc();
        x.focus = Focus::Editor;
        x.doc.as_mut().unwrap().buffer.goto(Cursor::new(1, 2));

        x.key(ctrl(KeyCode::Left), 10, &fs);
        assert_eq!(x.focus, Focus::Tree, "mid-line and still crossed");
        x.key(ctrl(KeyCode::Right), 10, &fs);
        assert_eq!(x.focus, Focus::Editor);
    }

    #[test]
    fn typing_reaches_the_buffer_and_not_the_search_query() {
        let fs = Fake::new(&[]);
        let mut x = with_doc();
        x.focus = Focus::Editor;
        x.doc.as_mut().unwrap().buffer.goto(Cursor::new(0, 0));

        for c in "hi ".chars() {
            x.key(key(KeyCode::Char(c)), 10, &fs);
        }
        assert_eq!(x.doc.as_ref().unwrap().buffer.text(), "hi one\ntwo");
    }

    #[test]
    fn delete_removes_a_character_and_cannot_reach_a_file() {
        // The binding that matters most: outside the explorer this key deletes
        // the highlighted file from disk.
        let fs = Fake::new(&[]);
        let mut x = with_doc();
        x.focus = Focus::Editor;
        x.doc.as_mut().unwrap().buffer.goto(Cursor::new(0, 0));

        x.key(key(KeyCode::Delete), 10, &fs);
        assert_eq!(x.doc.as_ref().unwrap().buffer.text(), "ne\ntwo");
    }

    #[test]
    fn ctrl_s_saves_rather_than_cycling_the_sort_order() {
        let fs = Fake::new(&[]);
        let mut x = with_doc();
        assert_eq!(x.key(ctrl(KeyCode::Char('s')), 10, &fs), Action::Save);
    }

    #[test]
    fn enter_on_a_directory_expands_it_and_on_a_file_opens_it() {
        let fs = Fake::new(&[(r"C:\p", &[("src", true), ("a.txt", false)])]);
        let mut x = Explorer::new(r"C:\p".into());

        assert_eq!(x.key(key(KeyCode::Enter), 10, &fs), Action::None);
        assert!(x.tree.is_expanded(Path::new(r"C:\p\src")));

        x.tree.selected = 1;
        assert_eq!(
            x.key(key(KeyCode::Enter), 10, &fs),
            Action::Open(PathBuf::from(r"C:\p\a.txt"))
        );
    }

    #[test]
    fn right_expands_a_closed_directory_before_it_crosses() {
        let fs = Fake::new(&[(r"C:\p", &[("src", true)])]);
        let mut x = Explorer::new(r"C:\p".into());

        x.key(key(KeyCode::Right), 10, &fs);
        assert!(x.tree.is_expanded(Path::new(r"C:\p\src")));
        assert_eq!(x.focus, Focus::Tree, "the first Right did the expanding");

        x.key(key(KeyCode::Right), 10, &fs);
        assert_eq!(x.focus, Focus::Editor, "nothing left to expand");
    }

    #[test]
    fn esc_asks_to_leave_rather_than_quitting_outright() {
        // Outside the explorer this key exits the program.
        let fs = Fake::new(&[]);
        let mut x = with_doc();
        assert_eq!(x.key(key(KeyCode::Esc), 10, &fs), Action::Leave);
    }

    #[test]
    fn undo_is_reachable_from_the_editor() {
        let fs = Fake::new(&[]);
        let mut x = with_doc();
        x.focus = Focus::Editor;
        x.key(key(KeyCode::Char('X')), 10, &fs);
        assert!(x.doc.as_ref().unwrap().buffer.text().starts_with('X'));
        x.key(ctrl(KeyCode::Char('z')), 10, &fs);
        assert_eq!(x.doc.as_ref().unwrap().buffer.text(), "one\ntwo");
    }

    #[test]
    fn the_caret_stays_on_screen_as_it_moves() {
        let fs = Fake::new(&[]);
        let mut x = Explorer::new(r"C:\p".into());
        x.doc = Some(Doc {
            path: r"C:\p\long.txt".into(),
            buffer: Buffer::from_str(&"line\n".repeat(60)),
            stamp: None,
        });
        x.focus = Focus::Editor;

        for _ in 0..30 {
            x.key(key(KeyCode::Down), 10, &fs);
        }
        let line = x.doc.as_ref().unwrap().buffer.cursor().line;
        assert!(
            line >= x.top_line && line < x.top_line + 10,
            "caret at {line} outside window starting {}",
            x.top_line
        );
    }

    #[test]
    fn editing_the_path_bar_handles_multi_byte_characters() {
        let fs = Fake::new(&[]);
        let mut x = Explorer::new(r"C:\p".into());
        x.focus = Focus::Path;
        x.path_text = "C:\\héllo".into();
        x.path_caret = x.path_text.chars().count();

        x.key(key(KeyCode::Backspace), 10, &fs);
        assert_eq!(x.path_text, "C:\\héll", "one character, not one byte");

        // Delete the multi-byte character itself: a byte offset used where a
        // character index belongs slices mid-character, and `panic = "abort"`
        // makes that the end of the process.
        x.path_caret = 4;
        x.key(key(KeyCode::Delete), 10, &fs);
        assert_eq!(x.path_text, "C:\\hll");

        // And backspace over one from the right.
        x.path_text = "ä".into();
        x.path_caret = 1;
        x.key(key(KeyCode::Backspace), 10, &fs);
        assert_eq!(x.path_text, "");
    }
}
