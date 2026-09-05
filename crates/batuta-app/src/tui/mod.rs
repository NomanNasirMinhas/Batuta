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
pub(crate) mod explorer;
pub(crate) mod layout;
pub(crate) mod terminal;
pub(crate) mod theme;
mod ui;

pub use app::{App, Mode};

use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEventKind,
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
use crate::tui::app::{Exit, PendingDiscard};
use crate::tui::explorer::state::{Action, Explorer, Focus};
use crate::tui::explorer::tree::Disk;
use crate::tui::terminal::keys as termkeys;
use crate::tui::terminal::session::Session;
use crate::{query, scan};

/// Where answers come from.
enum Source {
    /// A running daemon, whose index is live.
    Daemon(Box<PipeStream>),
    /// This process, answering from the last snapshot.
    Local {
        /// Shared rather than owned, so a background scan can read it without
        /// the UI thread having to hand it over or load it twice.
        index: Arc<Index>,
        searcher: Searcher,
    },
}

/// Answers one request away from the UI thread.
///
/// Duplicate detection reads file contents and takes seconds. Doing that
/// inline froze the whole interface — you could not switch modes, scroll, or
/// even quit until it finished — so it is handed to a worker and collected
/// when it is done.
enum Worker {
    Daemon,
    Local(Arc<Index>),
}

impl Worker {
    fn answer(&self, req: &Request) -> Response {
        match self {
            // The daemon already serves concurrent clients, so the scan opens
            // its own connection instead of borrowing the UI's, which stays
            // free to answer everything else meanwhile.
            Worker::Daemon => match PipeStream::connect() {
                Ok(mut s) => s.request(req).unwrap_or_else(|e| Response::Error {
                    message: format!("daemon: {e}"),
                }),
                Err(e) => Response::Error {
                    message: format!("daemon: {e}"),
                },
            },
            Worker::Local(index) => query::execute(index, req, &mut Searcher::new(), false, 0),
        }
    }
}

/// A duplicate scan in flight.
struct DupeJob {
    rx: Receiver<Response>,
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

    /// A handle that can answer a request on another thread.
    fn worker(&self) -> Worker {
        match self {
            Source::Daemon(_) => Worker::Daemon,
            Source::Local { index, .. } => Worker::Local(Arc::clone(index)),
        }
    }
}

/// Start a duplicate scan in the background.
fn spawn_dupes(backend: &Source, req: Request) -> DupeJob {
    let worker = backend.worker();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        // The receiver is dropped if the UI exits first; nothing to do but
        // let the send fail and the thread end.
        let _ = tx.send(worker.answer(&req));
    });
    DupeJob { rx }
}

/// Put the terminal back before a panic takes the process down.
///
/// The guard below handles every ordinary exit. It does **not** handle a
/// panic: the release profile sets `panic = "abort"`, so there is no
/// unwinding and no destructor runs. The process dies with raw mode still on
/// and the alternate screen still up, leaving the shell unusable and the
/// panic message hidden behind a screen that is never restored — which is
/// exactly when you most need to read it.
///
/// A hook is the one piece of cleanup that does run, because it runs *before*
/// the abort rather than during an unwind.
fn install_panic_hook() {
    use std::sync::Once;
    static ONCE: Once = Once::new();

    ONCE.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = disable_raw_mode();
            let _ = execute!(std::io::stdout(), LeaveAlternateScreen, DisableMouseCapture);
            previous(info);
        }));
    });
}

