//! The line being typed: editing, history, and Tab completion.
//!
//! Pure state with no terminal in it, so the fiddly parts — a caret that has
//! to stay on a character boundary, history that must not lose what you were
//! halfway through typing — are testable directly.
//!
//! The shell needs its own line editing because it *is* the program reading
//! the keyboard. There is nothing underneath it to provide the arrows, the
//! history or the completion; that was PowerShell's job, and this replaces
//! PowerShell.

/// The caret counts characters, never bytes: the same rule the editor's buffer
/// follows, and for the same reason — mixing them slices mid-character, and
/// `panic = "abort"` makes that the end of the process.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Line {
    text: String,
    caret: usize,
}

fn byte_of(s: &str, col: usize) -> usize {
    s.char_indices().nth(col).map(|(i, _)| i).unwrap_or(s.len())
}

impl Line {
    pub fn new(text: impl Into<String>) -> Line {
        let text = text.into();
        let caret = text.chars().count();
        Line { text, caret }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn caret(&self) -> usize {
        self.caret
    }

    pub fn len(&self) -> usize {
        self.text.chars().count()
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn set(&mut self, text: impl Into<String>) {
        *self = Line::new(text);
    }

    pub fn insert(&mut self, c: char) {
        let at = byte_of(&self.text, self.caret);
        self.text.insert(at, c);
        self.caret += 1;
    }

    pub fn backspace(&mut self) {
        if self.caret == 0 {
            return;
        }
        let at = byte_of(&self.text, self.caret - 1);
        self.text.remove(at);
        self.caret -= 1;
    }

    pub fn delete(&mut self) {
        if self.caret >= self.len() {
            return;
        }
        let at = byte_of(&self.text, self.caret);
        self.text.remove(at);
    }

    pub fn left(&mut self) {
        self.caret = self.caret.saturating_sub(1);
    }

    pub fn right(&mut self) {
        self.caret = (self.caret + 1).min(self.len());
    }

    pub fn home(&mut self) {
        self.caret = 0;
    }

    pub fn end(&mut self) {
        self.caret = self.len();
    }

    /// Delete the word before the caret, as `Ctrl+W` does everywhere.
    pub fn delete_word(&mut self) {
        let chars: Vec<char> = self.text.chars().collect();
        let mut at = self.caret;
        while at > 0 && chars[at - 1].is_whitespace() {
            at -= 1;
        }
        while at > 0 && !chars[at - 1].is_whitespace() {
            at -= 1;
        }
        let (from, to) = (byte_of(&self.text, at), byte_of(&self.text, self.caret));
        self.text.replace_range(from..to, "");
        self.caret = at;
    }

    /// Delete from the caret back to the start, as `Ctrl+U` does.
    pub fn delete_to_start(&mut self) {
        let to = byte_of(&self.text, self.caret);
        self.text.replace_range(..to, "");
        self.caret = 0;
    }

    /// The word the caret sits in, and where it starts.
    ///
    /// Quotes are respected so a path with a space in it completes as one
    /// word rather than from the middle of it.
    pub fn word_at_caret(&self) -> (usize, String) {
        let chars: Vec<char> = self.text.chars().collect();
        let mut start = 0;
        let mut quote: Option<char> = None;

        for (i, &c) in chars.iter().enumerate().take(self.caret) {
            match quote {
                Some(q) if c == q => quote = None,
                Some(_) => {}
                None if c == '"' || c == '\'' => {
                    quote = Some(c);
                    if i + 1 > start {
                        start = i + 1;
                    }
                }
                None if c.is_whitespace() || c == '|' || c == '>' || c == '<' => start = i + 1,
                None => {}
            }
        }
        (start, chars[start..self.caret].iter().collect())
    }
}

/// Command history, with the line you were typing kept aside.
#[derive(Debug, Default)]
pub struct History {
    entries: Vec<String>,
    /// Where in the history we are, counting back from the end.
    at: Option<usize>,
    /// What was being typed before history was walked into.
    stashed: Option<String>,
}

/// Beyond this the oldest entries are dropped.
const MAX_HISTORY: usize = 1000;

impl History {
    pub fn push(&mut self, line: &str) {
        let line = line.trim();
        // Blank lines and an immediate repeat are noise: walking back through
        // ten identical commands is worse than not recording them.
        if line.is_empty() || self.entries.last().map(String::as_str) == Some(line) {
            self.reset();
            return;
        }
        self.entries.push(line.to_string());
        if self.entries.len() > MAX_HISTORY {
            self.entries.remove(0);
        }
        self.reset();
    }

    pub fn reset(&mut self) {
        self.at = None;
        self.stashed = None;
    }

    /// How many entries are remembered. Only the tests ask; the shell itself
    /// never needs to know.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Step back one entry, returning what the line should become.
    pub fn previous(&mut self, current: &str) -> Option<String> {
        if self.entries.is_empty() {
            return None;
        }
        let next = match self.at {
            None => {
                // Keep what was half-typed so coming back returns it rather
                // than an empty line.
                self.stashed = Some(current.to_string());
                0
            }
            Some(i) if i + 1 < self.entries.len() => i + 1,
            Some(i) => i,
        };
        self.at = Some(next);
        Some(self.entries[self.entries.len() - 1 - next].clone())
    }

    /// Step forward one entry, or back to what was being typed.
    pub fn next(&mut self) -> Option<String> {
        match self.at {
            None => None,
            Some(0) => {
                self.at = None;
                Some(self.stashed.take().unwrap_or_default())
            }
            Some(i) => {
                self.at = Some(i - 1);
                Some(self.entries[self.entries.len() - i].clone())
            }
        }
    }
}

/// Where completion candidates come from, injected so this is testable
/// without a filesystem.
pub trait Candidates {
    /// Names inside `dir`, and whether each is a directory.
    fn list(&self, dir: &str) -> Vec<(String, bool)>;
}

/// The longest prefix every candidate shares.
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

/// What Tab should replace the current word with, if anything.
///
/// Returns the completed word. Completing to the longest shared prefix rather
/// than to the first match is what makes repeated Tab useful: it takes you as
/// far as the candidates agree and stops where a choice has to be made.
pub fn complete(word: &str, source: &dyn Candidates) -> Option<String> {
    let (dir, partial) = match word.rfind(['\\', '/']) {
        Some(i) => (&word[..=i], &word[i + 1..]),
        None => ("", word),
    };

    let wanted = partial.to_ascii_lowercase();
    let hits: Vec<(String, bool)> = source
        .list(if dir.is_empty() { "." } else { dir })
        .into_iter()
        .filter(|(name, _)| name.to_ascii_lowercase().starts_with(&wanted))
        .collect();

    if hits.is_empty() {
        return None;
    }

    let names: Vec<String> = hits.iter().map(|(n, _)| n.clone()).collect();
    let completed = common_prefix(&names);
    if completed.len() < partial.len() {
        return None;
    }

    let mut out = format!("{dir}{completed}");
    // Only when it is unambiguous: a shared prefix is not a directory that
    // exists, and claiming it is would put a separator where nothing follows.
    if hits.len() == 1 && hits[0].1 && hits[0].0 == completed {
        out.push('\\');
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake(Vec<(String, bool)>);

    impl Fake {
        fn new(items: &[(&str, bool)]) -> Fake {
            Fake(items.iter().map(|(n, d)| (n.to_string(), *d)).collect())
        }
    }

    impl Candidates for Fake {
        fn list(&self, _dir: &str) -> Vec<(String, bool)> {
            self.0.clone()
        }
    }

    #[test]
    fn typing_and_deleting_move_the_caret_with_the_text() {
        let mut l = Line::default();
        for c in "abc".chars() {
            l.insert(c);
        }
        assert_eq!(l.text(), "abc");
        assert_eq!(l.caret(), 3);

        l.left();
        l.insert('X');
        assert_eq!(l.text(), "abXc");
        assert_eq!(l.caret(), 3);

        l.backspace();
        assert_eq!(l.text(), "abc");
    }

    #[test]
    fn editing_never_slices_a_multi_byte_character() {
        // The failure here is a panic, and with `panic = "abort"` that takes
        // the shell down mid-line.
        let mut l = Line::new("héllo → wörld");
        l.home();
        l.right();
        l.delete();
        assert_eq!(l.text(), "hllo → wörld");

        l.end();
        for _ in 0..6 {
            l.backspace();
        }
        assert_eq!(l.text(), "hllo →");
    }

    #[test]
    fn the_caret_cannot_walk_off_either_end() {
        let mut l = Line::new("ab");
        l.home();
        l.left();
        l.left();
        assert_eq!(l.caret(), 0);
        l.backspace();
        assert_eq!(l.text(), "ab");

        l.end();
        l.right();
        assert_eq!(l.caret(), 2);
        l.delete();
        assert_eq!(l.text(), "ab");
    }

    #[test]
    fn ctrl_w_deletes_a_word_and_the_space_before_it() {
        let mut l = Line::new("git commit --amend");
        l.delete_word();
        assert_eq!(l.text(), "git commit ");
        l.delete_word();
        assert_eq!(l.text(), "git ");
    }

    #[test]
    fn ctrl_u_clears_back_to_the_start_only() {
        let mut l = Line::new("keep this");
        l.home();
        for _ in 0..5 {
            l.right();
        }
        l.delete_to_start();
        assert_eq!(l.text(), "this");
        assert_eq!(l.caret(), 0);
    }

    #[test]
    fn history_walks_back_and_forward() {
        let mut h = History::default();
        h.push("first");
        h.push("second");

        assert_eq!(h.previous("").as_deref(), Some("second"));
        assert_eq!(h.previous("").as_deref(), Some("first"));
        // Already at the oldest: stays rather than wrapping round.
        assert_eq!(h.previous("").as_deref(), Some("first"));

        assert_eq!(h.next().as_deref(), Some("second"));
        assert_eq!(h.next().as_deref(), Some(""));
        assert_eq!(h.next(), None);
    }

    #[test]
    fn history_gives_back_what_you_were_halfway_through_typing() {
        // Losing a half-typed line because you glanced at history is the
        // single most irritating thing a shell can do.
        let mut h = History::default();
        h.push("earlier");

        assert_eq!(h.previous("half typed").as_deref(), Some("earlier"));
        assert_eq!(h.next().as_deref(), Some("half typed"));
    }

    #[test]
    fn history_ignores_blanks_and_immediate_repeats() {
        let mut h = History::default();
        h.push("same");
        h.push("same");
        h.push("   ");
        h.push("");
        assert_eq!(h.len(), 1);
    }

    #[test]
    fn history_is_bounded() {
        let mut h = History::default();
        for i in 0..MAX_HISTORY + 50 {
            h.push(&format!("command {i}"));
        }
        assert_eq!(h.len(), MAX_HISTORY);
    }

    #[test]
    fn history_on_an_empty_list_does_nothing() {
        let mut h = History::default();
        assert!(h.is_empty());
        assert_eq!(h.previous("typing"), None);
        assert_eq!(h.next(), None);
    }

    #[test]
    fn tab_completes_a_unique_name_and_marks_a_directory() {
        let fs = Fake::new(&[("Downloads", true), ("Music", true)]);
        assert_eq!(complete("Dow", &fs).as_deref(), Some("Downloads\\"));
    }

    #[test]
    fn tab_stops_where_the_candidates_stop_agreeing() {
        let fs = Fake::new(&[("report-jan.txt", false), ("report-feb.txt", false)]);
        let out = complete("rep", &fs).unwrap();
        assert_eq!(out, "report-");
        assert!(!out.ends_with('\\'), "a shared prefix is not a directory");
    }

    #[test]
    fn tab_keeps_the_directory_part_of_the_word() {
        let fs = Fake::new(&[("main.rs", false)]);
        assert_eq!(
            complete(r"src\ma", &fs).as_deref(),
            Some(r"src\main.rs"),
            "the path in front must survive"
        );
    }

    #[test]
    fn tab_matches_regardless_of_case() {
        let fs = Fake::new(&[("Cargo.toml", false)]);
        assert_eq!(complete("cargo", &fs).as_deref(), Some("Cargo.toml"));
    }

    #[test]
    fn tab_with_nothing_matching_offers_nothing() {
        let fs = Fake::new(&[("Music", true)]);
        assert_eq!(complete("zzz", &fs), None);
    }

    #[test]
    fn the_word_under_the_caret_is_what_gets_completed() {
        let l = Line::new("git checkout br");
        let (start, word) = l.word_at_caret();
        assert_eq!(word, "br");
        assert_eq!(start, 13);
    }

    #[test]
    fn a_quoted_path_completes_as_one_word() {
        // Otherwise a path with a space in it completes from its middle.
        let l = Line::new(r#"cd "C:\Program Fi"#);
        let (_, word) = l.word_at_caret();
        assert_eq!(word, r"C:\Program Fi");
    }

    #[test]
    fn a_word_after_a_pipe_is_its_own_word() {
        let l = Line::new("dir | fin");
        let (_, word) = l.word_at_caret();
        assert_eq!(word, "fin");
    }

    #[test]
    fn a_word_after_a_redirect_is_its_own_word() {
        let l = Line::new("echo hi >out");
        let (_, word) = l.word_at_caret();
        assert_eq!(word, "out");
    }
}
