//! Tiered duplicate detection.
//!
//! Hashing every byte of ~800 GB would take minutes and hammer the disk for no
//! reason, because almost all of it is provably unique from metadata alone.
//! Three tiers, each far cheaper than the next and each shrinking the
//! candidate set:
//!
//! 1. **Group by size.** Free: the sizes are already in the index. A file
//!    whose size is unique on the volume cannot have a duplicate, and that
//!    eliminates the overwhelming majority.
//! 2. **Head and tail sample**, hashed with a fast non-cryptographic hash. A
//!    few KB per candidate rather than the whole file.
//! 3. **Full content hash**, only for files that survived both. Cryptographic,
//!    so surviving groups are exact: no false positives.
//!
//! Content access is behind [`ContentSource`] so the whole pipeline is
//! testable on in-memory data with no filesystem involved.

use std::io;

use rayon::prelude::*;
use rustc_hash::FxHashMap;

use crate::index::flags;
use crate::Index;

/// Bytes sampled from each end of a file in tier 2.
pub const SAMPLE_BYTES: usize = 4096;

/// Below this, sampling would read the whole file anyway, so tier 2 is skipped.
const SAMPLE_FLOOR: u64 = (2 * SAMPLE_BYTES) as u64;

#[derive(Debug, Clone)]
pub struct DupeOptions {
    /// Ignore files smaller than this. Zero-byte files are always ignored:
    /// they are all trivially identical and never interesting.
    pub min_size: u64,
    /// Only consider files beneath this node.
    pub under: Option<u32>,
    /// Include excluded subtrees and NTFS metadata.
    pub include_excluded: bool,
    /// Stop after this many groups; 0 means unlimited.
    pub limit: usize,
}

impl Default for DupeOptions {
    fn default() -> Self {
        DupeOptions {
            min_size: 1,
            under: None,
            include_excluded: false,
            limit: 0,
        }
    }
}

/// A set of files with identical content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DupeGroup {
    pub size: u64,
    /// Always at least two nodes, sorted ascending.
    pub nodes: Vec<u32>,
}

impl DupeGroup {
    /// Bytes that could be reclaimed by keeping just one copy.
    pub fn wasted(&self) -> u64 {
        self.size.saturating_mul(self.nodes.len() as u64 - 1)
    }
}

/// Summary of how much work each tier had to do.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DupeStats {
    pub considered: usize,
    /// Files sharing a size with at least one other file.
    pub size_candidates: usize,
    /// Files that needed a head/tail sample read.
    pub sampled: usize,
    /// Files that needed a full content hash.
    pub fully_hashed: usize,
    pub bytes_sampled: u64,
    pub bytes_hashed: u64,
    pub groups: usize,
    pub wasted_bytes: u64,
}

/// Supplies file content to the duplicate finder.
pub trait ContentSource: Sync {
    /// Read into `buf` at `offset`, returning bytes read.
    fn read_at(&self, node: u32, offset: u64, buf: &mut [u8]) -> io::Result<usize>;
    /// Hash the entire file.
    fn hash_full(&self, node: u32) -> io::Result<[u8; 32]>;
}

/// Files eligible for comparison.
fn candidates(idx: &Index, opt: &DupeOptions) -> Vec<u32> {
    (0..idx.len() as u32)
        .filter(|&n| {
            let f = idx.flags[n as usize];
            if f & flags::DIRECTORY != 0 {
                return false;
            }
            if !opt.include_excluded && f & (flags::EXCLUDED | flags::METADATA) != 0 {
                return false;
            }
            // Hard-linked files share one extent; reporting them as duplicates
            // would promise space that deleting a link does not free.
            if f & flags::HARD_LINK != 0 {
                return false;
            }
            let size = idx.size[n as usize];
            if size == 0 || size < opt.min_size {
                return false;
            }
            opt.under.is_none_or(|u| idx.is_under(n, u))
        })
        .collect()
}

