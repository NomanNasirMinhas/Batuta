//! Executing a [`SetupPlan`].
//!
//! Every step reports its own outcome instead of aborting the run. A machine
//! where the hotkey combination is taken, or where `PATH` cannot be written,
//! should still end up with a working index and service — and the user should
//! be told exactly which parts did not happen, rather than seeing one error
//! and having to guess what state they are in.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;

use super::flow::{Resident, SetupPlan};
use super::{acl, pathenv, procs, task};
use crate::config::Config;
use crate::{hotkey, scan, service};

/// What happened to one step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Done(String),
    /// Nothing needed doing.
    Skipped(String),
    /// Tried and failed. The rest of the run continues.
    Failed(String),
}

impl Outcome {
    pub fn is_failure(&self) -> bool {
        matches!(self, Outcome::Failed(_))
    }

    fn marker(&self) -> &'static str {
        match self {
            Outcome::Done(_) => "  ok",
            Outcome::Skipped(_) => "  --",
            Outcome::Failed(_) => "FAIL",
        }
    }

    fn text(&self) -> &str {
        match self {
            Outcome::Done(s) | Outcome::Skipped(s) | Outcome::Failed(s) => s,
        }
    }
}

/// The result of applying a whole plan.
#[derive(Debug, Default)]
pub struct Report {
    pub steps: Vec<(String, Outcome)>,
    /// Set when a step failed badly enough that the rest was not attempted.
    pub aborted: Option<String>,
}

impl Report {
    fn add(&mut self, name: &str, outcome: Outcome) -> &Outcome {
        self.steps.push((name.to_string(), outcome));
        &self.steps.last().unwrap().1
    }

    pub fn failures(&self) -> usize {
        self.steps.iter().filter(|(_, o)| o.is_failure()).count()
    }

    pub fn render(&self) -> String {
        let mut out = String::new();
        for (name, outcome) in &self.steps {
            out.push_str(&format!(
                "{} {name}: {}\n",
                outcome.marker(),
                outcome.text()
            ));
        }
        out
    }
}

/// How long an existing index may be before setup rebuilds it anyway.
const INDEX_STILL_FRESH: u64 = 60 * 60;

/// Why the index does or does not need rebuilding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanDecision {
    /// Rebuild, for this reason.
    Rebuild(&'static str),
    /// Reuse what is already there.
    Reuse { nodes: u64, age_secs: u64 },
}

/// Decide whether the existing snapshot can stand.
///
/// Re-running setup to change one setting should not cost a full rescan, but
/// reusing an index that covers different drives would silently leave a volume
/// unindexed, and reusing a stale one would answer with old data. So the
/// snapshot is kept only when it covers exactly the requested drives and is
/// recent.
pub fn decide_scan(
    existing: Option<&batuta_core::snapshot::SnapshotInfo>,
    wanted: &[char],
    now: u64,
) -> ScanDecision {
    let Some(info) = existing else {
        return ScanDecision::Rebuild("no usable index yet");
    };

    let mut have: Vec<char> = info.drives.clone();
    let mut want: Vec<char> = wanted.to_vec();
    have.sort_unstable();
    want.sort_unstable();
    if have != want {
        return ScanDecision::Rebuild("the selected drives changed");
    }

    let age = now.saturating_sub(info.written_at);
    if age > INDEX_STILL_FRESH {
        return ScanDecision::Rebuild("the existing index is out of date");
    }
    ScanDecision::Reuse {
        nodes: info.nodes,
        age_secs: age,
    }
}

/// Record a step and echo it as it happens, so a long apply shows progress
/// rather than sitting silent until the end.
fn say(report: &mut Report, log: &mut dyn FnMut(&str), name: &str, outcome: Outcome) {
    let o = report.add(name, outcome);
    log(&format!("{} {name}: {}", o.marker(), o.text()));
}

/// Turn a `Result` into an outcome, so one failing step does not end the run.
fn step<T>(result: Result<T>, ok: impl FnOnce(T) -> String) -> Outcome {
    match result {
        Ok(v) => Outcome::Done(ok(v)),
        Err(e) => Outcome::Failed(format!("{e:#}")),
    }
}

