//! Find-in-file: a prompt, the hits it produced, and where you were before.
//!
//! Pure over a slice of lines, so every rule here — where a hit starts, which
//! one comes next, what wrapping does at the end of the file — is testable
//! without a terminal or a document.
//!
//! Matching is ASCII case-insensitive, the same rule the result list uses. Not
//! Unicode case folding: that changes string length, so a hit's column would no
//! longer be a position in the line being searched, and highlighting it would
//! land somewhere else. A file where that matters wants a real editor.

use super::buffer::Cursor;

/// The find bar's state, kept whether or not the bar is showing.
///
/// The query outlives the bar on purpose: closing the prompt and carrying on
/// with `F3` is how everyone expects find to work, and re-typing the word to
/// reach the next hit would be absurd.
#[derive(Debug, Default, Clone)]
pub struct Find {
    pub query: String,
    /// Caret in the query, in characters.
    pub caret: usize,
    /// Whether the prompt is showing and taking the keyboard.
    pub open: bool,
    /// Where the cursor was when the prompt opened, restored if it is
    /// abandoned. Without this, a search that found nothing would still have
    /// moved you.
    origin: Cursor,
    /// Every hit for the current query, in document order.
    pub hits: Vec<Cursor>,
    /// Which hit the cursor is on, if any.
    pub current: Option<usize>,
}

/// Every occurrence of `needle` in `lines`, in document order.
///
/// Columns count characters, not bytes, because that is what [`Cursor`] means
/// everywhere else — and a byte column on a line with an accent in it would put
/// the highlight in the wrong place.
///
/// Overlapping matches are not reported: after a hit, the scan resumes past it.
/// `aa` in `aaa` is one match, which is what every editor does.
pub fn matches(lines: &[String], needle: &str) -> Vec<Cursor> {
    if needle.is_empty() {
        return Vec::new();
    }
    let wanted: Vec<char> = needle.chars().map(|c| c.to_ascii_lowercase()).collect();
    let mut out = Vec::new();

    for (n, line) in lines.iter().enumerate() {
        let chars: Vec<char> = line.chars().map(|c| c.to_ascii_lowercase()).collect();
        if chars.len() < wanted.len() {
            continue;
        }
        let mut col = 0;
        while col + wanted.len() <= chars.len() {
            if chars[col..col + wanted.len()] == wanted[..] {
                out.push(Cursor::new(n, col));
                col += wanted.len();
            } else {
                col += 1;
            }
        }
    }
    out
}

/// The first hit at or after `from`, wrapping to the top.
///
/// "At or after" rather than "after": typing into the prompt should land on the
/// hit under the cursor rather than skipping past it, and stepping forward is
/// what [`Find::next`] is for.
fn first_from(hits: &[Cursor], from: Cursor) -> Option<usize> {
    if hits.is_empty() {
        return None;
    }
    let at = hits
        .iter()
        .position(|h| (h.line, h.col) >= (from.line, from.col));
    // Wrapping rather than stopping: a file's last match is rarely the one you
    // wanted, and a search that silently does nothing reads as broken.
    Some(at.unwrap_or(0))
}

impl Find {
    /// Show the prompt, remembering where to come back to.
    ///
    /// The previous query survives and is re-run, so reopening find on the same
    /// word shows its hits immediately.
    pub fn open(&mut self, at: Cursor, lines: &[String]) {
        self.open = true;
        self.origin = at;
        self.caret = self.query.chars().count();
        self.refresh(lines);
    }

    /// Re-run the current query, keeping the cursor on the nearest hit.
    ///
    /// Called after every keystroke in the prompt — searching as you type is
    /// the difference between find and a dialogue box — and after an edit,
    /// which can move or destroy the hits found before it.
    pub fn refresh(&mut self, lines: &[String]) {
        self.hits = matches(lines, &self.query);
        self.current = first_from(&self.hits, self.origin);
    }

    /// Re-run the query and anchor on `from` rather than on the origin.
    ///
    /// This is what `F3` uses after the prompt has closed. [`refresh`] anchors
    /// on where the search started, which is right while you are typing and
    /// wrong once you are stepping: it would throw away every step taken so
    /// far and send the next one back to the beginning.
    ///
    /// [`refresh`]: Self::refresh
    pub fn rescan(&mut self, lines: &[String], from: Cursor) {
        self.hits = matches(lines, &self.query);
        self.current = first_from(&self.hits, from);
    }

    /// Where the cursor should go, if anywhere.
    pub fn cursor(&self) -> Option<Cursor> {
        self.current.and_then(|i| self.hits.get(i)).copied()
    }

    /// Where the cursor was before the search started.
    pub fn origin(&self) -> Cursor {
        self.origin
    }

    /// Step to the next hit, wrapping at the end.
    pub fn next(&mut self) -> Option<Cursor> {
        if self.hits.is_empty() {
            return None;
        }
        self.current = Some(match self.current {
            Some(i) => (i + 1) % self.hits.len(),
            None => 0,
        });
        self.cursor()
    }

