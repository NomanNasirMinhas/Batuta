//! TUI state and the logic around it.
//!
//! Kept free of rendering and terminal handling so the parts that are easy to
//! get subtly wrong — scrolling, windowing, when a refetch is needed — can be
//! tested directly.
//!
//! Results are **windowed**: a query can match hundreds of thousands of files,
//! and the app only ever holds the rows currently on screen. Selection is an
//! absolute position within the full match set; `window_start` says which slice
//! of it is loaded.

use batuta_ipc::{DupeGroupRows, Request, Row, SearchArgs};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Search,
    Bloat,
    /// Exact duplicate groups, ranked by reclaimable bytes. Served from a
    /// cache the user refreshes with F5, because a scan reads file contents
    /// and takes seconds, not microseconds.
    Dupes,
    /// A shell, running in a pseudo-console.
    ///
    /// Outside the cycle for the same reasons as the explorer, and one more:
    /// almost every key belongs to the program running inside it.
    Terminal,
    /// Directory tree, path bar and text editor.
    ///
    /// Deliberately outside the `Shift+Tab` cycle below: cycling into an
    /// editor by accident, or out of one holding unsaved changes, is a trap,
    /// and `Shift+Tab` means outdent in every editor anyone has used. `Ctrl+E`
    /// is the way in, `Esc` the way out.
    Explore,
}

impl Mode {
    pub fn next(self) -> Self {
        match self {
            Mode::Search => Mode::Bloat,
            Mode::Bloat => Mode::Dupes,
            Mode::Dupes => Mode::Search,
            // Never reached by the cycle; defined so the function stays total.
            Mode::Explore | Mode::Terminal => Mode::Search,
        }
    }
}

/// One line of the duplicates view.
#[derive(Debug, Clone, PartialEq)]
pub enum DupeEntry {
    /// A group summary, drawn as a divider above the copies it describes.
    Group { size: u64, copies: u32, wasted: u64 },
    /// One of the byte-identical files, with how many paths hold it.
    Copy { row: Row, copies: u32 },
}

/// How many duplicate groups one scan asks for. The result is held in full
/// locally, so this is also the most rows the mode can show.
pub const DUPES_TOP: u32 = 500;

/// Duplicate files below this size are not worth reading or reporting.
pub const DUPES_MIN_SIZE: u64 = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sort {
    Name,
    Size,
    Modified,
}

impl Sort {
    pub fn code(self) -> u8 {
        match self {
            Sort::Name => 0,
            Sort::Size => 1,
            Sort::Modified => 2,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Sort::Name => "name",
            Sort::Size => "size",
            Sort::Modified => "modified",
        }
    }

    pub fn next(self) -> Self {
        match self {
            Sort::Name => Sort::Size,
            Sort::Size => Sort::Modified,
            Sort::Modified => Sort::Name,
        }
    }
}

/// Which kinds of entry to show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    All,
    FilesOnly,
    DirsOnly,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::All => "all",
            Kind::FilesOnly => "files",
            Kind::DirsOnly => "dirs",
        }
    }

    pub fn next(self) -> Self {
        match self {
            Kind::All => Kind::FilesOnly,
            Kind::FilesOnly => Kind::DirsOnly,
            Kind::DirsOnly => Kind::All,
        }
    }
}

/// A deletion the user has been asked to confirm.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingDelete {
    pub path: String,
    pub is_dir: bool,
    /// Files beneath it, for a directory. Shown so the user knows the weight
    /// of what they are about to remove.
    pub files: u32,
    pub size: u64,
}

/// Where to go once the user has decided about their unsaved changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Exit {
    LeaveExplorer,
    Quit,
    Open(std::path::PathBuf),
}

/// A file with unsaved changes, and what was being attempted.
#[derive(Debug, Clone)]
pub struct PendingDiscard {
    pub path: String,
    pub then: Exit,
}

pub struct App {
    pub mode: Mode,
    pub query: String,
    pub sort: Sort,
    pub kind: Kind,

    /// The loaded slice of the result set.
    pub rows: Vec<Row>,
    /// Absolute index of `rows[0]` within the full result set.
    pub window_start: usize,
    /// Absolute index of the highlighted row.
    pub selected: usize,
    pub total: u64,
    pub elapsed_us: u64,

    /// Duplicate groups, flattened into display lines and held in full:
    /// a scan reads file contents, so scrolling must never trigger another.
    pub dupes: Vec<DupeEntry>,
    /// The slice of `dupes` currently on screen.
    pub dupes_window: Vec<DupeEntry>,
    /// A duplicate scan is due before the next draw.
    pub dupes_pending: bool,