/// Restores the terminal on every ordinary exit. See [`install_panic_hook`]
/// for the case this cannot cover.
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

    // A console opened for us is ours to shape: the hotkey should produce a
    // launcher panel, not a full-screen command window. A terminal the user
    // already had open is left exactly as they arranged it.
    if crate::console::owns_console_alone() {
        crate::console::float();
    }

    install_panic_hook();
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
        index: Arc::new(built.index),
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
    let mut dupe_job: Option<DupeJob> = None;
    // How many rows the last fetch was sized for. Starts at a count no layout
    // can produce, so it cannot match the first draw and the opening fetch is
    // armed by the same rule that handles every later resize.
    let mut fetched_for = usize::MAX;

    const DEBOUNCE: Duration = Duration::from_millis(30);

    loop {
        terminal.draw(|f| {
            visible = ui::draw(f, app);
        })?;

        // Keep the shell's idea of its size matching the pane it is drawn
        // in, and take whatever it has produced. Both every frame: a terminal
        // that only updated on a keystroke would look frozen while a build
        // runs.
        if app.mode == Mode::Terminal {
            if let Ok(size) = terminal.size() {
                let area = Rect {
                    x: 0,
                    y: 0,
                    width: size.width,
                    height: size.height,
                };
                let screen = layout::terminal(area).screen;
                if let Some(t) = app.terminal.as_mut() {
                    if t.grid.size() != (screen.width as usize, screen.height as usize) {
                        t.resize(screen.width, screen.height);
                    }
                    t.pump();
                }
            }
        }

        // A resize changes how many rows fit, but the fetch that ran before it
        // asked for the old count. Left alone, a window that just got taller
        // keeps showing the shorter list against a screenful of blank rows —
        // the resize event arrives on the frame *before* the layout that
        // reacts to it, so re-arming on the event itself is a frame too early.
        if visible != fetched_for {
            pending = true;
        }

        // Collect a finished scan. Never blocks: the point of the worker is
        // that the interface keeps responding while it runs.
        if let Some(job) = &dupe_job {
            match job.rx.try_recv() {
                Ok(response) => {
                    apply_dupe_result(app, response, visible);
                    dupe_job = None;
                }
                // The worker died without answering, which would otherwise
                // leave the view scanning forever.
                Err(TryRecvError::Disconnected) => {
                    app.status = "the duplicate scan stopped unexpectedly".into();
                    app.dupes_pending = false;
                    dupe_job = None;
                }
                Err(TryRecvError::Empty) => {}
            }
        }

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
                        MouseEventKind::Down(MouseButton::Left) => {
                            if let Ok(size) = terminal.size() {
                                window_button(app, m.column, m.row, size.width, size.height);
                            }
                        }
                        // In the terminal the wheel walks the shell's history
                        // rather than a result list.
                        MouseEventKind::ScrollDown => match app.mode {
                            Mode::Terminal => {
                                if let Some(t) = app.terminal.as_mut() {
                                    t.grid.scroll_view(3);
                                }
                            }
                            _ => app.move_selection(3, visible),
                        },
                        MouseEventKind::ScrollUp => match app.mode {
                            Mode::Terminal => {
                                if let Some(t) = app.terminal.as_mut() {
                                    t.grid.scroll_view(-3);
                                }
                            }
                            _ => app.move_selection(-3, visible),
                        },
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
            refresh(app, backend, visible, &mut dupe_job);
            fetched_for = visible;
            pending = false;
        }
    }
}

/// Fold a finished duplicate scan into the app.
fn apply_dupe_result(app: &mut App, response: Response, visible: usize) {
    match response {
        Response::Dupes {
            groups,
            wasted_total,
            elapsed_us,
        } => {
            app.apply_dupes(groups, wasted_total, elapsed_us, visible);
            app.status.clear();
        }
        Response::Error { message } => {
            // Leave the cache alone and stay armed, so F5 can retry.
            app.status = message;
            app.dupes_pending = true;
        }
        other => {
            app.status = format!("unexpected response: {other:?}");
            app.dupes_pending = true;
        }
    }
}

