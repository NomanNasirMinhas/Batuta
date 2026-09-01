//! Producing an index: either from a cached snapshot or by reading the MFT.

use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use batuta_core::index::IndexBuilder;
use batuta_core::Index;
use batuta_ntfs::usn::{self, JournalStatus};
use batuta_ntfs::{is_elevated, MftScanner, ScanStats};

use crate::config::Config;

pub struct BuiltIndex {
    pub index: Index,
    pub per_volume: Vec<ScanStats>,
    pub per_volume_time: Vec<Duration>,
    pub dir_counts: Vec<u32>,
    /// Subtrees pruned by configuration, with the bytes they held.
    pub excluded: Vec<(String, u64)>,
    pub scan_time: Duration,
    pub build_time: Duration,
    pub total_time: Duration,
    /// True when this came from a snapshot rather than a fresh MFT read.
    pub from_snapshot: bool,
}

/// How the caller wants the index obtained.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// Use a snapshot if one is present, otherwise scan.
    Cached,
    /// Always read the MFT.
    Fresh,
}

/// Load a snapshot if there is one, else scan the volumes.
///
/// Query commands take this path so they do not pay for an MFT read on every
/// invocation; only `scan` forces a fresh read.
pub fn load_index(cfg: &Config, source: Source) -> Result<BuiltIndex> {
    if source == Source::Cached {
        let path = cfg.snapshot_path();
        if path.exists() {
            let started = Instant::now();
            match Index::load(&path) {
                Ok(mut index) => {
                    let excluded = apply_exclusions(&mut index, cfg);
                    let elapsed = started.elapsed();
                    let n = index.volumes.len();
                    return Ok(BuiltIndex {
                        index,
                        per_volume: vec![ScanStats::default(); n],
                        per_volume_time: vec![Duration::ZERO; n],
                        dir_counts: vec![0; n],
                        excluded,
                        scan_time: Duration::ZERO,
                        build_time: elapsed,
                        total_time: elapsed,
                        from_snapshot: true,
                    });
                }
                Err(e) => {
                    // A damaged or outdated snapshot is not fatal; say so and
                    // rebuild rather than refusing to run.
                    eprintln!("note: ignoring snapshot ({e}); rescanning");
                }
            }
        }
    }
    scan_volumes(cfg, false)
}

/// Read every configured volume's MFT and build a fresh index.
pub fn scan_volumes(cfg: &Config, verbose: bool) -> Result<BuiltIndex> {
    if !is_elevated() {
        bail!(
            "batuta needs Administrator to read the MFT.\n\
             Re-run from an elevated terminal, or use a cached index if one exists."
        );
    }
    if cfg.drives.is_empty() {
        bail!("no drives configured; see `batuta config`");
    }

    let started = Instant::now();
    let mut builder = IndexBuilder::new();
    let mut per_volume = Vec::new();
    let mut per_volume_time = Vec::new();
    let mut dir_counts = Vec::new();
    let mut scan_total = Duration::ZERO;

    for &drive in &cfg.drives {
        let t0 = Instant::now();

        // Take the journal position *before* reading the MFT. Anything that
        // changes during the scan then replays afterwards; the reverse order
        // would lose those changes silently.
        let next_usn = match usn::query(drive) {
            Ok(JournalStatus::Active(info)) => info.next_usn,
            _ => 0,
        };

        let scanner =
            MftScanner::open(drive).with_context(|| format!("opening volume {drive}:"))?;

        if verbose {
            let b = scanner.boot();
            eprintln!(
                "{drive}: cluster {} B, record {} B, MFT {} across {} runs",
                b.cluster_size(),
                b.file_record_size,
                crate::fmt::bytes(scanner.mft_bytes()),
                scanner.run_count(),
            );
        }

        let (batch, stats) = scanner
            .scan()
            .with_context(|| format!("scanning {drive}: MFT"))?;
        let elapsed = t0.elapsed();
        scan_total += elapsed;

        let dirs = batch
            .flags
            .iter()
            .filter(|f| *f & batuta_ntfs::mft::eflags::DIRECTORY != 0)
            .count() as u32;

        builder.add_volume(
            drive,
            scanner.boot().volume_serial,
            next_usn,
            &batch,
            scanner.max_records(),
        );
        per_volume.push(stats);
        per_volume_time.push(elapsed);
        dir_counts.push(dirs);
    }

    let t1 = Instant::now();
    let mut index = builder.build();
    let build_time = t1.elapsed();

    // Persist before pruning: snapshots hold the unpruned tree so changing
    // `exclude` later costs a re-prune rather than another MFT read.
    let path = cfg.snapshot_path();
    if let Err(e) = index.save(&path) {
        eprintln!(
            "warning: could not write snapshot to {}: {e}",
            path.display()
        );
    }

    let excluded = apply_exclusions(&mut index, cfg);

    Ok(BuiltIndex {
        index,
        per_volume,
        per_volume_time,
        dir_counts,
        excluded,
        scan_time: scan_total,
        build_time,
        total_time: started.elapsed(),
        from_snapshot: false,
    })
}

/// Prune configured subtrees. A path that does not exist is not an error:
/// `C:\Program Files (x86)` is absent on some systems.
fn apply_exclusions(index: &mut Index, cfg: &Config) -> Vec<(String, u64)> {
    let mut excluded = Vec::new();
    for path in &cfg.exclude {
        if let Some(node) = index.lookup(path) {
            let size = index.size[node as usize];
            index.exclude_subtree(node);
            excluded.push((path.clone(), size));
        }
    }
    excluded
}