    /// Step to the previous hit, wrapping at the start.
    pub fn previous(&mut self) -> Option<Cursor> {
        if self.hits.is_empty() {
            return None;
        }
        self.current = Some(match self.current {
            Some(0) | None => self.hits.len() - 1,
            Some(i) => i - 1,
        });
        self.cursor()
    }

    pub fn insert(&mut self, c: char, lines: &[String]) {
        let at = byte_at(&self.query, self.caret);
        self.query.insert(at, c);
        self.caret += 1;
        self.refresh(lines);
    }

    pub fn backspace(&mut self, lines: &[String]) {
        if self.caret == 0 {
            return;
        }
        let at = byte_at(&self.query, self.caret - 1);
        self.query.remove(at);
        self.caret -= 1;
        self.refresh(lines);
    }

    pub fn left(&mut self) {
        self.caret = self.caret.saturating_sub(1);
    }

    pub fn right(&mut self) {
        self.caret = (self.caret + 1).min(self.query.chars().count());
    }

    /// What the bar says on the right: which hit, out of how many.
    pub fn tally(&self) -> String {
        if self.query.is_empty() {
            String::new()
        } else if self.hits.is_empty() {
            "no matches".to_string()
        } else {
            let n = self.current.map(|i| i + 1).unwrap_or(0);
            format!("{n}/{}", self.hits.len())
        }
    }
}

/// The hits that fall on one line.
///
/// Binary search rather than a filter: the renderer asks this once per visible
/// line, and a file where every line matches would otherwise make drawing
/// quadratic in the number of hits.
pub fn on_line(hits: &[Cursor], line: usize) -> &[Cursor] {
    let start = hits.partition_point(|h| h.line < line);
    let end = hits.partition_point(|h| h.line <= line);
    &hits[start..end]
}

