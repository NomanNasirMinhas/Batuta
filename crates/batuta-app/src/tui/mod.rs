//! The interactive terminal UI.
//!
//! It is a thin client over the same request/response protocol the CLI uses.
//! When the daemon is running it answers from a live index; otherwise the TUI
//! loads the snapshot itself and serves its own queries, so it works either
//! way.
//!
//! Two properties matter for it to feel instant on millions of files:
//!
//! - **Windowing.** Only the rows currently on screen are ever fetched or
//!   held, so a query matching half the volume costs the same as one matching
//!   ten files.
//! - **A persistent `Searcher`.** Whether local or over the pipe, the same
//!   searcher is reused across keystrokes, so typing forward narrows the
//!   previous result set instead of rescanning the name arena.

pub(crate) mod app;
mod ui;

pub use app::{App, Mode};

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::prelude::*;

use batuta_core::search::Searcher;
use batuta_core::Index;
use batuta_ipc::{Request, Response};

use crate::config::Config;
use crate::pipe::PipeStream;
use crate::{query, scan};

/// Where answers come from.
enum Source {
    /// A running daemon, whose index is live.
    Daemon(Box<PipeStream>),
    /// This process, answering from the last snapshot.
    Local {
        index: Box<Index>,
        searcher: Searcher,
    },
}

impl Source {
    fn ask(&mut self, req: &Request) -> Response {
        match self {
            Source::Daemon(stream) => stream.request(req).unwrap_or_else(|e| Response::Error {
                message: format!("daemon: {e}"),
            }),
            Source::Local { index, searcher } => query::execute(index, req, searcher, false, 0),
        }
    }

    fn is_live(&self) -> bool {
        matches!(self, Source::Daemon(_))
    }
}

/// Restores the terminal even if the UI panics.
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(std::io::stdout(), LeaveAlternateScreen, DisableMouseCapture);
    }
}

pub fn run(cfg: &Config, source: scan::Source) -> Result<()> {
    // Prefer the daemon: its index is live, and using it costs no elevation.
    let mut backend = if source == scan::Source::Cached {
        match PipeStream::connect() {
            Ok(s) => Source::Daemon(Box::new(s)),
            Err(_) => local_source(cfg, source)?,
        }
    } else {
        local_source(cfg, source)?
    };

    let mut app = App::new(backend.is_live());
    app.status = if app.live {
        "connected to daemon (live index)".into()
    } else {
        "using cached index; run `batuta serve` for live sizes".into()
    };

    // Opened by the hotkey we get a console of our own; use all of it. A
    // terminal the user already had open is left at whatever size they chose.
    if crate::console::owns_console_alone() {
        crate::console::maximize();
    }

    enable_raw_mode().context("entering raw mode")?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture).context("entering alt screen")?;
    let _guard = TerminalGuard;

    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    let result = event_loop(&mut terminal, &mut app, &mut backend);

    // The guard restores the terminal; surface any UI error afterwards.
    drop(_guard);
    terminal.show_cursor().ok();
    result
}

fn local_source(cfg: &Config, source: scan::Source) -> Result<Source> {
    let built = scan::load_index(cfg, source)?;
    Ok(Source::Local {
        index: Box::new(built.index),
        searcher: Searcher::new(),
    })
}

fn event_loop<B: Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
    backend: &mut Source,
) -> Result<()> {
    // Rows available for results, learned from the first draw.
    let mut visible = terminal
        .size()
        .map(|s| s.height.saturating_sub(5) as usize)
        .unwrap_or(20);
    let mut last_input = Instant::now();
    let mut pending = true;

    const DEBOUNCE: Duration = Duration::from_millis(30);

    loop {
        terminal.draw(|f| {
            visible = ui::draw(f, app);
        })?;

        // Wait for input, but never longer than the debounce window, so a
        // query that has settled still gets issued.
        if event::poll(DEBOUNCE)? {
            // Drain the whole burst before doing any work. Typing quickly
            // otherwise costs one query per character, and each of those
            // delays the next keystroke from even being read.
            loop {
                match event::read()? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        handle_key(app, key, visible);
                        last_input = Instant::now();
                        pending = true;
                    }
                    Event::Mouse(m) => match m.kind {
                        MouseEventKind::ScrollDown => app.move_selection(3, visible),
                        MouseEventKind::ScrollUp => app.move_selection(-3, visible),
                        _ => {}
                    },
                    Event::Resize(_, _) => pending = true,
                    _ => {}
                }
                if app.quit || !event::poll(Duration::ZERO)? {
                    break;
                }
            }
        }

        if app.quit {
            return Ok(());
        }

        // Refresh only once typing has paused. Scrolling is served from the
        // previous ordering, so it does not wait on this.
        if (app.dirty || pending) && last_input.elapsed() >= DEBOUNCE {
            refresh(app, backend, visible);
            pending = false;
        }
    }
}

