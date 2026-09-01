//! The resident daemon: holds the index, follows the change journal, and
//! answers queries over the named pipe.
//!
//! Concurrency is deliberately plain. One reader thread per volume parks
//! inside a blocking journal read; a single applier thread owns all mutation;
//! query handlers take a read lock for the microsecond or so a search needs.
//!
//! The index is behind an `RwLock` rather than being swapped atomically. An
//! atomic swap would need a fresh copy of the whole structure for every
//! change, which for a ~140 MB index is absurd when the change itself is a
//! handful of array writes. Readers block only for as long as one batch of
//! updates takes to apply.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

use anyhow::{bail, Result};
use parking_lot::RwLock;

use batuta_core::search::Searcher;
use batuta_core::watch::WatchStats;
use batuta_core::Index;
use batuta_ipc::{Request, Response};
use batuta_ntfs::usn::{JournalReader, Poll, UsnRecords};

use crate::config::Config;
use crate::pipe::{PipeServer, PipeStream};
use crate::{query, scan, watcher};

/// Shared daemon state.
/// Release the daemon's working set back to the system.
///
/// The index stays allocated and valid; Windows simply stops keeping its pages
/// resident, so reported memory drops to a few megabytes. The next query
/// faults back only what it touches. Used when the user asked not to keep the
/// index in memory.
#[cfg(windows)]
fn release_working_set() {
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, SetProcessWorkingSetSize};
    // usize::MAX for both bounds is the documented way to ask for a trim.
    unsafe { SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX) };
}

#[cfg(not(windows))]
fn release_working_set() {}

/// How long the daemon must go unqueried before it lets the index page out.
const IDLE_BEFORE_RELEASE: Duration = Duration::from_secs(90);

struct Shared {
    cfg: Config,
    index: RwLock<Index>,
    /// Milliseconds since start, at the last client request. Drives the decision
    /// to let the index page out when nobody is using it.
    last_query: AtomicU64,
    watching: AtomicBool,
    changes: AtomicU64,
    /// Set when a volume's journal has moved past our position and only a
    /// full rescan can restore correctness.
    desynced: AtomicBool,
}

/// A stop signal for the daemon.
///
/// The accept loop blocks inside `ConnectNamedPipe`, so setting a flag is not
/// enough to wake it — [`Stop::trigger`] also connects to the pipe, which
/// completes the pending accept and lets the loop notice the flag. Without
/// that a service `STOP` would time out and the SCM would kill the process.
#[derive(Clone, Default)]
pub struct Stop(Arc<AtomicBool>);

impl Stop {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_set(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }

    /// Ask the daemon to shut down, and unblock it so it notices.
    pub fn trigger(&self) {
        self.0.store(true, Ordering::Relaxed);
        let _ = PipeStream::connect();
    }
}

/// Run the daemon in the foreground until interrupted.
pub fn serve(cfg: &Config, verbose: bool) -> Result<()> {
    serve_with(cfg, verbose, Stop::new(), || {})
}

