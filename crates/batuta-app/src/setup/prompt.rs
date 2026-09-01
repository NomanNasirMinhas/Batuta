//! Console prompt primitives.
//!
//! Every prompt reads through the [`Ask`] trait rather than stdin directly, so
//! the whole wizard can be driven by a scripted list of answers in tests. That
//! is what makes the branching and the validation testable without a terminal
//! and without elevation.

use std::io::{self, BufRead, Write};
use std::ops::RangeInclusive;

/// A source of answers.
///
/// The primitive is [`Ask::line`], a read-one-line call. The typed methods on
/// top of it have default implementations that speak the console line protocol
/// (empty input takes the default, a bad answer is re-asked), so a terminal
/// front-end only overrides what it can do better — the ratatui wizard renders
/// widgets instead of parsing text, but both answer the same questions, and
/// the flow cannot tell them apart.
pub trait Ask {
    /// Show `prompt` and read one line. `Ok(None)` means input ended.
    fn line(&mut self, prompt: &str) -> io::Result<Option<String>>;

    /// Print a line of context that is not a question.
    fn say(&mut self, text: &str);

    /// A yes/no question. `default` is what empty or Esc input means.
    fn confirm(&mut self, question: &str, default: bool) -> io::Result<bool> {
        line_yes_no(self, question, default)
    }

    /// A number within `range`. `default` is what empty or Esc input means.
    fn number(
        &mut self,
        question: &str,
        range: RangeInclusive<u32>,
        default: u32,
    ) -> io::Result<u32> {
        line_number(self, question, range, default)
    }

    /// Free text. `default` is what empty or Esc input means.
    fn text(&mut self, question: &str, default: &str) -> io::Result<String> {
        line_text(self, question, default)
    }

    /// Pick one of `options` by index. `default_index` is the empty/Esc answer.
    fn choose(
        &mut self,
        question: &str,
        options: &[&str],
        default_index: usize,
    ) -> io::Result<usize> {
        line_choose(self, question, options, default_index)
    }

    /// Pick any number of `items` by index. Entries whose `selectable` flag is
    /// false are shown but refused. `default` is the empty/Esc answer.
    fn multi_select(
        &mut self,
        question: &str,
        items: &[String],
        selectable: &[bool],
        default: &[usize],
    ) -> io::Result<Vec<usize>> {
        line_multi_select(self, question, items, selectable, default)
    }

    /// One line of apply progress, as it happens.
    fn progress(&mut self, line: &str) {
        println!("{line}");
    }

    /// The closing screen: a report worth reading before the window closes.
    fn finish(&mut self, lines: &[String]) {
        for line in lines {
            println!("{line}");
        }
    }
}

/// Reads real answers from stdin.
pub struct Console;

impl Ask for Console {
    fn line(&mut self, prompt: &str) -> io::Result<Option<String>> {
        print!("{prompt}");
        io::stdout().flush()?;
        let mut buf = String::new();
        let n = io::stdin().lock().read_line(&mut buf)?;
        if n == 0 {
            return Ok(None); // stdin closed
        }
        Ok(Some(buf.trim_end_matches(['\r', '\n']).to_string()))
    }

    fn say(&mut self, text: &str) {
        println!("{text}");
    }
}

/// A canned list of answers, for tests.
#[cfg(test)]
#[derive(Default)]
pub struct Scripted {
    answers: std::collections::VecDeque<String>,
    /// Everything the wizard printed, so tests can assert on what was shown.
    pub shown: Vec<String>,
    /// Every prompt that was asked, in order.
    pub asked: Vec<String>,
}

#[cfg(test)]
impl Scripted {
    pub fn new<I, S>(answers: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Scripted {
            answers: answers.into_iter().map(Into::into).collect(),
            shown: Vec::new(),
            asked: Vec::new(),
        }
    }

    /// Answers not consumed. A wizard that skipped a branch leaves some.
    pub fn remaining(&self) -> usize {
        self.answers.len()
    }

    /// Did any prompt or message mention this text?
    pub fn mentioned(&self, needle: &str) -> bool {
        self.asked
            .iter()
            .chain(self.shown.iter())
            .any(|s| s.contains(needle))
    }
}

#[cfg(test)]
impl Ask for Scripted {
    fn line(&mut self, prompt: &str) -> io::Result<Option<String>> {
        self.asked.push(prompt.to_string());
        Ok(self.answers.pop_front())
    }

    fn say(&mut self, text: &str) {
        self.shown.push(text.to_string());
    }
}

/// Input ran out before the wizard finished.
fn eof() -> io::Error {
    io::Error::new(io::ErrorKind::UnexpectedEof, "input ended during setup")
}

/// Bound the retry loops. Without this, a scripted test whose answers run out
/// (or a piped stdin at EOF) would spin forever re-asking.
const MAX_RETRIES: usize = 20;

