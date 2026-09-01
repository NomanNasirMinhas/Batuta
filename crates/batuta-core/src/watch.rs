//! Applying filesystem changes to a live index.
//!
//! This is what makes folder sizes real-time. Every change costs O(depth) —
//! roughly 8 to 15 array updates walking to the volume root — rather than any
//! kind of re-walk.
//!
//! ## Keeping the name arena valid
//!
//! Search maps a match offset back to a node by binary searching `name_off`,
//! which requires that array to stay sorted. Mutation has to respect that:
//!
//! - **Creates** append, both a node and its name, so the largest offset goes
//!   to the largest node id and the order still holds.
//! - **Deletes** tombstone the node instead of removing it, since removing one
//!   would renumber everything after it.
//! - **Renames** cannot rewrite a name in place — a longer name would not fit,
//!   and repointing `name_off` would break the ordering. The new name goes in
//!   a small overflow arena and the node is flagged; the stale bytes in the
//!   main arena are ignored until the next full scan compacts them away.
//!
//! Changes are expressed as plain data so the whole layer is testable without
//! a volume, a journal, or elevation.

use rustc_hash::FxHashMap;

use crate::index::{flags, Index, NO_NODE};

/// One filesystem change, already resolved to a volume and MFT record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Created {
        volume: usize,
        record: u64,
        sequence: u16,
        parent_record: u64,
        name: String,
        is_dir: bool,
        size: u64,
        mtime: u32,
    },
    Deleted {
        volume: usize,
        record: u64,
        sequence: u16,
    },
    /// The file's data changed size.
    Resized {
        volume: usize,
        record: u64,
        sequence: u16,
        size: u64,
        mtime: u32,
    },
    /// The file was renamed, moved, or both.
    Renamed {
        volume: usize,
        record: u64,
        sequence: u16,
        parent_record: u64,
        name: String,
    },
}

impl Change {
    pub fn volume(&self) -> usize {
        match self {
            Change::Created { volume, .. }
            | Change::Deleted { volume, .. }
            | Change::Resized { volume, .. }
            | Change::Renamed { volume, .. } => *volume,
        }
    }

    pub fn record(&self) -> u64 {
        match self {
            Change::Created { record, .. }
            | Change::Deleted { record, .. }
            | Change::Resized { record, .. }
            | Change::Renamed { record, .. } => *record,
        }
    }
}

/// What applying a change actually did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applied {
    Created(u32),
    Deleted(u32),
    Resized(u32),
    Moved(u32),
    Renamed(u32),
    /// The change referred to something we do not have indexed.
    Unknown,
    /// The change was for an excluded subtree, or otherwise not our business.
    Ignored,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WatchStats {
    pub created: u64,
    pub deleted: u64,
    pub resized: u64,
    pub moved: u64,
    pub renamed: u64,
    pub unknown: u64,
    pub ignored: u64,
}

impl WatchStats {
    pub fn record(&mut self, a: Applied) {
        match a {
            Applied::Created(_) => self.created += 1,
            Applied::Deleted(_) => self.deleted += 1,
            Applied::Resized(_) => self.resized += 1,
            Applied::Moved(_) => self.moved += 1,
            Applied::Renamed(_) => self.renamed += 1,
            Applied::Unknown => self.unknown += 1,
            Applied::Ignored => self.ignored += 1,
        }
    }

    pub fn total(&self) -> u64 {
        self.created + self.deleted + self.resized + self.moved + self.renamed
    }
}

/// Names replaced since the last full scan.
#[derive(Default, Debug, Clone)]
pub struct NameOverrides {
    arena: Vec<u8>,
    by_node: FxHashMap<u32, (u32, u16)>,
}

impl NameOverrides {
    pub fn len(&self) -> usize {
        self.by_node.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_node.is_empty()
    }

    pub fn get(&self, node: u32) -> Option<&str> {
        let &(off, len) = self.by_node.get(&node)?;
        std::str::from_utf8(self.arena.get(off as usize..off as usize + len as usize)?).ok()
    }

    pub fn set(&mut self, node: u32, name: &str) {
        let off = self.arena.len() as u32;
        let bytes = name.as_bytes();
        let len = bytes.len().min(u16::MAX as usize);
        self.arena.extend_from_slice(&bytes[..len]);
        self.by_node.insert(node, (off, len as u16));
    }