/// Run the daemon, with a stop signal and a callback fired once the pipe is
/// accepting. The service uses both: it reports `StartPending` until the
/// callback runs, then `Running`.
pub fn serve_with(cfg: &Config, verbose: bool, stop: Stop, on_ready: impl FnOnce()) -> Result<()> {
    if !batuta_ntfs::is_elevated() {
        bail!("the daemon needs Administrator to read the MFT and the change journal");
    }
    if PipeStream::daemon_running() {
        bail!("a daemon is already listening on {}", batuta_ipc::PIPE_NAME);
    }

    eprintln!("loading index...");
    let built = scan::load_index(cfg, scan::Source::Cached)?;
    let volumes: Vec<(usize, char, i64)> = built
        .index
        .volumes
        .iter()
        .enumerate()
        .map(|(i, v)| (i, v.drive, v.next_usn))
        .collect();

    let snapshot = cfg.snapshot_path();
    let shared = Arc::new(Shared {
        cfg: cfg.clone(),
        index: RwLock::new(built.index),
        last_query: AtomicU64::new(0),
        watching: AtomicBool::new(false),
        changes: AtomicU64::new(0),
        desynced: AtomicBool::new(false),
    });

    // ---- journal readers -------------------------------------------------
    let (tx, rx) = mpsc::channel::<(usize, Vec<u8>, bool)>();
    let shared_desync = Arc::clone(&shared);
    let shared_desync = &shared_desync.desynced;
    let mut watched = 0;
    for (vi, drive, start_usn) in volumes {
        let tx = tx.clone();
        match JournalReader::open(drive, start_usn) {
            Ok(mut reader) => {
                if reader.skipped_history() {
                    // The stored position had aged out of the journal, so
                    // changes since the last checkpoint were never seen.
                    // Serving this index as live would be a lie.
                    eprintln!(
                        "warning: {drive}: journal no longer reaches the last checkpoint;                          the index is stale. Run `batuta scan` to rebuild."
                    );
                    shared_desync.store(true, Ordering::Relaxed);
                }
                watched += 1;
                eprintln!("watching {drive}: from usn {}", reader.next_usn());
                std::thread::Builder::new()
                    .name(format!("usn-{drive}"))
                    .spawn(move || loop {
                        match reader.read(1) {
                            Ok(Poll::Records) => {
                                if tx.send((vi, reader.raw().to_vec(), false)).is_err() {
                                    return;
                                }
                            }
                            Ok(Poll::Desynchronised) => {
                                let _ = tx.send((vi, Vec::new(), true));
                                return;
                            }
                            Err(e) => {
                                eprintln!("watcher for {drive} stopped: {e}");
                                return;
                            }
                        }
                    })?;
            }
            Err(e) => eprintln!("warning: not watching {drive}: {e}"),
        }
    }
    drop(tx);
    shared.watching.store(watched > 0, Ordering::Relaxed);
    if shared.desynced.load(Ordering::Relaxed) {
        shared.watching.store(false, Ordering::Relaxed);
    }

    // ---- applier ---------------------------------------------------------
    {
        let shared = Arc::clone(&shared);
        std::thread::Builder::new()
            .name("applier".into())
            .spawn(move || apply_loop(shared, rx, verbose))?;
    }

    // ---- checkpointer ----------------------------------------------------
    {
        let shared = Arc::clone(&shared);
        let snapshot = snapshot.clone();
        std::thread::Builder::new()
            .name("checkpoint".into())
            .spawn(move || loop {
                std::thread::sleep(std::time::Duration::from_secs(60));
                if shared.changes.load(Ordering::Relaxed) == 0 {
                    continue;
                }
                // Snapshotting takes a read lock, so queries still proceed.
                let guard = shared.index.read();
                if let Err(e) = guard.save(&snapshot) {
                    eprintln!("warning: checkpoint failed: {e}");
                }
            })?;
    }

    // ---- idle release ----------------------------------------------------
    // Only when the user chose not to keep the index resident. The allocation
    // stays valid either way; this just stops Windows holding the pages in the
    // working set, trading a slower first query for a much smaller footprint.
    if !cfg.keep_in_ram {
        let shared = Arc::clone(&shared);
        let started = std::time::Instant::now();
        std::thread::Builder::new()
            .name("idle-release".into())
            .spawn(move || loop {
                std::thread::sleep(Duration::from_secs(15));
                let idle = started.elapsed().as_millis() as u64
                    - shared.last_query.load(Ordering::Relaxed);
                if idle >= IDLE_BEFORE_RELEASE.as_millis() as u64 {
                    release_working_set();
                }
            })?;
    }

    // ---- accept loop -----------------------------------------------------
    // The recorded owner, not this process's identity: as a service this is
    // LocalSystem, and using it would lock out the user the pipe is for.
    let server = PipeServer::bind(cfg.owner_sid.as_deref())?;
    eprintln!("listening on {}", batuta_ipc::PIPE_NAME);
    on_ready();

    while !stop.is_set() {
        match server.accept() {
            Ok(stream) => {
                // A connection made purely to wake this loop carries no
                // request; checking the flag again here avoids serving it.
                if stop.is_set() {
                    break;
                }
                let shared = Arc::clone(&shared);
                // One thread per client. Queries are short, so this stays
                // cheap, and a slow client cannot stall the others.
                std::thread::spawn(move || handle_client(shared, stream));
            }
            Err(e) => {
                if stop.is_set() {
                    break;
                }
                eprintln!("accept failed: {e}");
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
        }
    }

    // Checkpoint on the way out, so an ordinary stop loses nothing rather than
    // relying on journal replay to recover the last minute.
    let guard = shared.index.read();
    if let Err(e) = guard.save(&snapshot) {
        eprintln!("warning: final checkpoint failed: {e}");
    }
    eprintln!("stopped");
    Ok(())
}

/// Apply journal batches to the index.
fn apply_loop(shared: Arc<Shared>, rx: mpsc::Receiver<(usize, Vec<u8>, bool)>, verbose: bool) {
    let mut totals = WatchStats::default();
    while let Ok((vi, raw, desynced)) = rx.recv() {
        if desynced {
            eprintln!("volume {vi}: change journal wrapped; run `batuta scan` to rebuild");
            shared.desynced.store(true, Ordering::Relaxed);
            shared.watching.store(false, Ordering::Relaxed);
            continue;
        }

        // Translation reads file metadata, so it runs under a read lock and
        // releases before the write lock is taken. Holding the write lock
        // across disk I/O would stall every query behind it.
        let (next_usn, changes) = {
            let guard = shared.index.read();
            let (next, records) = UsnRecords::new(&raw);
            (next, watcher::translate(&guard, vi, records))
        };

        // Advance the stored position even when the batch produced nothing we
        // act on. Otherwise the checkpointed USN never moves, ages out of the
        // journal, and a later restart silently loses everything since.
        let mut guard = shared.index.write();
        guard.set_next_usn(vi, next_usn);
        for c in &changes {
            let applied = guard.apply(c);
            totals.record(applied);
            if verbose {
                if let Some(n) = crate::applied_node(applied) {
                    eprintln!("{:<8} {}", crate::label(applied), guard.path(n));
                }
            }
        }
        drop(guard);
        shared.changes.store(totals.total(), Ordering::Relaxed);
    }
}

/// Serve one client until it disconnects.
///
/// Generic over the transport so the whole request loop can be exercised
/// against an in-memory stream, with no pipe and no elevation.
fn handle_client<S: Read + Write>(shared: Arc<Shared>, mut stream: S) {
    let opened = std::time::Instant::now();
    // Held for the life of the connection. A UI sends one request per
    // keystroke, and this is what lets each of those narrow the previous
    // result set instead of rescanning the whole name arena.
    let mut searcher = Searcher::new();

    loop {
        let frame = match batuta_ipc::read_frame(&mut stream) {
            Ok(f) if f.is_empty() => return,
            Ok(f) => f,
            Err(_) => return, // disconnect, or a frame we refused to size
        };

        let response = match Request::decode(&frame) {
            // Rescanning has to happen here. A separate process writing
            // index.bin achieves nothing: clients ask the daemon, which serves
            // its own in-memory copy and overwrites that file at the next
            // checkpoint. So the repair runs in-process and replaces it.
            Ok(Request::Rescan) => match scan::scan_volumes(&shared.cfg, false) {
                Ok(built) => {
                    let nodes = built.index.len() as u64;
                    *shared.index.write() = built.index;
                    shared.desynced.store(false, Ordering::Relaxed);
                    Response::Status {
                        nodes,
                        memory: 0,
                        watching: shared.watching.load(Ordering::Relaxed),
                        changes_applied: shared.changes.load(Ordering::Relaxed),
                        volumes: Vec::new(),
                    }
                }
                Err(e) => Response::Error {
                    message: format!("rescan failed: {e:#}"),
                },
            },
            Ok(req) => {
                shared
                    .last_query
                    .store(opened.elapsed().as_millis() as u64, Ordering::Relaxed);
                // Queries take this guard for microseconds; a `Dupes` request
                // holds it for the whole scan, because it reads file contents
                // against the index it was started from. That stalls change
                // application (other queries still proceed) for as long as
                // the hashing takes — acceptable for something only a user
                // can explicitly ask for, over a pipe restricted to them.
                let guard = shared.index.read();
                // A wrapped journal means the index has drifted and only a
                // rescan can fix it. `watching` goes false on desync, which
                // `status` surfaces, so callers can tell live answers from
                // stale ones.
                query::execute(
                    &guard,
                    &req,
                    &mut searcher,
                    shared.watching.load(Ordering::Relaxed)
                        && !shared.desynced.load(Ordering::Relaxed),
                    shared.changes.load(Ordering::Relaxed),
                )
            }
            // Malformed input from an unprivileged client is answered, not
            // acted on, and never crashes the daemon.
            Err(e) => Response::Error {
                message: format!("bad request: {e}"),
            },
        };

        if batuta_ipc::write_frame(&mut stream, &response.encode()).is_err() {
            return;
        }
    }
}

/// Ask a running daemon to answer a request, if one is listening.
pub fn try_daemon(req: &Request) -> Option<Response> {
    let mut stream = PipeStream::connect().ok()?;
    stream.request(req).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use batuta_core::testtree::{TreeBuilder, ROOT_REC};
    use batuta_ipc::SearchArgs;
    use std::io::Cursor;

    /// An in-memory stand-in for a connected pipe: reads a scripted request
    /// stream, collects what the daemon writes back.
    struct MockStream {
        input: Cursor<Vec<u8>>,
        output: Vec<u8>,
    }

    impl MockStream {
        fn new(requests: &[Request]) -> Self {
            let mut buf = Vec::new();
            for r in requests {
                batuta_ipc::write_frame(&mut buf, &r.encode()).unwrap();
            }
            MockStream {
                input: Cursor::new(buf),
                output: Vec::new(),
            }
        }

        fn raw(bytes: Vec<u8>) -> Self {
            MockStream {
                input: Cursor::new(bytes),
                output: Vec::new(),
            }
        }

        /// Decode everything the daemon wrote.
        fn responses(&self) -> Vec<Response> {
            let mut out = Vec::new();
            let mut cur = Cursor::new(self.output.clone());
            while let Ok(frame) = batuta_ipc::read_frame(&mut cur) {
                match Response::decode(&frame) {
                    Ok(r) => out.push(r),
                    Err(_) => break,
                }
            }
            out
        }
    }

    impl Read for MockStream {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.input.read(buf)
        }
    }

    impl Write for MockStream {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.output.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn shared() -> Arc<Shared> {
        let mut t = TreeBuilder::new('C');
        let users = t.dir(ROOT_REC, "Users");
        let hacker = t.dir(users, "Hacker");
        t.file(hacker, "notes.txt", 100);
        t.file(hacker, "installer.exe", 50 * 1024 * 1024);
        Arc::new(Shared {
            cfg: Config::default(),
            index: RwLock::new(t.build()),
            last_query: AtomicU64::new(0),
            watching: AtomicBool::new(true),
            changes: AtomicU64::new(7),
            desynced: AtomicBool::new(false),
        })
    }

    fn search(q: &str) -> Request {
        Request::Search(SearchArgs {
            query: q.into(),
            limit: 10,
            ..Default::default()
        })
    }

    #[test]
    fn answers_a_sequence_of_requests_on_one_connection() {
        let mut stream = MockStream::new(&[search("notes"), Request::Status]);
        handle_client(shared(), &mut stream);

        let responses = stream.responses();
        assert_eq!(responses.len(), 2, "one reply per request");
        match &responses[0] {
            Response::Rows { rows, total, .. } => {
                assert_eq!(*total, 1);
                assert!(rows[0].path.ends_with("notes.txt"));
            }
            other => panic!("unexpected {other:?}"),
        }
        match &responses[1] {
            Response::Status {
                watching,
                changes_applied,
                ..
            } => {
                assert!(*watching);
                assert_eq!(*changes_applied, 7);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn narrowing_state_persists_across_requests_on_a_connection() {
        // The reason each connection owns a Searcher: consecutive keystrokes
        // must narrow, and must still give the same answer as a cold scan.
        let mut stream =
            MockStream::new(&[search("i"), search("in"), search("ins"), search("inst")]);
        handle_client(shared(), &mut stream);

        let responses = stream.responses();
        assert_eq!(responses.len(), 4);
        match responses.last().unwrap() {
            Response::Rows { rows, total, .. } => {
                assert_eq!(*total, 1);
                assert!(rows[0].path.ends_with("installer.exe"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn a_malformed_request_is_answered_not_fatal() {
        // An unprivileged client must not be able to kill a privileged daemon,
        // and a bad frame must not desynchronise the ones after it.
        let mut buf = Vec::new();
        batuta_ipc::write_frame(&mut buf, &[0xFF, 0xFF, 0xFF]).unwrap(); // unknown tag
        batuta_ipc::write_frame(&mut buf, &search("notes").encode()).unwrap();

        let mut stream = MockStream::raw(buf);
        handle_client(shared(), &mut stream);

        let responses = stream.responses();
        assert_eq!(responses.len(), 2, "the connection survived the bad frame");
        assert!(matches!(responses[0], Response::Error { .. }));
        assert!(matches!(responses[1], Response::Rows { .. }));
    }

    #[test]
    fn an_oversized_frame_closes_the_connection_without_allocating() {
        let mut buf = (batuta_ipc::MAX_FRAME + 1).to_le_bytes().to_vec();
        buf.extend_from_slice(b"payload that never arrives");
        let mut stream = MockStream::raw(buf);
        handle_client(shared(), &mut stream);
        assert!(
            stream.responses().is_empty(),
            "no reply, and no huge allocation"
        );
    }

    #[test]
    fn a_truncated_stream_ends_the_loop_cleanly() {
        let mut buf = 500u32.to_le_bytes().to_vec();
        buf.extend_from_slice(b"only a few bytes");
        let mut stream = MockStream::raw(buf);
        handle_client(shared(), &mut stream); // must return, not hang
        assert!(stream.responses().is_empty());
    }

    #[test]
    fn a_rescan_request_is_carried_out_here_not_pushed_back_to_the_user() {
        // This used to answer "run `batuta scan` yourself", which achieved
        // nothing: a separate process writes a snapshot the daemon neither
        // reads nor keeps, because its own checkpoint overwrites it. The
        // rebuild has to happen in the daemon.
        let mut stream = MockStream::new(&[Request::Rescan]);
        handle_client(shared(), &mut stream);

        match &stream.responses()[0] {
            // Under test there is no elevation, so the scan cannot run — but
            // it must have been attempted rather than declined.
            Response::Error { message } => {
                assert!(
                    message.starts_with("rescan failed"),
                    "should report the attempt, got: {message}"
                );
                assert!(
                    !message.contains("batuta scan"),
                    "must not send the user off to a command that cannot help"
                );
            }
            Response::Status { .. } => {} // elevated CI could genuinely succeed
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn a_desynced_daemon_stops_claiming_to_be_live() {
        // After the journal wraps, the index has drifted. `status` must say so
        // rather than presenting stale totals as current.
        let shared = shared();
        shared.desynced.store(true, Ordering::Relaxed);

        let mut stream = MockStream::new(&[Request::Status]);
        handle_client(shared, &mut stream);
        match &stream.responses()[0] {
            Response::Status { watching, .. } => {
                assert!(!watching, "a desynced index must not report itself live")
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn errors_for_bad_paths_reach_the_client() {
        let mut stream = MockStream::new(&[Request::Size {
            path: r"C:\does\not\exist".into(),
            top: 5,
        }]);
        handle_client(shared(), &mut stream);
        assert!(matches!(stream.responses()[0], Response::Error { .. }));
    }
}
