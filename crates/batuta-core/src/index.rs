//! The in-memory index: a struct-of-arrays node table with rolled-up
//! directory sizes.
//!
//! Layout notes, since they are what keep this small enough to be resident:
//!
//! - Nodes are stored as parallel arrays rather than a `Vec<Node>`, so a scan
//!   that only touches sizes never pulls names into cache.
//! - Names live in one contiguous UTF-8 arena, in node order. Storing them in
//!   node order is what lets a bulk substring scan map a match offset back to
//!   a node with a single binary search (see `search`).
//! - MFT record number to node id is a directly-indexed `Vec<u32>`, not a hash
//!   map: NTFS record numbers are dense, so this is O(1) with no hashing and
//!   costs about 4 bytes per record instead of ~17 for a hash map.

use crate::names::NameArena;

/// Sentinel for "no such node".
pub const NO_NODE: u32 = u32::MAX;

/// A parent chain longer than this is treated as corrupt rather than followed.
const MAX_DEPTH: u16 = 512;

pub mod flags {
    pub use batuta_ntfs::mft::eflags::*;
    /// Set on nodes pruned by configuration (e.g. `C:\Windows`).
    pub const EXCLUDED: u16 = 1 << 11;
    /// Set on nodes whose parent could not be resolved.
    pub const ORPHAN: u16 = 1 << 12;
    /// Tombstoned by a delete. The node stays so ids remain stable.
    pub const DELETED: u16 = 1 << 13;
    /// Renamed since the scan; the live name is in the override table, and
    /// the bytes still in the arena are stale.
    pub const NAME_OVERRIDDEN: u16 = 1 << 14;
}

/// One indexed volume.
#[derive(Debug, Clone)]
pub struct VolumeInfo {
    pub drive: char,
    pub serial: u64,
    /// Node id of this volume's root directory.
    pub root: u32,
    /// First node id belonging to this volume.
    pub first_node: u32,
    pub node_count: u32,
    /// USN the snapshot was taken at, for resuming the change journal.
    pub next_usn: i64,
}

/// The complete index.
pub struct Index {
    pub volumes: Vec<VolumeInfo>,

    // Per-node parallel arrays.
    pub parent: Vec<u32>,
    pub name_off: Vec<u32>,
    pub name_len: Vec<u16>,
    pub flags: Vec<u16>,
    /// Files: logical size. Directories: rolled-up subtree total.
    pub size: Vec<u64>,
    /// Files: bytes on disk. Directories: rolled-up subtree total.
    pub alloc: Vec<u64>,
    /// Unix seconds; 0 when unknown.
    pub mtime: Vec<u32>,
    /// MFT sequence number, bumped by NTFS each time a record is recycled.
    ///
    /// Without this a record freed by a deletion we never saw stays mapped to
    /// the old node, and the next file to be given that record is mistaken for
    /// it — the create is swallowed and the deleted entry never goes away.
    pub sequence: Vec<u16>,
    /// Directories only: number of files anywhere beneath them.
    pub subtree_files: Vec<u32>,
    pub depth: Vec<u16>,

    pub names: NameArena,
    /// Names replaced by renames since the last full scan.
    pub(crate) overrides: crate::watch::NameOverrides,
    /// Bumped on every mutation. Cached query state that was derived from an
    /// older generation cannot be reused, because nodes may have appeared or
    /// changed name since.
    pub(crate) generation: u64,

    /// Per volume, MFT record number to node id. Parallel to `volumes`.
    pub(crate) rec_to_node: Vec<Vec<u32>>,
}