/// Copy the running executable into the install directory.
///
/// A service or scheduled task pointed at `target\release` would break on the
/// next rebuild, so setup takes its own copy.
///
/// Replacing it on a later run is the awkward case: Windows locks a running
/// executable, so the service and the hotkey helper both have to let go of it
/// first. Whatever was stopped is reported back, since the user is about to be
/// told their service restarted.
fn install_binary(install_dir: &Path, note: &mut Vec<String>) -> Result<PathBuf> {
    let current = std::env::current_exe()?;
    let dest = install_dir.join("batuta.exe");
    std::fs::create_dir_all(install_dir)?;

    // Running from the destination already (a re-run of setup) is fine.
    if current
        .canonicalize()
        .ok()
        .zip(dest.canonicalize().ok())
        .is_some_and(|(a, b)| a == b)
    {
        return Ok(dest);
    }

    // Identical content needs no copy at all, which is the common case when
    // setup is re-run only to change a setting.
    if same_contents(&current, &dest) {
        return Ok(dest);
    }

    if dest.exists() {
        // The service holds the file open. Stopping it also means it picks up
        // the new binary when setup starts it again.
        if service::is_installed() && service::stop(Duration::from_secs(30)).is_ok() {
            note.push("stopped the service".into());
        }
        // So does the hotkey helper — and only one process can own a hotkey,
        // so an old one left running would block the new one's registration.
        let stopped = procs::stop_running_from(&dest);
        if stopped > 0 {
            note.push(format!("stopped {stopped} running helper(s)"));
        }
    }

    match std::fs::copy(&current, &dest) {
        Ok(_) => Ok(dest),
        Err(e) if dest.exists() => {
            // Still locked by something we could not stop. A running image can
            // always be renamed even when it cannot be overwritten, so move it
            // aside and write the new one in its place.
            let aside = dest.with_extension("old");
            let _ = std::fs::remove_file(&aside);
            std::fs::rename(&dest, &aside).map_err(|_| e)?;
            match std::fs::copy(&current, &dest) {
                Ok(_) => {
                    if std::fs::remove_file(&aside).is_err() {
                        note.push(
                            "the previous copy is still running and will be cleaned up later"
                                .into(),
                        );
                    }
                    Ok(dest)
                }
                Err(e2) => {
                    // Put it back rather than leaving no executable at all.
                    let _ = std::fs::rename(&aside, &dest);
                    Err(e2.into())
                }
            }
        }
        Err(e) => Err(e.into()),
    }
}

/// Are these two files byte-identical?
fn same_contents(a: &Path, b: &Path) -> bool {
    let (Ok(ma), Ok(mb)) = (std::fs::metadata(a), std::fs::metadata(b)) else {
        return false;
    };
    if ma.len() != mb.len() {
        return false;
    }
    matches!((std::fs::read(a), std::fs::read(b)), (Ok(x), Ok(y)) if x == y)
}

fn run_schtasks(args: Vec<String>) -> Result<()> {
    let out = std::process::Command::new("schtasks")
        .args(&args)
        .output()?;
    if out.status.success() {
        return Ok(());
    }
    let msg = String::from_utf8_lossy(&out.stderr);
    let msg = if msg.trim().is_empty() {
        String::from_utf8_lossy(&out.stdout).to_string()
    } else {
        msg.to_string()
    };
    anyhow::bail!("schtasks failed: {}", msg.trim())
}