    pub fn nodes(&self) -> impl Iterator<Item = u32> + '_ {
        self.by_node.keys().copied()
    }
}

impl Index {
    /// Apply one change.
    pub fn apply(&mut self, change: &Change) -> Applied {
        self.bump();
        match change {
            Change::Created {
                volume,
                record,
                sequence,
                parent_record,
                name,
                is_dir,
                size,
                mtime,
            } => self.apply_create(
                *volume,
                *record,
                *sequence,
                *parent_record,
                name,
                *is_dir,
                *size,
                *mtime,
            ),
            Change::Deleted {
                volume,
                record,
                sequence,
            } => self.apply_delete(*volume, *record, *sequence),
            Change::Resized {
                volume,
                record,
                sequence,
                size,
                mtime,
            } => self.apply_resize(*volume, *record, *sequence, *size, *mtime),
            Change::Renamed {
                volume,
                record,
                sequence,
                parent_record,
                name,
            } => self.apply_rename(*volume, *record, *sequence, *parent_record, name),
        }
    }

    /// Apply a batch, returning what happened.
    pub fn apply_all<'a, I: IntoIterator<Item = &'a Change>>(&mut self, changes: I) -> WatchStats {
        let mut stats = WatchStats::default();
        for c in changes {
            stats.record(self.apply(c));
        }
        stats
    }

    #[inline]
    pub fn is_deleted(&self, n: u32) -> bool {
        self.flags[n as usize] & flags::DELETED != 0
    }

    /// A node's current name. Alias for [`Index::name`], which already
    /// honours renames; kept for readability at the call sites here.
    #[inline]
    pub fn live_name(&self, n: u32) -> &str {
        self.name(n)
    }

    pub fn overrides(&self) -> &NameOverrides {
        &self.overrides
    }

    #[allow(clippy::too_many_arguments)] // mirrors the Change::Created fields
    fn apply_create(
        &mut self,
        volume: usize,
        record: u64,
        sequence: u16,
        parent_record: u64,
        name: &str,
        is_dir: bool,
        size: u64,
        mtime: u32,
    ) -> Applied {
        // A create for a record we already hold *at the same sequence* is a
        // replay; treat it as a resize so re-reading the journal is
        // idempotent. A different sequence means NTFS recycled the record and
        // `resolve_record` has just retired whatever was there, so this really
        // is a new file.
        if let Some(existing) = self.resolve_record(volume, record, sequence) {
            if !self.is_deleted(existing) {
                return self.apply_resize(volume, record, sequence, size, mtime);
            }
        }
        let Some(parent) = self.node_of_record(volume, parent_record) else {
            return Applied::Unknown;
        };
        if self.is_excluded(parent) {
            return Applied::Ignored;
        }

        // Appending keeps `name_off` sorted, which search depends on.
        let node = self.parent.len() as u32;
        let (off, len) = self.names.push(name);
        self.parent.push(parent);
        self.name_off.push(off);
        self.name_len.push(len);
        self.flags.push(if is_dir { flags::DIRECTORY } else { 0 });
        self.size.push(if is_dir { 0 } else { size });
        self.alloc.push(if is_dir { 0 } else { size });
        self.mtime.push(mtime);
        self.sequence.push(sequence);
        self.subtree_files.push(0);
        let d = self.depth[parent as usize].saturating_add(1);
        self.depth.push(d);

        self.set_record(volume, record, node);

        if !is_dir {
            self.propagate(node, size as i64, size as i64, 1);
        }
        Applied::Created(node)
    }

    fn apply_delete(&mut self, volume: usize, record: u64, sequence: u16) -> Applied {
        let Some(node) = self.resolve_record(volume, record, sequence) else {
            return Applied::Unknown;
        };
        if self.is_deleted(node) {
            return Applied::Ignored;
        }
        if self.is_excluded(node) {
            self.flags[node as usize] |= flags::DELETED;
            return Applied::Ignored;
        }

        // NTFS empties a directory before removing it, and each child arrives
        // as its own record, so by now the subtree total is its own weight.
        let size = self.size[node as usize];
        let alloc = self.alloc[node as usize];
        let files = if self.is_dir(node) {
            self.subtree_files[node as usize]
        } else {
            1
        };
        self.propagate(node, -(size as i64), -(alloc as i64), -(files as i64));

        self.flags[node as usize] |= flags::DELETED;
        self.size[node as usize] = 0;
        self.alloc[node as usize] = 0;
        self.subtree_files[node as usize] = 0;
        self.clear_record(volume, record);
        Applied::Deleted(node)
    }

