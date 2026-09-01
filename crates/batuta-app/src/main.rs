//! Batuta: a real-time NTFS file index.

mod config;
mod console;
mod content;
mod daemon;
mod elevate;
mod fmt;
mod hotkey;
mod pipe;
mod query;
mod scan;
mod service;
mod setup;
mod tui;
mod watcher;

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use std::io::IsTerminal;

use batuta_core::dupes::{find_duplicates, DupeOptions};
use batuta_core::watch::WatchStats;
use batuta_core::Index;
use batuta_ipc::{Request, Response, SearchArgs};
use config::Config;
use setup::prompt::Ask;

#[derive(Parser)]
#[command(
    name = "batuta",
    about = "Real-time NTFS file index: instant search, live folder sizes, duplicate detection",
    version
)]
struct Cli {
    /// Path to the config file.
    #[arg(long, global = true)]
    config: Option<std::path::PathBuf>,

    /// Index only this drive, overriding the config.
    #[arg(long, global = true)]
    drive: Option<char>,

    /// Ignore the cached index and the daemon; re-read the MFT.
    #[arg(long, global = true)]
    fresh: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Scan the configured volumes and report what was found.
    Scan {
        /// Print per-volume detail.
        #[arg(long)]
        verbose: bool,
    },
    /// Find files and directories by name.
    Search {
        /// Text to look for; a substring by default.
        query: String,
        #[command(flatten)]
        opts: SearchOpts,
    },
    /// Show the rolled-up size of a directory and its largest children.
    Size {
        /// Directory to report on, e.g. `C:\Users\Hacker`.
        path: String,
        /// How many children to list.
        #[arg(long, default_value_t = 20)]
        top: usize,
    },
    /// Rank directories by rolled-up size: what is eating the disk.
    Bloat {
        /// How many directories to list.
        #[arg(long, default_value_t = 30)]
        top: usize,
        /// Restrict to this subtree.
        #[arg(long)]
        under: Option<String>,
    },
    /// Find byte-identical files.
    Dupes {
        /// Ignore files below this size, e.g. `1M`.
        #[arg(long, default_value = "1M")]
        min_size: String,
        /// Restrict to this subtree.
        #[arg(long)]
        under: Option<String>,
        /// How many groups to list.
        #[arg(long, default_value_t = 25)]
        top: usize,
    },
    /// Report index and change-journal state.
    Status,
    /// Follow the change journal in the foreground, keeping sizes live.
    Watch {
        /// Print each change as it is applied.
        #[arg(long)]
        verbose: bool,
        /// Stop after this many seconds; 0 runs until interrupted.
        #[arg(long, default_value_t = 0)]
        duration: u64,
    },
    /// Run the resident daemon: follow changes and answer queries on a pipe.
    Serve {
        /// Print each change as it is applied.
        #[arg(long)]
        verbose: bool,
    },
    /// Interactive terminal UI: search as you type, browse folder sizes.
    Ui,
    /// Guided first-time setup: install, index, and wire up shortcuts.
    Setup {
        /// Set by the elevated relaunch; not meant to be typed.
        #[arg(long, hide = true)]
        elevated: bool,
    },
    /// Undo what setup installed.
    Uninstall,
    /// Service entry point, started by Windows.
    #[command(hide = true)]
    ServiceRun,
    /// Resident global-hotkey helper.
    #[command(hide = true)]
    Hotkey {
        #[arg(long)]
        combo: String,
    },
    /// Show or write the configuration.
    Config {
        /// Write the current configuration to disk.
        #[arg(long)]
        init: bool,
    },
}

#[derive(Args)]
struct SearchOpts {
    /// Match the whole name as a glob (`*` and `?`) instead of a substring.
    #[arg(long)]
    glob: bool,
    /// Match case exactly.
    #[arg(long)]
    case_sensitive: bool,
    /// Only this extension, without the dot.
    #[arg(long)]
    ext: Option<String>,
    /// Minimum size, e.g. `10M`.
    #[arg(long)]
    min_size: Option<String>,
    /// Maximum size, e.g. `1G`.
    #[arg(long)]
    max_size: Option<String>,
    /// Restrict to this subtree.
    #[arg(long)]
    under: Option<String>,
    /// Only directories.
    #[arg(long)]
    dirs: bool,
    /// Only files.
    #[arg(long)]
    files: bool,
    /// Include excluded subtrees and NTFS metadata files.
    #[arg(long)]
    all: bool,
    /// Sort order.
    #[arg(long, value_parser = ["name", "size", "modified"], default_value = "name")]
    sort: String,
    /// Maximum rows to print.
    #[arg(long, default_value_t = 50)]
    limit: usize,
}

