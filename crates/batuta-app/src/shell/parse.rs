//! Turning a typed line into something runnable.
//!
//! Quoting, expansion, pipes and redirection all decided here, with no I/O, so
//! the part where shells traditionally go wrong is testable on its own.
//!
//! ## What is deliberately not supported
//!
//! No control flow, no functions, no globbing, no subshells. This is a shell
//! for running commands in a folder you just found, not a language — PowerShell
//! already exists and is better at being one. Everything below is there because
//! typing a command without it is annoying.

#[cfg(test)]
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Redirect {
    /// `> path` or `>> path`
    Out { path: String, append: bool },
    /// `2> path` or `2>> path`
    Err { path: String, append: bool },
    /// `< path`
    In { path: String },
    /// `2>&1`
    ErrToOut,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Command {
    /// Program name followed by its arguments, already expanded and unquoted.
    pub words: Vec<String>,
    pub redirects: Vec<Redirect>,
}

impl Command {
    pub fn program(&self) -> Option<&str> {
        self.words.first().map(String::as_str)
    }

    pub fn args(&self) -> &[String] {
        self.words.get(1..).unwrap_or(&[])
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Pipeline {
    pub commands: Vec<Command>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    UnterminatedQuote(char),
    /// A `|` with nothing on one side of it.
    EmptyStage,
    /// A redirection with no filename after it.
    MissingTarget(String),
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::UnterminatedQuote(q) => write!(f, "unterminated {q} quote"),
            ParseError::EmptyStage => write!(f, "a pipe needs a command on both sides"),
            ParseError::MissingTarget(op) => write!(f, "{op} needs a file after it"),
        }
    }
}

impl std::error::Error for ParseError {}

/// A token, with enough context to know whether it may still be expanded.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    /// A word, already unquoted and expanded.
    Word(String),
    Pipe,
    /// A redirection operator, awaiting its target.
    Op(String),
    /// `2>&1`, which takes no target.
    ErrToOut,
}

/// Where environment variables come from.
///
/// Injected rather than read from the process so expansion is testable, and so
/// a future `set` builtin has somewhere to put things.
pub type Env<'a> = &'a dyn Fn(&str) -> Option<String>;