    fn apply_resize(
        &mut self,
        volume: usize,
        record: u64,
        sequence: u16,
        size: u64,
        mtime: u32,
    ) -> Applied {
        let Some(node) = self.resolve_record(volume, record, sequence) else {
            return Applied::Unknown;
        };
        if self.is_deleted(node) || self.is_dir(node) {
            return Applied::Ignored;
        }
        if self.is_excluded(node) {
            self.size[node as usize] = size;
            return Applied::Ignored;
        }

        let old = self.size[node as usize];
        let delta = size as i64 - old as i64;
        self.size[node as usize] = size;
        self.alloc[node as usize] = size;
        if mtime != 0 {
            self.mtime[node as usize] = mtime;
        }
        if delta != 0 {
            self.propagate(node, delta, delta, 0);
        }
        Applied::Resized(node)
    }

    fn apply_rename(
        &mut self,
        volume: usize,
        record: u64,
        sequence: u16,
        parent_record: u64,
        name: &str,
    ) -> Applied {
        let Some(node) = self.resolve_record(volume, record, sequence) else {
            return Applied::Unknown;
        };
        if self.is_deleted(node) {
            return Applied::Unknown;
        }
        let Some(new_parent) = self.node_of_record(volume, parent_record) else {
            return Applied::Unknown;
        };

        let old_parent = self.parent[node as usize];
        let moved = old_parent != new_parent;

        if moved {
            // Move the subtree's weight from one ancestor chain to the other.
            let size = self.size[node as usize] as i64;
            let alloc = self.alloc[node as usize] as i64;
            let files = if self.is_dir(node) {
                self.subtree_files[node as usize] as i64
            } else {
                1
            };
            self.propagate(node, -size, -alloc, -files);
            self.parent[node as usize] = new_parent;
            self.depth[node as usize] = self.depth[new_parent as usize].saturating_add(1);
            self.propagate(node, size, alloc, files);
        }

        if self.live_name(node) != name {
            // The name cannot be rewritten in place without breaking the
            // sorted arena, so it goes to the overflow side table.
            self.overrides.set(node, name);
            self.flags[node as usize] |= flags::NAME_OVERRIDDEN;
            return Applied::Renamed(node);
        }
        if moved {
            Applied::Moved(node)
        } else {
            Applied::Ignored
        }
    }