/// Apply the plan, reporting each step. Never returns early on a step failure.
pub fn apply(plan: &SetupPlan, base: &Config, log: &mut dyn FnMut(&str)) -> Report {
    let mut report = Report::default();

    // ---- 1. install the binary ----------------------------------------
    let mut note = Vec::new();
    let exe = install_binary(&plan.install_dir, &mut note);
    let exe_path = exe.as_ref().ok().cloned();
    say(
        &mut report,
        log,
        "install",
        step(exe, |p| {
            let mut msg = format!("ready at {}", p.display());
            if !note.is_empty() {
                msg.push_str(&format!(" ({})", note.join(", ")));
            }
            msg
        }),
    );
    // Everything below points at the installed copy. Continuing without it
    // would register a service and a hotkey against a file that is not there,
    // so the run stops here — and says so, rather than letting the caller
    // report the remaining steps as though they had happened.
    let Some(exe_path) = exe_path else {
        report.aborted =
            Some("batuta.exe could not be replaced, so nothing else was changed".into());
        return report;
    };

    // ---- 2. configuration ----------------------------------------------
    let mut cfg = plan.to_config(base);
    // Captured here, while running as the human user: the daemon runs as
    // LocalSystem and could only ever discover SYSTEM's own SID.
    if let Ok(sid) = crate::pipe::current_user_sid() {
        cfg.owner_sid = Some(sid);
    }
    let cfg_path = Config::default_path();
    say(
        &mut report,
        log,
        "config",
        step(cfg.save(&cfg_path), |_| {
            format!("written to {}", cfg_path.display())
        }),
    );

    // ---- 3. data directory and its permissions -------------------------
    say(
        &mut report,
        log,
        "data folder",
        step(
            std::fs::create_dir_all(&plan.data_dir).map_err(Into::into),
            |_| format!("ready at {}", plan.data_dir.display()),
        ),
    );

    let acl_outcome = if !plan.restrict_index {
        Outcome::Skipped("left readable by all local users, as chosen".into())
    } else {
        match crate::pipe::current_user_sid() {
            Ok(sid) => step(
                acl::restrict(&plan.data_dir, &sid).map_err(Into::into),
                |_| "restricted to you, Administrators and SYSTEM".into(),
            ),
            Err(e) => Outcome::Failed(format!("could not read your account SID: {e}")),
        }
    };
    say(&mut report, log, "permissions", acl_outcome);

    // ---- 4. build the index --------------------------------------------
    // Before the service starts, so the daemon comes up against a ready index
    // rather than blocking its first client for several seconds.
    let existing = batuta_core::snapshot::peek(&cfg.snapshot_path()).ok();
    let index_outcome = match decide_scan(existing.as_ref(), &plan.drives, crate::fmt::now_unix()) {
        ScanDecision::Reuse { nodes, age_secs } => Outcome::Skipped(format!(
            "reusing the existing index, {} entries, {} old",
            crate::fmt::count(nodes),
            crate::fmt::duration(age_secs)
        )),
        ScanDecision::Rebuild(why) => {
            log(&format!(
                "     building the index ({why}), this takes a few seconds..."
            ));
            step(scan::scan_volumes(&cfg, false), |b| {
                format!(
                    "{} entries in {:.1?}",
                    crate::fmt::count(b.index.len() as u64),
                    b.total_time
                )
            })
        }
    };
    say(&mut report, log, "index", index_outcome);

    // ---- 5. keep it up to date -----------------------------------------
    match &plan.resident {
        Resident::Service => {
            let installed = step(service::install(&exe_path), |_| "registered".into());
            let failed = installed.is_failure();
            say(&mut report, log, "service", installed);

            if !failed {
                let started = step(service::start(Duration::from_secs(120)), |_| {
                    "started".into()
                });
                let failed = started.is_failure();
                say(&mut report, log, "service start", started);

                if !failed {
                    // Running is not the same as working: only a round trip
                    // over the pipe shows it is actually answering.
                    let v = service::verify(Duration::from_secs(30));
                    say(
                        &mut report,
                        log,
                        "service check",
                        match v {
                            Ok(r) if r.ok() => Outcome::Done(r.describe()),
                            Ok(r) => Outcome::Failed(r.describe()),
                            Err(e) => Outcome::Failed(format!("{e:#}")),
                        },
                    );
                }
            }
        }
        Resident::Task { minutes } => {
            say(
                &mut report,
                log,
                "scheduled task",
                step(run_schtasks(task::create_args(&exe_path, *minutes)), |_| {
                    format!("re-indexing every {minutes} minute(s)")
                }),
            );
        }
        Resident::Manual => {
            say(
                &mut report,
                log,
                "updates",
                Outcome::Skipped("nothing resident; run `batuta scan` to refresh".into()),
            );
        }
    }

    // ---- 6. hotkey ------------------------------------------------------
    let hotkey_outcome = match &plan.hotkey {
        None => Outcome::Skipped("no shortcut requested".into()),
        Some(spec) => match hotkey::autostart_enable(&exe_path, spec) {
            Ok(()) => match start_hotkey_helper(&exe_path, spec) {
                Ok(()) => Outcome::Done(format!("{spec} opens the search bar")),
                // The autostart entry is in place, so it will work after the
                // next sign-in even if it could not start right now.
                Err(e) => Outcome::Failed(format!(
                    "{spec} registered for next sign-in, but could not start now: {e}"
                )),
            },
            Err(e) => Outcome::Failed(format!("{e}")),
        },
    };
    say(&mut report, log, "hotkey", hotkey_outcome);

    // ---- 7. PATH --------------------------------------------------------
    let path_outcome = if !plan.add_to_path {
        Outcome::Skipped("not requested".into())
    } else {
        match pathenv::add(&plan.install_dir) {
            Ok(true) => Outcome::Done("added; open a new terminal to pick it up".into()),
            Ok(false) => Outcome::Skipped("already on PATH".into()),
            Err(e) => Outcome::Failed(format!("{e}")),
        }
    };
    say(&mut report, log, "PATH", path_outcome);

    report
}