fn refresh(app: &mut App, backend: &mut Source, visible: usize) {
    // A duplicate scan reads file contents and takes seconds, so it runs only
    // when one is explicitly pending. Every other refresh in the mode —
    // scrolling, re-entering it — is served by moving the window over the
    // cache.
    if app.mode == Mode::Dupes && !app.dupes_pending {
        app.window_dupes(visible);
        return;
    }

    // Did this refresh answer a change of state — a new query, filter, mode,
    // or window — or is it merely settling after a keystroke that changed
    // nothing? Every key press arms `pending`, so a refresh fires ~30ms after
    // Enter on a missing file, and only a state change may erase what the
    // status line has to say. Otherwise the explanation ("no longer on
    // disk: ...") is wiped before the user can read it, and the keypress
    // reads as having done nothing at all.
    let state_changed = app.dirty;

    let req = app.request(visible);
    match backend.ask(&req) {
        Response::Rows {
            rows,
            total,
            elapsed_us,
        } => {
            if app.mode == Mode::Bloat {
                app.apply_bloat(rows, visible);
            } else {
                app.apply_rows(rows, total, elapsed_us, visible);
            }
            if state_changed {
                app.status.clear();
            }
        }
        Response::Dupes {
            groups,
            wasted_total,
            elapsed_us,
        } => {
            app.apply_dupes(groups, wasted_total, elapsed_us, visible);
            app.status.clear();
        }
        Response::Error { message } => {
            app.status = message;
            if app.mode == Mode::Dupes {
                // Leave the cache alone and stay armed, so F5 can retry.
                app.dupes_pending = true;
                app.dirty = false;
            } else {
                app.apply_rows(Vec::new(), 0, 0, visible);
            }
        }
        other => {
            app.status = format!("unexpected response: {other:?}");
            app.dirty = false;
        }
    }
}