    /// How long the last duplicate scan took. Kept apart from `elapsed_us`
    /// because a scan can now finish while another view is on screen, and it
    /// must not overwrite that view's timing.
    pub dupes_elapsed_us: u64,
    /// Reclaimable bytes across all groups in `dupes`.
    pub dupes_wasted: u64,

    /// A delete awaiting confirmation. Nothing is removed until the user
    /// answers, and the prompt names the exact path so there is no doubt
    /// about what is about to go.
    pub confirm_delete: Option<PendingDelete>,

    /// Show the rail. On by default; the layout still withholds it when the
    /// terminal is too narrow to spare the columns.
    pub rail: bool,

    /// The file a first Tab stepped into the directory of, waiting for a
    /// second Tab to name it.
    ///
    /// It has to be remembered rather than re-derived: the first Tab changes
    /// the query, which refetches and resets the selection, so by the time the
    /// second Tab arrives the file is no longer the highlighted row.
    pub pending_file: Option<String>,

    /// The explorer, built the first time it is opened.
    pub explorer: Option<crate::tui::explorer::state::Explorer>,

    /// The running shell, if one has been opened.
    pub terminal: Option<crate::tui::terminal::session::Session>,

    /// Where `Esc` came from, so leaving unwinds one step at a time.
    pub came_from: Option<Mode>,

    /// A pending "you have unsaved changes" prompt.
    ///
    /// Kept separate from `confirm_delete` rather than folded into one modal
    /// enum: the delete prompt is already proven, and its two-outcome shape is
    /// not this dialog's three.
    pub confirm_discard: Option<PendingDiscard>,

    /// A refetch is needed before the next draw.
    pub dirty: bool,
    pub quit: bool,
    /// Answers are coming from a live daemon rather than a snapshot.
    pub live: bool,
    pub status: String,
}

impl Default for App {
    fn default() -> Self {
        App {
            mode: Mode::Search,
            query: String::new(),
            sort: Sort::Name,
            kind: Kind::All,
            rail: true,
            pending_file: None,
            explorer: None,
            terminal: None,
            came_from: None,
            confirm_discard: None,
            rows: Vec::new(),
            window_start: 0,
            selected: 0,
            total: 0,
            elapsed_us: 0,
            dupes: Vec::new(),
            dupes_window: Vec::new(),
            dupes_pending: true,
            dupes_elapsed_us: 0,
            dupes_wasted: 0,
            confirm_delete: None,
            dirty: true,
            quit: false,
            live: false,
            status: String::new(),
        }
    }
}

impl App {
    pub fn new(live: bool) -> Self {
        App {
            live,
            ..Default::default()
        }
    }

    /// Ask about deleting whatever is highlighted.
    ///
    /// Returns false when there is nothing selected. Nothing is removed here:
    /// this only raises the prompt.
    pub fn ask_delete(&mut self) -> bool {
        let Some(row) = self.current() else {
            return false;
        };
        self.confirm_delete = Some(PendingDelete {
            path: row.path.clone(),
            is_dir: row.is_dir,
            files: row.files,
            size: row.size,
        });
        true
    }

    pub fn cancel_delete(&mut self) {
        self.confirm_delete = None;
    }

    /// Is a confirmation on screen? While it is, ordinary keys must not reach
    /// the query or the result list.
    pub fn awaiting_confirmation(&self) -> bool {
        self.confirm_delete.is_some() || self.confirm_discard.is_some()
    }

    /// The row under the cursor, if it is currently loaded.
    ///
    /// In the duplicates view a cursor can sit on a group divider, which names
    /// nothing; that reports no row, so pressing Enter on it does nothing.
    pub fn current(&self) -> Option<&Row> {
        match self.mode {
            Mode::Dupes => self
                .dupes_window
                .get(self.index_in_window()?)
                .and_then(|e| match e {
                    DupeEntry::Copy { row, .. } => Some(row),
                    DupeEntry::Group { .. } => None,
                }),
            _ => self.rows.get(self.index_in_window()?),
        }
    }

    fn index_in_window(&self) -> Option<usize> {
        self.selected.checked_sub(self.window_start)
    }

    /// Index of the highlighted row within the loaded window.
    pub fn cursor_in_window(&self) -> Option<usize> {
        let loaded = match self.mode {
            Mode::Dupes => self.dupes_window.len(),
            _ => self.rows.len(),
        };
        self.index_in_window().filter(|&i| i < loaded)
    }

    // -------------------------------------------------------------- editing

    pub fn push_char(&mut self, c: char) {
        self.query.push(c);
        self.reset_scroll();
    }

    pub fn backspace(&mut self) {
        if self.query.pop().is_some() {
            self.reset_scroll();
        }
    }