/// Summary only. A derived `Debug` would try to print every node, which for a
/// real volume means millions of entries.
impl std::fmt::Debug for Index {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Index")
            .field("nodes", &self.len())
            .field("names_bytes", &self.names.len())
            .field(
                "volumes",
                &self.volumes.iter().map(|v| v.drive).collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl Index {
    pub fn len(&self) -> usize {
        self.parent.len()
    }

    pub fn is_empty(&self) -> bool {
        self.parent.is_empty()
    }

    /// How many times this index has been mutated.
    #[inline]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    #[inline]
    pub(crate) fn bump(&mut self) {
        self.generation = self.generation.wrapping_add(1);
    }

    #[inline]
    pub fn is_dir(&self, n: u32) -> bool {
        self.flags[n as usize] & flags::DIRECTORY != 0
    }

    /// A volume root, which is its own parent.
    ///
    /// Roots are filtered out of results rather than being given an empty
    /// name, so the name arena stays strictly ordered.
    #[inline]
    pub fn is_root(&self, n: u32) -> bool {
        self.parent[n as usize] == n
    }

    #[inline]
    pub fn is_excluded(&self, n: u32) -> bool {
        self.flags[n as usize] & flags::EXCLUDED != 0
    }

    /// The node's current name, including any rename applied since the scan.
    ///
    /// Everything user-facing goes through here; [`Index::raw_name`] is only
    /// for the search engine's arena bookkeeping.
    #[inline]
    pub fn name(&self, n: u32) -> &str {
        if self.flags[n as usize] & flags::NAME_OVERRIDDEN != 0 {
            if let Some(s) = self.overrides.get(n) {
                return s;
            }
        }
        self.raw_name(n)
    }

    /// A node's current name as bytes, without UTF-8 validation.
    ///
    /// Used by sort comparators, where validating on every access would
    /// dominate the cost of ordering millions of rows.
    #[inline]
    pub fn name_bytes(&self, n: u32) -> &[u8] {
        if self.flags[n as usize] & flags::NAME_OVERRIDDEN != 0 {
            if let Some(s) = self.overrides.get(n) {
                return s.as_bytes();
            }
        }
        self.names
            .bytes(self.name_off[n as usize], self.name_len[n as usize])
    }

    /// The name as stored in the arena, ignoring renames.
    #[inline]
    pub fn raw_name(&self, n: u32) -> &str {
        self.names
            .get(self.name_off[n as usize], self.name_len[n as usize])
    }

    /// Which volume a node belongs to.
    pub fn volume_of(&self, n: u32) -> &VolumeInfo {
        self.volumes
            .iter()
            .find(|v| n >= v.first_node && n < v.first_node + v.node_count)
            .unwrap_or(&self.volumes[0])
    }

    /// The per-volume record tables, parallel to `volumes`.
    pub(crate) fn rec_tables(&self) -> &[Vec<u32>] {
        &self.rec_to_node
    }

    /// Record how far this volume's change journal has been consumed.
    ///
    /// Checkpoints persist this, so a restarting daemon resumes where it left
    /// off instead of replaying from the original scan. Without it the stored
    /// position ages until it falls out of the journal's retained window, at
    /// which point changes are lost with nothing to signal it.
    pub fn set_next_usn(&mut self, volume: usize, usn: i64) {
        if let Some(v) = self.volumes.get_mut(volume) {
            if usn > v.next_usn {
                v.next_usn = usn;
            }
        }
    }

    /// Look up a node by volume index and MFT record number.
    pub fn node_of_record(&self, vol: usize, record: u64) -> Option<u32> {
        let table = self.rec_to_node.get(vol)?;
        match table.get(record as usize).copied() {
            Some(NO_NODE) | None => None,
            Some(n) => Some(n),
        }
    }

    /// True when `node` is `ancestor` or lies beneath it.
    pub fn is_under(&self, node: u32, ancestor: u32) -> bool {
        let mut cur = node;
        for _ in 0..=MAX_DEPTH {
            if cur == ancestor {
                return true;
            }
            let p = self.parent[cur as usize];
            if p == cur || p == NO_NODE {
                return false;
            }
            cur = p;
        }
        false
    }

    /// Reconstruct a node's full path, e.g. `C:\Users\Hacker\notes.txt`.
    ///
    /// Done on demand for the handful of rows actually shown rather than
    /// stored per node, which is what keeps the arena to just base names.
    pub fn path(&self, n: u32) -> String {
        let mut parts: Vec<&str> = Vec::with_capacity(12);
        let mut cur = n;
        let mut root = cur;
        for _ in 0..=MAX_DEPTH {
            let p = self.parent[cur as usize];
            if p == cur || p == NO_NODE {
                root = cur;
                break;
            }
            parts.push(self.name(cur));
            root = cur;
            cur = p;
        }
        // `cur` is now the volume root; find which volume it belongs to.
        let drive = self.volume_of(root).drive;

        let mut out = String::with_capacity(64);
        out.push(drive);
        out.push(':');
        for part in parts.iter().rev() {
            out.push('\\');
            out.push_str(part);
        }
        if parts.is_empty() {
            out.push('\\');
        }
        out
    }

    /// Resolve a filesystem path to a node id.
    pub fn lookup(&self, path: &str) -> Option<u32> {
        let path = path.trim();
        let mut chars = path.chars();
        let drive = chars.next()?.to_ascii_uppercase();
        if chars.next()? != ':' {
            return None;
        }
        let vol = self.volumes.iter().find(|v| v.drive == drive)?;
        let mut cur = vol.root;

        let rest: String = chars.collect();
        for seg in rest.split(['\\', '/']).filter(|s| !s.is_empty()) {
            let mut found = None;
            // Children are contiguous in neither array, so this is a linear
            // probe over the volume. Callers resolve paths rarely (once per
            // query), so the simplicity is worth more than an index here.
            for (i, &p) in self.parent.iter().enumerate() {
                if p == cur && i as u32 != cur && self.name(i as u32).eq_ignore_ascii_case(seg) {
                    found = Some(i as u32);
                    break;
                }
            }
            cur = found?;
        }
        Some(cur)
    }

    /// Direct children of a directory.
    pub fn children(&self, dir: u32) -> Vec<u32> {
        let mut out = Vec::new();
        for (i, &p) in self.parent.iter().enumerate() {
            if p == dir && i as u32 != dir {
                out.push(i as u32);
            }
        }
        out
    }

    /// Remove a node's weight from its ancestors and tombstone it.
    pub(crate) fn retire(&mut self, node: u32) {
        if self.is_deleted(node) {
            return;
        }
        let i = node as usize;
        let (size, alloc) = (self.size[i], self.alloc[i]);
        let files = if self.is_dir(node) {
            self.subtree_files[i]
        } else {
            1
        };
        if !self.is_excluded(node) {
            self.propagate_full(node, -(size as i64), -(alloc as i64), -(files as i64));
        }
        self.flags[i] |= flags::DELETED;
        self.size[i] = 0;
        self.alloc[i] = 0;
        self.subtree_files[i] = 0;
    }

    /// Find the node holding an MFT record, checking the sequence number.
    ///
    /// NTFS recycles record numbers and bumps a sequence each time. A stored
    /// mapping whose sequence no longer matches belongs to a file that was
    /// deleted while nothing was watching: it is retired here, so the record
    /// is free for whatever now owns it. Without this the new file's create
    /// looks like a duplicate and is discarded.
    pub(crate) fn resolve_record(
        &mut self,
        volume: usize,
        record: u64,
        sequence: u16,
    ) -> Option<u32> {
        let node = self.node_of_record(volume, record)?;
        if self.sequence[node as usize] == sequence {
            return Some(node);
        }
        self.retire(node);
        self.clear_record_at(volume, record);
        None
    }

    pub(crate) fn clear_record_at(&mut self, volume: usize, record: u64) {
        if let Some(table) = self.rec_to_node.get_mut(volume) {
            if let Some(slot) = table.get_mut(record as usize) {
                *slot = NO_NODE;
            }
        }
    }

    /// Apply a size delta to a node and every ancestor above it.
    ///
    /// This is what keeps folder sizes live: a write anywhere costs O(depth),
    /// around 8 to 15 steps, rather than a re-walk of the tree.
    pub(crate) fn propagate_full(&mut self, node: u32, size: i64, alloc: i64, files: i64) {
        let mut cur = self.parent[node as usize];
        let mut guard = 0u32;
        loop {
            if cur == NO_NODE || guard > 512 {
                break;
            }
            let i = cur as usize;
            self.size[i] = self.size[i].saturating_add_signed(size);
            self.alloc[i] = self.alloc[i].saturating_add_signed(alloc);
            self.subtree_files[i] = self.subtree_files[i]
                .saturating_add_signed(files.clamp(i32::MIN as i64, i32::MAX as i64) as i32);
            let p = self.parent[i];
            if p == cur {
                break;
            }
            cur = p;
            guard += 1;
        }
    }

    pub fn propagate_delta(&mut self, node: u32, size_delta: i64, alloc_delta: i64) {
        let mut cur = self.parent[node as usize];
        let mut guard = 0;
        loop {
            if cur == NO_NODE || guard > MAX_DEPTH {
                break;
            }
            let i = cur as usize;
            self.size[i] = self.size[i].saturating_add_signed(size_delta);
            self.alloc[i] = self.alloc[i].saturating_add_signed(alloc_delta);
            let p = self.parent[i];
            if p == cur {
                break;
            }
            cur = p;
            guard += 1;
        }
    }
}

/// Accumulates volumes and produces an [`Index`].
#[derive(Default)]
pub struct IndexBuilder {
    volumes: Vec<VolumeInfo>,
    parent: Vec<u32>,
    name_off: Vec<u32>,
    name_len: Vec<u16>,
    flags: Vec<u16>,
    size: Vec<u64>,
    alloc: Vec<u64>,
    mtime: Vec<u32>,
    sequence: Vec<u16>,
    names: NameArena,
    rec_to_node: Vec<Vec<u32>>,
}

/// Windows FILETIME epoch (1601-01-01) to Unix epoch, in seconds.
const FILETIME_UNIX_DELTA: i64 = 11_644_473_600;

fn filetime_to_unix(ft: i64) -> u32 {
    if ft <= 0 {
        return 0;
    }
    let secs = ft / 10_000_000 - FILETIME_UNIX_DELTA;
    secs.clamp(0, u32::MAX as i64) as u32
}

impl IndexBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add one volume's parsed MFT.
    pub fn add_volume(
        &mut self,
        drive: char,
        serial: u64,
        next_usn: i64,
        batch: &batuta_ntfs::EntryBatch,
        max_records: u64,
    ) {
        let first_node = self.parent.len() as u32;

        // Record number to node id. Sized to the MFT's record capacity so the
        // lookup is a plain index rather than a hash.
        let cap = (max_records.max(
            batch
                .record_no
                .iter()
                .copied()
                .max()
                .map(|m| m + 1)
                .unwrap_or(1),
        )) as usize;
        let mut table = vec![NO_NODE; cap];

        for i in 0..batch.len() {
            let node = self.parent.len() as u32;
            let rec = batch.record_no[i] as usize;
            if rec < table.len() {
                table[rec] = node;
            }
            // Parent is filled in on the second pass, once every record has
            // a node id.
            self.parent.push(NO_NODE);
            let (off, len) = self.names.push(batch.name(i));
            self.name_off.push(off);
            self.name_len.push(len);
            self.flags.push(batch.flags[i]);
            self.size.push(batch.size[i]);
            self.alloc.push(batch.allocated[i]);
            self.mtime.push(filetime_to_unix(batch.mtime[i]));
            self.sequence.push(batch.sequence[i]);
        }

        // Second pass: resolve parent record numbers to node ids.
        let mut root = NO_NODE;
        for i in 0..batch.len() {
            let node = first_node + i as u32;
            let rec = batch.record_no[i];
            let parent_rec = batuta_ntfs::FileRef(batch.parent[i]).record();

            if rec == 5 {
                // The volume root is its own parent. Its stored name is ".",
                // which is left in the arena untouched: `name_off` must stay
                // strictly increasing because the search engine maps a match
                // offset back to a node by binary searching it. Roots are
                // recognised structurally instead, via `is_root`.
                self.parent[node as usize] = node;
                root = node;
                continue;
            }

            match table.get(parent_rec as usize).copied() {
                Some(p) if p != NO_NODE => self.parent[node as usize] = p,
                _ => {
                    // Parent record is missing or unused. Keep the node so its
                    // bytes still count, but attach it to the root.
                    self.parent[node as usize] = NO_NODE;
                    self.flags[node as usize] |= flags::ORPHAN;
                }
            }
        }

        // Orphans attach to the root so their size is still accounted for.
        if root != NO_NODE {
            for i in 0..batch.len() {
                let node = (first_node + i as u32) as usize;
                if self.parent[node] == NO_NODE {
                    self.parent[node] = root;
                }
            }
        }

        let node_count = self.parent.len() as u32 - first_node;
        self.volumes.push(VolumeInfo {
            drive,
            serial,
            root: if root == NO_NODE { first_node } else { root },
            first_node,
            node_count,
            next_usn,
        });
        self.rec_to_node.push(table);
    }

    pub fn build(self) -> Index {
        let n = self.parent.len();
        let mut idx = Index {
            volumes: self.volumes,
            parent: self.parent,
            name_off: self.name_off,
            name_len: self.name_len,
            flags: self.flags,
            size: self.size,
            alloc: self.alloc,
            mtime: self.mtime,
            sequence: self.sequence,
            subtree_files: vec![0; n],
            depth: vec![0; n],
            names: self.names,
            overrides: Default::default(),
            generation: 0,
            rec_to_node: self.rec_to_node,
        };
        idx.compute_depths();
        idx.rollup();
        idx
    }
}

impl Index {
    /// Compute each node's depth below its volume root.
    ///
    /// Iterative with an explicit cap: a corrupt MFT could otherwise present a
    /// parent cycle and hang the scan.
    fn compute_depths(&mut self) {
        let n = self.len();
        let mut depth = vec![u16::MAX; n];
        let mut chain: Vec<u32> = Vec::with_capacity(64);

        for start in 0..n {
            if depth[start] != u16::MAX {
                continue;
            }
            chain.clear();
            let mut cur = start as u32;
            // Assigned on every path out of the loop below.
            let known: u16;

            loop {
                let p = self.parent[cur as usize];
                if p == cur || p == NO_NODE {
                    known = 0; // reached a root
                    break;
                }
                if depth[cur as usize] != u16::MAX {
                    known = depth[cur as usize];
                    break;
                }
                if chain.len() as u16 >= MAX_DEPTH {
                    // Cycle or pathological nesting: stop and treat as root.
                    known = 0;
                    break;
                }
                chain.push(cur);
                cur = p;
            }

            if chain.is_empty() {
                depth[cur as usize] = known;
            } else {
                // Unwind, assigning increasing depths back down the chain.
                let mut d = known;
                depth[cur as usize] = d;
                for &node in chain.iter().rev() {
                    d = d.saturating_add(1);
                    depth[node as usize] = d;
                }
            }
        }

        for (i, d) in depth.iter().enumerate() {
            self.depth[i] = if *d == u16::MAX { 0 } else { *d };
        }
    }