/// Ask a yes/no question. Empty input takes `default`.
pub(crate) fn line_yes_no<A: Ask + ?Sized>(
    ask: &mut A,
    question: &str,
    default: bool,
) -> io::Result<bool> {
    let hint = if default { "[Y/n]" } else { "[y/N]" };
    for _ in 0..MAX_RETRIES {
        let Some(raw) = ask.line(&format!("{question} {hint} "))? else {
            return Err(eof());
        };
        match raw.trim().to_ascii_lowercase().as_str() {
            "" => return Ok(default),
            "y" | "yes" => return Ok(true),
            "n" | "no" => return Ok(false),
            other => ask.say(&format!("  '{other}' is not yes or no.")),
        }
    }
    Err(eof())
}

/// Parse one answer to a number question. `Ok(None)` means empty input (the
/// caller takes its default); `Err` is the complaint to show, worded the same
/// for every front-end.
pub(crate) fn number_answer(raw: &str, range: &RangeInclusive<u32>) -> Result<Option<u32>, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(None);
    }
    match raw.parse::<u32>() {
        Ok(v) if range.contains(&v) => Ok(Some(v)),
        Ok(v) => Err(format!(
            "  {v} is outside {}-{}.",
            range.start(),
            range.end()
        )),
        Err(_) => Err(format!("  '{raw}' is not a number.")),
    }
}

/// Ask for a number within `range`. Empty input takes `default`.
pub(crate) fn line_number<A: Ask + ?Sized>(
    ask: &mut A,
    question: &str,
    range: RangeInclusive<u32>,
    default: u32,
) -> io::Result<u32> {
    for _ in 0..MAX_RETRIES {
        let Some(raw) = ask.line(&format!("{question} [{default}] "))? else {
            return Err(eof());
        };
        match number_answer(&raw, &range) {
            Ok(Some(v)) => return Ok(v),
            Ok(None) => return Ok(default),
            Err(complaint) => ask.say(&complaint),
        }
    }
    Err(eof())
}

/// Ask for free text. Empty input takes `default`.
pub(crate) fn line_text<A: Ask + ?Sized>(
    ask: &mut A,
    question: &str,
    default: &str,
) -> io::Result<String> {
    let Some(raw) = ask.line(&format!("{question}\n  [{default}] "))? else {
        return Err(eof());
    };
    let raw = raw.trim();
    Ok(if raw.is_empty() {
        default.to_string()
    } else {
        raw.to_string()
    })
}

/// Pick one of `options`. Empty input takes `default_index`.
pub(crate) fn line_choose<A: Ask + ?Sized>(
    ask: &mut A,
    question: &str,
    options: &[&str],
    default_index: usize,
) -> io::Result<usize> {
    for _ in 0..MAX_RETRIES {
        ask.say(question);
        for (i, opt) in options.iter().enumerate() {
            let marker = if i == default_index { " (default)" } else { "" };
            ask.say(&format!("  {}. {opt}{marker}", i + 1));
        }
        let Some(raw) = ask.line(&format!(
            "  choose 1-{} [{}] ",
            options.len(),
            default_index + 1
        ))?
        else {
            return Err(eof());
        };
        let raw = raw.trim();
        if raw.is_empty() {
            return Ok(default_index);
        }
        match raw.parse::<usize>() {
            Ok(v) if v >= 1 && v <= options.len() => return Ok(v - 1),
            _ => ask.say(&format!("  '{raw}' is not one of the choices.")),
        }
    }
    Err(eof())
}

/// Pick any number of `items` by number, or `all`.
///
/// `selectable` marks which entries may be chosen; the rest are shown with a
/// reason but refused, because silently hiding a drive the user can see in
/// Explorer would look like a bug.
pub(crate) fn line_multi_select<A: Ask + ?Sized>(
    ask: &mut A,
    question: &str,
    items: &[String],
    selectable: &[bool],
    default: &[usize],
) -> io::Result<Vec<usize>> {
    let default_label: String = default
        .iter()
        .map(|i| (i + 1).to_string())
        .collect::<Vec<_>>()
        .join(",");

    for _ in 0..MAX_RETRIES {
        ask.say(question);
        for (i, item) in items.iter().enumerate() {
            ask.say(&format!("  {}. {item}", i + 1));
        }
        let Some(raw) = ask.line(&format!(
            "  numbers separated by commas, or 'all' [{default_label}] "
        ))?
        else {
            return Err(eof());
        };

        let raw = raw.trim();
        if raw.is_empty() {
            return Ok(default.to_vec());
        }

        let picks: Vec<usize> = if raw.eq_ignore_ascii_case("all") {
            (0..items.len()).filter(|&i| selectable[i]).collect()
        } else {
            let mut out = Vec::new();
            let mut bad = None;
            for part in raw.split(',') {
                match part.trim().parse::<usize>() {
                    Ok(v) if v >= 1 && v <= items.len() => out.push(v - 1),
                    _ => {
                        bad = Some(part.trim().to_string());
                        break;
                    }
                }
            }
            if let Some(b) = bad {
                ask.say(&format!("  '{b}' is not one of the numbers listed."));
                continue;
            }
            out
        };

        let refused: Vec<usize> = picks.iter().copied().filter(|&i| !selectable[i]).collect();
        if !refused.is_empty() {
            for i in refused {
                ask.say(&format!("  {} cannot be indexed.", items[i]));
            }
            continue;
        }
        if picks.is_empty() {
            ask.say("  choose at least one.");
            continue;
        }

        let mut picks = picks;
        picks.sort_unstable();
        picks.dedup();
        return Ok(picks);
    }
    Err(eof())
}

