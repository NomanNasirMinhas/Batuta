//! Persisting the index so a restart does not require re-reading the MFT.
//!
//! The format is a fixed header followed by the node arrays written back to
//! back in their in-memory layout, so loading is a handful of bulk reads with
//! no per-element decoding.
//!
//! It is deliberately *not* memory-mapped. A mapped index would keep its pages
//! file-backed and evictable, which sounds attractive, but the daemon mutates
//! sizes on every filesystem change; those pages would fault to private copies
//! almost immediately and the benefit would evaporate. Reading into owned
//! vectors is simpler, avoids threading a lifetime through every type, and a
//! bulk read of ~140 MB from NVMe still lands well inside the restart budget.
//!
//! Snapshots store the index **before** configured exclusions are applied, so
//! changing `exclude` in the config costs a re-prune rather than a rescan.
//!
//! Everything here parses bytes that may be truncated by a crash or corrupted
//! on disk, and the daemon that will read them runs privileged, so the loader
//! validates rather than trusts: sizes are checked against the real file
//! length before allocating, and every index-like field is bounds-checked
//! before it can be used to subscript an array.

use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;

use xxhash_rust::xxh3::Xxh3;

use crate::index::{Index, VolumeInfo, NO_NODE};
use crate::names::NameArena;

const MAGIC: [u8; 8] = *b"BATUTA\x00\x01";
const FORMAT_VERSION: u32 = 2;
const ENDIAN_MARK: u32 = 0x0102_0304;
const HEADER_BYTES: u64 = 64;
const VOLUME_BYTES: u64 = 40;

/// Refuse to allocate for a header claiming more than this.
const MAX_NODES: u64 = 512_000_000;