    /// Accumulate file sizes into every ancestor directory.
    ///
    /// Nodes are visited deepest-first so each parent is only touched after
    /// all of its children are final: one linear pass, no recursion.
    fn rollup(&mut self) {
        let n = self.len();
        if n == 0 {
            return;
        }
        let max_depth = self.depth.iter().copied().max().unwrap_or(0) as usize;

        // Counting sort node ids by depth.
        let mut counts = vec![0u32; max_depth + 2];
        for &d in &self.depth {
            counts[d as usize + 1] += 1;
        }
        for i in 1..counts.len() {
            counts[i] += counts[i - 1];
        }
        let mut order = vec![0u32; n];
        let mut cursor = counts.clone();
        for i in 0..n {
            let d = self.depth[i] as usize;
            order[cursor[d] as usize] = i as u32;
            cursor[d] += 1;
        }

        // Directories start empty; only files contribute their own bytes.
        for i in 0..n {
            if self.flags[i] & flags::DIRECTORY != 0 {
                self.size[i] = 0;
                self.alloc[i] = 0;
            } else {
                self.subtree_files[i] = 1;
            }
        }

        // Walk deepest first, folding each node into its parent.
        for d in (1..=max_depth).rev() {
            let lo = counts[d] as usize;
            let hi = counts[d + 1] as usize;
            for &node in &order[lo..hi] {
                let i = node as usize;
                let p = self.parent[i] as usize;
                if p == i {
                    continue;
                }
                let (s, a, f) = (self.size[i], self.alloc[i], self.subtree_files[i]);
                self.size[p] = self.size[p].saturating_add(s);
                self.alloc[p] = self.alloc[p].saturating_add(a);
                self.subtree_files[p] = self.subtree_files[p].saturating_add(f);
            }
        }

        // A file counts itself only for its parents' totals, not its own.
        for i in 0..n {
            if self.flags[i] & flags::DIRECTORY == 0 {
                self.subtree_files[i] = 0;
            }
        }
    }