fn main() {
    let result = run();
    if let Err(e) = &result {
        eprintln!();
        eprintln!("error: {e:#}");
    }

    // A window we own alone vanishes the moment we exit, taking the output
    // with it — but only some commands leave output worth reading. The UI in
    // particular is launched by the hotkey into a console of its own, and
    // should disappear when it closes rather than leaving a prompt behind.
    if console::owns_console_alone() && (result.is_err() || leaves_output_to_read()) {
        console::wait_for_enter();
    }
    if result.is_err() {
        std::process::exit(1);
    }
}

/// Does this invocation print something the user still needs after it exits?
///
/// Setup and uninstall report what they changed; everything else either shows
/// its own interface or is being read by another program.
fn leaves_output_to_read() -> bool {
    match std::env::args().nth(1).as_deref() {
        // A bare launch goes straight to Quick Setup.
        None => true,
        Some("setup") | Some("uninstall") => true,
        _ => false,
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let cfg_path = cli.config.clone().unwrap_or_else(Config::default_path);
    let mut cfg = Config::load(&cfg_path)
        .with_context(|| format!("reading config at {}", cfg_path.display()))?;
    if let Some(d) = cli.drive {
        cfg.drives = vec![d];
    }
    let source = if cli.fresh {
        scan::Source::Fresh
    } else {
        scan::Source::Cached
    };

    // Run with no arguments — a double-click — means the user wants to set
    // Batuta up, not read a usage message.
    let Some(command) = &cli.command else {
        return do_setup(&cfg, false);
    };

    match command {
        Command::Setup { elevated } => do_setup(&cfg, *elevated),
        Command::Uninstall => do_uninstall(&cfg),
        Command::ServiceRun => service::run(),
        Command::Hotkey { combo } => hotkey::run(combo, &cfg.exe_path()).map_err(Into::into),

        Command::Config { init } => {
            if *init {
                cfg.save(&cfg_path)?;
                println!("wrote {}", cfg_path.display());
            } else {
                println!("# {}", cfg_path.display());
                print!("{}", toml::to_string_pretty(&cfg)?);
            }
            Ok(())
        }

        Command::Scan { verbose } => do_scan(&cfg, *verbose),

        Command::Serve { verbose } => daemon::serve(&cfg, *verbose),

        Command::Status => do_status(&cfg, cli.fresh),

        Command::Ui => tui::run(&cfg, source),

        Command::Search { query, opts } => {
            let req = Request::Search(build_search(query, opts)?);
            answer(&cfg, source, &req, |r| query::render_rows(r, false))
        }

        Command::Size { path, top } => {
            let req = Request::Size {
                path: path.clone(),
                top: *top as u32,
            };
            answer(&cfg, source, &req, query::render_size)
        }

        Command::Bloat { top, under } => {
            let req = Request::Bloat {
                top: *top as u32,
                under: under.clone(),
            };
            answer(&cfg, source, &req, |r| query::render_rows(r, true))
        }

        // Duplicate detection reads file contents, so the CLI runs it here
        // rather than asking the daemon to do bulk I/O for a client. The UI
        // is the one exception: it holds no index of its own when the daemon
        // answers, so it sends a `Dupes` request over its owner-restricted
        // pipe instead of loading a second, stale copy of the index.
        Command::Dupes {
            min_size,
            under,
            top,
        } => {
            let built = scan::load_index(&cfg, source)?;
            note_if_stale(&cfg, &built);
            do_dupes(&built.index, min_size, under.as_deref(), *top)
        }

        Command::Watch { verbose, duration } => {
            let built = scan::load_index(&cfg, source)?;
            do_watch(&cfg, built, *verbose, *duration)
        }
    }
}

/// Answer a query from the daemon if one is listening, otherwise locally.
///
/// The daemon's index is live; the local fallback reads the last snapshot.
/// Both go through the same `execute` and the same renderer, so the two paths
/// cannot drift apart.
fn answer(
    cfg: &Config,
    source: scan::Source,
    req: &Request,
    render: impl Fn(&Response) -> bool,
) -> Result<()> {
    if source == scan::Source::Cached {
        if let Some(resp) = daemon::try_daemon(req) {
            return finish(render(&resp));
        }
    }
    let built = scan::load_index(cfg, source)?;
    note_if_stale(cfg, &built);
    let mut searcher = batuta_core::search::Searcher::new();
    let resp = query::execute(&built.index, req, &mut searcher, false, 0);
    finish(render(&resp))
}

fn finish(ok: bool) -> Result<()> {
    if ok {
        Ok(())
    } else {
        std::process::exit(1);
    }
}

fn build_search(text: &str, o: &SearchOpts) -> Result<SearchArgs> {
    Ok(SearchArgs {
        query: text.to_string(),
        glob: o.glob,
        case_sensitive: o.case_sensitive,
        ext: o.ext.clone(),
        min_size: match &o.min_size {
            Some(s) => Some(fmt::parse_size(s).with_context(|| format!("bad --min-size {s}"))?),
            None => None,
        },
        max_size: match &o.max_size {
            Some(s) => Some(fmt::parse_size(s).with_context(|| format!("bad --max-size {s}"))?),
            None => None,
        },
        under: o.under.clone(),
        dirs_only: o.dirs,
        files_only: o.files,
        include_excluded: o.all,
        sort: match o.sort.as_str() {
            "size" => 1,
            "modified" => 2,
            _ => 0,
        },
        offset: 0,
        limit: o.limit as u32,
    })
}

/// Warn when answering from a cached index that has had time to drift.
///
/// Nothing keeps the snapshot current unless the daemon or `batuta watch` is
/// running, so silently answering from an hours-old index would mislead.
fn note_if_stale(cfg: &Config, built: &scan::BuiltIndex) {
    const STALE_AFTER: u64 = 30 * 60;
    if !built.from_snapshot {
        return;
    }
    if let Some(written) = batuta_core::snapshot::written_at(&cfg.snapshot_path()) {
        let age = fmt::now_unix().saturating_sub(written);
        if age >= STALE_AFTER {
            eprintln!(
                "note: index is {} old; run `batuta scan` to refresh, or `batuta serve` to track changes",
                fmt::duration(age)
            );
        }
    }
}

fn report_scan(cfg: &Config, built: &scan::BuiltIndex) {
    let idx = &built.index;
    println!();
    for (i, v) in idx.volumes.iter().enumerate() {
        let root = v.root as usize;
        let stats = &built.per_volume[i];
        println!("{}:\\", v.drive);
        println!(
            "  files      {:>14}",
            fmt::count(idx.subtree_files[root] as u64)
        );
        println!(
            "  dirs       {:>14}",
            fmt::count(built.dir_counts[i] as u64)
        );
        println!("  size       {:>14}", fmt::bytes(idx.size[root]));
        println!("  on disk    {:>14}", fmt::bytes(idx.alloc[root]));
        println!(
            "  MFT read   {:>14}  ({} records swept)",
            fmt::bytes(stats.bytes_read),
            fmt::count(stats.records_swept)
        );
        println!("  scan time  {:>14.2?}", built.per_volume_time[i]);
        println!();
    }

    println!("total");
    println!("  nodes      {:>14}", fmt::count(idx.len() as u64));
    println!("  memory     {:>14}", fmt::bytes(idx.memory_bytes() as u64));
    println!("  read+parse {:>14.2?}", built.scan_time);
    println!("  index build{:>14.2?}", built.build_time);
    println!("  total      {:>14.2?}", built.total_time);
    if !built.excluded.is_empty() {
        println!();
        println!("excluded");
        for (path, size) in &built.excluded {
            println!("  {:<40} {}", path, fmt::bytes(*size));
        }
    }
    println!();
    println!("index saved to {}", cfg.snapshot_path().display());
}

fn do_status(cfg: &Config, fresh: bool) -> Result<()> {
    println!(
        "elevated       {}",
        if batuta_ntfs::is_elevated() {
            "yes"
        } else {
            "no"
        }
    );

    // A running daemon is the authority on the index; ask it first.
    let daemon = if fresh {
        None
    } else {
        daemon::try_daemon(&Request::Status)
    };
    match &daemon {
        Some(Response::Status {
            nodes,
            memory,
            watching,
            changes_applied,
            ..
        }) => {
            println!("daemon         running");
            println!("  nodes        {}", fmt::count(*nodes));
            println!("  memory       {}", fmt::bytes(*memory));
            println!("  watching     {}", if *watching { "yes" } else { "no" });
            println!("  changes      {}", fmt::count(*changes_applied));
        }
        // "not installed" and "installed but stopped" are different
        // problems with different fixes; saying only "not running" would
        // leave the user guessing which one they have.
        _ if service::is_installed() => {
            println!("daemon         installed as a service, but not responding")
        }
        _ => println!("daemon         not running (no service installed)"),
    }

    print!("hotkey         ");
    match &cfg.hotkey {
        None => println!("none"),
        Some(k) => {
            // Registered to start and actually running are different things:
            // a helper that died, or lost the combination to another program,
            // leaves the shortcut dead with nothing to show for it.
            let autostart = hotkey::is_autostart_enabled();
            let live = !setup::procs::running_from(&cfg.exe_path()).is_empty();
            let note = match (autostart, live) {
                (true, true) => "active",
                (true, false) => "registered for sign-in, but not running now",
                (false, true) => "running, but will not start at sign-in",
                (false, false) => "configured, but neither running nor registered",
            };
            println!("{k} ({note})");
        }
    }

    let snap = cfg.snapshot_path();
    print!("index          ");
    match batuta_core::snapshot::written_at(&snap) {
        Some(t) => {
            let age = fmt::now_unix().saturating_sub(t);
            println!(
                "{}  (written {}, {} ago)",
                snap.display(),
                fmt::timestamp(t as u32),
                fmt::duration(age)
            );
        }
        None => println!("none yet at {}", snap.display()),
    }

    println!();
    println!("{:<7} {:<12} {:>12}  NOTE", "DRIVE", "JOURNAL", "SIZE");
    for &drive in &cfg.drives {
        match batuta_ntfs::usn::query(drive) {
            Ok(batuta_ntfs::usn::JournalStatus::Active(info)) => println!(
                "{drive}:      {:<12} {:>12}  next usn {}",
                "active",
                fmt::bytes(info.max_size),
                info.next_usn
            ),
            Ok(batuta_ntfs::usn::JournalStatus::NotActive) => println!(
                "{drive}:      {:<12} {:>12}  no journal; changes cannot be tracked",
                "none", "-"
            ),
            Ok(batuta_ntfs::usn::JournalStatus::AccessDenied) => {
                println!("{drive}:      {:<12} {:>12}  cannot query", "denied", "-")
            }
            Err(e) => println!("{drive}:      {:<12} {:>12}  {e}", "error", "-"),
        }
    }
    Ok(())
}

fn do_dupes(idx: &Index, min_size: &str, under: Option<&str>, top: usize) -> Result<()> {
    let mut opt = DupeOptions {
        min_size: fmt::parse_size(min_size)
            .with_context(|| format!("bad --min-size {min_size}"))?,
        limit: top,
        ..Default::default()
    };
    if let Some(u) = under {
        opt.under = Some(
            idx.lookup(u)
                .with_context(|| format!("no indexed directory matches {u}"))?,
        );
    }

    let started = std::time::Instant::now();
    let source = content::FsContent::new(idx);
    let (groups, stats) = find_duplicates(idx, &source, &opt);
    let elapsed = started.elapsed();

    for g in &groups {
        println!(
            "{} x{}  (reclaim {})",
            fmt::bytes(g.size),
            g.nodes.len(),
            fmt::bytes(g.wasted())
        );
        for &n in &g.nodes {
            println!("    {}", idx.path(n));
        }
        println!();
    }

    println!(
        "{} of {} files shared a size; {} sampled, {} fully hashed",
        fmt::count(stats.size_candidates as u64),
        fmt::count(stats.considered as u64),
        fmt::count(stats.sampled as u64),
        fmt::count(stats.fully_hashed as u64)
    );
    println!(
        "read {} sampling + {} hashing, in {:.2?}",
        fmt::bytes(stats.bytes_sampled),
        fmt::bytes(stats.bytes_hashed),
        elapsed
    );
    println!(
        "{} duplicate group{}, {} reclaimable",
        fmt::count(stats.groups as u64),
        if stats.groups == 1 { "" } else { "s" },
        fmt::bytes(stats.wasted_bytes)
    );
    Ok(())
}

fn do_watch(cfg: &Config, mut built: scan::BuiltIndex, verbose: bool, duration: u64) -> Result<()> {
    use batuta_ntfs::usn::{JournalReader, Poll};
    use std::sync::mpsc;

    if !batuta_ntfs::is_elevated() {
        bail!("following the change journal requires Administrator");
    }

    let (tx, rx) = mpsc::channel::<(usize, Vec<u8>, bool)>();
    let volumes: Vec<(usize, char, i64)> = built
        .index
        .volumes
        .iter()
        .enumerate()
        .map(|(i, v)| (i, v.drive, v.next_usn))
        .collect();

    let mut started_any = false;
    for (vi, drive, start_usn) in volumes {
        let tx = tx.clone();
        match JournalReader::open(drive, start_usn) {
            Ok(mut reader) => {
                if reader.skipped_history() {
                    println!(
                        "warning: {drive}: the journal no longer reaches the last checkpoint,                          so the index is stale. Run `batuta scan` to rebuild."
                    );
                }
                started_any = true;
                println!("watching {drive}: from usn {}", reader.next_usn());
                std::thread::spawn(move || loop {
                    // A non-zero wait parks in the kernel until the journal
                    // grows, so an idle watcher costs no CPU at all.
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
                        Err(_) => return,
                    }
                });
            }
            Err(e) => eprintln!("warning: cannot watch {drive}: {e}"),
        }
    }
    drop(tx);
    if !started_any {
        bail!("no volume could be watched");
    }

    println!("press Ctrl-C to stop");
    println!();

    let deadline = (duration > 0)
        .then(|| std::time::Instant::now() + std::time::Duration::from_secs(duration));
    let mut totals = WatchStats::default();
    let mut last_save = std::time::Instant::now();

    loop {
        match rx.recv_timeout(std::time::Duration::from_millis(500)) {
            Ok((vi, raw, desynced)) => {
                if desynced {
                    println!("volume {vi}: journal wrapped; run `batuta scan` to rebuild");
                    break;
                }
                let (next_usn, records) = batuta_ntfs::usn::UsnRecords::new(&raw);
                let changes = watcher::translate(&built.index, vi, records);
                // Record progress even for a batch that changed nothing we
                // track, so the checkpointed position keeps moving forward.
                built.index.set_next_usn(vi, next_usn);
                for c in &changes {
                    let applied = built.index.apply(c);
                    totals.record(applied);
                    if verbose {
                        if let Some(n) = applied_node(applied) {
                            println!(
                                "{:<8} {:>10}  {}",
                                label(applied),
                                fmt::bytes(built.index.size[n as usize]),
                                built.index.path(n)
                            );
                        }
                    }
                }
                if !changes.is_empty() && !verbose {
                    use std::io::Write;
                    print!(
                        "\r{} changes ({} created, {} deleted, {} resized, {} moved/renamed)   ",
                        fmt::count(totals.total()),
                        totals.created,
                        totals.deleted,
                        totals.resized,
                        totals.moved + totals.renamed
                    );
                    let _ = std::io::stdout().flush();
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }

        // Checkpoint periodically so an interruption costs at most a minute
        // of tracking rather than the whole session.
        if totals.total() > 0 && last_save.elapsed() >= std::time::Duration::from_secs(60) {
            checkpoint(&mut built.index, &cfg.snapshot_path());
            last_save = std::time::Instant::now();
        }
        if deadline.is_some_and(|d| std::time::Instant::now() >= d) {
            break;
        }
    }

    println!();
    println!(
        "applied {} changes: {} created, {} deleted, {} resized, {} moved, {} renamed",
        fmt::count(totals.total()),
        totals.created,
        totals.deleted,
        totals.resized,
        totals.moved,
        totals.renamed
    );
    if totals.unknown > 0 {
        println!(
            "{} changes referred to files outside the index",
            fmt::count(totals.unknown)
        );
    }
    checkpoint(&mut built.index, &cfg.snapshot_path());
    Ok(())
}

fn checkpoint(index: &mut Index, path: &std::path::Path) {
    if let Err(e) = index.save(path) {
        eprintln!("warning: could not checkpoint index: {e}");
    }
}

pub(crate) fn applied_node(a: batuta_core::Applied) -> Option<u32> {
    use batuta_core::Applied::*;
    match a {
        Created(n) | Deleted(n) | Resized(n) | Moved(n) | Renamed(n) => Some(n),
        _ => None,
    }
}

pub(crate) fn label(a: batuta_core::Applied) -> &'static str {
    use batuta_core::Applied::*;
    match a {
        Created(_) => "created",
        Deleted(_) => "deleted",
        Resized(_) => "resized",
        Moved(_) => "moved",
        Renamed(_) => "renamed",
        Unknown => "unknown",
        Ignored => "ignored",
    }
}

fn do_setup(cfg: &Config, already_elevated: bool) -> Result<()> {
    // Reading the MFT, registering a service and creating a SYSTEM task all
    // need Administrator. Rather than sending the user off to find an elevated
    // console, restart through UAC and carry on there.
    if !batuta_ntfs::is_elevated() {
        if already_elevated {
            bail!(
                "setup restarted but still does not have Administrator rights.\n\
                 Right-click batuta.exe and choose 'Run as administrator'."
            );
        }
        println!("Setup needs Administrator. Approving the prompt will reopen it in a new window.");
        elevate::relaunch_as_admin(&["setup", "--elevated"])?;
        return Ok(());
    }

    let drives = setup::drives::list();
    if drives.is_empty() {
        bail!("no fixed drives were found to index");
    }

    // Ask with dialogs when we have a terminal to draw on, and fall back to
    // the line protocol when piped or scripted — the flow cannot tell the two
    // apart.
    let tty = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let mut ask: Box<dyn setup::prompt::Ask> = if tty {
        Box::new(setup::wizard::Wizard::new()?)
    } else {
        Box::new(setup::prompt::Console)
    };

    // The wizard owns the screen from here; Ctrl+C during the questions is a
    // clean stop, because nothing has been applied yet.
    let asked = setup::flow::run(ask.as_mut(), &drives, cfg);
    let plan = match asked {
        Ok(plan) => plan,
        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {
            // Through the front-end: the wizard still owns the screen, and a
            // plain println would land on the alternate one and vanish.
            ask.finish(&["Setup cancelled. Nothing was changed.".into()]);
            return Ok(());
        }
        Err(e) => return Err(e.into()),
    };

    // Catch an unusable data directory now, while the answer can still be
    // changed, rather than at the first checkpoint hours later.
    let needs_system = plan.resident == setup::flow::Resident::Service;
    if let Some(problem) = setup::flow::check_data_dir(&plan.data_dir, needs_system) {
        ask.say(&format!("warning: {problem}"));
    }
    for line in plan.summary().lines() {
        ask.say(line);
    }

    if !ask.confirm("Apply these changes?", true)? {
        // Same reason as the cancel path: the screen is still the wizard's.
        ask.finish(&["Nothing was changed.".into()]);
        return Ok(());
    }

    let report = setup::apply::apply(&plan, cfg, &mut |line| ask.progress(line));

    // An aborted run applied none of the later steps, so it must not be
    // described as if it had. Telling the user to press a hotkey that was
    // never registered is worse than telling them nothing.
    let mut closing = Vec::new();
    if let Some(reason) = &report.aborted {
        closing.push(format!("Setup stopped: {reason}"));
        closing.push(String::new());
        closing.extend(report.render().lines().map(|l| l.to_string()));
        closing.push(String::new());
        closing.push("Nothing on your machine was changed by this run.".into());
        if service::is_installed() {
            closing.push("Your existing installation is untouched and still working.".into());
        }
    } else if report.failures() == 0 {
        closing.push("Setup finished.".into());
    } else {
        closing.push(format!(
            "Setup finished with {} problem(s) — everything else was applied:",
            report.failures()
        ));
        closing.push(String::new());
        closing.extend(report.render().lines().map(|l| l.to_string()));
    }

    closing.push(String::new());
    closing.push("  Search from any terminal:  batuta ui".into());
    // Only advertise what actually succeeded.
    if let Some(k) = &plan.hotkey {
        if !step_failed(&report, "hotkey") {
            closing.push(format!("  Or press {k}"));
        }
    }
    if plan.add_to_path && !step_failed(&report, "PATH") {
        closing.push("  (open a new terminal first, so it sees the updated PATH)".into());
    }

    // The wizard shows this as a final screen and waits for a key; the console
    // just prints it.
    ask.finish(&closing);
    Ok(())
}

/// Did a named step fail? Used so the closing advice only mentions things
/// that are actually in place.
fn step_failed(report: &setup::apply::Report, name: &str) -> bool {
    report
        .steps
        .iter()
        .any(|(n, o)| n == name && o.is_failure())
}

fn do_uninstall(cfg: &Config) -> Result<()> {
    if !batuta_ntfs::is_elevated() {
        println!("Removing the service needs Administrator. Approving the prompt reopens this.");
        elevate::relaunch_as_admin(&["uninstall"])?;
        return Ok(());
    }

    let mut ask = setup::prompt::Console;
    println!();
    println!("This removes the Batuta service, scheduled task, startup shortcut and PATH entry.");
    if !ask.confirm("Continue?", false)? {
        println!("Nothing was changed.");
        return Ok(());
    }
    println!();

    let report = setup::apply::uninstall(cfg, &mut |line| println!("{line}"));

    // The index is the user's data, not ours to delete without asking.
    println!();
    let snap = cfg.snapshot_path();
    if snap.exists() {
        let size = std::fs::metadata(&snap).map(|m| m.len()).unwrap_or(0);
        if ask.confirm(
            &format!(
                "Also delete the index at {} ({})?",
                snap.display(),
                fmt::bytes(size)
            ),
            false,
        )? {
            match std::fs::remove_file(&snap) {
                Ok(()) => println!("  ok index: deleted"),
                Err(e) => println!("FAIL index: {e}"),
            }
        }
    }

    println!();
    println!(
        "Removed{}.",
        if report.failures() == 0 {
            String::new()
        } else {
            format!(", with {} problem(s)", report.failures())
        }
    );
    Ok(())
}

/// Rebuild the index.
///
/// When the daemon is running it must do this itself. Scanning in a separate
/// process would write a file nobody reads — queries are answered from the
/// daemon's in-memory index, and its next checkpoint would overwrite the fresh
/// snapshot with the stale one. That is why a scan appeared to do nothing.
fn do_scan(cfg: &Config, verbose: bool) -> Result<()> {
    if pipe::PipeStream::daemon_running() {
        println!("A daemon is running; asking it to rebuild its index...");
        match daemon::try_daemon(&Request::Rescan) {
            Some(Response::Status { nodes, .. }) => {
                println!("rebuilt: {} entries indexed", fmt::count(nodes));
                println!("the daemon is serving the new index now");
                return Ok(());
            }
            Some(Response::Error { message }) => bail!("{message}"),
            Some(other) => bail!("unexpected response: {other:?}"),
            // Falling through here would scan into a file the daemon then
            // overwrites, so say so rather than appearing to succeed.
            None => bail!(
                "could not reach the daemon to rebuild.\n\
                 Stop it first (`sc stop Batuta`) if you want to scan directly."
            ),
        }
    }

    let built = scan::scan_volumes(cfg, verbose)?;
    report_scan(cfg, &built);
    Ok(())
}