    /// Delete the last whitespace-delimited word.
    pub fn delete_word(&mut self) {
        let trimmed = self.query.trim_end_matches(|c: char| !c.is_whitespace());
        let cut = trimmed.trim_end_matches(char::is_whitespace);
        if cut.len() != self.query.len() {
            self.query.truncate(cut.len());
            self.reset_scroll();
        }
    }

    /// Replace the query wholesale — the completion path, which is typing a
    /// whole path at once rather than a character.
    pub fn set_query(&mut self, text: &str) {
        if self.query == text {
            return;
        }
        self.query.clear();
        self.query.push_str(text);
        self.reset_scroll();
    }

    pub fn clear_query(&mut self) {
        if !self.query.is_empty() {
            self.query.clear();
            self.reset_scroll();
        }
    }

    /// What Tab does when the cursor is on a row: extend the query to that
    /// row's path, so path typing becomes drill-down completion — type
    /// `C:\Users`, highlight `C:\Users\Hacker`, press Tab, and the query bar
    /// now reads the real path, ready to keep narrowing inside it.
    ///
    /// A directory gains a trailing separator, so the completed query
    /// immediately lists the directory's children. Nothing is offered when the
    /// query is not path-shaped: completing a plain name search to a full path
    /// would be a jump nobody asked for, and Tab falls back to switching modes
    /// there instead.
    /// The directory part of a path, if it has one.
    fn parent_dir(path: &str) -> Option<&str> {
        let (dir, name) = path.rsplit_once(['\\', '/'])?;
        // A bare drive (`D:`) is a legitimate parent and becomes `D:\` once
        // the separator is appended; an empty one is not a path at all.
        (!dir.is_empty() && !name.is_empty()).then_some(dir)
    }

    /// The completed text for the highlighted row.
    ///
    /// This deliberately does not require the query to already look like a
    /// path. Searching broadly by name and then drilling into whichever hit
    /// looks right is the natural way to use this: you rarely know the path
    /// you want in advance, which is the entire reason for searching.
    ///
    /// A directory completes to itself. A file completes in two steps —
    /// first to the directory holding it, then, on a second Tab, to the file.
    /// Going straight to the file would replace the query with one matching
    /// only that file: a dead end with nothing left to narrow. Landing in its
    /// directory first puts the siblings on screen, which is usually what you
    /// were looking for anyway, and the second Tab is there when it is not.
    pub fn complete_selection(&self) -> Option<String> {
        if self.mode != Mode::Search {
            return None;
        }

        // Second Tab: the first stepped into the directory, this names the
        // file it was holding.
        if let Some(file) = &self.pending_file {
            return Some(file.clone());
        }

        let row = self.current()?;
        // The trailing separator is what makes the completion land ready to
        // keep narrowing inside the directory rather than alongside it.
        if row.is_dir {
            return Some(format!("{}\\", row.path));
        }

        // Already sitting on the file itself: stepping back to its directory
        // would undo the last Tab rather than continue it.
        if self.query == row.path {
            return None;
        }
        Self::parent_dir(&row.path).map(|dir| format!("{dir}\\"))
    }

    /// Apply a Tab press.
    pub fn complete(&mut self) {
        let Some(text) = self.complete_selection() else {
            return;
        };
        // Arm the second step only when the first one just happened; a second
        // Tab consumes it rather than re-arming, so repeated presses settle
        // instead of cycling between the directory and the file.
        self.pending_file = if self.pending_file.is_some() {
            None
        } else {
            self.current()
                .filter(|row| !row.is_dir)
                .map(|row| row.path.clone())
        };
        self.set_query(&text);
    }

    /// The text to highlight inside result paths.
    ///
    /// For a path query the rows are children of a directory filtered by the
    /// component still being typed, so that component is the part worth
    /// emphasizing — the part of each name the keystrokes have picked out.
    /// Anything after the last separator; the whole query when it is not a
    /// path. Empty means no highlighting, which is what a query ending in a
    /// separator wants: its rows are unfiltered children.
    pub fn needle(&self) -> &str {
        match self.query.rsplit_once(['\\', '/']) {
            Some((_, tail)) => tail,
            None => self.query.as_str(),
        }
    }

    fn reset_scroll(&mut self) {
        self.selected = 0;
        self.window_start = 0;
        self.dirty = true;
    }

    pub fn cycle_mode(&mut self) {
        self.mode = self.mode.next();
        self.reset_scroll();
    }

    /// Ctrl+D: jump straight to the duplicates view, or back out of it.
    pub fn toggle_dupes(&mut self) {
        self.mode = match self.mode {
            Mode::Dupes => Mode::Search,
            _ => Mode::Dupes,
        };
        self.reset_scroll();
    }

    pub fn cycle_sort(&mut self) {
        self.sort = self.sort.next();
        self.reset_scroll();
    }