    /// Mark a subtree excluded, and remove its bytes from its ancestors.
    ///
    /// Exclusions are applied here rather than skipped during the scan,
    /// because the MFT is read in one sequential pass regardless: pruning
    /// afterwards means changing the configured roots costs nothing.
    pub fn exclude_subtree(&mut self, root: u32) {
        if self.is_excluded(root) {
            return;
        }
        self.bump();
        let (size, alloc, files) = (
            self.size[root as usize],
            self.alloc[root as usize],
            self.subtree_files[root as usize],
        );

        // Mark the whole subtree. Nodes are not ordered by ancestry, so this
        // walks all nodes once, testing ancestry by depth-bounded climb.
        let mut stack = vec![root];
        let children = self.children_map();
        while let Some(node) = stack.pop() {
            self.flags[node as usize] |= flags::EXCLUDED;
            if let Some(kids) = children.get(&node) {
                stack.extend_from_slice(kids);
            }
        }

        // Subtract the pruned bytes from every remaining ancestor.
        let mut cur = self.parent[root as usize];
        let mut guard = 0;
        while cur != NO_NODE && guard <= MAX_DEPTH {
            let i = cur as usize;
            self.size[i] = self.size[i].saturating_sub(size);
            self.alloc[i] = self.alloc[i].saturating_sub(alloc);
            self.subtree_files[i] = self.subtree_files[i].saturating_sub(files);
            if self.parent[i] == cur {
                break;
            }
            cur = self.parent[i];
            guard += 1;
        }
    }

    /// Build a parent to children map. Allocates, so callers that need it
    /// repeatedly should hold on to the result.
    pub fn children_map(&self) -> rustc_hash::FxHashMap<u32, Vec<u32>> {
        let mut map: rustc_hash::FxHashMap<u32, Vec<u32>> = rustc_hash::FxHashMap::default();
        for (i, &p) in self.parent.iter().enumerate() {
            if p != NO_NODE && p != i as u32 {
                map.entry(p).or_default().push(i as u32);
            }
        }
        map
    }

    /// Approximate resident size of the index, in bytes.
    pub fn memory_bytes(&self) -> usize {
        let n = self.len();
        n * (4 + 4 + 2 + 2 + 8 + 8 + 4 + 4 + 2)
            + self.names.len()
            + self.rec_to_node.iter().map(|t| t.len() * 4).sum::<usize>()
    }
}