fn refresh(app: &mut App, backend: &mut Source, visible: usize, dupe_job: &mut Option<DupeJob>) {
    // A duplicate scan reads file contents and takes seconds, so it runs only
    // when one is explicitly pending, and it runs on a worker: the UI thread
    // must stay free to switch modes, scroll and quit while it works. Every
    // other refresh in the mode — scrolling, re-entering it — is served by
    // moving the window over the cache.
    if app.mode == Mode::Terminal {
        app.dirty = false;
        return;
    }

    // The explorer reads the disk, not the index, so there is nothing to ask
    // for here beyond re-flattening the tree for the next frame.
    if app.mode == Mode::Explore {
        if let Some(x) = &mut app.explorer {
            x.sync(&Disk);
        }
        app.dirty = false;
        return;
    }

    if app.mode == Mode::Dupes {
        if app.dupes_pending {
            if dupe_job.is_none() {
                *dupe_job = Some(spawn_dupes(backend, app.request(visible)));
            }
            // Nothing more to do until the worker answers; leaving `dirty` set
            // would spin this branch on every frame.
            app.dirty = false;
            return;
        }
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

/// Where the explorer opens when there is nothing to seed it from.
fn home_dir() -> std::path::PathBuf {
    std::env::var_os("USERPROFILE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(r"C:\"))
}

/// Open the explorer, seeded from whatever is highlighted.
///
/// The seeding is the point of the shortcut: finding a file and then having to
/// navigate back to it by hand would make the two views feel like two separate
/// programs.
fn enter_explorer(app: &mut App) {
    let start = app.current().map(|r| std::path::PathBuf::from(&r.path));
    let mut file = None;
    let root = match start {
        Some(p) if p.is_dir() => p,
        Some(p) => {
            let parent = p
                .parent()
                .map(std::path::Path::to_path_buf)
                .unwrap_or_else(home_dir);
            file = Some(p);
            parent
        }
        None => home_dir(),
    };

    let mut x = Explorer::new(root);
    if let Some(path) = file {
        x.tree.reveal(&path);
        x.load(&path);
        x.focus = Focus::Editor;
    }
    app.explorer = Some(x);
    app.mode = Mode::Explore;
    app.status.clear();
    app.dirty = true;
}

/// Raise the unsaved-changes prompt if there is anything to lose.
///
/// Returns whether it did, so callers can tell "handled, wait for the user"
/// from "nothing in the way, carry on".
fn guard_unsaved(app: &mut App, then: Exit) -> bool {
    let Some(x) = &app.explorer else {
        return false;
    };
    if !x.modified() {
        return false;
    }
    app.confirm_discard = Some(PendingDiscard {
        path: x
            .open_path()
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
        then,
    });
    true
}

/// Carry out whatever the discard prompt was blocking.
fn resolve_discard(app: &mut App, save_first: bool) {
    let Some(pending) = app.confirm_discard.take() else {
        return;
    };
    if save_first {
        if let Some(x) = &mut app.explorer {
            app.status = x.save();
            // A save that failed - read-only, or changed underneath us - must
            // not then throw the work away anyway.
            if x.modified() {
                return;
            }
        }
    }
    match pending.then {
        Exit::LeaveExplorer => {
            app.mode = Mode::Search;
            app.dirty = true;
        }
        Exit::Quit => app.quit = true,
        Exit::Open(path) => {
            if let Some(x) = &mut app.explorer {
                x.load(&path);
                x.focus = Focus::Editor;
            }
        }
    }
}

fn leave_explorer(app: &mut App) {
    if guard_unsaved(app, Exit::LeaveExplorer) {
        return;
    }
    app.mode = Mode::Search;
    app.dirty = true;
    app.status.clear();
}

/// Hand a path to whatever application owns it.
fn shell_open(path: &std::path::Path) -> bool {
    use std::os::windows::ffi::OsStrExt;
    use std::ptr;
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    const SHELL_SUCCESS: isize = 32;

    let verb: Vec<u16> = "open".encode_utf16().chain(std::iter::once(0)).collect();
    let target: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    let rc = unsafe {
        ShellExecuteW(
            ptr::null_mut(),
            verb.as_ptr(),
            target.as_ptr(),
            ptr::null(),
            ptr::null(),
            SW_SHOWNORMAL,
        )
    } as isize;
    rc > SHELL_SUCCESS
}

/// Act on a click in the custom title bar.
///
/// Only the views that draw one respond: a click at the top of the search
/// results is a click on results, not on a control that is not there.
fn window_button(app: &mut App, col: u16, row: u16, width: u16, height: u16) {
    let area = Rect {
        x: 0,
        y: 0,
        width,
        height,
    };
    let bar = match app.mode {
        Mode::Terminal => layout::terminal(area).title,
        // The title bar is the top row whatever else is showing, so the find
        // prompt makes no difference to where the buttons are.
        Mode::Explore => layout::explorer(area, true, false).title,
        _ => return,
    };

    match layout::title_button_at(bar, col, row) {
        Some(layout::TitleButton::Minimize) => crate::console::minimize(),
        Some(layout::TitleButton::Maximize) => crate::console::toggle_maximize(),
        // The only place a click is allowed to end the program.
        Some(layout::TitleButton::Close) => app.quit = true,
        None => {}
    }
}

/// The folder a terminal should open in: the highlighted one, or the folder
/// holding the highlighted file.
fn terminal_cwd(app: &App) -> std::path::PathBuf {
    // The explorer's selection wins when it is on screen, because that is
    // what the user is looking at.
    if let Some(x) = &app.explorer {
        if let Some(row) = x.selected_row() {
            return if row.is_dir {
                row.path.clone()
            } else {
                row.path
                    .parent()
                    .map(std::path::Path::to_path_buf)
                    .unwrap_or_else(home_dir)
            };
        }
        return x.tree.root().to_path_buf();
    }
    match app.current().map(|r| std::path::PathBuf::from(&r.path)) {
        Some(p) if p.is_dir() => p,
        Some(p) => p
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(home_dir),
        None => home_dir(),
    }
}

/// Open a shell, remembering where to go back to.
fn enter_terminal(app: &mut App, cols: u16, rows: u16) {
    let cwd = terminal_cwd(app);
    match Session::open(&cwd, cols, rows) {
        Ok(session) => {
            app.came_from = Some(app.mode);
            app.terminal = Some(session);
            app.mode = Mode::Terminal;
            app.status.clear();
        }
        Err(e) => app.status = format!("could not open a shell: {e}"),
    }
}

/// Leave the terminal, ending the shell with it.
fn leave_terminal(app: &mut App) {
    // Dropped rather than kept alive in the background: a shell nobody can
    // see, still holding a directory open, is a surprise later.
    app.terminal = None;
    app.mode = app.came_from.take().unwrap_or(Mode::Search);
    app.dirty = true;
    app.status.clear();
}

/// Route one key through the terminal.
///
/// Almost everything belongs to the program running inside it — including
/// `Ctrl+C`, which is an interrupt there and nothing else, and `Esc`, which is
/// how anyone leaves insert mode. Only two keys are kept back.
fn terminal_key(app: &mut App, key: KeyEvent) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);

    match key.code {
        KeyCode::Char('q' | 'Q') if ctrl => {
            app.quit = true;
            return;
        }
        KeyCode::Char('e' | 'E') if ctrl => {
            leave_terminal(app);
            return;
        }
        // Scrolling the history is the terminal's own, not the shell's.
        KeyCode::PageUp if shift => {
            if let Some(t) = &mut app.terminal {
                t.grid.scroll_view(-10);
            }
            return;
        }
        KeyCode::PageDown if shift => {
            if let Some(t) = &mut app.terminal {
                t.grid.scroll_view(10);
            }
            return;
        }
        _ => {}
    }

    if let Some(bytes) = termkeys::encode(key) {
        if let Some(t) = &mut app.terminal {
            t.send(&bytes);
        }
    }
}

/// Route one key through the explorer.
fn explorer_key(app: &mut App, key: KeyEvent, visible: usize) {
    // The escape hatch for a file the editor will not open. Handled here
    // rather than inside the explorer because launching something is the one
    // thing the explorer does not own.
    if key.code == KeyCode::Enter && key.modifiers.contains(KeyModifiers::SHIFT) {
        let target = app.explorer.as_ref().and_then(|x| {
            x.open_path()
                .map(std::path::Path::to_path_buf)
                .or_else(|| x.selected_row().map(|r| r.path.clone()))
        });
        app.status = match target {
            Some(path) if shell_open(&path) => String::new(),
            Some(path) => format!("nothing is registered to open {}", path.display()),
            None => "nothing selected".into(),
        };
        return;
    }

    let action = match &mut app.explorer {
        Some(x) => x.key(key, visible, &Disk),
        None => Action::Leave,
    };

    match action {
        Action::None => {}
        Action::Complete => {
            if let Some(x) = &mut app.explorer {
                x.complete_path(&Disk);
            }
        }
        Action::Save => {
            if let Some(x) = &mut app.explorer {
                app.status = x.save();
            }
        }
        Action::Leave => leave_explorer(app),
        Action::OpenTerminal => enter_terminal(app, 80, 24),
        Action::Quit => {
            if !guard_unsaved(app, Exit::Quit) {
                app.quit = true;
            }
        }
        Action::Open(path) => {
            if !guard_unsaved(app, Exit::Open(path.clone())) {
                if let Some(x) = &mut app.explorer {
                    x.load(&path);
                    x.focus = Focus::Editor;
                }
            }
        }
    }
}

fn handle_key(app: &mut App, key: KeyEvent, visible: usize) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);

    // The second half of a two-step Tab is only owed while Tab is what is
    // being pressed. Any other key means the user moved on, and completing to
    // a file they have since navigated away from would be baffling.
    if key.code != KeyCode::Tab {
        app.pending_file = None;
    }

    // A confirmation prompt takes the keyboard entirely. Letting keys through
    // would mean typing into the query behind a dialog, or worse, having
    // Enter mean two different things at once.
    // Three outcomes, not the delete prompt's two - and `Enter` does nothing
    // here for the same reason it does nothing there: it is the key most
    // likely to be hit from habit, and one of these branches throws away work.
    if app.confirm_discard.is_some() {
        match key.code {
            KeyCode::Char('s') | KeyCode::Char('S') => resolve_discard(app, true),
            KeyCode::Char('d') | KeyCode::Char('D') => resolve_discard(app, false),
            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
                app.confirm_discard = None;
                app.status = "still editing".into();
            }
            _ => {}
        }
        return;
    }

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

    // The explorer owns its keyboard outright. Every binding below is wrong
    // or dangerous with a text buffer on screen — `Delete` most of all, which
    // out here removes the highlighted file from disk.
    if app.mode == Mode::Explore {
        explorer_key(app, key, visible);
        return;
    }
    if app.mode == Mode::Terminal {
        terminal_key(app, key);
        return;
    }

    match key.code {
        // Ctrl arms must come before the plain-character arm below, or
        // these letters would be typed into the query instead.
        //
        // Both cases, because Caps Lock is a modifier as far as the terminal
        // is concerned: with it on, Ctrl+S arrives as `Char('S')` and matching
        // only lowercase silently does nothing. Shift+Ctrl+S lands here too,
        // which is what anyone pressing it would expect.
        // Ctrl+C opens a shell now; Ctrl+Q is what quits. Inside the
        // terminal Ctrl+C reverts to its real meaning, interrupting whatever
        // is running, which is the one binding it would be perverse to take.
        KeyCode::Char('c' | 'C') if ctrl => enter_terminal(app, 80, 24),
        KeyCode::Char('q' | 'Q') if ctrl => app.quit = true,
        KeyCode::Char('u' | 'U') if ctrl => app.clear_query(),
        KeyCode::Char('w' | 'W') if ctrl => app.delete_word(),
        KeyCode::Char('s' | 'S') if ctrl => app.cycle_sort(),
        KeyCode::Char('t' | 'T') if ctrl => app.cycle_kind(),
        // Reclaim the rail's columns for paths without leaving the app.
        KeyCode::Char('b' | 'B') if ctrl => app.rail = !app.rail,
        KeyCode::Char('e' | 'E') if ctrl => enter_explorer(app),
        // The duplicate scan reads file contents, so it can take a while.
        // Announce it before the request blocks the loop, so the user sees
        // why the UI has gone quiet instead of a frozen result box.
        KeyCode::Char('d' | 'D') if ctrl => {
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
        KeyCode::Tab => app.complete(),
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
        // Must precede the plain Enter arm, or the modifier is ignored.
        KeyCode::Enter if shift => app.quit = open_selected(app),
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

/// Check the selected row still exists, returning its path.
///
/// The index can be ahead of the disk, and both Explorer and the shell fail
/// the same silent way on a missing path — appearing to do nothing, or opening
/// somewhere unexpected. Saying so is better than either.
fn live_path(app: &mut App) -> Option<String> {
    let path = app.current()?.path.clone();
    if std::path::Path::new(&path).exists() {
        return Some(path);
    }
    // A cached snapshot cannot know about a deletion, so say why nothing will
    // correct itself rather than leaving the user wondering.
    app.status = if app.live {
        format!("no longer on disk: {path}")
    } else {
        format!(
            "no longer on disk: {path} — the index is a snapshot; \
             run `batuta scan` or `batuta serve` to refresh it"
        )
    };
    None
}

/// Open the selected entry itself: a folder in Explorer, a file in whatever
/// application owns it.
///
/// This is deliberately a different key from revealing it. Revealing answers
/// "where is this", opening answers "let me have it", and guessing wrong in
/// either direction is annoying in a way that a second binding is not.
///
/// Returns whether anything was launched, so the caller knows whether there is
/// a message worth staying open to show.
fn open_selected(app: &mut App) -> bool {
    use std::os::windows::ffi::OsStrExt;
    use std::ptr;
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    /// `ShellExecuteW` reports success as a value above this; below it the
    /// return is a legacy `HINSTANCE`-shaped error code.
    const SHELL_SUCCESS: isize = 32;

    let Some(path) = live_path(app) else {
        return false;
    };

    let verb: Vec<u16> = "open".encode_utf16().chain(std::iter::once(0)).collect();
    let target: Vec<u16> = std::ffi::OsStr::new(&path)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    let rc = unsafe {
        ShellExecuteW(
            ptr::null_mut(),
            verb.as_ptr(),
            target.as_ptr(),
            ptr::null(),
            ptr::null(),
            SW_SHOWNORMAL,
        )
    } as isize;

    if rc > SHELL_SUCCESS {
        true
    } else {
        // Most often a file type with nothing registered to open it.
        app.status = format!("nothing is registered to open {path}");
        false
    }
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
    use super::{apply_dupe_result, handle_key, refresh, reveal_arg, Source};
    use crate::tui::app::{App, Kind, Sort};
    use batuta_core::search::Searcher;
    use batuta_core::testtree::TreeBuilder;
    use batuta_ipc::{Response, Row};
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    /// A local backend holding an (almost) empty index: enough to answer
    /// search requests, no pipe, no elevation.
    fn local_backend() -> Source {
        Source::Local {
            index: std::sync::Arc::new(TreeBuilder::new('C').build()),
            searcher: Searcher::new(),
        }
    }

    #[test]
    fn a_duplicate_scan_never_blocks_the_interface() {
        // Before this moved to a worker, refresh sat inside the scan and the
        // whole UI froze: no mode switch, no scrolling, no quitting until it
        // finished.
        let mut backend = local_backend();
        let mut app = App::new(false);
        app.mode = crate::tui::Mode::Dupes;
        app.dupes_pending = true;

        let mut job = None;
        refresh(&mut app, &mut backend, 10, &mut job);

        assert!(job.is_some(), "the scan should have gone to a worker");
        assert!(
            !app.dirty,
            "leaving this set would respin the branch every frame"
        );

        // The proof that matters: the interface still takes input.
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE),
            10,
        );
        assert_ne!(
            app.mode,
            crate::tui::Mode::Dupes,
            "modes must switch during a scan"
        );
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            10,
        );
        assert!(app.quit, "quitting must not wait for the scan either");
    }

    #[test]
    fn a_scan_already_running_is_not_started_a_second_time() {
        let mut backend = local_backend();
        let mut app = App::new(false);
        app.mode = crate::tui::Mode::Dupes;
        app.dupes_pending = true;

        let mut job = None;
        refresh(&mut app, &mut backend, 10, &mut job);
        assert!(job.is_some());

        // Re-entering the view, or any other refresh, must not pile up a
        // second scan over the same files.
        app.dirty = true;
        refresh(&mut app, &mut backend, 10, &mut job);
        assert!(job.is_some());
    }

    #[test]
    fn a_finished_scan_that_failed_stays_armed_for_a_retry() {
        let mut app = App::new(false);
        app.mode = crate::tui::Mode::Dupes;
        app.dupes_pending = true;

        apply_dupe_result(
            &mut app,
            Response::Error {
                message: "disk went away".into(),
            },
            10,
        );

        assert_eq!(app.status, "disk went away");
        assert!(app.dupes_pending, "F5 must still be able to retry");
    }

    #[test]
    fn moving_off_a_row_cancels_the_second_half_of_a_tab() {
        let mut app = App::default();
        typed(&mut app, "file");
        app.apply_rows(
            vec![Row {
                path: r"C:\data\file_0001.bin".into(),
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

        press(&mut app, KeyCode::Tab);
        assert_eq!(app.query, r"C:\data\", "first Tab steps into the directory");
        assert!(app.pending_file.is_some(), "the file is owed a second Tab");

        // Any other key means the user moved on, and completing later to a
        // file they have since navigated away from would be baffling.
        press(&mut app, KeyCode::Down);
        assert_eq!(app.pending_file, None, "the second step must not survive");
    }

    #[test]
    fn the_shortcuts_still_work_with_caps_lock_on() {
        // Caps Lock is a modifier as far as the terminal is concerned: it
        // sends `Char('S')`, so matching only lowercase made every Ctrl
        // shortcut quietly stop working.
        let mut app = App::default();
        let before = app.sort;
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('S'), KeyModifiers::CONTROL),
            10,
        );
        assert_ne!(app.sort, before, "Ctrl+Shift+S must still cycle the sort");

        let mut app = App::default();
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('E'), KeyModifiers::CONTROL),
            10,
        );
        assert_eq!(
            app.mode,
            crate::tui::Mode::Explore,
            "Ctrl+Shift+E must still explore"
        );

        let mut app = App::default();
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('Q'), KeyModifiers::CONTROL),
            10,
        );
        assert!(app.quit, "Ctrl+Shift+Q must still quit");
    }

    #[test]
    fn an_uppercase_letter_with_no_ctrl_is_still_just_typing() {
        // The fix must not swallow capitals into shortcuts.
        let mut app = App::default();
        press(&mut app, KeyCode::Char('S'));
        assert_eq!(app.query, "S");
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
        assert_eq!(app.mode, crate::tui::Mode::Dupes);
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
        assert_eq!(app.mode, crate::tui::Mode::Dupes);
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
        app.mode = crate::tui::Mode::Dupes;
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

        // With nothing highlighted there is nothing to complete to, so the
        // query is left exactly as typed — and Tab still is not a mode key.
        let mut app = App::default();
        typed(&mut app, "report");
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.query, "report", "nothing was highlighted to complete");
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
    fn ctrl_q_quits_and_ctrl_c_does_not() {
        // Ctrl+C had to move: inside a terminal it means "interrupt what is
        // running", and that is not a binding worth taking from someone.
        let mut app = App::default();
        ctrl(&mut app, 'q');
        assert!(app.quit, "Ctrl+Q is what ends the program now");

        let mut app = App::default();
        ctrl(&mut app, 'c');
        assert!(
            !app.quit,
            "Ctrl+C must never quit again - it opens a shell instead"
        );
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
        refresh(&mut app, &mut backend, 10, &mut None);
        assert!(
            app.status.contains("no longer on disk"),
            "the settle must not erase the message: {}",
            app.status
        );

        // A change of state — typing — is the user moving on, and that clears.
        typed(&mut app, "x");
        refresh(&mut app, &mut backend, 10, &mut None);
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