    pub fn cycle_kind(&mut self) {
        self.kind = self.kind.next();
        self.reset_scroll();
    }

    // ------------------------------------------------------------ scrolling

    /// Move the selection by `delta` rows, keeping it inside the result set
    /// and pulling the loaded window along with it.
    pub fn move_selection(&mut self, delta: isize, visible: usize) {
        if self.total == 0 {
            self.selected = 0;
            self.window_start = 0;
            return;
        }
        let last = (self.total - 1) as isize;
        let target = (self.selected as isize + delta).clamp(0, last);
        self.selected = target as usize;
        self.scroll_into_view(visible);
    }

    pub fn go_first(&mut self, visible: usize) {
        self.selected = 0;
        self.scroll_into_view(visible);
    }

    pub fn go_last(&mut self, visible: usize) {
        self.selected = (self.total.saturating_sub(1)) as usize;
        self.scroll_into_view(visible);
    }

    /// Adjust the loaded window so the selection sits inside it.
    fn scroll_into_view(&mut self, visible: usize) {
        if visible == 0 {
            return;
        }
        let new_start = if self.selected < self.window_start {
            self.selected
        } else if self.selected >= self.window_start + visible {
            self.selected + 1 - visible
        } else {
            self.window_start
        };
        if new_start != self.window_start {
            self.window_start = new_start;
            self.dirty = true;
        }
    }

    /// Clamp state after a response, in case the result set shrank under us.
    pub fn apply_rows(&mut self, rows: Vec<Row>, total: u64, elapsed_us: u64, visible: usize) {
        self.rows = rows;
        self.total = total;
        self.elapsed_us = elapsed_us;
        self.dirty = false;

        if total == 0 {
            self.selected = 0;
            self.window_start = 0;
            return;
        }
        let last = (total - 1) as usize;
        if self.selected > last {
            self.selected = last;
        }
        if self.window_start > last {
            self.window_start = last;
        }
        // A live index can shrink between the request and the response; fix
        // the window rather than leaving the cursor pointing at nothing.
        if visible > 0 && self.selected >= self.window_start + visible {
            self.window_start = self.selected + 1 - visible;
            self.dirty = true;
        }
    }

    // ------------------------------------------------------------- requests

    /// The request that would fill the current window.
    pub fn request(&self, visible: usize) -> Request {
        let limit = visible.max(1) as u32;
        match self.mode {
            Mode::Search => Request::Search(SearchArgs {
                query: self.query.clone(),
                glob: self.query.contains('*') || self.query.contains('?'),
                case_sensitive: false,
                ext: None,
                min_size: None,
                max_size: None,
                under: None,
                dirs_only: self.kind == Kind::DirsOnly,
                files_only: self.kind == Kind::FilesOnly,
                include_excluded: false,
                sort: self.sort.code(),
                offset: self.window_start as u32,
                limit,
            }),
            // Bloat and dupes have no paging in the protocol, so they ask for
            // enough rows to cover the window and scroll within them.
            Mode::Bloat => Request::Bloat {
                top: (self.window_start + visible.max(1) + 64) as u32,
                under: None,
            },
            // A dupes scan is far too expensive to repeat per scroll, so the
            // result is fetched whole, held, and windowed locally. Only a
            // pending scan issues this request at all.
            // Neither of these queries the index - one reads the disk, the
            // other a pseudo-console - and `refresh` returns before this is
            // reached for both.
            Mode::Explore | Mode::Terminal => Request::Status,
            Mode::Dupes => Request::Dupes {
                min_size: DUPES_MIN_SIZE,
                top: DUPES_TOP,
                under: None,
            },
        }
    }

    /// Bloat responses are not paged, so the window is applied here instead.
    pub fn apply_bloat(&mut self, rows: Vec<Row>, visible: usize) {
        let total = rows.len() as u64;
        let start = self.window_start.min(rows.len());
        let end = (start + visible.max(1)).min(rows.len());
        let window = rows[start..end].to_vec();
        self.apply_rows(window, total, 0, visible);
    }

    /// Store a duplicate scan and show the first window of it.
    ///
    /// Each group becomes a divider line naming what the files share, then
    /// one line per copy — the file mapped against the paths it duplicates.
    pub fn apply_dupes(
        &mut self,
        groups: Vec<DupeGroupRows>,
        wasted_total: u64,
        elapsed_us: u64,
        visible: usize,
    ) {
        self.dupes = groups
            .into_iter()
            .flat_map(|g| {
                let copies = g.rows.len() as u32;
                std::iter::once(DupeEntry::Group {
                    size: g.size,
                    copies,
                    wasted: g.wasted,
                })
                .chain(
                    g.rows
                        .into_iter()
                        .map(move |row| DupeEntry::Copy { row, copies }),
                )
            })
            .collect();
        self.dupes_pending = false;
        self.dupes_wasted = wasted_total;
        self.dupes_elapsed_us = elapsed_us;

        // The scan runs off the UI thread, so it can land while the user is
        // looking at something else. Filling the cache is always right;
        // taking over the window, selection and counters is only right if the
        // duplicates view is the one on screen.
        if self.mode == Mode::Dupes {
            self.window_dupes(visible);
        }
    }