/// Launch the hotkey helper for this session, detached.
fn start_hotkey_helper(exe: &Path, spec: &str) -> Result<()> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const DETACHED_PROCESS: u32 = 0x0000_0008;

    std::process::Command::new(exe)
        .args(["hotkey", "--combo", spec])
        .creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS)
        .spawn()?;
    Ok(())
}

/// Undo what setup installed. Reports each step the same way.
pub fn uninstall(cfg: &Config, log: &mut dyn FnMut(&str)) -> Report {
    let mut report = Report::default();

    say(
        &mut report,
        log,
        "service",
        match service::uninstall() {
            Ok(true) => Outcome::Done("stopped and removed".into()),
            Ok(false) => Outcome::Skipped("not installed".into()),
            Err(e) => Outcome::Failed(format!("{e:#}")),
        },
    );

    // Ask first: schtasks fails the same way for "absent" and "could not
    // delete", and reporting a removal that did not happen would be worse
    // than saying nothing.
    let task_existed = run_schtasks(task::query_args()).is_ok();
    say(
        &mut report,
        log,
        "scheduled task",
        if !task_existed {
            Outcome::Skipped("not present".into())
        } else {
            match run_schtasks(task::delete_args()) {
                Ok(()) => Outcome::Done("removed".into()),
                Err(e) => Outcome::Failed(format!("{e:#}")),
            }
        },
    );

    say(
        &mut report,
        log,
        "hotkey",
        match hotkey::autostart_disable() {
            Ok(true) => Outcome::Done("autostart removed; it stops at next sign-out".into()),
            Ok(false) => Outcome::Skipped("not registered".into()),
            Err(e) => Outcome::Failed(format!("{e}")),
        },
    );

    say(
        &mut report,
        log,
        "PATH",
        match pathenv::remove(&cfg.install_dir) {
            Ok(true) => Outcome::Done("entry removed".into()),
            Ok(false) => Outcome::Skipped("not on PATH".into()),
            Err(e) => Outcome::Failed(format!("{e}")),
        },
    );

    report
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report_with(outcomes: Vec<(&str, Outcome)>) -> Report {
        Report {
            steps: outcomes
                .into_iter()
                .map(|(n, o)| (n.to_string(), o))
                .collect(),
            aborted: None,
        }
    }

    use batuta_core::snapshot::SnapshotInfo;

    fn info(drives: &[char], age_secs: u64, now: u64) -> SnapshotInfo {
        SnapshotInfo {
            written_at: now - age_secs,
            nodes: 3_401_288,
            drives: drives.to_vec(),
        }
    }

    const NOW: u64 = 1_800_000_000;

    #[test]
    fn a_fresh_index_over_the_same_drives_is_reused() {
        // Re-running setup to change a hotkey should not cost a full rescan.
        let d = decide_scan(Some(&info(&['C', 'D'], 120, NOW)), &['C', 'D'], NOW);
        assert!(
            matches!(
                d,
                ScanDecision::Reuse {
                    nodes: 3_401_288,
                    ..
                }
            ),
            "{d:?}"
        );
    }

    #[test]
    fn drive_order_does_not_force_a_rebuild() {
        let d = decide_scan(Some(&info(&['D', 'C'], 60, NOW)), &['C', 'D'], NOW);
        assert!(matches!(d, ScanDecision::Reuse { .. }), "{d:?}");
    }

    #[test]
    fn changing_the_drive_selection_forces_a_rebuild() {
        // Reusing here would silently leave the newly chosen volume unindexed.
        for (have, want) in [
            (vec!['C'], vec!['C', 'D']),
            (vec!['C', 'D'], vec!['C']),
            (vec!['C', 'D'], vec!['C', 'E']),
        ] {
            let d = decide_scan(Some(&info(&have, 60, NOW)), &want, NOW);
            assert_eq!(
                d,
                ScanDecision::Rebuild("the selected drives changed"),
                "{have:?} -> {want:?}"
            );
        }
    }

    #[test]
    fn a_stale_index_is_rebuilt() {
        let d = decide_scan(
            Some(&info(&['C', 'D'], INDEX_STILL_FRESH + 1, NOW)),
            &['C', 'D'],
            NOW,
        );
        assert_eq!(
            d,
            ScanDecision::Rebuild("the existing index is out of date")
        );

        // Right at the boundary it still counts as fresh.
        let d = decide_scan(
            Some(&info(&['C', 'D'], INDEX_STILL_FRESH, NOW)),
            &['C', 'D'],
            NOW,
        );
        assert!(matches!(d, ScanDecision::Reuse { .. }), "{d:?}");
    }

    #[test]
    fn a_missing_or_unreadable_index_is_always_built() {
        assert_eq!(
            decide_scan(None, &['C'], NOW),
            ScanDecision::Rebuild("no usable index yet")
        );
    }

    #[test]
    fn a_snapshot_from_the_future_is_not_treated_as_stale() {
        // Clock skew must not cause a pointless rebuild.
        let mut i = info(&['C'], 0, NOW);
        i.written_at = NOW + 5_000;
        assert!(matches!(
            decide_scan(Some(&i), &['C'], NOW),
            ScanDecision::Reuse { .. }
        ));
    }

    #[test]
    fn failures_are_counted_and_marked() {
        let r = report_with(vec![
            ("install", Outcome::Done("copied".into())),
            (
                "hotkey",
                Outcome::Failed("Ctrl+Space is already in use".into()),
            ),
            ("PATH", Outcome::Skipped("not requested".into())),
        ]);
        assert_eq!(r.failures(), 1);

        let text = r.render();
        assert!(text.contains("FAIL hotkey"), "{text}");
        assert!(
            text.contains("already in use"),
            "the reason must survive: {text}"
        );
        assert!(text.contains("  ok install"), "{text}");
        assert!(text.contains("  -- PATH"), "{text}");
    }

    #[test]
    fn an_aborted_run_is_distinguishable_from_a_completed_one() {
        // The bug this guards: a failed install returned a report with one
        // failure, and the caller announced "everything else was applied"
        // before advertising a hotkey that had never been registered.
        let mut r = report_with(vec![(
            "install",
            Outcome::Failed("used by another process".into()),
        )]);
        assert_eq!(r.aborted, None, "a plain failure is not an abort");

        r.aborted = Some("batuta.exe could not be replaced".into());
        assert!(r.aborted.is_some());
        assert_eq!(r.steps.len(), 1, "no later step should have been recorded");
        assert_eq!(r.failures(), 1);
    }

    #[test]
    fn a_clean_run_reports_no_failures() {
        let r = report_with(vec![
            ("install", Outcome::Done("copied".into())),
            ("service", Outcome::Done("registered".into())),
        ]);
        assert_eq!(r.failures(), 0);
    }

    #[test]
    fn skipping_is_not_failing() {
        // Declining the hotkey or PATH must not make setup look broken.
        let r = report_with(vec![
            ("hotkey", Outcome::Skipped("no shortcut requested".into())),
            ("PATH", Outcome::Skipped("not requested".into())),
        ]);
        assert_eq!(r.failures(), 0);
        assert!(!r.render().contains("FAIL"));
    }

    #[test]
    fn step_converts_errors_without_losing_the_message() {
        let ok = step(Ok::<_, anyhow::Error>(7), |v| format!("got {v}"));
        assert_eq!(ok, Outcome::Done("got 7".into()));

        let bad: Result<u32> = Err(anyhow::anyhow!("disk full"));
        let out = step(bad, |_| "unused".into());
        assert!(out.is_failure());
        assert!(out.text().contains("disk full"));
    }
}