#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    #[error("not a batuta snapshot")]
    BadMagic,

    #[error("snapshot format version {found}, expected {expected}; a rescan is needed")]
    Version { found: u32, expected: u32 },

    #[error("snapshot was written on a different byte order")]
    Endian,

    #[error("snapshot is corrupt: {0}")]
    Corrupt(&'static str),

    #[error("snapshot is truncated: expected {expected} bytes, file holds {actual}")]
    Truncated { expected: u64, actual: u64 },

    #[error("snapshot checksum mismatch; the file is damaged")]
    Checksum,

    #[error("io error: {0}")]
    Io(#[from] io::Error),
}

pub type Result<T> = std::result::Result<T, SnapshotError>;

/// Bytes one node occupies on disk, across all the parallel arrays.
const BYTES_PER_NODE: u64 = 4 + 4 + 2 + 2 + 8 + 8 + 4 + 4 + 2 + 2;

fn write_slice<T: bytemuck::Pod, W: Write>(w: &mut W, h: &mut Xxh3, v: &[T]) -> io::Result<()> {
    let bytes = bytemuck::cast_slice(v);
    h.update(bytes);
    w.write_all(bytes)
}

fn read_vec<T, R: Read>(r: &mut R, h: &mut Xxh3, n: usize) -> io::Result<Vec<T>>
where
    T: bytemuck::Pod + Default + Clone,
{
    let mut v = vec![T::default(); n];
    {
        let bytes = bytemuck::cast_slice_mut(&mut v);
        r.read_exact(bytes)?;
        h.update(bytes);
    }
    Ok(v)
}

impl Index {
    /// Write the index to `path`, atomically via a temporary file.
    ///
    /// Writing in place would leave a half-written snapshot behind if the
    /// process died mid-write, and that file would then be loaded on restart.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("tmp");

        {
            let file = File::create(&tmp)?;
            let mut w = BufWriter::with_capacity(1 << 20, file);
            let mut h = Xxh3::new();

            // Header is rewritten at the end, once the payload hash is known.
            w.write_all(&[0u8; HEADER_BYTES as usize])?;

            for (v, table) in self.volumes.iter().zip(self.rec_tables()) {
                let mut buf = [0u8; VOLUME_BYTES as usize];
                buf[0..4].copy_from_slice(&(v.drive as u32).to_le_bytes());
                buf[4..8].copy_from_slice(&v.root.to_le_bytes());
                buf[8..12].copy_from_slice(&v.first_node.to_le_bytes());
                buf[12..16].copy_from_slice(&v.node_count.to_le_bytes());
                buf[16..24].copy_from_slice(&v.serial.to_le_bytes());
                buf[24..32].copy_from_slice(&v.next_usn.to_le_bytes());
                buf[32..40].copy_from_slice(&(table.len() as u64).to_le_bytes());
                h.update(&buf);
                w.write_all(&buf)?;
            }

            // Renames since the scan live in a side table. Fold them back
            // into a fresh arena before writing, so a reloaded snapshot needs
            // no override state and the stale bytes are reclaimed. Rebuilding
            // in node order keeps `name_off` sorted, which search requires.
            let compacted = self.compact_names();
            let (name_off, name_len, names, flags) = match &compacted {
                Some((o, l, n, f)) => (o.as_slice(), l.as_slice(), n.as_slice(), f.as_slice()),
                None => (
                    self.name_off.as_slice(),
                    self.name_len.as_slice(),
                    self.names.as_bytes(),
                    self.flags.as_slice(),
                ),
            };

            write_slice(&mut w, &mut h, &self.parent)?;
            write_slice(&mut w, &mut h, name_off)?;
            write_slice(&mut w, &mut h, name_len)?;
            write_slice(&mut w, &mut h, flags)?;
            write_slice(&mut w, &mut h, &self.size)?;
            write_slice(&mut w, &mut h, &self.alloc)?;
            write_slice(&mut w, &mut h, &self.mtime)?;
            write_slice(&mut w, &mut h, &self.subtree_files)?;
            write_slice(&mut w, &mut h, &self.depth)?;
            write_slice(&mut w, &mut h, &self.sequence)?;
            write_slice(&mut w, &mut h, names)?;
            for table in self.rec_tables() {
                write_slice(&mut w, &mut h, table)?;
            }

            let payload_hash = h.digest();
            let mut file = w.into_inner().map_err(|e| e.into_error())?;
            file.seek(SeekFrom::Start(0))?;

            let mut header = [0u8; HEADER_BYTES as usize];
            header[0..8].copy_from_slice(&MAGIC);
            header[8..12].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
            header[12..16].copy_from_slice(&ENDIAN_MARK.to_le_bytes());
            header[16..24].copy_from_slice(&(self.len() as u64).to_le_bytes());
            header[24..32].copy_from_slice(&(names.len() as u64).to_le_bytes());
            header[32..36].copy_from_slice(&(self.volumes.len() as u32).to_le_bytes());
            header[40..48].copy_from_slice(&payload_hash.to_le_bytes());
            header[48..56].copy_from_slice(&now_unix().to_le_bytes());
            file.write_all(&header)?;
            file.sync_all()?;
        }

        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Load an index written by [`Index::save`].
    pub fn load(path: &Path) -> Result<Index> {
        let file = File::open(path)?;
        let file_len = file.metadata()?.len();
        // Too small to even hold a header: report it as "not a snapshot"
        // rather than letting the read fail with a bare end-of-file.
        if file_len < HEADER_BYTES {
            return Err(SnapshotError::BadMagic);
        }
        let mut r = BufReader::with_capacity(1 << 20, file);

        let mut header = [0u8; HEADER_BYTES as usize];
        r.read_exact(&mut header)?;
        if header[0..8] != MAGIC {
            return Err(SnapshotError::BadMagic);
        }
        let version = u32::from_le_bytes(header[8..12].try_into().unwrap());
        if version != FORMAT_VERSION {
            return Err(SnapshotError::Version {
                found: version,
                expected: FORMAT_VERSION,
            });
        }
        if u32::from_le_bytes(header[12..16].try_into().unwrap()) != ENDIAN_MARK {
            return Err(SnapshotError::Endian);
        }

        let node_count = u64::from_le_bytes(header[16..24].try_into().unwrap());
        let name_bytes = u64::from_le_bytes(header[24..32].try_into().unwrap());
        let volume_count = u32::from_le_bytes(header[32..36].try_into().unwrap()) as u64;
        let want_hash = u64::from_le_bytes(header[40..48].try_into().unwrap());

        if node_count > MAX_NODES {
            return Err(SnapshotError::Corrupt("implausible node count"));
        }
        if volume_count == 0 || volume_count > 64 {
            return Err(SnapshotError::Corrupt("implausible volume count"));
        }

        let mut h = Xxh3::new();

        // Volume table first: it carries the per-volume record table lengths
        // needed to size the rest of the file.
        let mut volumes = Vec::with_capacity(volume_count as usize);
        let mut rec_lens = Vec::with_capacity(volume_count as usize);
        for _ in 0..volume_count {
            let mut buf = [0u8; VOLUME_BYTES as usize];
            r.read_exact(&mut buf)?;
            h.update(&buf);
            let drive = char::from_u32(u32::from_le_bytes(buf[0..4].try_into().unwrap()))
                .ok_or(SnapshotError::Corrupt("bad drive letter"))?;
            let rec_len = u64::from_le_bytes(buf[32..40].try_into().unwrap());
            if rec_len > MAX_NODES {
                return Err(SnapshotError::Corrupt("implausible record table length"));
            }
            volumes.push(VolumeInfo {
                drive,
                root: u32::from_le_bytes(buf[4..8].try_into().unwrap()),
                first_node: u32::from_le_bytes(buf[8..12].try_into().unwrap()),
                node_count: u32::from_le_bytes(buf[12..16].try_into().unwrap()),
                serial: u64::from_le_bytes(buf[16..24].try_into().unwrap()),
                next_usn: i64::from_le_bytes(buf[24..32].try_into().unwrap()),
            });
            rec_lens.push(rec_len);
        }

        // Check the file is actually big enough before allocating for it.
        let expected = HEADER_BYTES
            + volume_count * VOLUME_BYTES
            + node_count * BYTES_PER_NODE
            + name_bytes
            + rec_lens.iter().sum::<u64>() * 4;
        if file_len < expected {
            return Err(SnapshotError::Truncated {
                expected,
                actual: file_len,
            });
        }

        let n = node_count as usize;
        let parent = read_vec::<u32, _>(&mut r, &mut h, n)?;
        let name_off = read_vec::<u32, _>(&mut r, &mut h, n)?;
        let name_len = read_vec::<u16, _>(&mut r, &mut h, n)?;
        let flags = read_vec::<u16, _>(&mut r, &mut h, n)?;
        let size = read_vec::<u64, _>(&mut r, &mut h, n)?;
        let alloc = read_vec::<u64, _>(&mut r, &mut h, n)?;
        let mtime = read_vec::<u32, _>(&mut r, &mut h, n)?;
        let subtree_files = read_vec::<u32, _>(&mut r, &mut h, n)?;
        let depth = read_vec::<u16, _>(&mut r, &mut h, n)?;
        let sequence = read_vec::<u16, _>(&mut r, &mut h, n)?;
        let names_raw = read_vec::<u8, _>(&mut r, &mut h, name_bytes as usize)?;

        let mut rec_to_node = Vec::with_capacity(volume_count as usize);
        for &len in &rec_lens {
            rec_to_node.push(read_vec::<u32, _>(&mut r, &mut h, len as usize)?);
        }

        if h.digest() != want_hash {
            return Err(SnapshotError::Checksum);
        }

        let idx = Index {
            volumes,
            parent,
            name_off,
            name_len,
            flags,
            size,
            alloc,
            mtime,
            sequence,
            subtree_files,
            depth,
            names: NameArena::from_bytes(names_raw),
            overrides: Default::default(),
            generation: 0,
            rec_to_node,
        };
        idx.validate()?;
        Ok(idx)
    }

    /// Reject a snapshot whose indices would subscript out of bounds.
    ///
    /// The checksum proves the bytes survived the round trip; it says nothing
    /// about whether they were meaningful. This is what stops a damaged or
    /// hand-edited file from turning into a panic inside a privileged process.
    fn validate(&self) -> Result<()> {
        let n = self.len();
        if n == 0 {
            return Err(SnapshotError::Corrupt("snapshot holds no nodes"));
        }
        for arr_len in [
            self.name_off.len(),
            self.name_len.len(),
            self.flags.len(),
            self.size.len(),
            self.alloc.len(),
            self.mtime.len(),
            self.subtree_files.len(),
            self.depth.len(),
            self.sequence.len(),
        ] {
            if arr_len != n {
                return Err(SnapshotError::Corrupt("node arrays disagree on length"));
            }
        }

        let names_len = self.names.len();
        for i in 0..n {
            let p = self.parent[i];
            if p != NO_NODE && p as usize >= n {
                return Err(SnapshotError::Corrupt(
                    "parent points outside the node table",
                ));
            }
            let end = self.name_off[i] as usize + self.name_len[i] as usize;
            if end > names_len {
                return Err(SnapshotError::Corrupt("name range runs past the arena"));
            }
        }
        // The search engine binary searches `name_off`, which requires order.
        if self.name_off.windows(2).any(|w| w[0] > w[1]) {
            return Err(SnapshotError::Corrupt("name offsets are not sorted"));
        }

        for v in &self.volumes {
            if v.root as usize >= n {
                return Err(SnapshotError::Corrupt("volume root is out of range"));
            }
            if v.first_node as usize > n || v.first_node as usize + v.node_count as usize > n {
                return Err(SnapshotError::Corrupt("volume node range is out of bounds"));
            }
        }
        Ok(())
    }
}

/// A rebuilt name arena: offsets, lengths, bytes, and the flags array with
/// `NAME_OVERRIDDEN` cleared.
type CompactedNames = (Vec<u32>, Vec<u16>, Vec<u8>, Vec<u16>);

impl Index {
    /// Rebuild the name arena from live names, or `None` if nothing was
    /// renamed and the existing arena is already correct.
    ///
    /// Returns the flags array too, with `NAME_OVERRIDDEN` cleared: once a
    /// name is baked into the arena the flag is not just redundant but wrong,
    /// because search treats a flagged node's arena bytes as stale and skips
    /// it. Leaving it set would make every renamed file invisible after a
    /// restart, since the override table it would look in is empty by then.
    fn compact_names(&self) -> Option<CompactedNames> {
        if self.overrides.is_empty() {
            return None;
        }
        let n = self.len();
        let mut off = Vec::with_capacity(n);
        let mut len = Vec::with_capacity(n);
        let mut arena = Vec::with_capacity(self.names.len());
        for i in 0..n {
            let name = self.name(i as u32);
            off.push(arena.len() as u32);
            let bytes = name.as_bytes();
            let take = bytes.len().min(u16::MAX as usize);
            arena.extend_from_slice(&bytes[..take]);
            len.push(take as u16);
        }
        let flags = self
            .flags
            .iter()
            .map(|f| f & !crate::index::flags::NAME_OVERRIDDEN)
            .collect();
        Some((off, len, arena, flags))
    }
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// What a snapshot says about itself, without reading its payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotInfo {
    pub written_at: u64,
    pub nodes: u64,
    /// Drive letters the snapshot covers, in stored order.
    pub drives: Vec<char>,
}

/// Read a snapshot's header and volume table, skipping the payload entirely.
///
/// Enough to decide whether an existing index still matches what is wanted,
/// which would otherwise mean loading hundreds of megabytes just to look at a
/// list of drive letters.
pub fn peek(path: &Path) -> Result<SnapshotInfo> {
    let file = File::open(path)?;
    if file.metadata()?.len() < HEADER_BYTES {
        return Err(SnapshotError::BadMagic);
    }
    let mut r = BufReader::new(file);

    let mut header = [0u8; HEADER_BYTES as usize];
    r.read_exact(&mut header)?;
    if header[0..8] != MAGIC {
        return Err(SnapshotError::BadMagic);
    }
    let version = u32::from_le_bytes(header[8..12].try_into().unwrap());
    if version != FORMAT_VERSION {
        return Err(SnapshotError::Version {
            found: version,
            expected: FORMAT_VERSION,
        });
    }

    let nodes = u64::from_le_bytes(header[16..24].try_into().unwrap());
    let volume_count = u32::from_le_bytes(header[32..36].try_into().unwrap()) as u64;
    let written_at = u64::from_le_bytes(header[48..56].try_into().unwrap());
    if volume_count == 0 || volume_count > 64 {
        return Err(SnapshotError::Corrupt("implausible volume count"));
    }

    let mut drives = Vec::with_capacity(volume_count as usize);
    for _ in 0..volume_count {
        let mut buf = [0u8; VOLUME_BYTES as usize];
        r.read_exact(&mut buf)?;
        let raw = u32::from_le_bytes(buf[0..4].try_into().unwrap());
        drives.push(char::from_u32(raw).ok_or(SnapshotError::Corrupt("bad drive letter"))?);
    }

    Ok(SnapshotInfo {
        written_at,
        nodes,
        drives,
    })
}

/// When a snapshot was written, in Unix seconds.
pub fn written_at(path: &Path) -> Option<u64> {
    let mut f = File::open(path).ok()?;
    let mut header = [0u8; HEADER_BYTES as usize];
    f.read_exact(&mut header).ok()?;
    if header[0..8] != MAGIC {
        return None;
    }
    Some(u64::from_le_bytes(header[48..56].try_into().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::{Query, Searcher};
    use crate::testtree::{build_multi, TreeBuilder, ROOT_REC};

    fn tmp(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("batuta-test-{}-{}.idx", name, std::process::id()));
        p
    }

    fn sample() -> Index {
        let mut c = TreeBuilder::new('C');
        let users = c.dir(ROOT_REC, "Users");
        let hacker = c.dir(users, "Hacker");
        c.file(hacker, "notes.txt", 100);
        c.file(hacker, "café_日本.md", 2048);
        let dl = c.dir(hacker, "Downloads");
        c.file(dl, "installer.exe", 50 * 1024 * 1024);

        let mut d = TreeBuilder::new('D');
        let proj = d.dir(ROOT_REC, "Projects");
        d.file(proj, "main.rs", 4096);

        build_multi(vec![c, d])
    }

    #[test]
    fn round_trips_every_field() {
        let path = tmp("roundtrip");
        let a = sample();
        a.save(&path).unwrap();
        let b = Index::load(&path).unwrap();

        assert_eq!(a.len(), b.len());
        assert_eq!(a.parent, b.parent);
        assert_eq!(a.name_off, b.name_off);
        assert_eq!(a.name_len, b.name_len);
        assert_eq!(a.flags, b.flags);
        assert_eq!(a.size, b.size);
        assert_eq!(a.alloc, b.alloc);
        assert_eq!(a.mtime, b.mtime);
        assert_eq!(a.subtree_files, b.subtree_files);
        assert_eq!(a.depth, b.depth);
        assert_eq!(a.names.as_bytes(), b.names.as_bytes());
        assert_eq!(a.volumes.len(), b.volumes.len());
        for (x, y) in a.volumes.iter().zip(&b.volumes) {
            assert_eq!(x.drive, y.drive);
            assert_eq!(x.serial, y.serial);
            assert_eq!(x.root, y.root);
            assert_eq!(x.first_node, y.first_node);
            assert_eq!(x.node_count, y.node_count);
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_reloaded_index_answers_queries_identically() {
        let path = tmp("queries");
        let a = sample();
        a.save(&path).unwrap();
        let b = Index::load(&path).unwrap();

        // Paths, sizes and lookups must all survive the trip.
        let n = b
            .lookup(r"C:\Users\Hacker\Downloads\installer.exe")
            .unwrap();
        assert_eq!(b.path(n), r"C:\Users\Hacker\Downloads\installer.exe");
        assert_eq!(b.size[n as usize], 50 * 1024 * 1024);
        assert_eq!(
            b.size[b.lookup(r"C:\Users\Hacker").unwrap() as usize],
            a.size[a.lookup(r"C:\Users\Hacker").unwrap() as usize]
        );

        // Including a Unicode name and the second volume.
        assert_eq!(
            b.lookup(r"D:\Projects\main.rs").map(|x| b.size[x as usize]),
            Some(4096)
        );

        let mut sa = Searcher::new();
        let mut sb = Searcher::new();
        for q in ["notes", "日本", "main", "installer"] {
            let ra = sa.search(&a, &Query::new(q));
            let rb = sb.search(&b, &Query::new(q));
            assert_eq!(ra.total, rb.total, "query {q:?} disagreed");
            let pa: Vec<_> = ra.nodes.iter().map(|&x| a.path(x)).collect();
            let pb: Vec<_> = rb.nodes.iter().map(|&x| b.path(x)).collect();
            assert_eq!(pa, pb);
            sa.reset();
            sb.reset();
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn record_lookup_survives_the_round_trip() {
        // The record table is what the USN watcher will use to find a node
        // from a change record, so it has to persist correctly.
        let path = tmp("rectable");
        let a = sample();
        let node = a.lookup(r"C:\Users\Hacker\notes.txt").unwrap();
        a.save(&path).unwrap();
        let b = Index::load(&path).unwrap();

        let mut found = None;
        for rec in 0..64u64 {
            if a.node_of_record(0, rec) == Some(node) {
                found = Some(rec);
                break;
            }
        }
        let rec = found.expect("node should be reachable by record number");
        assert_eq!(b.node_of_record(0, rec), Some(node));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn rejects_a_foreign_file() {
        let path = tmp("foreign");

        // Shorter than a header.
        std::fs::write(&path, b"nope").unwrap();
        assert!(matches!(Index::load(&path), Err(SnapshotError::BadMagic)));

        // Long enough to read a header, but not ours.
        std::fs::write(&path, vec![0x5Au8; 4096]).unwrap();
        assert!(matches!(Index::load(&path), Err(SnapshotError::BadMagic)));

        // Empty.
        std::fs::write(&path, b"").unwrap();
        assert!(matches!(Index::load(&path), Err(SnapshotError::BadMagic)));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn rejects_a_future_format_version() {
        let path = tmp("version");
        sample().save(&path).unwrap();
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[8..12].copy_from_slice(&99u32.to_le_bytes());
        std::fs::write(&path, &bytes).unwrap();

        match Index::load(&path) {
            Err(SnapshotError::Version { found, expected }) => {
                assert_eq!(found, 99);
                assert_eq!(expected, FORMAT_VERSION);
            }
            other => panic!("expected a version error, got {other:?}"),
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn rejects_a_truncated_file() {
        let path = tmp("truncated");
        sample().save(&path).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        // Keep the header and volume table, drop most of the payload.
        std::fs::write(&path, &bytes[..bytes.len() / 2]).unwrap();

        assert!(
            matches!(Index::load(&path), Err(SnapshotError::Truncated { .. })),
            "a half-written snapshot must be refused, not partially loaded"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn rejects_corrupted_payload() {
        let path = tmp("corrupt");
        sample().save(&path).unwrap();
        let mut bytes = std::fs::read(&path).unwrap();
        // Flip a byte deep in the payload, leaving the header intact.
        let at = bytes.len() - 8;
        bytes[at] ^= 0xFF;
        std::fs::write(&path, &bytes).unwrap();

        assert!(matches!(Index::load(&path), Err(SnapshotError::Checksum)));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn rejects_out_of_range_indices_that_still_checksum() {
        // Corrupt a parent pointer *and* fix the checksum, so only the
        // validation pass can catch it. This is the case that would otherwise
        // panic while subscripting inside a privileged process.
        let path = tmp("validate");
        let idx = sample();
        idx.save(&path).unwrap();
        let mut bytes = std::fs::read(&path).unwrap();

        let vol_bytes = idx.volumes.len() as u64 * VOLUME_BYTES;
        let parent0 = (HEADER_BYTES + vol_bytes) as usize;
        bytes[parent0..parent0 + 4].copy_from_slice(&999_999u32.to_le_bytes());

        // Recompute the payload hash so it passes the integrity check.
        let mut h = Xxh3::new();
        h.update(&bytes[HEADER_BYTES as usize..]);
        let fixed = h.digest();
        bytes[40..48].copy_from_slice(&fixed.to_le_bytes());
        std::fs::write(&path, &bytes).unwrap();

        match Index::load(&path) {
            Err(SnapshotError::Corrupt(msg)) => assert!(msg.contains("parent")),
            other => panic!("expected a validation failure, got {other:?}"),
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn peek_reports_the_drives_without_reading_the_payload() {
        let path = tmp("peek");
        let idx = sample();
        idx.save(&path).unwrap();

        let info = peek(&path).unwrap();
        assert_eq!(info.drives, vec!['C', 'D']);
        assert_eq!(info.nodes as usize, idx.len());
        assert!(info.written_at > 1_700_000_000);

        // It must agree with a full load, or the skip decision built on it
        // would be made from different facts than the index actually holds.
        let full = Index::load(&path).unwrap();
        let full_drives: Vec<char> = full.volumes.iter().map(|v| v.drive).collect();
        assert_eq!(info.drives, full_drives);
        assert_eq!(info.written_at, written_at(&path).unwrap());

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn peek_refuses_the_same_files_a_load_would() {
        let path = tmp("peekbad");

        std::fs::write(&path, b"nope").unwrap();
        assert!(matches!(peek(&path), Err(SnapshotError::BadMagic)));

        std::fs::write(&path, vec![0x5Au8; 4096]).unwrap();
        assert!(matches!(peek(&path), Err(SnapshotError::BadMagic)));

        // A future format must not be misread as usable.
        sample().save(&path).unwrap();
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[8..12].copy_from_slice(&99u32.to_le_bytes());
        std::fs::write(&path, &bytes).unwrap();
        assert!(matches!(peek(&path), Err(SnapshotError::Version { .. })));

        assert!(peek(Path::new("no-such-snapshot.bin")).is_err());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn exposes_the_write_time() {
        let path = tmp("written");
        sample().save(&path).unwrap();
        let t = written_at(&path).expect("snapshot should carry a timestamp");
        assert!(t > 1_700_000_000, "timestamp looks wrong: {t}");
        assert!(written_at(Path::new("nonexistent.idx")).is_none());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn saving_folds_renames_back_into_the_arena() {
        // A live index accumulates renames in a side table. Saving must bake
        // them in, so a reloaded snapshot carries no override state and the
        // stale arena bytes are reclaimed.
        use crate::watch::Change;

        let path = tmp("renames");
        let mut idx = sample();
        let node = idx.lookup(r"C:\Users\Hacker\notes.txt").unwrap();

        // Find the record numbers backing the node and its parent, so the
        // rename can address them the way the journal would.
        let rec = (0..256u64)
            .find(|&r| idx.node_of_record(0, r) == Some(node))
            .expect("node should be reachable by record");
        let parent = idx.parent[node as usize];
        let parent_rec = (0..256u64)
            .find(|&r| idx.node_of_record(0, r) == Some(parent))
            .expect("parent should be reachable by record");

        idx.apply(&Change::Renamed {
            volume: 0,
            record: rec,
            sequence: 1,
            parent_record: parent_rec,
            name: "journal-entries.md".into(),
        });
        assert_eq!(idx.overrides().len(), 1);

        idx.save(&path).unwrap();
        let back = Index::load(&path).unwrap();

        assert!(
            back.overrides().is_empty(),
            "renames should be baked in, not carried"
        );
        assert_eq!(back.name(node), "journal-entries.md");
        assert_eq!(back.path(node), r"C:\Users\Hacker\journal-entries.md");
        assert!(
            back.name_off.windows(2).all(|w| w[0] <= w[1]),
            "the rebuilt arena must still be sorted"
        );

        // The new name is findable and the old one is gone.
        let mut s = Searcher::new();
        assert_eq!(s.search(&back, &Query::new("journal-entries")).total, 1);
        s.reset();
        assert_eq!(s.search(&back, &Query::new("notes.txt")).total, 0);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn saving_leaves_no_temporary_behind() {
        let path = tmp("atomic");
        sample().save(&path).unwrap();
        assert!(path.exists());
        assert!(
            !path.with_extension("tmp").exists(),
            "temp file must be renamed away"
        );
        let _ = std::fs::remove_file(&path);
    }
}