    /// Slice the cached duplicate scan into the loaded window.
    pub fn window_dupes(&mut self, visible: usize) {
        let start = self.window_start.min(self.dupes.len());
        let end = (start + visible.max(1)).min(self.dupes.len());
        self.dupes_window = self.dupes[start..end].to_vec();
        self.total = self.dupes.len() as u64;
        self.dirty = false;

        if self.total == 0 {
            self.selected = 0;
            self.window_start = 0;
            return;
        }
        let last = (self.total - 1) as usize;
        if self.selected > last {
            self.selected = last;
        }
        if self.window_start > last {
            self.window_start = last;
        }
    }

    /// How many groups the cache holds.
    pub fn dupe_group_count(&self) -> usize {
        self.dupes
            .iter()
            .filter(|e| matches!(e, DupeEntry::Group { .. }))
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(n: usize) -> Vec<Row> {
        (0..n)
            .map(|i| Row {
                path: format!(r"C:\data\file_{i:04}.bin"),
                size: i as u64,
                mtime: 0,
                is_dir: false,
                files: 0,
                own: 0,
            })
            .collect()
    }

    #[test]
    fn typing_resets_the_scroll_position() {
        let mut app = App::default();
        app.apply_rows(rows(20), 500, 0, 20);
        app.move_selection(50, 20);
        assert!(app.selected > 0 && app.window_start > 0);

        app.push_char('x');
        assert_eq!(app.selected, 0, "a new query starts at the top");
        assert_eq!(app.window_start, 0);
        assert!(app.dirty);
    }

    #[test]
    fn selection_cannot_leave_the_result_set() {
        let mut app = App::default();
        app.apply_rows(rows(10), 10, 0, 10);

        app.move_selection(-5, 10);
        assert_eq!(app.selected, 0, "cannot move above the first row");

        app.move_selection(1000, 10);
        assert_eq!(app.selected, 9, "cannot move past the last row");
    }

    #[test]
    fn scrolling_down_drags_the_window_with_it() {
        let mut app = App::default();
        let visible = 10;
        app.apply_rows(rows(visible), 1000, 0, visible);
        assert_eq!(app.window_start, 0);

        // Moving inside the window does not need a refetch.
        app.dirty = false;
        app.move_selection(5, visible);
        assert_eq!(app.window_start, 0);
        assert!(!app.dirty, "no refetch while the cursor stays on screen");

        // Stepping past the bottom scrolls by exactly one row.
        app.move_selection(5, visible);
        assert_eq!(app.selected, 10);
        assert_eq!(app.window_start, 1);
        assert!(app.dirty, "leaving the window requires a refetch");
    }

    #[test]
    fn scrolling_up_drags_the_window_back() {
        let mut app = App::default();
        let visible = 10;
        app.total = 1000;
        app.selected = 500;
        app.window_start = 495;

        app.move_selection(-10, visible);
        assert_eq!(app.selected, 490);
        assert_eq!(app.window_start, 490, "the window follows the cursor up");
    }

    #[test]
    fn paging_and_jumping_stay_in_range() {
        let mut app = App::default();
        let visible = 20;
        app.apply_rows(rows(visible), 137, 0, visible);

        app.go_last(visible);
        assert_eq!(app.selected, 136);
        assert_eq!(app.window_start, 136 + 1 - visible);

        app.go_first(visible);
        assert_eq!(app.selected, 0);
        assert_eq!(app.window_start, 0);

        // A page down from the top lands exactly one screen lower.
        app.move_selection(visible as isize, visible);
        assert_eq!(app.selected, visible);
        assert_eq!(app.window_start, 1);
    }

    #[test]
    fn the_request_asks_only_for_the_visible_window() {
        // The whole point of windowing: a query matching a million files must
        // still only transfer a screenful.
        let app = App {
            query: "log".into(),
            window_start: 4_000,
            ..Default::default()
        };
        match app.request(25) {
            Request::Search(a) => {
                assert_eq!(a.offset, 4_000);
                assert_eq!(a.limit, 25);
                assert_eq!(a.query, "log");
                assert!(!a.glob);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn wildcards_switch_the_request_to_glob_mode() {
        let app = App {
            query: "*.rs".into(),
            ..Default::default()
        };
        match app.request(10) {
            Request::Search(a) => assert!(a.glob, "a wildcard should mean a glob search"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn filters_and_sort_reach_the_request() {
        let mut app = App::default();
        app.cycle_sort(); // name -> size
        app.cycle_kind(); // all -> files
        match app.request(10) {
            Request::Search(a) => {
                assert_eq!(a.sort, Sort::Size.code());
                assert!(a.files_only);
                assert!(!a.dirs_only);
            }
            other => panic!("unexpected {other:?}"),
        }
        app.cycle_kind(); // files -> dirs
        match app.request(10) {
            Request::Search(a) => assert!(a.dirs_only && !a.files_only),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn a_shrinking_result_set_does_not_strand_the_cursor() {
        // The index is live, so a result set can shrink between the request
        // and the response. The cursor must be pulled back into range.
        let mut app = App {
            selected: 900,
            window_start: 890,
            ..Default::default()
        };
        app.apply_rows(rows(3), 3, 0, 10);

        assert_eq!(app.selected, 2);
        assert!(app.window_start <= app.selected);
        assert!(app.cursor_in_window().is_some());
    }

    #[test]
    fn an_empty_result_set_is_handled() {
        let mut app = App::default();
        app.apply_rows(Vec::new(), 0, 0, 10);
        assert_eq!(app.selected, 0);
        assert!(app.current().is_none());
        assert!(app.cursor_in_window().is_none());

        // Moving around an empty list must not panic or go negative.
        app.move_selection(-1, 10);
        app.move_selection(10, 10);
        app.go_last(10);
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn the_cursor_maps_into_the_loaded_window() {
        let mut app = App {
            window_start: 100,
            ..Default::default()
        };
        app.apply_rows(rows(10), 1000, 0, 10);
        app.selected = 104;
        assert_eq!(app.cursor_in_window(), Some(4));
        assert_eq!(app.current().unwrap().path, r"C:\data\file_0004.bin");

        // A selection outside the loaded slice reports nothing rather than
        // indexing into the wrong row.
        app.selected = 500;
        assert_eq!(app.cursor_in_window(), None);
        assert!(app.current().is_none());
    }

    #[test]
    fn word_deletion_trims_back_to_the_previous_word() {
        let mut app = App {
            query: "some long query".into(),
            ..Default::default()
        };
        app.delete_word();
        assert_eq!(app.query, "some long");
        app.delete_word();
        assert_eq!(app.query, "some");
        app.delete_word();
        assert_eq!(app.query, "");
        app.delete_word();
        assert_eq!(app.query, "", "deleting from empty is a no-op");
    }

    #[test]
    fn tab_completes_a_path_query_to_the_highlighted_row() {
        // The drill-down flow: type a path, the rows below are its children,
        // Tab adopts the highlighted one as the new query.
        let mut app = App {
            query: r"C:\data".into(),
            ..Default::default()
        };
        app.apply_rows(
            vec![Row {
                path: r"C:\Data\Hacker".into(),
                size: 0,
                mtime: 0,
                is_dir: true,
                files: 10,
                own: 0,
            }],
            1,
            0,
            10,
        );

        let done = app.complete_selection().expect("a dir row completes");
        assert_eq!(
            done, r"C:\Data\Hacker\",
            "dirs gain a separator to keep drilling"
        );

        app.set_query(&done);
        assert_eq!(app.query, r"C:\Data\Hacker\");
        assert_eq!(app.selected, 0, "completion starts the listing at the top");
        assert!(app.dirty, "completion must refetch");
    }

    #[test]
    fn a_file_completes_to_its_directory_first_and_itself_second() {
        let mut app = App {
            query: r"C:\data\file".into(),
            ..Default::default()
        };
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
        // First Tab: into the directory, so the siblings are on screen.
        app.complete();
        assert_eq!(app.query, r"C:\data\");

        // Second Tab: the file itself, even though the refetch has moved the
        // selection off it.
        app.complete();
        assert_eq!(app.query, r"C:\data\file_0001.bin");
    }

    #[test]
    fn repeated_tabs_settle_instead_of_cycling() {
        // Without the guard this ping-pongs: file, directory, file, directory.
        let mut app = App {
            query: "file".into(),
            ..Default::default()
        };
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

        app.complete();
        app.complete();
        let settled = app.query.clone();
        app.complete();
        assert_eq!(app.query, settled, "a third Tab must not step back out");
    }

    #[test]
    fn a_directory_completes_in_one_step_with_nothing_pending() {
        let mut app = App {
            query: "downloads".into(),
            ..Default::default()
        };
        app.apply_rows(
            vec![Row {
                path: r"D:\Downloads".into(),
                size: 0,
                mtime: 0,
                is_dir: true,
                files: 4,
                own: 0,
            }],
            1,
            0,
            10,
        );

        app.complete();
        assert_eq!(app.query, r"D:\Downloads\");
        assert_eq!(
            app.pending_file, None,
            "a directory has no second step to arm"
        );
    }

    #[test]
    fn a_file_at_the_root_of_a_drive_completes_to_the_drive() {
        let mut app = App {
            query: "boot".into(),
            ..Default::default()
        };
        app.apply_rows(
            vec![Row {
                path: r"D:\boot.ini".into(),
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
        app.complete();
        assert_eq!(app.query, r"D:\", "a bare drive is still a directory");
    }

    #[test]
    fn tab_completes_from_a_plain_name_search_too() {
        // You rarely know the path you want in advance — that is what the
        // search is for. Finding a directory by name and pressing Tab has to
        // drill into it, not refuse because the query was not already a path.
        let mut app = App {
            query: "downloads".into(),
            ..Default::default()
        };
        app.apply_rows(
            vec![Row {
                path: r"D:\Downloads".into(),
                size: 0,
                mtime: 0,
                is_dir: true,
                files: 4,
                own: 0,
            }],
            1,
            0,
            10,
        );

        assert_eq!(app.complete_selection().unwrap(), r"D:\Downloads\");
    }

    #[test]
    fn completing_a_name_search_produces_something_that_actually_browses() {
        // Textually right is not enough: the completed query has to be one
        // the browser recognises, or Tab would land you on a path query that
        // silently falls back to a name search.
        let mut app = App {
            query: "downloads".into(),
            ..Default::default()
        };
        // A real directory name with a space in it, which is where naive
        // path handling tends to come apart.
        app.apply_rows(
            vec![Row {
                path: r"D:\Downloads\Asus Downloads".into(),
                size: 1_320_000,
                mtime: 1_788_134_400,
                is_dir: true,
                files: 3,
                own: 0,
            }],
            1,
            0,
            10,
        );

        let done = app.complete_selection().expect("a directory completes");
        assert_eq!(done, r"D:\Downloads\Asus Downloads\");
        assert!(
            crate::query::looks_like_path(&done),
            "the completion must read as a path, or browsing it does nothing"
        );
    }

    #[test]
    fn tab_completion_needs_something_highlighted() {
        let app = App {
            query: r"C:\nowhere".into(),
            ..Default::default()
        };
        assert_eq!(app.complete_selection(), None);

        let mut app = App {
            mode: Mode::Bloat,
            query: r"C:\data".into(),
            ..Default::default()
        };
        app.apply_rows(rows(3), 3, 0, 10);
        assert_eq!(
            app.complete_selection(),
            None,
            "bloat rows are not completion targets"
        );
    }

    #[test]
    fn the_needle_is_the_path_component_still_being_typed() {
        let app = App {
            query: r"C:\Users\hac".into(),
            ..Default::default()
        };
        assert_eq!(app.needle(), "hac");

        // A query ending in a separator lists unfiltered children: nothing
        // deserves highlighting.
        let app = App {
            query: r"C:\Users\".into(),
            ..Default::default()
        };
        assert_eq!(app.needle(), "");

        let app = App {
            query: "report".into(),
            ..Default::default()
        };
        assert_eq!(app.needle(), "report");
    }

    #[test]
    fn bloat_rows_are_windowed_locally() {
        // Bloat is not paged in the protocol, so the app slices it itself.
        let mut app = App {
            mode: Mode::Bloat,
            window_start: 5,
            ..Default::default()
        };
        app.apply_bloat(rows(40), 10);

        assert_eq!(app.total, 40);
        assert_eq!(app.rows.len(), 10);
        assert_eq!(app.rows[0].path, r"C:\data\file_0005.bin");
    }

    #[test]
    fn switching_mode_resets_position() {
        let mut app = App::default();
        app.apply_rows(rows(10), 100, 0, 10);
        app.move_selection(50, 10);
        app.cycle_mode();
        assert_eq!(app.mode, Mode::Bloat);
        assert_eq!(app.selected, 0);
        assert_eq!(app.window_start, 0);
        assert!(app.dirty);
    }

    #[test]
    fn tab_cycles_through_all_three_modes() {
        let mut app = App::default();
        app.cycle_mode();
        assert_eq!(app.mode, Mode::Bloat);
        app.cycle_mode();
        assert_eq!(app.mode, Mode::Dupes);
        app.cycle_mode();
        assert_eq!(app.mode, Mode::Search);
    }

    fn dupe_groups(n_groups: usize, copies: usize) -> Vec<batuta_ipc::DupeGroupRows> {
        (0..n_groups)
            .map(|g| {
                let rows: Vec<Row> = (0..copies)
                    .map(|c| Row {
                        path: format!(r"C:\dupes\g{g}_copy{c}.bin"),
                        size: 1024,
                        mtime: 0,
                        is_dir: false,
                        files: 0,
                        own: 0,
                    })
                    .collect();
                batuta_ipc::DupeGroupRows {
                    size: 1024,
                    wasted: 1024 * (copies as u64 - 1),
                    rows,
                }
            })
            .collect()
    }

    #[test]
    fn ctrl_d_jumps_to_dupes_and_back() {
        let mut app = App::default();
        app.toggle_dupes();
        assert_eq!(app.mode, Mode::Dupes);
        assert!(app.dupes_pending, "the first entry must trigger a scan");

        app.toggle_dupes();
        assert_eq!(app.mode, Mode::Search, "a second press backs out");
    }

    #[test]
    fn a_scan_landing_while_another_view_is_open_does_not_take_it_over() {
        // The scan runs off the UI thread now, so it can finish at any moment
        // — including while the user is halfway down a search result list.
        // Filling the cache is right; moving their selection is not.
        let mut app = App::default();
        app.apply_rows(rows(40), 400, 0, 10);
        app.selected = 17;
        app.window_start = 12;
        let (total, selected, start) = (app.total, app.selected, app.window_start);

        assert_eq!(app.mode, Mode::Search, "precondition");
        app.apply_dupes(dupe_groups(2, 2), 4096, 1_234_567, 10);

        assert_eq!(app.total, total, "the search count was overwritten");
        assert_eq!(app.selected, selected, "the selection moved");
        assert_eq!(app.window_start, start, "the window scrolled");

        // The cache still has to be filled, or coming back would rescan.
        assert!(!app.dupes_pending, "the result should have been cached");
        assert!(!app.dupes.is_empty());
        assert_eq!(app.dupes_elapsed_us, 1_234_567);
        assert_eq!(app.elapsed_us, 0, "the search timing must be left alone");

        // And it slices into view on return, with no second scan.
        app.mode = Mode::Dupes;
        app.window_dupes(10);
        assert!(app.total > 0);
    }

    #[test]
    fn dupes_are_held_in_full_and_windowed_locally() {
        // A scan reads file contents, so scrolling through the result must
        // never issue another request — only the window moves.
        let mut app = App {
            mode: Mode::Dupes,
            ..Default::default()
        };
        app.apply_dupes(dupe_groups(20, 2), 20 * 1024, 5_000, 10);

        assert_eq!(app.total, 60, "20 dividers plus 40 copies");
        assert_eq!(app.dupes_window.len(), 10);
        assert_eq!(app.dupe_group_count(), 20);
        assert_eq!(app.dupes_wasted, 20 * 1024);
        assert!(!app.dupes_pending);

        app.dirty = false;
        app.move_selection(20, 10);
        assert!(app.dirty, "scrolling re-windows locally");
        app.window_dupes(10);
        assert!(!app.dirty);
        assert_eq!(app.window_start, 11);
        assert_eq!(
            app.dupes_window[0],
            DupeEntry::Copy {
                row: Row {
                    path: r"C:\dupes\g3_copy1.bin".into(),
                    size: 1024,
                    mtime: 0,
                    is_dir: false,
                    files: 0,
                    own: 0,
                },
                copies: 2,
            }
        );
    }

    #[test]
    fn the_cursor_never_lands_on_a_group_divider() {
        // Pressing Enter must do nothing on a divider, not open a fabricated
        // path: `current()` answers no row there.
        let mut app = App {
            mode: Mode::Dupes,
            ..Default::default()
        };
        app.apply_dupes(dupe_groups(3, 2), 0, 0, 10);

        // Entry 0 is the first group's divider.
        assert!(matches!(app.dupes_window[0], DupeEntry::Group { .. }));
        assert!(app.current().is_none(), "a divider opens nothing");

        app.move_selection(1, 10);
        assert!(app.current().is_some(), "a copy row opens in Explorer");
    }

    #[test]
    fn a_dupes_request_is_bounded_and_cheap_by_default() {
        let app = App {
            mode: Mode::Dupes,
            ..Default::default()
        };
        match app.request(10) {
            Request::Dupes {
                min_size,
                top,
                under,
            } => {
                assert_eq!(min_size, DUPES_MIN_SIZE);
                assert_eq!(top, DUPES_TOP);
                assert!(under.is_none());
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn an_empty_dupes_result_clamps_cleanly() {
        let mut app = App {
            mode: Mode::Dupes,
            ..Default::default()
        };
        app.apply_dupes(Vec::new(), 0, 0, 10);
        assert_eq!(app.total, 0);
        assert!(app.dupes_window.is_empty());
        app.move_selection(10, 10);
        app.go_last(10);
        assert_eq!(app.selected, 0);
    }
}