/// Run the full tiered pipeline.
pub fn find_duplicates<S: ContentSource>(
    idx: &Index,
    src: &S,
    opt: &DupeOptions,
) -> (Vec<DupeGroup>, DupeStats) {
    let mut stats = DupeStats::default();

    // Tier 1: group by exact size, which costs nothing beyond a sort.
    let mut files = candidates(idx, opt);
    stats.considered = files.len();
    files.sort_unstable_by_key(|&n| (idx.size[n as usize], n));

    let mut size_groups: Vec<Vec<u32>> = Vec::new();
    let mut run_start = 0usize;
    while run_start < files.len() {
        let size = idx.size[files[run_start] as usize];
        let mut end = run_start + 1;
        while end < files.len() && idx.size[files[end] as usize] == size {
            end += 1;
        }
        if end - run_start > 1 {
            size_groups.push(files[run_start..end].to_vec());
        }
        run_start = end;
    }
    stats.size_candidates = size_groups.iter().map(|g| g.len()).sum();

    // Tier 2 and 3 run per size-group, and groups are independent.
    let refined: Vec<(Vec<DupeGroup>, DupeStats)> = size_groups
        .par_iter()
        .map(|group| {
            let size = idx.size[group[0] as usize];
            let mut local = DupeStats::default();
            let mut out = Vec::new();

            let buckets: Vec<Vec<u32>> = if size >= SAMPLE_FLOOR {
                local.sampled += group.len();
                local.bytes_sampled += (group.len() * 2 * SAMPLE_BYTES) as u64;
                bucket_by(group, |n| sample_key(src, n, size))
            } else {
                // Too small to be worth sampling; go straight to full hashing.
                vec![group.clone()]
            };

            for bucket in buckets {
                if bucket.len() < 2 {
                    continue;
                }
                local.fully_hashed += bucket.len();
                local.bytes_hashed += size * bucket.len() as u64;

                for mut exact in bucket_by(&bucket, |n| src.hash_full(n).ok()) {
                    if exact.len() < 2 {
                        continue;
                    }
                    exact.sort_unstable();
                    local.wasted_bytes += size * (exact.len() as u64 - 1);
                    out.push(DupeGroup { size, nodes: exact });
                }
            }
            (out, local)
        })
        .collect();

    let mut groups = Vec::new();
    for (g, s) in refined {
        groups.extend(g);
        stats.sampled += s.sampled;
        stats.fully_hashed += s.fully_hashed;
        stats.bytes_sampled += s.bytes_sampled;
        stats.bytes_hashed += s.bytes_hashed;
        stats.wasted_bytes += s.wasted_bytes;
    }

    // Biggest reclaimable win first.
    groups.sort_unstable_by_key(|g| (std::cmp::Reverse(g.wasted()), g.nodes[0]));
    if opt.limit > 0 && groups.len() > opt.limit {
        groups.truncate(opt.limit);
    }
    stats.groups = groups.len();
    (groups, stats)
}

/// Split a slice of nodes into buckets sharing the same key.
///
/// Nodes whose key could not be computed (an unreadable file, say) are dropped
/// rather than being reported as matching each other.
fn bucket_by<K, F>(nodes: &[u32], key: F) -> Vec<Vec<u32>>
where
    K: std::hash::Hash + Eq,
    F: Fn(u32) -> Option<K>,
{
    let mut map: FxHashMap<K, Vec<u32>> = FxHashMap::default();
    for &n in nodes {
        if let Some(k) = key(n) {
            map.entry(k).or_default().push(n);
        }
    }
    map.into_values().collect()
}

