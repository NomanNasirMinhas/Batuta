//! The read-eval-print loop: the program that runs inside the terminal.
//!
//! This is where the shell reads the keyboard itself. Raw mode is on only
//! while a line is being typed and is turned off before anything is run —
//! a child process needs the console in its normal state, or `vim` opens into
//! a terminal whose echo and line discipline have been taken away from it.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::crossterm::terminal::{disable_raw_mode, enable_raw_mode};

use super::builtins::State;
use super::line::{self, Candidates, History, Line};
use super::{parse, run};

/// Completion candidates from the real filesystem, relative to the shell's
/// directory rather than the process's.
struct Disk<'a>(&'a Path);

impl Candidates for Disk<'_> {
    fn list(&self, dir: &str) -> Vec<(String, bool)> {
        let path = if dir == "." {
            self.0.to_path_buf()
        } else if Path::new(dir).is_absolute() {
            PathBuf::from(dir)
        } else {
            self.0.join(dir)
        };
        std::fs::read_dir(path)
            .map(|read| {
                read.flatten()
                    .map(|e| {
                        let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
                        (e.file_name().to_string_lossy().into_owned(), is_dir)
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// The prompt, kept short: the folder you are in and a marker.
fn prompt(state: &State) -> String {
    let where_ = state
        .cwd
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| state.cwd.display().to_string());
    format!("\x1b[36m{where_}\x1b[0m \x1b[32m>\x1b[0m ")
}

/// Redraw the line being edited, and put the caret where it belongs.
fn redraw(prompt: &str, line: &Line) -> std::io::Result<()> {
    let mut out = std::io::stdout();
    // Carriage return, erase to end of line, then the whole thing again.
    // Redrawing entirely is far simpler than tracking what changed, and at a
    // line's worth of text the difference is not measurable.
    write!(out, "\r\x1b[K{prompt}{}", line.text())?;
    let back = line.len() - line.caret();
    if back > 0 {
        write!(out, "\x1b[{back}D")?;
    }
    out.flush()
}

/// What reading a line produced.
enum Input {
    Line(String),
    /// Ctrl+C: abandon this line, keep the shell.
    Cancelled,
    /// Ctrl+D on an empty line, or the console going away.
    Eof,
}

fn read_line(state: &State, history: &mut History) -> std::io::Result<Input> {
    let prompt = prompt(state);
    let mut line = Line::default();
    redraw(&prompt, &line)?;

    loop {
        let Event::Key(key) = event::read()? else {
            continue;
        };
        // Windows reports both press and release; acting on both types
        // everything twice.
        if key.kind != KeyEventKind::Press {
            continue;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

        match key.code {
            KeyCode::Enter => {
                let mut out = std::io::stdout();
                writeln!(out)?;
                out.flush()?;
                return Ok(Input::Line(line.text().to_string()));
            }
            KeyCode::Char('c' | 'C') if ctrl => {
                let mut out = std::io::stdout();
                writeln!(out, "^C")?;
                out.flush()?;
                return Ok(Input::Cancelled);
            }
            KeyCode::Char('d' | 'D') if ctrl => {
                if line.is_empty() {
                    return Ok(Input::Eof);
                }
                line.delete();
            }
            KeyCode::Char('u' | 'U') if ctrl => line.delete_to_start(),
            KeyCode::Char('w' | 'W') if ctrl => line.delete_word(),
            KeyCode::Char('l' | 'L') if ctrl => {
                let mut out = std::io::stdout();
                write!(out, "\x1b[2J\x1b[H")?;
                out.flush()?;
            }
            KeyCode::Char(c) => line.insert(c),

            KeyCode::Backspace => line.backspace(),
            KeyCode::Delete => line.delete(),
            KeyCode::Left => line.left(),
            KeyCode::Right => line.right(),
            KeyCode::Home => line.home(),
            KeyCode::End => line.end(),

            KeyCode::Up => {
                if let Some(entry) = history.previous(line.text()) {
                    line.set(entry);
                }
            }
            KeyCode::Down => {
                if let Some(entry) = history.next() {
                    line.set(entry);
                }
            }

            KeyCode::Tab => {
                let (start, word) = line.word_at_caret();
                if let Some(completed) = line::complete(&word, &Disk(&state.cwd)) {
                    // Replace just the word, so the rest of the line and
                    // anything after the caret survive.
                    let mut text: Vec<char> = line.text().chars().collect();
                    text.splice(start..line.caret(), completed.chars());
                    let rebuilt: String = text.into_iter().collect();
                    let caret = start + completed.chars().count();
                    line.set(rebuilt);
                    // `set` puts the caret at the end; put it back where the
                    // completion finished so text after it stays reachable.
                    while line.caret() > caret {
                        line.left();
                    }
                }
            }
            _ => {}
        }
        redraw(&prompt, &line)?;
    }
}

/// Survive the Ctrl+C that stops a child.
///
/// Ctrl+C is delivered to every process attached to the console, and the
/// default handler ends the process. Without this, interrupting a long `ping`
/// or `cargo build` would take the shell — and with it the whole terminal
/// pane — down alongside it. Returning true says the event is handled: the
/// child, which has not claimed it, still dies.
///
/// While a line is being typed the console is in raw mode, so Ctrl+C arrives
/// as an ordinary key and never reaches here at all.
#[cfg(windows)]
fn survive_interrupts() {
    use windows_sys::Win32::Foundation::{BOOL, TRUE};
    use windows_sys::Win32::System::Console::{
        SetConsoleCtrlHandler, CTRL_BREAK_EVENT, CTRL_C_EVENT,
    };

    unsafe extern "system" fn handler(event: u32) -> BOOL {
        // Close, logoff and shutdown are deliberately not claimed: those mean
        // the shell really should go.
        BOOL::from(event == CTRL_C_EVENT || event == CTRL_BREAK_EVENT)
    }

    unsafe {
        SetConsoleCtrlHandler(Some(handler), TRUE);
    }
}

#[cfg(not(windows))]
fn survive_interrupts() {}

/// Run the shell until it is asked to stop.
pub fn main_loop() -> anyhow::Result<i32> {
    survive_interrupts();

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut state = State {
        cwd,
        previous: None,
        vars: HashMap::new(),
    };
    let mut history = History::default();
    let mut status = 0;

    {
        let mut out = std::io::stdout();
        let _ = writeln!(out, "batuta shell — type `help` for what it knows");
        let _ = out.flush();
    }

    loop {
        // Raw mode only while the line is being read. A child launched below
        // needs the console the way it expects to find it.
        enable_raw_mode()?;
        let input = read_line(&state, &mut history);
        disable_raw_mode()?;

        let line = match input {
            Ok(Input::Line(line)) => line,
            Ok(Input::Cancelled) => continue,
            Ok(Input::Eof) => break,
            // The console has gone; there is nobody left to report to.
            Err(_) => break,
        };

        if line.trim().is_empty() {
            continue;
        }
        history.push(&line);

        let vars = state.vars.clone();
        let env = move |name: &str| vars.get(name).cloned().or_else(|| std::env::var(name).ok());

        let pipeline = match parse::parse(&line, &env) {
            Ok(p) => p,
            Err(e) => {
                let _ = writeln!(std::io::stderr(), "batuta: {e}");
                status = 1;
                continue;
            }
        };

        match run::run(&mut state, &pipeline) {
            Ok(outcome) => {
                status = outcome.status;
                if outcome.exit {
                    break;
                }
            }
            Err(e) => {
                let _ = writeln!(std::io::stderr(), "batuta: {e}");
                status = 1;
            }
        }
    }

    Ok(status)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_prompt_names_the_folder_you_are_in() {
        let state = State {
            cwd: PathBuf::from(r"C:\Code\Batuta"),
            previous: None,
            vars: HashMap::new(),
        };
        let p = prompt(&state);
        assert!(p.contains("Batuta"), "{p}");
        assert!(p.contains('>'), "there should be something to type after");
    }

    #[test]
    fn a_drive_root_still_produces_a_prompt() {
        // `C:\` has no file name, and an empty prompt would look broken.
        let state = State {
            cwd: PathBuf::from(r"C:\"),
            previous: None,
            vars: HashMap::new(),
        };
        assert!(prompt(&state).contains("C:"), "{}", prompt(&state));
    }

    #[test]
    fn completion_candidates_come_from_the_shells_directory() {
        // Not the process's: the shell moves with `cd`, the process does not.
        let dir = std::env::temp_dir().join(format!("batuta-cand-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("subdir")).unwrap();
        std::fs::write(dir.join("file.txt"), b"x").unwrap();

        let found = Disk(&dir).list(".");
        assert!(found.iter().any(|(n, d)| n == "subdir" && *d));
        assert!(found.iter().any(|(n, d)| n == "file.txt" && !*d));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn listing_somewhere_unreadable_offers_nothing_rather_than_failing() {
        assert!(Disk(Path::new(r"C:\nope-does-not-exist"))
            .list(".")
            .is_empty());
    }
}