/// Byte offset of a character position. The one place the query is indexed.
fn byte_at(s: &str, col: usize) -> usize {
    s.char_indices().nth(col).map(|(i, _)| i).unwrap_or(s.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(text: &str) -> Vec<String> {
        text.lines().map(str::to_string).collect()
    }

    #[test]
    fn every_occurrence_is_found_in_document_order() {
        let l = lines("one two\nthree two\ntwo");
        let hits = matches(&l, "two");
        assert_eq!(
            hits,
            vec![Cursor::new(0, 4), Cursor::new(1, 6), Cursor::new(2, 0)]
        );
    }

    #[test]
    fn matching_ignores_case_both_ways() {
        let l = lines("Error: ERROR error");
        assert_eq!(matches(&l, "error").len(), 3);
        assert_eq!(matches(&l, "ERROR").len(), 3);
    }

    #[test]
    fn columns_count_characters_not_bytes() {
        // A byte column here would be 5, and the highlight would land a
        // character to the right of the word.
        let l = lines("héllo needle");
        assert_eq!(matches(&l, "needle"), vec![Cursor::new(0, 6)]);
    }

    #[test]
    fn matches_do_not_overlap() {
        let l = lines("aaa");
        assert_eq!(matches(&l, "aa"), vec![Cursor::new(0, 0)]);
    }

    #[test]
    fn an_empty_query_matches_nothing() {
        // Not "every position": an empty find bar should show no hits at all,
        // rather than claiming one per character.
        assert!(matches(&lines("anything"), "").is_empty());
    }

    #[test]
    fn a_needle_longer_than_the_line_is_not_a_panic() {
        assert!(matches(&lines("hi"), "hello there").is_empty());
    }

    #[test]
    fn opening_lands_on_the_hit_at_or_after_the_cursor() {
        let l = lines("two\ntwo\ntwo");
        let mut f = Find {
            query: "two".to_string(),
            ..Default::default()
        };
        f.open(Cursor::new(1, 0), &l);
        assert_eq!(f.cursor(), Some(Cursor::new(1, 0)), "not the one before");
    }

    #[test]
    fn searching_past_the_last_hit_wraps_to_the_first() {
        let l = lines("two\nnothing here");
        let mut f = Find {
            query: "two".to_string(),
            ..Default::default()
        };
        f.open(Cursor::new(1, 0), &l);
        assert_eq!(f.cursor(), Some(Cursor::new(0, 0)));
    }

    #[test]
    fn next_and_previous_wrap_in_both_directions() {
        let l = lines("x\nx\nx");
        let mut f = Find {
            query: "x".to_string(),
            ..Default::default()
        };
        f.open(Cursor::new(0, 0), &l);

        assert_eq!(f.next(), Some(Cursor::new(1, 0)));
        assert_eq!(f.next(), Some(Cursor::new(2, 0)));
        assert_eq!(f.next(), Some(Cursor::new(0, 0)), "past the end, wrap");
        assert_eq!(f.previous(), Some(Cursor::new(2, 0)), "before the start");
    }

    #[test]
    fn stepping_with_no_hits_does_nothing_rather_than_panicking() {
        let mut f = Find {
            query: "absent".to_string(),
            ..Default::default()
        };
        f.open(Cursor::new(0, 0), &lines("nothing"));
        assert_eq!(f.next(), None);
        assert_eq!(f.previous(), None);
        assert_eq!(f.cursor(), None);
    }

    #[test]
    fn typing_narrows_the_search_as_it_goes() {
        let l = lines("apple apricot avocado");
        let mut f = Find::default();
        f.open(Cursor::new(0, 0), &l);

        for c in "ap".chars() {
            f.insert(c, &l);
        }
        assert_eq!(f.hits.len(), 2, "apple and apricot");
        f.insert('p', &l);
        assert_eq!(f.hits.len(), 1, "only apple");
        f.backspace(&l);
        assert_eq!(f.hits.len(), 2, "back to both");
    }

    #[test]
    fn the_query_survives_the_prompt_closing() {
        // So F3 keeps working after Enter, which is the point of remembering it.
        let l = lines("target\ntarget");
        let mut f = Find::default();
        f.open(Cursor::new(0, 0), &l);
        for c in "target".chars() {
            f.insert(c, &l);
        }
        f.open = false;
        assert_eq!(f.next(), Some(Cursor::new(1, 0)));
    }

    #[test]
    fn abandoning_a_search_can_put_the_cursor_back() {
        let l = lines("a\nb\ntarget");
        let mut f = Find::default();
        f.open(Cursor::new(1, 0), &l);
        for c in "target".chars() {
            f.insert(c, &l);
        }
        assert_eq!(f.cursor(), Some(Cursor::new(2, 0)), "moved to the hit");
        assert_eq!(f.origin(), Cursor::new(1, 0), "but remembers where from");
    }

    #[test]
    fn the_tally_says_which_hit_out_of_how_many() {
        let l = lines("x\nx\nx");
        let mut f = Find::default();
        f.open(Cursor::new(0, 0), &l);
        assert_eq!(f.tally(), "", "nothing typed yet");
        f.insert('x', &l);
        assert_eq!(f.tally(), "1/3");
        f.next();
        assert_eq!(f.tally(), "2/3");
        f.insert('q', &l);
        assert_eq!(f.tally(), "no matches");
    }

    #[test]
    fn rescanning_anchors_where_the_cursor_is_not_where_the_search_began() {
        // The bug this exists for: F3 re-runs the search each time, and
        // anchoring on the origin would send every step back to the first hit.
        let l = lines("x\nx\nx");
        let mut f = Find {
            query: "x".to_string(),
            ..Default::default()
        };
        f.open(Cursor::new(0, 0), &l);

        f.rescan(&l, Cursor::new(1, 0));
        assert_eq!(f.cursor(), Some(Cursor::new(1, 0)));
        assert_eq!(f.next(), Some(Cursor::new(2, 0)));

        f.rescan(&l, Cursor::new(2, 0));
        assert_eq!(f.previous(), Some(Cursor::new(1, 0)), "and back again");
    }

    #[test]
    fn hits_can_be_asked_for_one_line_at_a_time() {
        let l = lines("x\nno\nx x\nno");
        let hits = matches(&l, "x");
        assert_eq!(on_line(&hits, 0), &[Cursor::new(0, 0)]);
        assert!(on_line(&hits, 1).is_empty());
        assert_eq!(on_line(&hits, 2), &[Cursor::new(2, 0), Cursor::new(2, 2)]);
        assert!(on_line(&hits, 3).is_empty());
        assert!(on_line(&hits, 99).is_empty(), "past the end is not a panic");
    }

    #[test]
    fn the_caret_moves_over_characters_and_stops_at_the_ends() {
        let mut f = Find::default();
        let l: Vec<String> = Vec::new();
        for c in "abc".chars() {
            f.insert(c, &l);
        }
        assert_eq!(f.caret, 3);
        f.right();
        assert_eq!(f.caret, 3, "cannot go past the end");
        f.left();
        f.left();
        f.left();
        f.left();
        assert_eq!(f.caret, 0, "cannot go before the start");
    }

    #[test]
    fn inserting_at_the_caret_rather_than_the_end() {
        let l: Vec<String> = Vec::new();
        let mut f = Find::default();
        for c in "ac".chars() {
            f.insert(c, &l);
        }
        f.left();
        f.insert('b', &l);
        assert_eq!(f.query, "abc");
    }

    #[test]
    fn an_edit_can_be_reflected_without_reopening() {
        // A hit found before an edit may no longer be there afterwards.
        let mut f = Find::default();
        f.open(Cursor::new(0, 0), &lines("needle"));
        for c in "needle".chars() {
            f.insert(c, &lines("needle"));
        }
        assert_eq!(f.hits.len(), 1);
        f.refresh(&lines("gone"));
        assert!(f.hits.is_empty());
        assert_eq!(f.cursor(), None);
    }
}
