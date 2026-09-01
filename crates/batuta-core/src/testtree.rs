//! Builds an [`Index`](crate::Index) from a compact tree description.
//!
//! The index logic (parent linking, depth, rollup, pruning, search) is
//! independent of where the rows came from, so it can be exercised against
//! hand-written trees instead of a raw volume.

use batuta_ntfs::mft::eflags;
use batuta_ntfs::EntryBatch;

use crate::index::{Index, IndexBuilder};

/// The MFT record number NTFS reserves for a volume root.
pub const ROOT_REC: u64 = 5;

/// Describes one volume's tree.
pub struct TreeBuilder {
    drive: char,
    batch: EntryBatch,
    next_rec: u64,
}

impl TreeBuilder {
    /// Start a volume. The root directory is created automatically.
    pub fn new(drive: char) -> Self {
        let mut t = TreeBuilder {
            drive,
            batch: EntryBatch::default(),
            next_rec: 16,
        };
        // NTFS stores the root's name as "."; the builder strips it.
        t.raw(ROOT_REC, ROOT_REC, ".", eflags::DIRECTORY, 0, 0);
        t
    }

    fn raw(&mut self, rec: u64, parent: u64, name: &str, flags: u16, size: u64, mtime: i64) {
        let b = &mut self.batch;
        let off = b.names.len() as u32;
        b.names.extend_from_slice(name.as_bytes());
        b.record_no.push(rec);
        b.sequence.push(1);
        b.parent.push(parent);
        b.name_off.push(off);
        b.name_len.push(name.len() as u16);
        b.size.push(size);
        b.allocated.push(size);
        b.mtime.push(mtime);
        b.flags.push(flags);
    }

    /// Add a directory under `parent`, returning its record number.
    pub fn dir(&mut self, parent: u64, name: &str) -> u64 {
        let rec = self.next_rec;
        self.next_rec += 1;
        self.raw(rec, parent, name, eflags::DIRECTORY, 0, 0);
        rec
    }

    /// Add a file under `parent`, returning its record number.
    pub fn file(&mut self, parent: u64, name: &str, size: u64) -> u64 {
        self.file_at(parent, name, size, 0)
    }

    /// Add a file with an explicit FILETIME modification stamp.
    pub fn file_at(&mut self, parent: u64, name: &str, size: u64, mtime: i64) -> u64 {
        let rec = self.next_rec;
        self.next_rec += 1;
        self.raw(rec, parent, name, 0, size, mtime);
        rec
    }

    /// Add a node with arbitrary flags, e.g. a reparse point.
    pub fn flagged(&mut self, parent: u64, name: &str, flags: u16, size: u64) -> u64 {
        let rec = self.next_rec;
        self.next_rec += 1;
        self.raw(rec, parent, name, flags, size, 0);
        rec
    }

    /// Add a file whose parent record does not exist.
    pub fn orphan(&mut self, name: &str, size: u64) -> u64 {
        let rec = self.next_rec;
        self.next_rec += 1;
        self.raw(rec, 999_999, name, 0, size, 0);
        rec
    }

    pub fn max_records(&self) -> u64 {
        self.next_rec + 1
    }

    /// Build a single-volume index.
    pub fn build(self) -> Index {
        let mut b = IndexBuilder::new();
        let max = self.max_records();
        b.add_volume(self.drive, 0xAAAA, 0, &self.batch, max);
        b.build()
    }
}

/// Build an index spanning several volumes.
pub fn build_multi(trees: Vec<TreeBuilder>) -> Index {
    let mut b = IndexBuilder::new();
    for t in trees {
        let max = t.max_records();
        b.add_volume(t.drive, 0xAAAA, 0, &t.batch, max);
    }
    b.build()
}