/// Expand `$VAR`, `${VAR}` and `%VAR%` in a word.
///
/// Both syntaxes because both get typed: `%USERPROFILE%` is what Windows
/// documents everywhere, and `$HOME` is what everyone's fingers do. An unset
/// variable expands to nothing, which is what every shell does and is less
/// surprising than leaving the literal text.
fn expand(text: &str, env: Env) -> String {
    let mut out = String::with_capacity(text.len());
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        match chars[i] {
            '$' if i + 1 < chars.len() => {
                let (name, next) = if chars[i + 1] == '{' {
                    let close = chars[i + 2..].iter().position(|&c| c == '}');
                    match close {
                        Some(rel) => {
                            let name: String = chars[i + 2..i + 2 + rel].iter().collect();
                            (name, i + 3 + rel)
                        }
                        // No closing brace: it is text, not a variable.
                        None => {
                            out.push('$');
                            i += 1;
                            continue;
                        }
                    }
                } else {
                    let end = chars[i + 1..]
                        .iter()
                        .position(|c| !c.is_alphanumeric() && *c != '_')
                        .map(|rel| i + 1 + rel)
                        .unwrap_or(chars.len());
                    (chars[i + 1..end].iter().collect::<String>(), end)
                };

                if name.is_empty() {
                    out.push('$');
                    i += 1;
                } else {
                    out.push_str(&env(&name).unwrap_or_default());
                    i = next;
                }
            }
            '%' => {
                // `%VAR%` only when there is a closing `%` and something
                // plausible between: a lone `%` is an ordinary character, and
                // people do type it.
                match chars[i + 1..].iter().position(|&c| c == '%') {
                    Some(rel) if rel > 0 => {
                        let name: String = chars[i + 1..i + 1 + rel].iter().collect();
                        if name.chars().all(|c| c.is_alphanumeric() || c == '_') {
                            out.push_str(&env(&name).unwrap_or_default());
                            i = i + rel + 2;
                        } else {
                            out.push('%');
                            i += 1;
                        }
                    }
                    _ => {
                        out.push('%');
                        i += 1;
                    }
                }
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

/// Expand a leading `~` to the home directory.
fn expand_home(word: &str, env: Env) -> String {
    if word == "~" || word.starts_with("~\\") || word.starts_with("~/") {
        if let Some(home) = env("USERPROFILE").or_else(|| env("HOME")) {
            return format!("{home}{}", &word[1..]);
        }
    }
    word.to_string()
}

// `flush!` resets its flags so it can be used again; the last call, after the
// loop, resets them for nobody. Leaving the resets in is what keeps the macro
// safe to call anywhere rather than only in the middle.
#[allow(unused_assignments)]
fn tokenize(line: &str, env: Env) -> Result<Vec<Token>, ParseError> {
    let mut tokens = Vec::new();
    // The word being built, and the unquoted run not yet expanded into it.
    //
    // Expansion has to happen where each unquoted run *ends*, not once over
    // the finished word: expanding at the end would re-expand text that came
    // out of single quotes, which is the one place a `$` must survive.
    let mut word = String::new();
    let mut raw = String::new();
    // A word that was quoted is still a word even when it is empty, which is
    // how `echo ""` passes an empty argument rather than none.
    let mut had_word = false;
    // `~` is only a home directory when nothing about the word was quoted.
    let mut quoted = false;

    let chars: Vec<char> = line.chars().collect();
    let mut i = 0;

    macro_rules! take_raw {
        () => {
            if !raw.is_empty() {
                word.push_str(&expand(&raw, env));
                raw.clear();
            }
        };
    }

    macro_rules! flush {
        () => {{
            take_raw!();
            if had_word {
                let finished = if quoted {
                    std::mem::take(&mut word)
                } else {
                    expand_home(&word, env)
                };
                word.clear();
                tokens.push(Token::Word(finished));
                had_word = false;
                quoted = false;
            }
        }};
    }

    while i < chars.len() {
        let c = chars[i];
        match c {
            ' ' | '\t' => {
                flush!();
                i += 1;
            }
            '|' => {
                flush!();
                tokens.push(Token::Pipe);
                i += 1;
            }
            '\'' => {
                // Single quotes are literal: no expansion at all, which is the
                // only way to type a path containing a `$` or `%`.
                take_raw!();
                let close = chars[i + 1..]
                    .iter()
                    .position(|&c| c == '\'')
                    .ok_or(ParseError::UnterminatedQuote('\''))?;
                word.extend(&chars[i + 1..i + 1 + close]);
                had_word = true;
                quoted = true;
                i += close + 2;
            }
            '"' => {
                take_raw!();
                let close = chars[i + 1..]
                    .iter()
                    .position(|&c| c == '"')
                    .ok_or(ParseError::UnterminatedQuote('"'))?;
                let inner: String = chars[i + 1..i + 1 + close].iter().collect();
                word.push_str(&expand(&inner, env));
                had_word = true;
                quoted = true;
                i += close + 2;
            }
            '<' => {
                flush!();
                tokens.push(Token::Op("<".into()));
                i += 1;
            }
            '>' => {
                flush!();
                let append = chars.get(i + 1) == Some(&'>');
                tokens.push(Token::Op(if append { ">>".into() } else { ">".into() }));
                i += if append { 2 } else { 1 };
            }
            // `2>`, `2>>` and `2>&1`, but only when the `2` stands alone —
            // `file2>out` redirects from `file2`, not from a stream.
            '2' if !had_word && raw.is_empty() && chars.get(i + 1) == Some(&'>') => {
                if chars.get(i + 2) == Some(&'&') && chars.get(i + 3) == Some(&'1') {
                    tokens.push(Token::ErrToOut);
                    i += 4;
                } else {
                    let append = chars.get(i + 2) == Some(&'>');
                    tokens.push(Token::Op(if append { "2>>".into() } else { "2>".into() }));
                    i += if append { 3 } else { 2 };
                }
            }
            _ => {
                raw.push(c);
                had_word = true;
                i += 1;
            }
        }
    }
    flush!();

    Ok(tokens)
}

/// Parse a line into a pipeline.
pub fn parse(line: &str, env: Env) -> Result<Pipeline, ParseError> {
    let tokens = tokenize(line, env)?;
    if tokens.is_empty() {
        return Ok(Pipeline::default());
    }

    let mut commands = Vec::new();
    let mut current = Command::default();
    let mut iter = tokens.into_iter().peekable();
    let mut saw_anything = false;

    while let Some(token) = iter.next() {
        match token {
            Token::Word(w) => {
                current.words.push(w);
                saw_anything = true;
            }
            Token::ErrToOut => {
                current.redirects.push(Redirect::ErrToOut);
                saw_anything = true;
            }
            Token::Pipe => {
                if current.words.is_empty() {
                    return Err(ParseError::EmptyStage);
                }
                commands.push(std::mem::take(&mut current));
                saw_anything = false;
            }
            Token::Op(op) => {
                let target = match iter.next() {
                    Some(Token::Word(w)) => w,
                    _ => return Err(ParseError::MissingTarget(op)),
                };
                current.redirects.push(match op.as_str() {
                    ">" => Redirect::Out {
                        path: target,
                        append: false,
                    },
                    ">>" => Redirect::Out {
                        path: target,
                        append: true,
                    },
                    "2>" => Redirect::Err {
                        path: target,
                        append: false,
                    },
                    "2>>" => Redirect::Err {
                        path: target,
                        append: true,
                    },
                    _ => Redirect::In { path: target },
                });
                saw_anything = true;
            }
        }
    }

    if current.words.is_empty() {
        // A trailing pipe, or a line that was only a redirection.
        if saw_anything || !commands.is_empty() {
            return Err(ParseError::EmptyStage);
        }
    } else {
        commands.push(current);
    }

    Ok(Pipeline { commands })
}

/// An environment backed by a map. Only the tests build one: the shell
/// itself layers its own variables over the process environment.
#[cfg(test)]
pub fn map_env(vars: HashMap<String, String>) -> impl Fn(&str) -> Option<String> {
    move |name| vars.get(name).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> impl Fn(&str) -> Option<String> {
        let mut vars = HashMap::new();
        vars.insert("USERPROFILE".to_string(), r"C:\Users\Dev".to_string());
        vars.insert("NAME".to_string(), "batuta".to_string());
        vars.insert("WITH SPACE".to_string(), "nope".to_string());
        vars.insert("DIR".to_string(), r"C:\Program Files".to_string());
        map_env(vars)
    }

    fn p(line: &str) -> Pipeline {
        parse(line, &env()).expect("should parse")
    }

    fn words(line: &str) -> Vec<String> {
        p(line).commands[0].words.clone()
    }

    #[test]
    fn a_bare_command_is_a_program_and_its_arguments() {
        assert_eq!(words("git status --short"), ["git", "status", "--short"]);
    }

    #[test]
    fn an_empty_line_is_a_pipeline_with_nothing_in_it() {
        assert!(p("").commands.is_empty());
        assert!(p("   \t ").commands.is_empty());
    }

    #[test]
    fn quotes_hold_a_path_with_spaces_together() {
        // The single most common thing to get wrong on Windows.
        assert_eq!(
            words(r#"cd "C:\Program Files\Git""#),
            ["cd", r"C:\Program Files\Git"]
        );
        assert_eq!(words("echo 'a b c'"), ["echo", "a b c"]);
    }

    #[test]
    fn single_quotes_are_literal_and_double_quotes_expand() {
        assert_eq!(words(r#"echo "$NAME""#), ["echo", "batuta"]);
        assert_eq!(words("echo '$NAME'"), ["echo", "$NAME"]);
    }

    #[test]
    fn an_empty_quoted_string_is_still_an_argument() {
        // `echo ""` passes one empty argument, not none.
        assert_eq!(words(r#"echo "" x"#), ["echo", "", "x"]);
    }

    #[test]
    fn both_variable_syntaxes_work_because_both_get_typed() {
        assert_eq!(words("echo $NAME"), ["echo", "batuta"]);
        assert_eq!(words("echo ${NAME}"), ["echo", "batuta"]);
        assert_eq!(words("echo %NAME%"), ["echo", "batuta"]);
    }

    #[test]
    fn an_expanded_variable_containing_spaces_stays_one_argument() {
        // Splitting here is why "it worked until the path had a space in it"
        // is such a common complaint about shells.
        assert_eq!(words("cd $DIR"), ["cd", r"C:\Program Files"]);
    }

    #[test]
    fn an_unset_variable_expands_to_nothing() {
        assert_eq!(words("echo $NOPE end"), ["echo", "", "end"]);
    }

    #[test]
    fn a_lone_percent_or_dollar_is_just_a_character() {
        // People type these. Treating every one as a broken variable would be
        // worse than leaving them alone.
        assert_eq!(words("echo 50%"), ["echo", "50%"]);
        assert_eq!(words("echo 100% done"), ["echo", "100%", "done"]);
        assert_eq!(words("echo $"), ["echo", "$"]);
    }

    #[test]
    fn tilde_becomes_the_home_directory() {
        assert_eq!(words("cd ~"), ["cd", r"C:\Users\Dev"]);
        assert_eq!(words(r"cd ~\Documents"), ["cd", r"C:\Users\Dev\Documents"]);
        // Only leading, and only as its own segment.
        assert_eq!(words("echo a~b"), ["echo", "a~b"]);
    }

    #[test]
    fn a_pipeline_splits_into_stages() {
        let pipe = p("dir | findstr rs | more");
        assert_eq!(pipe.commands.len(), 3);
        assert_eq!(pipe.commands[0].program(), Some("dir"));
        assert_eq!(pipe.commands[1].args(), ["rs"]);
        assert_eq!(pipe.commands[2].program(), Some("more"));
    }

    #[test]
    fn redirections_attach_to_their_stage() {
        let pipe = p("cargo build > out.txt 2> err.txt");
        let cmd = &pipe.commands[0];
        assert_eq!(cmd.words, ["cargo", "build"]);
        assert_eq!(
            cmd.redirects,
            vec![
                Redirect::Out {
                    path: "out.txt".into(),
                    append: false
                },
                Redirect::Err {
                    path: "err.txt".into(),
                    append: false
                }
            ]
        );
    }

    #[test]
    fn appending_is_distinct_from_truncating() {
        // Getting this backwards destroys a log file.
        let out = p("echo hi >> log.txt").commands[0].redirects.clone();
        assert_eq!(
            out,
            vec![Redirect::Out {
                path: "log.txt".into(),
                append: true
            }]
        );
    }

    #[test]
    fn stderr_can_be_folded_into_stdout() {
        let cmd = p("cargo build 2>&1").commands[0].clone();
        assert_eq!(cmd.words, ["cargo", "build"]);
        assert_eq!(cmd.redirects, vec![Redirect::ErrToOut]);
    }

    #[test]
    fn a_two_in_a_filename_is_not_a_stream_number() {
        // `file2>out` redirects from a file called `file2`.
        let cmd = p("echo file2>out.txt").commands[0].clone();
        assert_eq!(cmd.words, ["echo", "file2"]);
        assert_eq!(
            cmd.redirects,
            vec![Redirect::Out {
                path: "out.txt".into(),
                append: false
            }]
        );
    }

    #[test]
    fn input_can_be_redirected() {
        let cmd = p("sort < names.txt").commands[0].clone();
        assert_eq!(
            cmd.redirects,
            vec![Redirect::In {
                path: "names.txt".into()
            }]
        );
    }

    #[test]
    fn redirection_without_spaces_still_parses() {
        let cmd = p("echo hi>out.txt").commands[0].clone();
        assert_eq!(cmd.words, ["echo", "hi"]);
        assert_eq!(
            cmd.redirects,
            vec![Redirect::Out {
                path: "out.txt".into(),
                append: false
            }]
        );
    }

    #[test]
    fn an_unterminated_quote_is_reported_rather_than_guessed() {
        let e = parse(r#"echo "unfinished"#, &env()).unwrap_err();
        assert_eq!(e, ParseError::UnterminatedQuote('"'));
        assert!(e.to_string().contains("unterminated"));
    }

    #[test]
    fn a_pipe_with_nothing_on_one_side_is_an_error() {
        assert_eq!(
            parse("| grep x", &env()).unwrap_err(),
            ParseError::EmptyStage
        );
        assert_eq!(parse("dir |", &env()).unwrap_err(), ParseError::EmptyStage);
        assert_eq!(
            parse("dir | | wc", &env()).unwrap_err(),
            ParseError::EmptyStage
        );
    }

    #[test]
    fn a_redirection_with_no_file_is_an_error() {
        let e = parse("echo hi >", &env()).unwrap_err();
        assert_eq!(e, ParseError::MissingTarget(">".into()));
        assert!(e.to_string().contains("needs a file"));
    }

    #[test]
    fn quoting_survives_a_path_that_looks_like_an_operator() {
        // A filename really can contain these.
        assert_eq!(words(r#"cat "weird>name.txt""#), ["cat", "weird>name.txt"]);
        assert_eq!(words(r#"cat "a|b.txt""#), ["cat", "a|b.txt"]);
    }
}