    /// Add deltas to every ancestor of `node`.
    fn propagate(&mut self, node: u32, size: i64, alloc: i64, files: i64) {
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

    fn set_record(&mut self, volume: usize, record: u64, node: u32) {
        if let Some(table) = self.rec_to_node.get_mut(volume) {
            let i = record as usize;
            if i >= table.len() {
                // The MFT reuses low record numbers, but it can also grow.
                // Cap the growth so a bogus record number cannot exhaust memory.
                if i > table.len() + 8_000_000 {
                    return;
                }
                table.resize(i + 1, NO_NODE);
            }
            table[i] = node;
        }
    }

    fn clear_record(&mut self, volume: usize, record: u64) {
        if let Some(table) = self.rec_to_node.get_mut(volume) {
            if let Some(slot) = table.get_mut(record as usize) {
                *slot = NO_NODE;
            }
        }
    }

    /// Number of tombstoned nodes, i.e. how much a rescan would reclaim.
    pub fn tombstones(&self) -> usize {
        self.flags
            .iter()
            .filter(|f| *f & flags::DELETED != 0)
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::{Query, Searcher};
    use crate::testtree::{TreeBuilder, ROOT_REC};

    const MB: u64 = 1024 * 1024;

    /// C:\Users\Hacker with a Downloads folder, plus the record numbers the
    /// tree builder assigned, so changes can name them.
    struct Fixture {
        idx: Index,
        users: u64,
        hacker: u64,
        downloads: u64,
        installer: u64,
    }

    fn fixture() -> Fixture {
        let mut t = TreeBuilder::new('C');
        let users = t.dir(ROOT_REC, "Users");
        let hacker = t.dir(users, "Hacker");
        let downloads = t.dir(hacker, "Downloads");
        t.file(hacker, "notes.txt", 100);
        let installer = t.file(downloads, "installer.exe", 50 * MB);
        Fixture {
            idx: t.build(),
            users,
            hacker,
            downloads,
            installer,
        }
    }

    fn node(idx: &Index, path: &str) -> u32 {
        idx.lookup(path)
            .unwrap_or_else(|| panic!("no such path: {path}"))
    }

    fn size_of(idx: &Index, path: &str) -> u64 {
        idx.size[node(idx, path) as usize]
    }

    #[test]
    fn growing_a_file_updates_every_ancestor() {
        let mut f = fixture();
        let root = f.idx.volumes[0].root;

        let before_root = f.idx.size[root as usize];
        let before_hacker = size_of(&f.idx, r"C:\Users\Hacker");

        let applied = f.idx.apply(&Change::Resized {
            volume: 0,
            record: f.installer,
            sequence: 1,
            size: 60 * MB,
            mtime: 1_788_000_000,
        });
        assert!(matches!(applied, Applied::Resized(_)));

        assert_eq!(size_of(&f.idx, r"C:\Users\Hacker\Downloads"), 60 * MB);
        assert_eq!(size_of(&f.idx, r"C:\Users\Hacker"), before_hacker + 10 * MB);
        assert_eq!(size_of(&f.idx, r"C:\Users"), before_hacker + 10 * MB);
        assert_eq!(f.idx.size[root as usize], before_root + 10 * MB);
    }

    #[test]
    fn shrinking_a_file_is_symmetric() {
        let mut f = fixture();
        let root = f.idx.volumes[0].root;
        let before = f.idx.size[root as usize];

        f.idx.apply(&Change::Resized {
            volume: 0,
            record: f.installer,
            sequence: 1,
            size: 60 * MB,
            mtime: 0,
        });
        f.idx.apply(&Change::Resized {
            volume: 0,
            record: f.installer,
            sequence: 1,
            size: 50 * MB,
            mtime: 0,
        });

        assert_eq!(
            f.idx.size[root as usize], before,
            "totals must return exactly"
        );
        assert_eq!(size_of(&f.idx, r"C:\Users\Hacker\Downloads"), 50 * MB);
    }

    #[test]
    fn creating_a_file_adds_it_to_search_and_to_sizes() {
        let mut f = fixture();
        let root = f.idx.volumes[0].root;
        let before = f.idx.size[root as usize];

        let applied = f.idx.apply(&Change::Created {
            volume: 0,
            record: 5000,
            sequence: 1,
            parent_record: f.downloads,
            name: "report.pdf".into(),
            is_dir: false,
            size: 3 * MB,
            mtime: 1_788_000_000,
        });
        let new_node = match applied {
            Applied::Created(n) => n,
            other => panic!("expected a create, got {other:?}"),
        };

        assert_eq!(
            f.idx.path(new_node),
            r"C:\Users\Hacker\Downloads\report.pdf"
        );
        assert_eq!(f.idx.size[root as usize], before + 3 * MB);
        assert_eq!(size_of(&f.idx, r"C:\Users\Hacker\Downloads"), 53 * MB);

        // And it is immediately findable.
        let mut s = Searcher::new();
        let hits = s.search(&f.idx, &Query::new("report"));
        assert_eq!(hits.nodes, vec![new_node]);
    }

    #[test]
    fn creates_keep_the_name_arena_sorted() {
        // Search maps a match offset to a node by binary searching `name_off`;
        // if a create broke that ordering, results would silently go wrong.
        let mut f = fixture();
        for i in 0..50 {
            f.idx.apply(&Change::Created {
                volume: 0,
                record: 6000 + i,
                sequence: 1,
                parent_record: f.downloads,
                name: format!("generated_{i:03}.dat"),
                is_dir: false,
                size: 1000,
                mtime: 0,
            });
        }
        assert!(
            f.idx.name_off.windows(2).all(|w| w[0] <= w[1]),
            "name offsets must stay sorted after appends"
        );

        let mut s = Searcher::new();
        assert_eq!(s.search(&f.idx, &Query::new("generated_")).total, 50);
        s.reset();
        // The pre-existing entries are still findable.
        assert_eq!(s.search(&f.idx, &Query::new("installer")).total, 1);
    }

    #[test]
    fn deleting_a_file_removes_its_bytes_and_hides_it() {
        let mut f = fixture();
        let root = f.idx.volumes[0].root;
        let before = f.idx.size[root as usize];
        let installer = node(&f.idx, r"C:\Users\Hacker\Downloads\installer.exe");

        let applied = f.idx.apply(&Change::Deleted {
            volume: 0,
            record: f.installer,
            sequence: 1,
        });
        assert!(matches!(applied, Applied::Deleted(_)));

        assert_eq!(f.idx.size[root as usize], before - 50 * MB);
        assert_eq!(size_of(&f.idx, r"C:\Users\Hacker\Downloads"), 0);
        assert!(f.idx.is_deleted(installer));

        let mut s = Searcher::new();
        assert_eq!(s.search(&f.idx, &Query::new("installer")).total, 0);
        assert_eq!(f.idx.tombstones(), 1);
    }

    #[test]
    fn deleting_is_idempotent() {
        let mut f = fixture();
        let root = f.idx.volumes[0].root;
        f.idx.apply(&Change::Deleted {
            volume: 0,
            record: f.installer,
            sequence: 1,
        });
        let after_first = f.idx.size[root as usize];

        // A replayed journal must not subtract the same bytes twice.
        let again = f.idx.apply(&Change::Deleted {
            volume: 0,
            record: f.installer,
            sequence: 1,
        });
        assert_eq!(again, Applied::Unknown);
        assert_eq!(f.idx.size[root as usize], after_first);
    }

    #[test]
    fn a_replayed_create_does_not_double_count() {
        let mut f = fixture();
        let root = f.idx.volumes[0].root;
        let before = f.idx.size[root as usize];

        let c = Change::Created {
            volume: 0,
            record: 7000,
            sequence: 1,
            parent_record: f.downloads,
            name: "once.bin".into(),
            is_dir: false,
            size: 2 * MB,
            mtime: 0,
        };
        f.idx.apply(&c);
        f.idx.apply(&c);

        assert_eq!(f.idx.size[root as usize], before + 2 * MB);
        let mut s = Searcher::new();
        assert_eq!(s.search(&f.idx, &Query::new("once.bin")).total, 1);
    }

    #[test]
    fn moving_a_file_transfers_weight_between_both_chains() {
        let mut f = fixture();
        let root = f.idx.volumes[0].root;
        let before_root = f.idx.size[root as usize];

        // installer.exe moves from Downloads up into Hacker.
        let applied = f.idx.apply(&Change::Renamed {
            volume: 0,
            record: f.installer,
            sequence: 1,
            parent_record: f.hacker,
            name: "installer.exe".into(),
        });
        assert!(matches!(applied, Applied::Moved(_)));

        assert_eq!(
            size_of(&f.idx, r"C:\Users\Hacker\Downloads"),
            0,
            "old parent loses it"
        );
        assert_eq!(size_of(&f.idx, r"C:\Users\Hacker"), 50 * MB + 100);
        assert_eq!(
            f.idx.size[root as usize], before_root,
            "the total is unchanged"
        );
        assert_eq!(
            f.idx.path(node(&f.idx, r"C:\Users\Hacker\installer.exe")),
            r"C:\Users\Hacker\installer.exe"
        );
    }

    #[test]
    fn moving_a_directory_carries_its_whole_subtree() {
        let mut f = fixture();
        // Downloads (50 MB inside) moves from Hacker up to Users.
        f.idx.apply(&Change::Renamed {
            volume: 0,
            record: f.downloads,
            sequence: 1,
            parent_record: f.users,
            name: "Downloads".into(),
        });

        assert_eq!(
            size_of(&f.idx, r"C:\Users\Hacker"),
            100,
            "only notes.txt remains"
        );
        assert_eq!(size_of(&f.idx, r"C:\Users"), 50 * MB + 100);
        // The child moved with its parent.
        assert_eq!(
            f.idx
                .path(node(&f.idx, r"C:\Users\Downloads\installer.exe")),
            r"C:\Users\Downloads\installer.exe"
        );
    }

    #[test]
    fn renaming_changes_the_name_without_breaking_search() {
        let mut f = fixture();
        let installer = node(&f.idx, r"C:\Users\Hacker\Downloads\installer.exe");

        let applied = f.idx.apply(&Change::Renamed {
            volume: 0,
            record: f.installer,
            sequence: 1,
            parent_record: f.downloads,
            name: "setup-v2.exe".into(),
        });
        assert!(matches!(applied, Applied::Renamed(_)));

        assert_eq!(f.idx.live_name(installer), "setup-v2.exe");
        assert_eq!(
            f.idx.path(installer),
            r"C:\Users\Hacker\Downloads\setup-v2.exe"
        );
        // The arena is still sorted; the stale bytes are simply ignored.
        assert!(f.idx.name_off.windows(2).all(|w| w[0] <= w[1]));

        let mut s = Searcher::new();
        assert_eq!(
            s.search(&f.idx, &Query::new("setup-v2")).total,
            1,
            "new name is findable"
        );
        s.reset();
        assert_eq!(
            s.search(&f.idx, &Query::new("installer")).total,
            0,
            "the old name must not still match"
        );
    }

    #[test]
    fn renaming_a_directory_keeps_its_children_attached() {
        let mut f = fixture();
        f.idx.apply(&Change::Renamed {
            volume: 0,
            record: f.downloads,
            sequence: 1,
            parent_record: f.hacker,
            name: "Installers".into(),
        });

        // A rename must not orphan the subtree: sizes and paths both follow.
        assert_eq!(size_of(&f.idx, r"C:\Users\Hacker\Installers"), 50 * MB);
        let child = node(&f.idx, r"C:\Users\Hacker\Installers\installer.exe");
        assert_eq!(
            f.idx.path(child),
            r"C:\Users\Hacker\Installers\installer.exe"
        );
    }

    #[test]
    fn a_rename_and_a_move_together_are_handled_as_one() {
        let mut f = fixture();
        f.idx.apply(&Change::Renamed {
            volume: 0,
            record: f.installer,
            sequence: 1,
            parent_record: f.hacker,
            name: "moved-and-renamed.exe".into(),
        });

        assert_eq!(size_of(&f.idx, r"C:\Users\Hacker\Downloads"), 0);
        assert_eq!(size_of(&f.idx, r"C:\Users\Hacker"), 50 * MB + 100);
        assert_eq!(
            f.idx
                .path(node(&f.idx, r"C:\Users\Hacker\moved-and-renamed.exe")),
            r"C:\Users\Hacker\moved-and-renamed.exe"
        );
    }

    #[test]
    fn a_recycled_record_retires_the_stale_node_instead_of_swallowing_the_create() {
        // The failure this guards against, seen in the wild: a large tree was
        // deleted while nothing was watching, so its nodes stayed mapped to
        // their MFT records. NTFS then recycled those records for new files,
        // and every such create looked like a duplicate — the new file never
        // appeared and the deleted tree never went away.
        let mut f = fixture();
        let root = f.idx.volumes[0].root;
        let before = f.idx.size[root as usize];

        let stale = node(&f.idx, r"C:\Users\Hacker\Downloads\installer.exe");
        assert_eq!(
            f.idx.sequence[stale as usize], 1,
            "fixtures start at sequence 1"
        );

        // A new file lands on that same record, with the sequence bumped.
        let applied = f.idx.apply(&Change::Created {
            volume: 0,
            record: f.installer,
            sequence: 2,
            parent_record: f.hacker,
            name: "totally-different.txt".into(),
            is_dir: false,
            size: 4096,
            mtime: 0,
        });

        let fresh = match applied {
            Applied::Created(n) => n,
            other => panic!("the create must not be swallowed, got {other:?}"),
        };
        assert_ne!(
            fresh, stale,
            "a recycled record must not reuse the old node"
        );

        // The stale entry is gone from results and from the totals.
        assert!(f.idx.is_deleted(stale), "the stale node must be retired");
        assert_eq!(
            f.idx.size[root as usize],
            before - 50 * MB + 4096,
            "the deleted file's bytes must come off, the new file's go on"
        );
        assert_eq!(f.idx.path(fresh), r"C:\Users\Hacker\totally-different.txt");

        let mut s = Searcher::new();
        assert_eq!(s.search(&f.idx, &Query::new("installer")).total, 0);
        s.reset();
        assert_eq!(s.search(&f.idx, &Query::new("totally-different")).total, 1);
    }

    #[test]
    fn a_matching_sequence_is_still_treated_as_a_replay() {
        // Re-reading the journal must stay idempotent: the same record at the
        // same sequence is the same file, not a recycled one.
        let mut f = fixture();
        let root = f.idx.volumes[0].root;
        let before = f.idx.size[root as usize];
        let installer = node(&f.idx, r"C:\Users\Hacker\Downloads\installer.exe");

        let applied = f.idx.apply(&Change::Created {
            volume: 0,
            record: f.installer,
            sequence: 1,
            parent_record: f.downloads,
            name: "installer.exe".into(),
            is_dir: false,
            size: 50 * MB,
            mtime: 0,
        });
        assert!(matches!(applied, Applied::Resized(_)), "got {applied:?}");
        assert!(!f.idx.is_deleted(installer));
        assert_eq!(f.idx.size[root as usize], before, "totals must not move");
    }

    #[test]
    fn a_stale_mapping_is_retired_on_delete_and_resize_too() {
        // Not just creates: any change arriving for a recycled record must
        // clear the old node rather than act on it.
        let mut f = fixture();
        let stale = node(&f.idx, r"C:\Users\Hacker\Downloads\installer.exe");

        let applied = f.idx.apply(&Change::Resized {
            volume: 0,
            record: f.installer,
            sequence: 7,
            size: 123,
            mtime: 0,
        });
        assert_eq!(
            applied,
            Applied::Unknown,
            "must not resize a recycled record"
        );
        assert!(f.idx.is_deleted(stale), "and must retire what was there");

        // A delete for an already-retired record is simply unknown.
        let mut f = fixture();
        f.idx.apply(&Change::Deleted {
            volume: 0,
            record: f.installer,
            sequence: 9,
        });
        assert!(f
            .idx
            .is_deleted(node(&f.idx, r"C:\Users\Hacker\Downloads\installer.exe")));
    }

    #[test]
    fn changes_to_unknown_records_are_reported_not_applied() {
        let mut f = fixture();
        let root = f.idx.volumes[0].root;
        let before = f.idx.size[root as usize];

        assert_eq!(
            f.idx.apply(&Change::Resized {
                volume: 0,
                record: 999_999,
                sequence: 1,
                size: 1,
                mtime: 0
            }),
            Applied::Unknown
        );
        assert_eq!(
            f.idx.apply(&Change::Deleted {
                volume: 0,
                record: 999_999,
                sequence: 1
            }),
            Applied::Unknown
        );
        // A create whose parent we do not know cannot be placed anywhere.
        assert_eq!(
            f.idx.apply(&Change::Created {
                volume: 0,
                record: 8000,
                sequence: 1,
                parent_record: 999_999,
                name: "orphan.txt".into(),
                is_dir: false,
                size: 10,
                mtime: 0,
            }),
            Applied::Unknown
        );
        assert_eq!(f.idx.size[root as usize], before);
    }

    #[test]
    fn changes_in_excluded_subtrees_are_ignored() {
        let mut t = TreeBuilder::new('C');
        let windows = t.dir(ROOT_REC, "Windows");
        let sys = t.file(windows, "kernel32.dll", MB);
        let users = t.dir(ROOT_REC, "Users");
        t.file(users, "notes.txt", 100);
        let mut idx = t.build();

        let win_node = idx.lookup(r"C:\Windows").unwrap();
        idx.exclude_subtree(win_node);
        let root = idx.volumes[0].root;
        let before = idx.size[root as usize];

        // A write inside an excluded subtree must not move the totals.
        let applied = idx.apply(&Change::Resized {
            volume: 0,
            record: sys,
            sequence: 1,
            size: 5 * MB,
            mtime: 0,
        });
        assert_eq!(applied, Applied::Ignored);
        assert_eq!(idx.size[root as usize], before);

        let applied = idx.apply(&Change::Created {
            volume: 0,
            record: 9100,
            sequence: 1,
            parent_record: windows,
            name: "new-system-file.dll".into(),
            is_dir: false,
            size: 9 * MB,
            mtime: 0,
        });
        assert_eq!(applied, Applied::Ignored);
        assert_eq!(idx.size[root as usize], before);
    }

    #[test]
    fn a_created_directory_can_receive_children() {
        let mut f = fixture();
        let applied = f.idx.apply(&Change::Created {
            volume: 0,
            record: 9000,
            sequence: 1,
            parent_record: f.hacker,
            name: "Projects".into(),
            is_dir: true,
            size: 0,
            mtime: 0,
        });
        assert!(matches!(applied, Applied::Created(_)));

        f.idx.apply(&Change::Created {
            volume: 0,
            record: 9001,
            sequence: 1,
            parent_record: 9000,
            name: "main.rs".into(),
            is_dir: false,
            size: 4096,
            mtime: 0,
        });

        assert_eq!(size_of(&f.idx, r"C:\Users\Hacker\Projects"), 4096);
        assert_eq!(
            f.idx
                .path(node(&f.idx, r"C:\Users\Hacker\Projects\main.rs")),
            r"C:\Users\Hacker\Projects\main.rs"
        );
    }

    #[test]
    fn a_burst_of_changes_leaves_totals_consistent() {
        let mut f = fixture();
        let root = f.idx.volumes[0].root;
        let before = f.idx.size[root as usize];

        let mut changes = Vec::new();
        for i in 0..200u64 {
            changes.push(Change::Created {
                volume: 0,
                record: 20_000 + i,
                sequence: 1,
                parent_record: f.downloads,
                name: format!("chunk_{i}.tmp"),
                is_dir: false,
                size: 1000,
                mtime: 0,
            });
        }
        let stats = f.idx.apply_all(changes.iter());
        assert_eq!(stats.created, 200);
        assert_eq!(f.idx.size[root as usize], before + 200_000);

        // Now delete them all again.
        let deletes: Vec<_> = (0..200u64)
            .map(|i| Change::Deleted {
                volume: 0,
                record: 20_000 + i,
                sequence: 1,
            })
            .collect();
        let stats = f.idx.apply_all(deletes.iter());
        assert_eq!(stats.deleted, 200);
        assert_eq!(
            f.idx.size[root as usize], before,
            "totals must return exactly"
        );
    }

    #[test]
    fn tracked_sizes_match_a_rebuild_from_scratch() {
        // The strongest check available without a real volume: drive an index
        // through a sequence of changes, build a second index describing the
        // same end state, and require every directory total to agree.
        let mut f = fixture();

        f.idx.apply(&Change::Resized {
            volume: 0,
            record: f.installer,
            sequence: 1,
            size: 20 * MB,
            mtime: 0,
        });
        f.idx.apply(&Change::Created {
            volume: 0,
            record: 30_001,
            sequence: 1,
            parent_record: f.downloads,
            name: "extra.bin".into(),
            is_dir: false,
            size: 7 * MB,
            mtime: 0,
        });
        f.idx.apply(&Change::Created {
            volume: 0,
            record: 30_002,
            sequence: 1,
            parent_record: f.hacker,
            name: "Notes".into(),
            is_dir: true,
            size: 0,
            mtime: 0,
        });
        f.idx.apply(&Change::Created {
            volume: 0,
            record: 30_003,
            sequence: 1,
            parent_record: 30_002,
            name: "todo.md".into(),
            is_dir: false,
            size: 512,
            mtime: 0,
        });

        let mut t = TreeBuilder::new('C');
        let users = t.dir(ROOT_REC, "Users");
        let hacker = t.dir(users, "Hacker");
        let downloads = t.dir(hacker, "Downloads");
        t.file(hacker, "notes.txt", 100);
        t.file(downloads, "installer.exe", 20 * MB);
        t.file(downloads, "extra.bin", 7 * MB);
        let notes = t.dir(hacker, "Notes");
        t.file(notes, "todo.md", 512);
        let fresh = t.build();

        for path in [
            r"C:\Users",
            r"C:\Users\Hacker",
            r"C:\Users\Hacker\Downloads",
            r"C:\Users\Hacker\Notes",
        ] {
            assert_eq!(
                size_of(&f.idx, path),
                size_of(&fresh, path),
                "size mismatch at {path}"
            );
            assert_eq!(
                f.idx.subtree_files[node(&f.idx, path) as usize],
                fresh.subtree_files[node(&fresh, path) as usize],
                "file count mismatch at {path}"
            );
        }
        assert_eq!(
            f.idx.size[f.idx.volumes[0].root as usize],
            fresh.size[fresh.volumes[0].root as usize]
        );
    }
}
