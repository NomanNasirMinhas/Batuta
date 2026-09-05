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
use super::find::Find;
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
    /// Open a shell in the highlighted folder.
    OpenTerminal,
    /// Complete the path bar. Handled by the caller because it needs a
    /// directory listing, which is the caller's to supply.
    Complete,
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
    /// Find-in-file. Present whether or not the prompt is showing, so `F3`
    /// keeps working after it is closed.
    pub find: Find,
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
            find: Find::default(),
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
        // Hits belong to the file they were found in. Carrying them into
        // another document would highlight lines at random.
        self.find = Find::default();

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

        // The find prompt, while it is showing, owns the keyboard outright.
        // Handled before everything else so a typed `s` narrows the search
        // rather than saving the file.
        if self.find.open {
            return self.key_find(key, visible, ctrl);
        }

        // Always available, whatever has focus.
        match key.code {
            // Ctrl+F rather than `/`: in an editor a slash is a character
            // someone is trying to type, and stealing it would be unusable.
            KeyCode::Char('f' | 'F') if ctrl => {
                self.open_find(visible);
                return Action::None;
            }
            // Stepping between hits without the prompt in the way, using the
            // query it left behind.
            KeyCode::F(3) => {
                self.step_find(!shift, visible);
                return Action::None;
            }
            // Both cases: Caps Lock makes these arrive uppercase, and a
            // shortcut that quietly stops working is worse than one that
            // never existed.
            // Ctrl+C opens a terminal here rather than quitting; Ctrl+Q is
            // what ends the program now.
            KeyCode::Char('c' | 'C') if ctrl => return Action::OpenTerminal,
            KeyCode::Char('q' | 'Q') if ctrl => return Action::Quit,
            KeyCode::Char('s' | 'S') if ctrl => return Action::Save,
            // The way out is the key that came in. Esc used to leave, which
            // made it far too easy to lose an editor by reflex.
            KeyCode::Char('e' | 'E') if ctrl => return Action::Leave,
            KeyCode::Esc => return Action::None,
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

    // ---- find in file -----------------------------------------------------

    /// Show the find prompt, if there is anything to search.
    ///
    /// A binary file or an unreadable one has no document, and a find bar over
    /// a message panel would be a prompt that can never match.
    fn open_find(&mut self, visible: usize) {
        let Some(doc) = self.doc.as_ref() else {
            return;
        };
        self.focus = Focus::Editor;
        let at = doc.buffer.cursor();
        let lines = doc.buffer.lines().to_vec();
        self.find.open(at, &lines);
        self.go_to_hit(visible);
    }

    /// Move to the hit the find state is pointing at, if any.
    fn go_to_hit(&mut self, visible: usize) {
        let Some(at) = self.find.cursor() else {
            return;
        };
        if let Some(doc) = self.doc.as_mut() {
            doc.buffer.goto(at);
        }
        self.scroll_into_view(visible);
    }

    /// `F3` and `Shift+F3` outside the prompt, on the remembered query.
    ///
    /// Re-running the search rather than trusting the stored hits: the file may
    /// have been edited since, and stepping to a position that no longer holds
    /// the word is worse than finding nothing.
    fn step_find(&mut self, forward: bool, visible: usize) {
        if self.find.query.is_empty() {
            return;
        }
        let Some(doc) = self.doc.as_ref() else {
            return;
        };
        let lines = doc.buffer.lines().to_vec();
        // Anchored where the cursor is, not where the search started, so
        // repeated F3 walks forward instead of restarting.
        self.find.rescan(&lines, doc.buffer.cursor());
        if forward {
            self.find.next();
        } else {
            self.find.previous();
        }
        self.go_to_hit(visible);
    }

    /// The keyboard while the find prompt is showing.
    fn key_find(&mut self, key: KeyEvent, visible: usize, ctrl: bool) -> Action {
        let lines = match self.doc.as_ref() {
            Some(doc) => doc.buffer.lines().to_vec(),
            // The document went away underneath the prompt; close it rather
            // than searching nothing.
            None => {
                self.find.open = false;
                return Action::None;
            }
        };

        match key.code {
            // Accept: keep the cursor where the search put it, and leave the
            // query behind for F3.
            KeyCode::Enter if !ctrl => {
                self.find.open = false;
                return Action::None;
            }
            // Abandon: back to where the search started. This is the one place
            // Esc still does something, and it closes a prompt rather than a
            // view, so it cannot lose an editor.
            KeyCode::Esc => {
                self.find.open = false;
                let origin = self.find.origin();
                if let Some(doc) = self.doc.as_mut() {
                    doc.buffer.goto(origin);
                }
                self.scroll_into_view(visible);
                return Action::None;
            }
            KeyCode::Up => {
                self.find.previous();
            }
            KeyCode::Down => {
                self.find.next();
            }
            KeyCode::F(3) => {
                self.find.next();
            }
            KeyCode::Left => self.find.left(),
            KeyCode::Right => self.find.right(),
            KeyCode::Backspace => self.find.backspace(&lines),
            KeyCode::Char(c) if !ctrl => self.find.insert(c, &lines),
            _ => {}
        }
        self.go_to_hit(visible);
        Action::None
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

    /// Complete the path bar against what is really on disk.
    ///
    /// A directory gains a trailing separator so the next Tab carries on
    /// inside it, which is what makes drilling down feel continuous.
    pub fn complete_path(&mut self, lister: &dyn Lister) {
        let text = self.path_text.trim_end().to_string();
        let Some((dir, partial)) = split_for_completion(&text) else {
            return;
        };
        let Ok(entries) = lister.list(Path::new(dir)) else {
            return;
        };

        let wanted = partial.to_ascii_lowercase();
        let hits: Vec<&super::tree::Entry> = entries
            .iter()
            .filter(|e| e.name.to_ascii_lowercase().starts_with(&wanted))
            .collect();
        if hits.is_empty() {
            return;
        }

        let names: Vec<String> = hits.iter().map(|e| e.name.clone()).collect();
        let completed = common_prefix(&names);
        if completed.len() < partial.len() {
            return;
        }

        let sep = if dir.ends_with(['\\', '/']) { "" } else { "\\" };
        let mut out = format!("{dir}{sep}{completed}");
        // Only when it is unambiguous: appending a separator to a shared
        // prefix would claim a directory exists that does not.
        if hits.len() == 1 && hits[0].is_dir && hits[0].name == completed {
            out.push('\\');
        }

        self.path_text = out;
        self.path_caret = self.path_text.chars().count();
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
            KeyCode::Tab => return Action::Complete,
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

/// Split path text into the directory to list and the part still being typed.
///
/// Returns `None` when there is no separator yet, because "list everything on
/// every drive" is not a useful completion.
fn split_for_completion(text: &str) -> Option<(&str, &str)> {
    let (dir, partial) = text.rsplit_once(['\\', '/'])?;
    // `C:\foo` splits to ("C:", "foo"), and `C:` alone names the drive's
    // current directory rather than its root - so it is put back.
    Some((if dir.is_empty() { "\\" } else { dir }, partial))
}

/// The longest prefix every candidate shares, from `partial` onwards.
///
/// Completing to the common prefix rather than to the first match is what
/// makes repeated Tab useful: each press gets you as far as the names agree,
/// and stops where a choice actually has to be made.
fn common_prefix(names: &[String]) -> String {
    let Some(first) = names.first() else {
        return String::new();
    };
    let mut end = first.len();
    for other in &names[1..] {
        let shared = first
            .char_indices()
            .zip(other.chars())
            .take_while(|((_, a), b)| a.eq_ignore_ascii_case(b))
            .last()
            .map(|((i, a), _)| i + a.len_utf8())
            .unwrap_or(0);
        end = end.min(shared);
    }
    first[..end].to_string()
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

    /// A document with something worth searching for.
    fn with_text(text: &str) -> Explorer {
        let mut x = with_doc();
        x.doc.as_mut().unwrap().buffer = Buffer::from_str(text);
        x
    }

    fn type_into(x: &mut Explorer, fs: &Fake, text: &str) {
        for c in text.chars() {
            x.key(key(KeyCode::Char(c)), 10, fs);
        }
    }

    #[test]
    fn ctrl_f_opens_find_and_typing_jumps_to_the_first_match() {
        let fs = Fake::new(&[]);
        let mut x = with_text("nothing\nthe needle\nmore");
        x.focus = Focus::Editor;

        x.key(ctrl(KeyCode::Char('f')), 10, &fs);
        assert!(x.find.open, "the prompt should be showing");

        type_into(&mut x, &fs, "needle");
        assert_eq!(x.find.query, "needle");
        assert_eq!(
            x.doc.as_ref().unwrap().buffer.cursor(),
            Cursor::new(1, 4),
            "the cursor follows the search"
        );
    }

    #[test]
    fn while_finding_a_letter_never_reaches_the_editor() {
        // `s` would otherwise be a character typed into the file, and Ctrl+S
        // would save mid-search.
        let fs = Fake::new(&[]);
        let mut x = with_text("abc");
        x.focus = Focus::Editor;
        x.key(ctrl(KeyCode::Char('f')), 10, &fs);
        type_into(&mut x, &fs, "s");

        assert_eq!(x.doc.as_ref().unwrap().buffer.line(0), "abc");
        assert!(!x.modified(), "the file must not have been edited");
    }

    #[test]
    fn escape_from_the_prompt_puts_the_cursor_back() {
        let fs = Fake::new(&[]);
        let mut x = with_text("start\n\n\nfar away needle");
        x.focus = Focus::Editor;
        x.doc.as_mut().unwrap().buffer.goto(Cursor::new(0, 2));

        x.key(ctrl(KeyCode::Char('f')), 10, &fs);
        type_into(&mut x, &fs, "needle");
        assert_eq!(x.doc.as_ref().unwrap().buffer.cursor().line, 3);

        x.key(key(KeyCode::Esc), 10, &fs);
        assert!(!x.find.open);
        assert_eq!(
            x.doc.as_ref().unwrap().buffer.cursor(),
            Cursor::new(0, 2),
            "abandoning a search should not have moved you"
        );
    }

    #[test]
    fn enter_keeps_the_hit_and_closes_the_prompt() {
        let fs = Fake::new(&[]);
        let mut x = with_text("a\nneedle");
        x.focus = Focus::Editor;
        x.key(ctrl(KeyCode::Char('f')), 10, &fs);
        type_into(&mut x, &fs, "needle");
        x.key(key(KeyCode::Enter), 10, &fs);

        assert!(!x.find.open);
        assert_eq!(x.doc.as_ref().unwrap().buffer.cursor(), Cursor::new(1, 0));
        assert_eq!(x.doc.as_ref().unwrap().buffer.len(), 2, "no newline typed");
    }

    #[test]
    fn f3_keeps_stepping_after_the_prompt_has_closed() {
        let fs = Fake::new(&[]);
        let mut x = with_text("x\nx\nx");
        x.focus = Focus::Editor;
        x.key(ctrl(KeyCode::Char('f')), 10, &fs);
        type_into(&mut x, &fs, "x");
        x.key(key(KeyCode::Enter), 10, &fs);

        x.key(key(KeyCode::F(3)), 10, &fs);
        assert_eq!(x.doc.as_ref().unwrap().buffer.cursor(), Cursor::new(1, 0));
        x.key(KeyEvent::new(KeyCode::F(3), KeyModifiers::SHIFT), 10, &fs);
        assert_eq!(
            x.doc.as_ref().unwrap().buffer.cursor(),
            Cursor::new(0, 0),
            "Shift+F3 goes back"
        );
    }

    #[test]
    fn f3_with_nothing_searched_for_does_nothing() {
        let fs = Fake::new(&[]);
        let mut x = with_text("abc");
        x.focus = Focus::Editor;
        x.doc.as_mut().unwrap().buffer.goto(Cursor::new(0, 2));
        x.key(key(KeyCode::F(3)), 10, &fs);
        assert_eq!(x.doc.as_ref().unwrap().buffer.cursor(), Cursor::new(0, 2));
    }

    #[test]
    fn find_reflects_an_edit_made_since_the_search() {
        // Stepping to a position where the word used to be would be worse than
        // finding nothing.
        let fs = Fake::new(&[]);
        let mut x = with_text("needle\nneedle");
        x.focus = Focus::Editor;
        x.key(ctrl(KeyCode::Char('f')), 10, &fs);
        type_into(&mut x, &fs, "needle");
        x.key(key(KeyCode::Enter), 10, &fs);

        // Wipe the second line's word.
        x.doc.as_mut().unwrap().buffer.goto(Cursor::new(1, 6));
        for _ in 0..6 {
            x.key(key(KeyCode::Backspace), 10, &fs);
        }

        x.key(key(KeyCode::F(3)), 10, &fs);
        assert_eq!(x.find.hits.len(), 1, "only the surviving one");
        assert_eq!(x.doc.as_ref().unwrap().buffer.cursor(), Cursor::new(0, 0));
    }

    #[test]
    fn find_will_not_open_over_a_file_that_has_no_text() {
        // A binary file shows a message, not a document; a prompt over it could
        // never match anything.
        let fs = Fake::new(&[]);
        let mut x = Explorer::new(r"C:\p".into());
        x.notice = Some("binary".into());
        x.key(ctrl(KeyCode::Char('f')), 10, &fs);
        assert!(!x.find.open);
    }

    #[test]
    fn opening_another_file_forgets_the_previous_search() {
        let fs = Fake::new(&[]);
        let mut x = with_text("needle");
        x.focus = Focus::Editor;
        x.key(ctrl(KeyCode::Char('f')), 10, &fs);
        type_into(&mut x, &fs, "needle");
        assert_eq!(x.find.hits.len(), 1);

        x.load(Path::new(r"C:\p\does-not-exist.txt"));
        assert!(x.find.hits.is_empty(), "hits belong to the old file");
        assert!(x.find.query.is_empty());
        assert!(!x.find.open);
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
    fn esc_does_nothing_here() {
        // Outside the explorer this key exits the program. Inside, reflexively
        // pressing it must not throw away an open editor.
        let fs = Fake::new(&[]);
        let mut x = with_doc();
        assert_eq!(x.key(key(KeyCode::Esc), 10, &fs), Action::None);
    }

    #[test]
    fn the_key_that_came_in_is_the_key_that_leaves() {
        let fs = Fake::new(&[]);
        let mut x = with_doc();
        assert_eq!(
            x.key(ctrl(KeyCode::Char('e')), 10, &fs),
            Action::Leave,
            "Ctrl+E toggles back out"
        );
    }

    #[test]
    fn ctrl_c_opens_a_terminal_and_ctrl_q_is_what_quits() {
        let fs = Fake::new(&[]);
        let mut x = with_doc();
        assert_eq!(
            x.key(ctrl(KeyCode::Char('c')), 10, &fs),
            Action::OpenTerminal
        );
        assert_eq!(x.key(ctrl(KeyCode::Char('q')), 10, &fs), Action::Quit);
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
    fn tab_completes_a_unique_directory_and_opens_it_for_more() {
        let fs = Fake::new(&[(r"C:\p", &[("Downloads", true), ("Music", true)])]);
        let mut x = Explorer::new(r"C:\p".into());
        x.path_text = r"C:\p\Dow".into();

        x.complete_path(&fs);
        assert_eq!(
            x.path_text, r"C:\p\Downloads\",
            "a directory ends ready to keep going"
        );
        assert_eq!(x.path_caret, x.path_text.chars().count());
    }

    #[test]
    fn tab_completes_a_file_without_a_trailing_separator() {
        let fs = Fake::new(&[(r"C:\p", &[("notes.txt", false)])]);
        let mut x = Explorer::new(r"C:\p".into());
        x.path_text = r"C:\p\not".into();
        x.complete_path(&fs);
        assert_eq!(x.path_text, r"C:\p\notes.txt");
    }

    #[test]
    fn tab_stops_where_the_names_stop_agreeing() {
        // Completing to the first match would silently pick one of several.
        // Going as far as they agree stops exactly where a choice is needed.
        let fs = Fake::new(&[(
            r"C:\p",
            &[("report-jan.txt", false), ("report-feb.txt", false)],
        )]);
        let mut x = Explorer::new(r"C:\p".into());
        x.path_text = r"C:\p\rep".into();

        x.complete_path(&fs);
        assert_eq!(x.path_text, r"C:\p\report-");
        assert!(
            !x.path_text.ends_with('\\'),
            "a shared prefix is not a directory that exists"
        );
    }

    #[test]
    fn tab_matches_regardless_of_case() {
        let fs = Fake::new(&[(r"C:\p", &[("Downloads", true)])]);
        let mut x = Explorer::new(r"C:\p".into());
        x.path_text = r"C:\p\dOwN".into();
        x.complete_path(&fs);
        assert_eq!(x.path_text, r"C:\p\Downloads\");
    }

    #[test]
    fn tab_with_nothing_matching_leaves_the_text_alone() {
        let fs = Fake::new(&[(r"C:\p", &[("Music", true)])]);
        let mut x = Explorer::new(r"C:\p".into());
        x.path_text = r"C:\p\zzz".into();
        x.complete_path(&fs);
        assert_eq!(x.path_text, r"C:\p\zzz", "nothing to say, so say nothing");
    }

    #[test]
    fn tab_on_a_bare_word_does_nothing() {
        // Without a separator there is no directory to list, and completing
        // against every drive is not a useful answer.
        let fs = Fake::new(&[(r"C:\p", &[("Music", true)])]);
        let mut x = Explorer::new(r"C:\p".into());
        x.path_text = "Mus".into();
        x.complete_path(&fs);
        assert_eq!(x.path_text, "Mus");
    }

    #[test]
    fn tab_after_a_separator_lists_the_directory_itself() {
        let fs = Fake::new(&[(r"C:\p", &[("only", true)])]);
        let mut x = Explorer::new(r"C:\p".into());
        x.path_text = r"C:\p\".into();
        x.complete_path(&fs);
        assert_eq!(x.path_text, r"C:\p\only\");
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