fn handle_key(app: &mut App, key: KeyEvent, visible: usize) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

    // A confirmation prompt takes the keyboard entirely. Letting keys through
    // would mean typing into the query behind a dialog, or worse, having
    // Enter mean two different things at once.
    if app.awaiting_confirmation() {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => confirm_delete(app),
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                app.cancel_delete();
                app.status = "deletion cancelled".into();
            }
            // Enter deliberately does nothing: it is the key most likely to be
            // hit out of habit, and this dialog destroys data.
            _ => {}
        }
        return;
    }

    match key.code {
        // Ctrl arms must come before the plain-character arm below, or
        // these letters would be typed into the query instead.
        KeyCode::Char('c') if ctrl => app.quit = true,
        KeyCode::Char('u') if ctrl => app.clear_query(),
        KeyCode::Char('w') if ctrl => app.delete_word(),
        KeyCode::Char('s') if ctrl => app.cycle_sort(),
        KeyCode::Char('t') if ctrl => app.cycle_kind(),
        // The duplicate scan reads file contents, so it can take a while.
        // Announce it before the request blocks the loop, so the user sees
        // why the UI has gone quiet instead of a frozen result box.
        KeyCode::Char('d') if ctrl => {
            if app.mode != Mode::Dupes && app.dupes_pending {
                app.status = "scanning for duplicates; hashing file contents...".into();
            }
            app.toggle_dupes();
        }
        // Escape always closes. Reached by a hotkey, this is a launcher: you
        // press the shortcut, look, and dismiss it. Making Escape clear the
        // query first would mean pressing it twice to get rid of the window.
        // Ctrl+U is there for clearing without closing.
        KeyCode::Esc => app.quit = true,

        KeyCode::Char(c) if !ctrl => app.push_char(c),
        KeyCode::Backspace => app.backspace(),

        KeyCode::Up => app.move_selection(-1, visible),
        KeyCode::Down => app.move_selection(1, visible),
        KeyCode::PageUp => app.move_selection(-(visible as isize), visible),
        KeyCode::PageDown => app.move_selection(visible as isize, visible),
        KeyCode::Home => app.go_first(visible),
        KeyCode::End => app.go_last(visible),

        // Tab only ever completes a path. It used to fall back to switching
        // modes when there was nothing to complete, which meant a stray Tab
        // silently threw you into another view. Shift+Tab is the one key that
        // changes mode, so neither can be mistaken for the other.
        KeyCode::Tab => {
            if let Some(text) = app.complete_selection() {
                app.set_query(&text);
            }
        }
        KeyCode::BackTab => app.cycle_mode(),

        // Delete asks first; nothing is removed without an explicit answer.
        KeyCode::Delete => {
            if !app.ask_delete() {
                app.status = "nothing selected to delete".into();
            }
        }
        // F5 re-runs whatever is on screen. In the duplicates view that means
        // the expensive scan, not just a refetch of the window.
        KeyCode::F(5) => {
            if app.mode == Mode::Dupes {
                app.status = "scanning for duplicates; hashing file contents...".into();
                app.dupes_pending = true;
            } else {
                app.dirty = true;
            }
        }

        // Opening a result finishes the job the window was opened for, so it
        // closes on the way out. A failed reveal leaves it up, because the
        // reason is on the status line and closing would hide it.
        KeyCode::Enter => app.quit = reveal(app),
        _ => {}
    }
}

/// Carry out a confirmed deletion.
///
/// Recursive for a directory, which is why the prompt states the file count
/// first. The index is left alone: the change journal reports the removal and
/// the daemon applies it, so forcing it here would only race that.
fn confirm_delete(app: &mut App) {
    let Some(pending) = app.confirm_delete.take() else {
        return;
    };
    let path = std::path::Path::new(&pending.path);

    let result = if pending.is_dir {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    };

    app.status = match result {
        Ok(()) => {
            // Drop it from the visible list straight away rather than waiting
            // for the journal round trip, so the row does not linger.
            app.dirty = true;
            format!("deleted {}", pending.path)
        }
        Err(e) => format!("could not delete {}: {e}", pending.path),
    };
}

/// Build the command line for `explorer /select`.
///
/// Explorer does not follow the usual Windows argument-quoting rules. It wants
/// `/select,` unquoted with the path quoted separately. Handing the whole
/// thing to `Command::arg` lets Rust escape it as one argument —
/// `"/select,C:\...\Telegram Desktop\x.png"` — which Explorer cannot parse and
/// answers by silently opening the user's Documents folder instead. So the
/// command line is written literally and passed with `raw_arg`.
///
/// A trailing separator is stripped: `/select` on `C:\Foo\` selects nothing,
/// while on `C:\Foo` it highlights `Foo` inside its parent. A bare volume root
/// keeps its separator, because `C:` names the current directory on drive C:
/// rather than the drive itself.
fn reveal_arg(path: &str) -> String {
    let trimmed = path.trim_end_matches(['\\', '/']);
    let is_bare_drive = trimmed.len() == 2 && trimmed.ends_with(':');
    let target = if trimmed.is_empty() || is_bare_drive {
        path
    } else {
        trimmed
    };
    format!("/select,\"{target}\"")
}