#[cfg(test)]
mod tests {
    use super::*;

    // The typed methods are the front door now; these tests go through them so
    // the console line protocol is exercised exactly as flow.rs sees it.
    #[test]
    fn yes_no_accepts_the_usual_spellings() {
        for (input, want) in [
            ("y", true),
            ("Y", true),
            ("yes", true),
            ("n", false),
            ("NO", false),
        ] {
            let mut s = Scripted::new([input]);
            assert_eq!(s.confirm("q", true).unwrap(), want, "input {input:?}");
        }
    }

    #[test]
    fn empty_input_takes_the_default() {
        let mut s = Scripted::new([""]);
        assert!(s.confirm("q", true).unwrap());
        let mut s = Scripted::new([""]);
        assert!(!s.confirm("q", false).unwrap());
        let mut s = Scripted::new([""]);
        assert_eq!(s.number("q", 1..=60, 15).unwrap(), 15);
        let mut s = Scripted::new([""]);
        assert_eq!(s.text("q", "C:/default").unwrap(), "C:/default");
    }

    #[test]
    fn a_bad_answer_reprompts_rather_than_being_accepted() {
        let mut s = Scripted::new(["maybe", "sort of", "y"]);
        assert!(s.confirm("q", false).unwrap());
        assert_eq!(s.asked.len(), 3, "should have asked three times");
        assert!(s.mentioned("is not yes or no"));
    }

    #[test]
    fn numbers_outside_the_range_are_refused() {
        // The interval is specified as 1-60; 0 and 61 must not slip through.
        let mut s = Scripted::new(["0", "61", "abc", "30"]);
        assert_eq!(s.number("q", 1..=60, 15).unwrap(), 30);
        assert!(s.mentioned("outside 1-60"));
        assert!(s.mentioned("is not a number"));

        for edge in ["1", "60"] {
            let mut s = Scripted::new([edge]);
            assert_eq!(s.number("q", 1..=60, 15).unwrap(), edge.parse().unwrap());
        }
    }

    #[test]
    fn running_out_of_input_is_an_error_not_a_hang() {
        let mut s = Scripted::new(Vec::<String>::new());
        assert!(s.confirm("q", true).is_err());

        // An endlessly invalid answer must terminate too.
        let mut s = Scripted::new(vec!["nope"; 100]);
        assert!(s.confirm("q", true).is_err());
        assert!(s.asked.len() <= MAX_RETRIES);
    }

    #[test]
    fn choose_takes_a_number_or_the_default() {
        let opts = ["Ctrl+Space", "Ctrl+Alt+Space", "Alt+Space"];
        let mut s = Scripted::new([""]);
        assert_eq!(s.choose("pick", &opts, 0).unwrap(), 0);
        let mut s = Scripted::new(["2"]);
        assert_eq!(s.choose("pick", &opts, 0).unwrap(), 1);
        let mut s = Scripted::new(["9", "3"]);
        assert_eq!(s.choose("pick", &opts, 0).unwrap(), 2);
    }

    fn drive_items() -> (Vec<String>, Vec<bool>) {
        (
            vec![
                "C: OS (NTFS)".into(),
                "D: Data (NTFS)".into(),
                "E: Stick (exFAT)".into(),
            ],
            vec![true, true, false],
        )
    }

    #[test]
    fn multi_select_parses_lists_and_all() {
        let (items, sel) = drive_items();

        let mut s = Scripted::new(["1,2"]);
        assert_eq!(s.multi_select("q", &items, &sel, &[0]).unwrap(), vec![0, 1]);

        // 'all' must skip the entries that cannot be indexed.
        let mut s = Scripted::new(["all"]);
        assert_eq!(s.multi_select("q", &items, &sel, &[0]).unwrap(), vec![0, 1]);

        let mut s = Scripted::new([" 2 , 1 "]);
        assert_eq!(s.multi_select("q", &items, &sel, &[0]).unwrap(), vec![0, 1]);

        let mut s = Scripted::new(["2,2,1"]);
        assert_eq!(s.multi_select("q", &items, &sel, &[0]).unwrap(), vec![0, 1]);
    }

    #[test]
    fn multi_select_refuses_unusable_drives_with_a_reason() {
        let (items, sel) = drive_items();
        let mut s = Scripted::new(["3", "1"]);
        assert_eq!(s.multi_select("q", &items, &sel, &[0]).unwrap(), vec![0]);
        assert!(
            s.mentioned("cannot be indexed"),
            "must say why, not just refuse"
        );
    }

    #[test]
    fn multi_select_rejects_out_of_range_and_empty_choices() {
        let (items, sel) = drive_items();
        let mut s = Scripted::new(["7", "x", "1"]);
        assert_eq!(s.multi_select("q", &items, &sel, &[0]).unwrap(), vec![0]);
        assert!(s.mentioned("not one of the numbers"));
    }
}