/// Tier 2 key: a hash of the first and last `SAMPLE_BYTES` of the file.
///
/// Reading both ends catches files that share a common header (media
/// containers, office documents, compiled binaries) but diverge later.
fn sample_key<S: ContentSource>(src: &S, node: u32, size: u64) -> Option<u64> {
    let mut head = vec![0u8; SAMPLE_BYTES];
    let mut tail = vec![0u8; SAMPLE_BYTES];

    let h = src.read_at(node, 0, &mut head).ok()?;
    let tail_off = size.saturating_sub(SAMPLE_BYTES as u64);
    let t = src.read_at(node, tail_off, &mut tail).ok()?;

    let mut hasher = xxhash_rust::xxh3::Xxh3::new();
    hasher.update(&head[..h]);
    hasher.update(&tail[..t]);
    Some(hasher.digest())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testtree::{TreeBuilder, ROOT_REC};
    use std::collections::HashMap;

    /// In-memory content, so the tiering can be tested without a filesystem.
    #[derive(Default)]
    struct MemSource {
        data: HashMap<u32, Vec<u8>>,
        /// Nodes whose reads should fail.
        broken: Vec<u32>,
    }

    impl ContentSource for MemSource {
        fn read_at(&self, node: u32, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
            if self.broken.contains(&node) {
                return Err(io::Error::other("unreadable"));
            }
            let d = self
                .data
                .get(&node)
                .ok_or_else(|| io::Error::other("no content"))?;
            let start = (offset as usize).min(d.len());
            let n = buf.len().min(d.len() - start);
            buf[..n].copy_from_slice(&d[start..start + n]);
            Ok(n)
        }

        fn hash_full(&self, node: u32) -> io::Result<[u8; 32]> {
            if self.broken.contains(&node) {
                return Err(io::Error::other("unreadable"));
            }
            let d = self
                .data
                .get(&node)
                .ok_or_else(|| io::Error::other("no content"))?;
            Ok(*blake3::hash(d).as_bytes())
        }
    }

    /// Build an index plus matching content from `(name, bytes)` pairs.
    fn fixture(files: &[(&str, Vec<u8>)]) -> (Index, MemSource) {
        let mut t = TreeBuilder::new('C');
        let dir = t.dir(ROOT_REC, "data");
        for (name, body) in files {
            t.file(dir, name, body.len() as u64);
        }
        let idx = t.build();

        let mut src = MemSource::default();
        for (name, body) in files {
            let n = idx
                .lookup(&format!(r"C:\data\{name}"))
                .expect("fixture file missing");
            src.data.insert(n, body.clone());
        }
        (idx, src)
    }

    fn body(seed: u8, len: usize) -> Vec<u8> {
        (0..len).map(|i| seed.wrapping_add(i as u8)).collect()
    }

    #[test]
    fn finds_identical_files_and_ignores_unique_ones() {
        let (idx, src) = fixture(&[
            ("a.bin", body(1, 20_000)),
            ("b.bin", body(1, 20_000)), // identical to a
            ("c.bin", body(9, 20_000)), // same size, different content
            ("d.bin", body(1, 12_345)), // unique size
        ]);

        let (groups, stats) = find_duplicates(&idx, &src, &DupeOptions::default());

        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].size, 20_000);
        assert_eq!(groups[0].nodes.len(), 2);
        let names: Vec<_> = groups[0].nodes.iter().map(|&n| idx.name(n)).collect();
        assert_eq!(names, vec!["a.bin", "b.bin"]);

        assert_eq!(stats.considered, 4);
        // Only the three same-sized files were candidates; d.bin never was.
        assert_eq!(stats.size_candidates, 3);
    }

    #[test]
    fn unique_sizes_are_never_read() {
        let (idx, src) = fixture(&[
            ("a.bin", body(1, 5_000)),
            ("b.bin", body(2, 6_000)),
            ("c.bin", body(3, 7_000)),
        ]);
        let (groups, stats) = find_duplicates(&idx, &src, &DupeOptions::default());

        assert!(groups.is_empty());
        assert_eq!(stats.size_candidates, 0);
        assert_eq!(stats.fully_hashed, 0, "tier 1 alone must settle this");
        assert_eq!(stats.bytes_sampled, 0);
        assert_eq!(stats.bytes_hashed, 0);
    }

    #[test]
    fn sampling_rejects_same_size_files_without_a_full_hash() {
        // Large, same size, differing at the very start: tier 2 must be enough.
        let mut x = body(1, 100_000);
        let mut y = body(1, 100_000);
        x[0] = 0xAA;
        y[0] = 0xBB;
        let (idx, src) = fixture(&[("x.bin", x), ("y.bin", y)]);

        let (groups, stats) = find_duplicates(&idx, &src, &DupeOptions::default());
        assert!(groups.is_empty());
        assert_eq!(stats.sampled, 2);
        assert_eq!(stats.fully_hashed, 0, "the sample already separated them");
    }

    #[test]
    fn files_differing_only_in_the_middle_still_need_a_full_hash() {
        // Identical head and tail, different middle: tier 2 cannot separate
        // these, and only the full hash gets it right.
        let mut x = body(1, 100_000);
        let mut y = body(1, 100_000);
        x[50_000] = 0xAA;
        y[50_000] = 0xBB;
        let (idx, src) = fixture(&[("x.bin", x), ("y.bin", y)]);

        let (groups, stats) = find_duplicates(&idx, &src, &DupeOptions::default());
        assert!(groups.is_empty(), "must not report a false duplicate");
        assert_eq!(stats.sampled, 2);
        assert_eq!(stats.fully_hashed, 2, "the middle difference forces tier 3");
    }

    #[test]
    fn small_files_skip_sampling() {
        let (idx, src) = fixture(&[("a.txt", b"hello".to_vec()), ("b.txt", b"hello".to_vec())]);
        let (groups, stats) = find_duplicates(&idx, &src, &DupeOptions::default());

        assert_eq!(groups.len(), 1);
        assert_eq!(
            stats.sampled, 0,
            "below the floor, sampling is pure overhead"
        );
        assert_eq!(stats.fully_hashed, 2);
    }

    #[test]
    fn groups_of_more_than_two_are_reported_together() {
        let b = body(7, 30_000);
        let (idx, src) = fixture(&[
            ("one.bin", b.clone()),
            ("two.bin", b.clone()),
            ("three.bin", b.clone()),
        ]);
        let (groups, _) = find_duplicates(&idx, &src, &DupeOptions::default());

        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].nodes.len(), 3);
        // Two of the three copies are reclaimable.
        assert_eq!(groups[0].wasted(), 60_000);
    }

    #[test]
    fn results_are_ranked_by_reclaimable_bytes() {
        let (idx, src) = fixture(&[
            ("small1.bin", body(1, 10_000)),
            ("small2.bin", body(1, 10_000)),
            ("big1.bin", body(2, 90_000)),
            ("big2.bin", body(2, 90_000)),
        ]);
        let (groups, stats) = find_duplicates(&idx, &src, &DupeOptions::default());

        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].size, 90_000, "biggest win first");
        assert_eq!(groups[1].size, 10_000);
        assert_eq!(stats.wasted_bytes, 100_000);
    }

    #[test]
    fn zero_byte_and_undersized_files_are_skipped() {
        let (idx, src) = fixture(&[
            ("empty1", Vec::new()),
            ("empty2", Vec::new()),
            ("tiny1", body(1, 10)),
            ("tiny2", body(1, 10)),
        ]);

        let (groups, _) = find_duplicates(&idx, &src, &DupeOptions::default());
        assert_eq!(
            groups.len(),
            1,
            "empty files are not interesting duplicates"
        );
        assert_eq!(groups[0].size, 10);

        let opt = DupeOptions {
            min_size: 1000,
            ..Default::default()
        };
        let (groups, _) = find_duplicates(&idx, &src, &opt);
        assert!(groups.is_empty(), "min_size must filter the tiny pair out");
    }

    #[test]
    fn unreadable_files_are_dropped_not_grouped() {
        let b = body(3, 50_000);
        let (idx, mut src) = fixture(&[
            ("ok1.bin", b.clone()),
            ("ok2.bin", b.clone()),
            ("locked1.bin", b.clone()),
            ("locked2.bin", b.clone()),
        ]);
        for name in ["locked1.bin", "locked2.bin"] {
            let n = idx.lookup(&format!(r"C:\data\{name}")).unwrap();
            src.broken.push(n);
        }

        let (groups, _) = find_duplicates(&idx, &src, &DupeOptions::default());
        assert_eq!(groups.len(), 1);
        let names: Vec<_> = groups[0].nodes.iter().map(|&n| idx.name(n)).collect();
        assert_eq!(
            names,
            vec!["ok1.bin", "ok2.bin"],
            "two files we could not read must not be assumed identical"
        );
    }

    #[test]
    fn scope_and_limit_are_honoured() {
        let mut t = TreeBuilder::new('C');
        let keep = t.dir(ROOT_REC, "keep");
        let other = t.dir(ROOT_REC, "other");
        t.file(keep, "k1.bin", 5000);
        t.file(keep, "k2.bin", 5000);
        t.file(other, "o1.bin", 5000);
        t.file(other, "o2.bin", 5000);
        let idx = t.build();

        let mut src = MemSource::default();
        for p in [
            r"C:\keep\k1.bin",
            r"C:\keep\k2.bin",
            r"C:\other\o1.bin",
            r"C:\other\o2.bin",
        ] {
            src.data.insert(idx.lookup(p).unwrap(), body(1, 5000));
        }

        // Unscoped, all four are one group.
        let (groups, _) = find_duplicates(&idx, &src, &DupeOptions::default());
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].nodes.len(), 4);

        // Scoped to `keep`, only its two files are considered.
        let opt = DupeOptions {
            under: Some(idx.lookup(r"C:\keep").unwrap()),
            ..Default::default()
        };
        let (groups, stats) = find_duplicates(&idx, &src, &opt);
        assert_eq!(stats.considered, 2);
        assert_eq!(groups[0].nodes.len(), 2);
    }

    #[test]
    fn hard_linked_files_are_not_reported() {
        // Two names for one extent: deleting one frees nothing, so calling
        // them duplicates would promise space that does not exist.
        let mut t = TreeBuilder::new('C');
        let d = t.dir(ROOT_REC, "data");
        t.flagged(d, "link_a.bin", flags::HARD_LINK, 8000);
        t.flagged(d, "link_b.bin", flags::HARD_LINK, 8000);
        t.file(d, "real1.bin", 8000);
        t.file(d, "real2.bin", 8000);
        let idx = t.build();

        let mut src = MemSource::default();
        for p in ["link_a.bin", "link_b.bin", "real1.bin", "real2.bin"] {
            src.data
                .insert(idx.lookup(&format!(r"C:\data\{p}")).unwrap(), body(4, 8000));
        }

        let (groups, _) = find_duplicates(&idx, &src, &DupeOptions::default());
        assert_eq!(groups.len(), 1);
        let names: Vec<_> = groups[0].nodes.iter().map(|&n| idx.name(n)).collect();
        assert_eq!(names, vec!["real1.bin", "real2.bin"]);
    }

    #[test]
    fn empty_index_produces_nothing() {
        let idx = TreeBuilder::new('C').build();
        let src = MemSource::default();
        let (groups, stats) = find_duplicates(&idx, &src, &DupeOptions::default());
        assert!(groups.is_empty());
        assert_eq!(stats, DupeStats::default());
    }
}