/// Open Explorer with the selected entry highlighted.
///
/// Launching a process is the one outward action the UI takes, and only ever
/// in response to the user pressing Enter on a row they selected.
///
/// Returns whether Explorer was actually launched, so the caller knows if
/// there is a message worth staying open to show.
fn reveal(app: &mut App) -> bool {
    use std::os::windows::process::CommandExt;

    let Some(row) = app.current() else {
        return false;
    };
    let path = row.path.clone();

    // The index can be ahead of the disk, and Explorer's failure mode for a
    // missing path is the same silent fallback. Say so rather than appearing
    // to open the wrong folder — and when the index is a cached snapshot, say
    // why nothing will update on its own: a static file cannot know about a
    // deletion, and the user should be told to rescan rather than left
    // wondering whether their delete worked.
    if !std::path::Path::new(&path).exists() {
        app.status = if app.live {
            format!("no longer on disk: {path}")
        } else {
            format!(
                "no longer on disk: {path} — the index is a snapshot; \
                 run `batuta scan` or `batuta serve` to refresh it"
            )
        };
        return false;
    }

    match std::process::Command::new("explorer.exe")
        .raw_arg(reveal_arg(&path))
        .spawn()
    {
        Ok(_) => {
            app.status = format!("revealed {path}");
            true
        }
        Err(e) => {
            app.status = format!("could not open Explorer: {e}");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{handle_key, refresh, reveal_arg, Source};
    use crate::tui::app::{App, Kind, Sort};
    use batuta_core::search::Searcher;
    use batuta_core::testtree::TreeBuilder;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    /// A local backend holding an (almost) empty index: enough to answer
    /// search requests, no pipe, no elevation.
    fn local_backend() -> Source {
        Source::Local {
            index: Box::new(TreeBuilder::new('C').build()),
            searcher: Searcher::new(),
        }
    }

    fn press(app: &mut App, code: KeyCode) {
        handle_key(app, KeyEvent::new(code, KeyModifiers::NONE), 10);
    }

    fn ctrl(app: &mut App, c: char) {
        handle_key(
            app,
            KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL),
            10,
        );
    }

    fn typed(app: &mut App, text: &str) {
        for c in text.chars() {
            press(app, KeyCode::Char(c));
        }
    }

    #[test]
    fn escape_closes_even_with_a_query_typed() {
        // Opened by a hotkey this is a launcher: one press dismisses it.
        // Clearing the query first would mean pressing Escape twice.
        let mut app = App::default();
        typed(&mut app, "report");
        assert_eq!(app.query, "report");

        press(&mut app, KeyCode::Esc);
        assert!(app.quit, "Escape must close, not just clear");
    }

    #[test]
    fn ctrl_u_clears_without_closing() {
        // The escape hatch for clearing, now that Escape closes.
        let mut app = App::default();
        typed(&mut app, "report");
        ctrl(&mut app, 'u');
        assert_eq!(app.query, "");
        assert!(!app.quit, "clearing must not close the window");
    }

    #[test]
    fn ctrl_s_and_ctrl_t_cycle_sort_and_filter() {
        let mut app = App::default();
        assert_eq!(app.sort, Sort::Name);
        ctrl(&mut app, 's');
        assert_eq!(app.sort, Sort::Size);

        assert_eq!(app.kind, Kind::All);
        ctrl(&mut app, 't');
        assert_eq!(app.kind, Kind::FilesOnly);

        // And the letters still reach the query when Ctrl is not held.
        typed(&mut app, "st");
        assert_eq!(app.query, "st");
        assert!(!app.quit);
    }

    #[test]
    fn typing_never_closes_the_window() {
        let mut app = App::default();
        typed(&mut app, "escape s t c u w");
        assert!(!app.quit);
        assert_eq!(app.query, "escape s t c u w");
    }

    #[test]
    fn ctrl_d_enters_dupes_and_announces_a_pending_scan() {
        let mut app = App::default();
        assert!(app.dupes_pending, "the first entry into dupes must scan");
        ctrl(&mut app, 'd');
        assert_eq!(app.mode, crate::tui::app::Mode::Dupes);
        assert!(
            app.status.contains("scanning for duplicates"),
            "the blocking scan must be announced: {}",
            app.status
        );

        // The scan completes (the event loop clears the announcement in
        // `refresh` once the response is applied).
        app.apply_dupes(Vec::new(), 0, 0, 10);
        app.status.clear();
        ctrl(&mut app, 'd');
        assert_eq!(app.mode, crate::tui::app::Mode::Search);
        ctrl(&mut app, 'd');
        assert_eq!(app.mode, crate::tui::app::Mode::Dupes);
        assert!(
            !app.dupes_pending,
            "a cached result must not re-hash every file"
        );
        assert!(app.status.is_empty(), "{}", app.status);
    }

    #[test]
    fn f5_in_dupes_forces_a_rescan_but_in_search_only_a_refetch() {
        let mut app = App::default();
        ctrl(&mut app, 'd');
        app.apply_dupes(Vec::new(), 0, 0, 10); // a scan has run
        press(&mut app, KeyCode::F(5));
        assert!(app.dupes_pending, "F5 in dupes must re-arm the scan");
        assert!(app.status.contains("scanning"));

        let mut app = App::default();
        press(&mut app, KeyCode::F(5));
        assert!(app.dirty);
        assert!(
            !app.status.contains("scanning"),
            "a search refetch is cheap and must not be announced as a scan"
        );
    }

    #[test]
    fn tab_completes_a_path_and_shift_tab_still_switches_modes() {
        // The drill-down the key exists for: a path query, a highlighted
        // child, Tab adopts its real path into the query bar.
        let mut app = App {
            query: r"C:\users".into(),
            ..Default::default()
        };
        app.apply_rows(
            vec![batuta_ipc::Row {
                path: r"C:\Users\Hacker".into(),
                size: 0,
                mtime: 0,
                is_dir: true,
                files: 1,
                own: 0,
            }],
            1,
            0,
            10,
        );
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.query, r"C:\Users\Hacker\");

        // Shift+Tab is the always-works mode key, even mid-drill-down.
        app.mode = crate::tui::app::Mode::Dupes;
        press(&mut app, KeyCode::BackTab);
        assert_eq!(
            app.mode,
            crate::tui::app::Mode::Search,
            "BackTab switches modes"
        );
        assert_eq!(
            app.query, r"C:\Users\Hacker\",
            "BackTab never touches the query"
        );
    }

    #[test]
    fn tab_never_switches_modes() {
        // Tab used to fall back to cycling modes when there was nothing to
        // complete, so a stray Tab threw you into another view unintentionally.
        // Completing a path is all it does now.
        let mut app = App::default();
        let before = app.mode;
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.mode, before, "Tab must not change mode");

        // A plain name search has no completion to offer, and still must not
        // become a mode switch.
        let mut app = App::default();
        typed(&mut app, "report");
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.query, "report", "a name search must not become a path");
        assert_eq!(app.mode, before, "Tab must not change mode");
    }

    #[test]
    fn shift_tab_is_the_only_mode_key() {
        let mut app = App::default();
        press(&mut app, KeyCode::BackTab);
        assert_ne!(
            app.mode,
            crate::tui::app::Mode::Search,
            "Shift+Tab cycles modes"
        );
    }

    #[test]
    fn ctrl_c_still_quits() {
        let mut app = App::default();
        ctrl(&mut app, 'c');
        assert!(app.quit);
    }

    #[test]
    fn enter_on_nothing_does_not_close() {
        // With no rows loaded there is nothing to open, so the window stays.
        let mut app = App::default();
        press(&mut app, KeyCode::Enter);
        assert!(!app.quit);
    }

    #[test]
    fn enter_on_a_missing_file_stays_open_to_show_why() {
        // The index can be ahead of the disk. Closing here would hide the
        // explanation the user needs.
        let mut app = App::default();
        app.apply_rows(
            vec![batuta_ipc::Row {
                path: r"C:\definitely\not\here\ghost.txt".into(),
                size: 1,
                mtime: 0,
                is_dir: false,
                files: 0,
                own: 0,
            }],
            1,
            0,
            10,
        );
        press(&mut app, KeyCode::Enter);
        assert!(!app.quit, "a failed open must leave the window up");
        assert!(app.status.contains("no longer on disk"), "{}", app.status);
        // A snapshot cannot learn about the deletion on its own — say so.
        assert!(app.status.contains("batuta scan"), "{}", app.status);

        // Against a live index the same failure is stated without the rescan
        // instruction, which would be wrong advice there.
        let mut app = App::new(true);
        app.apply_rows(
            vec![batuta_ipc::Row {
                path: r"C:\definitely\not\here\ghost.txt".into(),
                size: 1,
                mtime: 0,
                is_dir: false,
                files: 0,
                own: 0,
            }],
            1,
            0,
            10,
        );
        press(&mut app, KeyCode::Enter);
        assert!(app.status.contains("no longer on disk"), "{}", app.status);
        assert!(!app.status.contains("batuta scan"), "{}", app.status);
    }

    #[test]
    fn a_failed_reveal_message_survives_the_refresh_that_follows_it() {
        // The reported bug: Enter on a deleted folder sets a status message,
        // but every key press also arms `pending`, so a refresh fires ~30ms
        // later and used to clear the status before anyone could read it.
        // The keypress then looked like it did nothing at all.
        let mut app = App::default();
        app.apply_rows(
            vec![batuta_ipc::Row {
                path: r"C:\definitely\not\here\ghost.txt".into(),
                size: 1,
                mtime: 0,
                is_dir: false,
                files: 0,
                own: 0,
            }],
            1,
            0,
            10,
        );
        press(&mut app, KeyCode::Enter);
        assert!(app.status.contains("no longer on disk"));

        // The settle-refresh Enter armed: same query, nothing dirty.
        let mut backend = local_backend();
        refresh(&mut app, &mut backend, 10);
        assert!(
            app.status.contains("no longer on disk"),
            "the settle must not erase the message: {}",
            app.status
        );

        // A change of state — typing — is the user moving on, and that clears.
        typed(&mut app, "x");
        refresh(&mut app, &mut backend, 10);
        assert!(app.status.is_empty(), "{}", app.status);
    }

    #[test]
    fn paths_with_spaces_are_quoted_for_explorer() {
        // The reported bug: this path made Explorer open Documents instead.
        assert_eq!(
            reveal_arg(r"C:\Users\Hacker\Downloads\Telegram Desktop\sheldon.png"),
            "/select,\"C:\\Users\\Hacker\\Downloads\\Telegram Desktop\\sheldon.png\""
        );
    }

    #[test]
    fn the_select_verb_itself_is_never_inside_the_quotes() {
        // Explorer parses `/select,` itself; quoting it along with the path is
        // exactly what broke.
        let arg = reveal_arg(r"C:\a b\c.txt");
        assert!(arg.starts_with("/select,\""), "got {arg}");
        assert_eq!(arg.matches('"').count(), 2);
    }

    #[test]
    fn simple_paths_still_work() {
        assert_eq!(reveal_arg(r"D:\file.png"), "/select,\"D:\\file.png\"");
    }

    #[test]
    fn a_trailing_separator_is_stripped() {
        // `/select` on a path ending in a separator highlights nothing.
        assert_eq!(reveal_arg(r"D:\Downloads\"), "/select,\"D:\\Downloads\"");
        assert_eq!(reveal_arg("D:/Downloads/"), "/select,\"D:/Downloads\"");
    }

    #[test]
    fn a_bare_volume_root_is_left_alone() {
        // Trimming this to `C:` would name the current directory on C:,
        // not the drive.
        assert_eq!(reveal_arg(r"C:\"), "/select,\"C:\\\"");
    }

    #[test]
    fn paths_with_commas_survive() {
        // A comma in the name must not look like the end of the /select verb.
        let arg = reveal_arg(r"D:\Docs\report, final.pdf");
        assert_eq!(arg, "/select,\"D:\\Docs\\report, final.pdf\"");
    }
}

#[cfg(test)]
mod delete_tests {
    use super::*;
    use crate::tui::app::App;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn press(app: &mut App, code: KeyCode) {
        handle_key(app, KeyEvent::new(code, KeyModifiers::NONE), 10);
    }

    fn with_row(path: &str, is_dir: bool) -> App {
        let mut app = App::default();
        app.apply_rows(
            vec![batuta_ipc::Row {
                path: path.into(),
                size: 4096,
                mtime: 0,
                is_dir,
                files: 7,
                own: 0,
            }],
            1,
            0,
            10,
        );
        app
    }

    #[test]
    fn delete_asks_before_removing_anything() {
        let dir = std::env::temp_dir().join(format!("batuta-del-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("victim.txt");
        std::fs::write(&file, b"data").unwrap();

        let mut app = with_row(&file.display().to_string(), false);
        press(&mut app, KeyCode::Delete);

        assert!(app.awaiting_confirmation(), "Delete must raise a prompt");
        assert!(file.exists(), "nothing may be removed before the answer");
        let pending = app.confirm_delete.clone().unwrap();
        assert_eq!(pending.path, file.display().to_string());
        assert!(!pending.is_dir);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn answering_no_leaves_the_file_alone() {
        let dir = std::env::temp_dir().join(format!("batuta-keep-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("keep.txt");
        std::fs::write(&file, b"data").unwrap();

        let mut app = with_row(&file.display().to_string(), false);
        press(&mut app, KeyCode::Delete);
        press(&mut app, KeyCode::Char('n'));

        assert!(!app.awaiting_confirmation());
        assert!(file.exists(), "answering no must not delete");
        assert!(app.status.contains("cancelled"), "{}", app.status);

        // Escape is the other way out.
        press(&mut app, KeyCode::Delete);
        press(&mut app, KeyCode::Esc);
        assert!(!app.awaiting_confirmation());
        assert!(file.exists());
        assert!(!app.quit, "Escape must dismiss the dialog, not the window");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn enter_does_not_confirm_a_deletion() {
        // Enter is the key most likely to be pressed out of habit, and this is
        // the one dialog where that would destroy data.
        let dir = std::env::temp_dir().join(format!("batuta-enter-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("safe.txt");
        std::fs::write(&file, b"data").unwrap();

        let mut app = with_row(&file.display().to_string(), false);
        press(&mut app, KeyCode::Delete);
        press(&mut app, KeyCode::Enter);

        assert!(
            app.awaiting_confirmation(),
            "Enter must not answer the prompt"
        );
        assert!(file.exists());
        assert!(!app.quit, "and must not fall through to open-and-close");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn typing_is_swallowed_while_the_prompt_is_up() {
        let mut app = with_row(r"C:\somewhere\thing.txt", false);
        app.query = "before".into();
        press(&mut app, KeyCode::Delete);

        for c in ['x', 'y'] {
            if c == 'y' {
                break; // 'y' is the confirm key, tested separately
            }
            press(&mut app, KeyCode::Char(c));
        }
        assert_eq!(app.query, "before", "keys must not reach the query bar");
        assert!(app.awaiting_confirmation());
    }

    #[test]
    fn confirming_removes_a_file() {
        let dir = std::env::temp_dir().join(format!("batuta-gone-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("gone.txt");
        std::fs::write(&file, b"data").unwrap();

        let mut app = with_row(&file.display().to_string(), false);
        press(&mut app, KeyCode::Delete);
        press(&mut app, KeyCode::Char('y'));

        assert!(!file.exists(), "confirming must actually delete");
        assert!(!app.awaiting_confirmation());
        assert!(app.status.starts_with("deleted"), "{}", app.status);
        assert!(app.dirty, "the list must refresh so the row disappears");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn confirming_removes_a_directory_and_its_contents() {
        let dir = std::env::temp_dir().join(format!("batuta-tree-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("inner")).unwrap();
        std::fs::write(dir.join("inner").join("a.txt"), b"a").unwrap();

        let mut app = with_row(&dir.display().to_string(), true);
        press(&mut app, KeyCode::Delete);
        assert!(app.confirm_delete.as_ref().unwrap().is_dir);
        press(&mut app, KeyCode::Char('y'));

        assert!(!dir.exists(), "a directory must be removed recursively");
    }

    #[test]
    fn a_failed_delete_reports_why_and_keeps_the_prompt_closed() {
        let missing = std::env::temp_dir().join("batuta-does-not-exist-anywhere.txt");
        let _ = std::fs::remove_file(&missing);

        let mut app = with_row(&missing.display().to_string(), false);
        press(&mut app, KeyCode::Delete);
        press(&mut app, KeyCode::Char('y'));

        assert!(!app.awaiting_confirmation());
        assert!(app.status.starts_with("could not delete"), "{}", app.status);
    }

    #[test]
    fn delete_with_nothing_selected_says_so() {
        let mut app = App::default();
        press(&mut app, KeyCode::Delete);
        assert!(!app.awaiting_confirmation());
        assert!(app.status.contains("nothing selected"), "{}", app.status);
    }
}
