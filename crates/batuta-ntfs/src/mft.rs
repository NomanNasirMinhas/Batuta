//! Turning MFT file records into flat index rows.
//!
//! This is the semantic layer above [`crate::record`]: it decides which
//! `$FILE_NAME` wins, where a file's size comes from, and how attributes that
//! spilled into extension records are reunited with their base record.

use crate::error::{FileRef, NtfsError, ReadLe, Result};
use crate::record::{attr_type, AttrBody, FileRecord};

/// Flags carried on an index entry.
pub mod eflags {
    pub const DIRECTORY: u16 = 1 << 0;
    pub const REPARSE: u16 = 1 << 1;
    pub const COMPRESSED: u16 = 1 << 2;
    pub const SPARSE: u16 = 1 << 3;
    pub const HIDDEN: u16 = 1 << 4;
    pub const SYSTEM: u16 = 1 << 5;
    pub const READONLY: u16 = 1 << 6;
    /// The file has more than one name; its size is attributed to the first.
    pub const HARD_LINK: u16 = 1 << 7;
    pub const HAS_ADS: u16 = 1 << 8;
    pub const ENCRYPTED: u16 = 1 << 9;
    /// One of the reserved records below number 16 (`$MFT`, `$LogFile`, ...).
    pub const METADATA: u16 = 1 << 10;
}

/// DOS attribute bits as stored in `$STANDARD_INFORMATION`.
mod dos {
    pub const READONLY: u32 = 0x0001;
    pub const HIDDEN: u32 = 0x0002;
    pub const SYSTEM: u32 = 0x0004;
    pub const SPARSE_FILE: u32 = 0x0200;
    pub const REPARSE_POINT: u32 = 0x0400;
    pub const COMPRESSED: u32 = 0x0800;
    pub const ENCRYPTED: u32 = 0x4000;
}

/// The first 16 MFT records are NTFS metadata files.
pub const FIRST_USER_RECORD: u64 = 16;

/// A parsed base record, with its name still borrowed from the source buffer.
///
/// Keeping the name as raw UTF-16 avoids allocating a `String` for every one
/// of the millions of records on a volume; the caller transcodes straight into
/// its own arena.
#[derive(Debug, Clone, Copy)]
pub struct EntryInfo<'a> {
    pub record_no: u64,
    pub sequence: u16,
    pub parent: FileRef,
    pub name_utf16: &'a [u8],
    pub size: u64,
    pub allocated: u64,
    /// Modification time as a Windows FILETIME (100ns ticks since 1601).
    pub mtime: i64,
    pub flags: u16,
    pub hard_links: u16,
    /// The record has an `$ATTRIBUTE_LIST`, so its size may live elsewhere.
    pub spilled: bool,
    /// Namespace rank of the name that was chosen. A name recovered from an
    /// extension record only replaces this if it ranks higher.
    pub name_rank: u8,
}

/// A `$FILE_NAME` recovered from an extension record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpilledName {
    pub rank: u8,
    pub parent: u64,
    pub name: String,
}

/// What a single MFT record turned out to be.
#[derive(Debug, Clone)]
pub enum Parsed<'a> {
    /// A real file or directory.
    Entry(EntryInfo<'a>),
    /// An extension record holding attributes belonging to `base`.
    Extension {
        base: u64,
        size: Option<u64>,
        allocated: Option<u64>,
        /// A directory with a large index can push its `$FILE_NAME`
        /// attributes out here. If the Win32 name is among them, the base
        /// record may be left holding only the DOS 8.3 alias.
        name: Option<SpilledName>,
    },
    /// Unused slot, or a record carrying nothing we index.
    Skip,
}

/// Rank `$FILE_NAME` namespaces so the most useful name wins.
///
/// Win32 names are what users see; DOS 8.3 names are a legacy alias and are
/// only accepted when a record has nothing better.
fn namespace_rank(ns: u8) -> u8 {
    match ns {
        3 => 4, // Win32 and DOS share one entry
        1 => 3, // Win32
        0 => 2, // POSIX
        2 => 1, // DOS only
        _ => 0,
    }
}

/// Parse one fixed-up MFT record.
///
/// `record_no` is the slot index the record was read from, which is trusted in
/// preference to the record's self-reported number.
pub fn parse_record(buf: &[u8], record_no: u64) -> Result<Parsed<'_>> {
    let hdr = match FileRecord::parse(buf) {
        Ok(h) => h,
        // An unused slot is normal while sweeping the MFT, not an error.
        Err(NtfsError::NotAFileRecord) => return Ok(Parsed::Skip),
        Err(e) => return Err(e),
    };

    if !hdr.is_in_use() {
        return Ok(Parsed::Skip);
    }

    // Extension records are not files; harvest any unnamed $DATA size they
    // carry and hand it back to the base record.
    if hdr.is_extension() {
        let mut size = None;
        let mut allocated = None;
        let mut name: Option<SpilledName> = None;

        for attr in hdr.attributes(buf).flatten() {
            match attr.ty {
                attr_type::DATA if attr.is_unnamed() && attr.is_first_fragment() => {
                    if size.is_none() {
                        size = Some(attr.data_size());
                        allocated = Some(attr.allocated_size());
                    }
                }
                attr_type::FILE_NAME => {
                    if let AttrBody::Resident(v) = attr.body {
                        let chars = v.u8_at(0x40).unwrap_or(0) as usize;
                        let rank = namespace_rank(v.u8_at(0x41).unwrap_or(0));
                        if chars > 0 && name.as_ref().is_none_or(|n| rank > n.rank) {
                            if let Some(raw) = v.get(0x42..0x42 + chars * 2) {
                                name = Some(SpilledName {
                                    rank,
                                    parent: FileRef(v.u64_at(0x00).unwrap_or(0)).record(),
                                    name: crate::record::decode_utf16(raw),
                                });
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        return Ok(Parsed::Extension {
            base: hdr.base_record.record(),
            size,
            allocated,
            name,
        });
    }

    let mut flags: u16 = 0;
    if hdr.is_directory() {
        flags |= eflags::DIRECTORY;
    }
    if record_no < FIRST_USER_RECORD {
        flags |= eflags::METADATA | eflags::SYSTEM;
    }
    if hdr.hard_link_count > 1 {
        flags |= eflags::HARD_LINK;
    }

    let mut mtime: i64 = 0;
    let mut best_rank = 0u8;
    let mut parent = FileRef(0);
    let mut name_utf16: &[u8] = &[];
    let mut size: u64 = 0;
    let mut allocated: u64 = 0;
    let mut have_data = false;
    let mut spilled = false;
    let mut fallback_size: u64 = 0;
    let mut fallback_alloc: u64 = 0;

    for attr in hdr.attributes(buf).flatten() {
        match attr.ty {
            attr_type::STANDARD_INFORMATION => {
                if let AttrBody::Resident(v) = attr.body {
                    mtime = v.u64_at(0x08).unwrap_or(0) as i64;
                    let dos_attrs = v.u32_at(0x20).unwrap_or(0);
                    if dos_attrs & dos::READONLY != 0 {
                        flags |= eflags::READONLY;
                    }
                    if dos_attrs & dos::HIDDEN != 0 {
                        flags |= eflags::HIDDEN;
                    }
                    if dos_attrs & dos::SYSTEM != 0 {
                        flags |= eflags::SYSTEM;
                    }
                    if dos_attrs & dos::REPARSE_POINT != 0 {
                        flags |= eflags::REPARSE;
                    }
                    if dos_attrs & dos::COMPRESSED != 0 {
                        flags |= eflags::COMPRESSED;
                    }
                    if dos_attrs & dos::SPARSE_FILE != 0 {
                        flags |= eflags::SPARSE;
                    }
                    if dos_attrs & dos::ENCRYPTED != 0 {
                        flags |= eflags::ENCRYPTED;
                    }
                }
            }
            attr_type::FILE_NAME => {
                if let AttrBody::Resident(v) = attr.body {
                    let name_chars = v.u8_at(0x40).unwrap_or(0) as usize;
                    let ns = v.u8_at(0x41).unwrap_or(0);
                    let rank = namespace_rank(ns);
                    if rank > best_rank && name_chars > 0 {
                        if let Some(raw) = v.get(0x42..0x42 + name_chars * 2) {
                            best_rank = rank;
                            name_utf16 = raw;
                            parent = FileRef(v.u64_at(0x00).unwrap_or(0));
                            // $FILE_NAME caches a size that NTFS does not
                            // always keep current; only used as a last resort.
                            fallback_size = v.u64_at(0x30).unwrap_or(0);
                            fallback_alloc = v.u64_at(0x28).unwrap_or(0);
                        }
                    }
                }
            }
            attr_type::DATA => {
                if attr.is_unnamed() {
                    if attr.is_first_fragment() && !have_data {
                        size = attr.data_size();
                        allocated = attr.allocated_size();
                        have_data = true;
                    }
                } else {
                    flags |= eflags::HAS_ADS;
                }
                if attr.is_sparse_or_compressed() {
                    flags |= eflags::SPARSE;
                }
            }
            attr_type::ATTRIBUTE_LIST => spilled = true,
            attr_type::REPARSE_POINT => flags |= eflags::REPARSE,
            _ => {}
        }
    }

    // A record with no usable name is not indexable.
    if name_utf16.is_empty() {
        return Ok(Parsed::Skip);
    }

    // Directories hold no $DATA; their size is the rolled-up subtree total,
    // computed later by the index builder.
    if flags & eflags::DIRECTORY != 0 {
        size = 0;
        allocated = 0;
    } else if !have_data && spilled {
        // $DATA spilled into an extension record. Use the cached $FILE_NAME
        // size for now; the extension pass overwrites it when it finds better.
        size = fallback_size;
        allocated = fallback_alloc;
    }

    Ok(Parsed::Entry(EntryInfo {
        record_no,
        sequence: hdr.sequence,
        parent,
        name_utf16,
        size,
        allocated,
        mtime,
        flags,
        hard_links: hdr.hard_link_count,
        spilled,
        name_rank: best_rank,
    }))
}

/// A batch of parsed entries in struct-of-arrays form.
///
/// Names are transcoded into one contiguous UTF-8 arena rather than being
/// individually allocated, which is what keeps a full-volume parse cheap.
#[derive(Default, Debug)]
pub struct EntryBatch {
    pub record_no: Vec<u64>,
    pub sequence: Vec<u16>,
    pub parent: Vec<u64>,
    pub name_off: Vec<u32>,
    pub name_len: Vec<u16>,
    pub size: Vec<u64>,
    pub allocated: Vec<u64>,
    pub mtime: Vec<i64>,
    pub flags: Vec<u16>,
    pub names: Vec<u8>,
    /// Namespace rank of each entry's chosen name.
    pub name_rank: Vec<u8>,
    /// Sizes recovered from extension records, keyed by base record number.
    pub spill: Vec<(u64, u64, u64)>,
    /// Names recovered from extension records, keyed by base record number.
    pub name_spill: Vec<(u64, SpilledName)>,
}

impl EntryBatch {
    pub fn len(&self) -> usize {
        self.record_no.len()
    }

    pub fn is_empty(&self) -> bool {
        self.record_no.is_empty()
    }

    pub fn name(&self, i: usize) -> &str {
        let off = self.name_off[i] as usize;
        let len = self.name_len[i] as usize;
        // Written only by `push`, which always transcodes valid UTF-8.
        std::str::from_utf8(&self.names[off..off + len]).unwrap_or("")
    }

    pub fn push(&mut self, e: &EntryInfo<'_>) {
        let off = self.names.len() as u32;
        transcode_utf16(e.name_utf16, &mut self.names);
        let len = (self.names.len() - off as usize) as u16;

        self.record_no.push(e.record_no);
        self.sequence.push(e.sequence);
        self.parent.push(e.parent.0);
        self.name_off.push(off);
        self.name_len.push(len);
        self.size.push(e.size);
        self.allocated.push(e.allocated);
        self.mtime.push(e.mtime);
        self.flags.push(e.flags);
        self.name_rank.push(e.name_rank);
    }

    /// Append `other`, fixing up its name offsets to point into this arena.
    pub fn merge(&mut self, mut other: EntryBatch) {
        let base = self.names.len() as u32;
        self.names.append(&mut other.names);
        self.name_off
            .extend(other.name_off.iter().map(|o| o + base));
        self.record_no.append(&mut other.record_no);
        self.sequence.append(&mut other.sequence);
        self.parent.append(&mut other.parent);
        self.name_len.append(&mut other.name_len);
        self.size.append(&mut other.size);
        self.allocated.append(&mut other.allocated);
        self.mtime.append(&mut other.mtime);
        self.flags.append(&mut other.flags);
        self.name_rank.append(&mut other.name_rank);
        self.spill.append(&mut other.spill);
        self.name_spill.append(&mut other.name_spill);
    }

    /// Apply sizes and names harvested from extension records to their base
    /// records.
    ///
    /// Must run after all batches are merged, since an extension record can
    /// appear in a different chunk from its base.
    pub fn apply_spill(&mut self) {
        self.apply_name_spill();
        if self.spill.is_empty() {
            return;
        }
        let mut by_record: rustc_hash::FxHashMap<u64, (u64, u64)> =
            rustc_hash::FxHashMap::default();
        for &(base, size, alloc) in &self.spill {
            by_record.entry(base).or_insert((size, alloc));
        }
        for i in 0..self.record_no.len() {
            if let Some(&(size, alloc)) = by_record.get(&self.record_no[i]) {
                if self.flags[i] & eflags::DIRECTORY == 0 {
                    self.size[i] = size;
                    self.allocated[i] = alloc;
                }
            }
        }
    }
}

impl EntryBatch {
    /// Replace names that lost out to a DOS 8.3 alias because the real name
    /// spilled into an extension record.
    ///
    /// A directory with a large index can push its `$FILE_NAME` attributes out
    /// of the base record. When the Win32 name goes and the 8.3 alias stays,
    /// the entry ends up called something like `PHOTOS~1`, and the real name
    /// is missing from the index entirely.
    ///
    /// Names live in a packed arena addressed by offset, so a longer
    /// replacement cannot be written in place; the arena is rebuilt in node
    /// order, which keeps `name_off` sorted as the search engine requires.
    fn apply_name_spill(&mut self) {
        if self.name_spill.is_empty() {
            return;
        }
        let mut best: rustc_hash::FxHashMap<u64, usize> = rustc_hash::FxHashMap::default();
        for (i, (base, cand)) in self.name_spill.iter().enumerate() {
            match best.get(base) {
                Some(&j) if self.name_spill[j].1.rank >= cand.rank => {}
                _ => {
                    best.insert(*base, i);
                }
            }
        }

        let n = self.record_no.len();
        let mut names = Vec::with_capacity(self.names.len());
        let mut off = Vec::with_capacity(n);
        let mut len = Vec::with_capacity(n);
        let mut parents = Vec::with_capacity(n);
        let mut replaced = 0usize;

        for i in 0..n {
            let candidate = best
                .get(&self.record_no[i])
                .map(|&j| &self.name_spill[j].1)
                .filter(|c| c.rank > self.name_rank[i] && !c.name.is_empty());

            let (bytes, parent) = match candidate {
                Some(c) => {
                    replaced += 1;
                    (c.name.as_bytes(), FileRef(c.parent).0)
                }
                None => {
                    let a = self.name_off[i] as usize;
                    let b = a + self.name_len[i] as usize;
                    (&self.names[a..b], self.parent[i])
                }
            };

            off.push(names.len() as u32);
            let take = bytes.len().min(u16::MAX as usize);
            names.extend_from_slice(&bytes[..take]);
            len.push(take as u16);
            parents.push(parent);
        }

        if replaced > 0 {
            self.names = names;
            self.name_off = off;
            self.name_len = len;
            self.parent = parents;
        }
    }
}

/// Transcode UTF-16LE into UTF-8, appending to `out`.
///
/// Almost every filename on a real volume is pure ASCII, so that case gets a
/// tight loop that avoids the general decoder entirely.
fn transcode_utf16(raw: &[u8], out: &mut Vec<u8>) {
    let ascii = raw
        .as_chunks::<2>()
        .0
        .iter()
        .all(|c| c[1] == 0 && c[0] < 0x80);
    if ascii {
        out.reserve(raw.len() / 2);
        for c in raw.as_chunks::<2>().0 {
            out.push(c[0]);
        }
        return;
    }
    let units: Vec<u16> = raw
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    let mut buf = [0u8; 4];
    for r in char::decode_utf16(units) {
        let ch = r.unwrap_or(char::REPLACEMENT_CHARACTER);
        out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
    }
}

/// Sweeps a buffer of consecutive MFT records.
pub struct MftParser {
    pub bytes_per_sector: u32,
    pub record_size: u32,
}

impl MftParser {
    pub fn new(bytes_per_sector: u32, record_size: u32) -> Self {
        MftParser {
            bytes_per_sector,
            record_size,
        }
    }

    /// Parse every record in `chunk`, which must begin at record
    /// `first_record_no`. Corrupt individual records are skipped rather than
    /// aborting the sweep; a bad record should cost one file, not the volume.
    pub fn parse_chunk(&self, chunk: &mut [u8], first_record_no: u64, out: &mut EntryBatch) {
        let rs = self.record_size as usize;
        if rs == 0 {
            return;
        }
        for (i, rec) in chunk.chunks_mut(rs).enumerate() {
            if rec.len() < rs {
                break;
            }
            let record_no = first_record_no + i as u64;
            if crate::record::apply_fixups(rec, self.bytes_per_sector).is_err() {
                continue;
            }
            match parse_record(rec, record_no) {
                Ok(Parsed::Entry(e)) => out.push(&e),
                Ok(Parsed::Extension {
                    base,
                    size,
                    allocated,
                    name,
                }) => {
                    if let (Some(s), Some(a)) = (size, allocated) {
                        out.spill.push((base, s, a));
                    }
                    if let Some(n) = name {
                        out.name_spill.push((base, n));
                    }
                }
                Ok(Parsed::Skip) | Err(_) => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::decode_utf16;
    use crate::testutil::{namespace, RecordBuilder};

    fn entry_of<'a>(buf: &'a [u8], no: u64) -> EntryInfo<'a> {
        match parse_record(buf, no).unwrap() {
            Parsed::Entry(e) => e,
            other => panic!("expected an entry, got {other:?}"),
        }
    }

    #[test]
    fn parses_a_plain_file() {
        let rec = RecordBuilder::new(1024)
            .standard_information(0x0020, 132_000_000_000_000_000)
            .file_name(FileRef(5), "notes.txt", namespace::WIN32)
            .non_resident_data(&[0x21, 0x10, 0x00, 0x02, 0x00], 40_000, 40_960)
            .build();

        let e = entry_of(&rec, 100);
        assert_eq!(decode_utf16(e.name_utf16), "notes.txt");
        assert_eq!(e.parent, FileRef(5));
        assert_eq!(e.size, 40_000);
        assert_eq!(e.allocated, 40_960);
        assert_eq!(e.mtime, 132_000_000_000_000_000);
        assert_eq!(e.flags & eflags::DIRECTORY, 0);
    }

    #[test]
    fn resident_data_size_comes_from_the_value() {
        let rec = RecordBuilder::new(1024)
            .file_name(FileRef(5), "tiny.txt", namespace::WIN32)
            .resident_data(b"0123456789")
            .build();
        assert_eq!(entry_of(&rec, 101).size, 10);
    }

    #[test]
    fn directories_report_zero_size_for_later_rollup() {
        let rec = RecordBuilder::new(1024)
            .directory(true)
            .file_name(FileRef(5), "Documents", namespace::WIN32)
            .index_root()
            .build();
        let e = entry_of(&rec, 102);
        assert_ne!(e.flags & eflags::DIRECTORY, 0);
        assert_eq!(e.size, 0);
    }

    #[test]
    fn win32_name_beats_dos_alias() {
        // NTFS stores both names; the 8.3 alias must not win.
        let rec = RecordBuilder::new(1024)
            .file_name(FileRef(5), "PROGRA~1", namespace::DOS)
            .file_name(FileRef(5), "Program Files", namespace::WIN32)
            .resident_data(b"x")
            .build();
        assert_eq!(
            decode_utf16(entry_of(&rec, 103).name_utf16),
            "Program Files"
        );
    }

    #[test]
    fn dos_only_name_is_still_used_when_nothing_better_exists() {
        let rec = RecordBuilder::new(1024)
            .file_name(FileRef(5), "README~1.TXT", namespace::DOS)
            .resident_data(b"x")
            .build();
        assert_eq!(decode_utf16(entry_of(&rec, 104).name_utf16), "README~1.TXT");
    }

    #[test]
    fn unused_and_nameless_records_are_skipped() {
        let unused = RecordBuilder::new(1024)
            .in_use(false)
            .file_name(FileRef(5), "ghost.txt", namespace::WIN32)
            .build();
        assert!(matches!(parse_record(&unused, 105).unwrap(), Parsed::Skip));

        let nameless = RecordBuilder::new(1024).resident_data(b"x").build();
        assert!(matches!(
            parse_record(&nameless, 106).unwrap(),
            Parsed::Skip
        ));

        // A slot that was never written is not an error.
        assert!(matches!(
            parse_record(&[0u8; 1024], 107).unwrap(),
            Parsed::Skip
        ));
    }

    #[test]
    fn extension_records_yield_their_size_to_the_base() {
        let ext = RecordBuilder::new(1024)
            .extension_of(FileRef(200))
            .non_resident_data(&[0x21, 0x40, 0x00, 0x04, 0x00], 9_000_000, 9_007_104)
            .build();
        match parse_record(&ext, 201).unwrap() {
            Parsed::Extension {
                base,
                size,
                allocated,
                ..
            } => {
                assert_eq!(base, 200);
                assert_eq!(size, Some(9_000_000));
                assert_eq!(allocated, Some(9_007_104));
            }
            other => panic!("expected an extension record, got {other:?}"),
        }
    }

    #[test]
    fn spilled_size_is_reunited_with_its_base_record() {
        // A fragmented file whose $DATA lives in an extension record: without
        // the spill pass its size would be wrong.
        let base = RecordBuilder::new(1024)
            .record_number(200)
            .file_name_sized(FileRef(5), "huge.vhdx", namespace::WIN32, 0, 0)
            .attribute_list(&[(attr_type::DATA, 0, FileRef(201))])
            .build();
        let ext = RecordBuilder::new(1024)
            .extension_of(FileRef(200))
            .non_resident_data(&[0x21, 0x40, 0x00, 0x04, 0x00], 9_000_000, 9_007_104)
            .build();

        let mut batch = EntryBatch::default();
        let parser = MftParser::new(512, 1024);
        let mut buf = base.clone();
        parser_push(&parser, &mut buf, 200, &mut batch);
        let mut buf = ext.clone();
        parser_push(&parser, &mut buf, 201, &mut batch);

        assert_eq!(batch.len(), 1);
        assert_eq!(batch.size[0], 0, "size unknown before the spill pass");
        batch.apply_spill();
        assert_eq!(batch.size[0], 9_000_000);
        assert_eq!(batch.allocated[0], 9_007_104);
    }

    /// Feed one already-clean record through the batch path.
    fn parser_push(_p: &MftParser, rec: &mut [u8], no: u64, out: &mut EntryBatch) {
        match parse_record(rec, no).unwrap() {
            Parsed::Entry(e) => out.push(&e),
            Parsed::Extension {
                base,
                size,
                allocated,
                name,
            } => {
                if let (Some(s), Some(a)) = (size, allocated) {
                    out.spill.push((base, s, a));
                }
                if let Some(n) = name {
                    out.name_spill.push((base, n));
                }
            }
            Parsed::Skip => {}
        }
    }

    #[test]
    fn a_win32_name_that_spilled_replaces_the_dos_alias() {
        // A directory with a large index can push its $FILE_NAME attributes
        // into an extension record. If the Win32 name goes and the 8.3 alias
        // stays, the entry is left called `S-1-5-~1` and the real name is
        // missing from the index entirely.
        let long = "S-1-5-21-2205097650-901772425-1229515317-1001";

        let base = RecordBuilder::new(1024)
            .directory(true)
            .record_number(300)
            .file_name(FileRef(5), "S-1-5-~1", namespace::DOS)
            .attribute_list(&[(attr_type::FILE_NAME, 0, FileRef(301))])
            .index_root()
            .build();
        let ext = RecordBuilder::new(1024)
            .extension_of(FileRef(300))
            .file_name(FileRef(5), long, namespace::WIN32)
            .build();

        let mut batch = EntryBatch::default();
        let parser = MftParser::new(512, 1024);
        parser_push(&parser, &mut base.clone(), 300, &mut batch);
        parser_push(&parser, &mut ext.clone(), 301, &mut batch);

        assert_eq!(batch.len(), 1);
        assert_eq!(
            batch.name(0),
            "S-1-5-~1",
            "only the alias before the spill pass"
        );

        batch.apply_spill();
        assert_eq!(batch.name(0), long, "the real name must be recovered");
        assert!(
            batch.name_off.windows(2).all(|w| w[0] <= w[1]),
            "the rebuilt arena must stay sorted"
        );
    }

    #[test]
    fn a_spilled_name_does_not_override_a_better_one() {
        // The base record already holds the Win32 name; a DOS alias found in
        // an extension record must not win.
        let base = RecordBuilder::new(1024)
            .record_number(400)
            .file_name(FileRef(5), "Program Files", namespace::WIN32)
            .resident_data(b"x")
            .build();
        let ext = RecordBuilder::new(1024)
            .extension_of(FileRef(400))
            .file_name(FileRef(5), "PROGRA~1", namespace::DOS)
            .build();

        let mut batch = EntryBatch::default();
        let parser = MftParser::new(512, 1024);
        parser_push(&parser, &mut base.clone(), 400, &mut batch);
        parser_push(&parser, &mut ext.clone(), 401, &mut batch);

        batch.apply_spill();
        assert_eq!(batch.name(0), "Program Files");
    }

    #[test]
    fn spilled_names_and_sizes_are_applied_together() {
        // Both kinds of spill can come from the same extension record.
        let base = RecordBuilder::new(1024)
            .record_number(500)
            .file_name_sized(FileRef(5), "LONGNA~1.BIN", namespace::DOS, 0, 0)
            .attribute_list(&[(attr_type::DATA, 0, FileRef(501))])
            .build();
        let ext = RecordBuilder::new(1024)
            .extension_of(FileRef(500))
            .file_name(FileRef(5), "long name with spaces.bin", namespace::WIN32)
            .non_resident_data(&[0x21, 0x40, 0x00, 0x04, 0x00], 7_000_000, 7_004_160)
            .build();

        let mut batch = EntryBatch::default();
        let parser = MftParser::new(512, 1024);
        parser_push(&parser, &mut base.clone(), 500, &mut batch);
        parser_push(&parser, &mut ext.clone(), 501, &mut batch);

        batch.apply_spill();
        assert_eq!(batch.name(0), "long name with spaces.bin");
        assert_eq!(batch.size[0], 7_000_000);
    }

    #[test]
    fn genuine_short_names_are_left_alone() {
        // Plenty of files really are called `FOO~1.DLL`. With nothing spilled,
        // the name must survive untouched.
        let rec = RecordBuilder::new(1024)
            .record_number(600)
            .file_name(FileRef(5), "6BEA57~1.DLL", namespace::WIN32_AND_DOS)
            .resident_data(b"x")
            .build();

        let mut batch = EntryBatch::default();
        let parser = MftParser::new(512, 1024);
        parser_push(&parser, &mut rec.clone(), 600, &mut batch);
        batch.apply_spill();
        assert_eq!(batch.name(0), "6BEA57~1.DLL");
    }

    #[test]
    fn named_streams_are_flagged_but_do_not_change_size() {
        let rec = RecordBuilder::new(1024)
            .file_name(FileRef(5), "download.exe", namespace::WIN32)
            .resident_data(b"abcd")
            .named_resident_data("Zone.Identifier", b"[ZoneTransfer]rn")
            .build();
        let e = entry_of(&rec, 108);
        assert_eq!(e.size, 4, "ADS must not inflate the file size");
        assert_ne!(e.flags & eflags::HAS_ADS, 0);
    }

    #[test]
    fn reparse_and_attribute_bits_are_carried_through() {
        let rec = RecordBuilder::new(1024)
            .standard_information(
                dos::REPARSE_POINT | dos::HIDDEN | dos::SYSTEM | dos::READONLY | dos::COMPRESSED,
                1,
            )
            .file_name(FileRef(5), "OneDrive", namespace::WIN32)
            .resident_data(b"")
            .build();
        let e = entry_of(&rec, 109);
        for bit in [
            eflags::REPARSE,
            eflags::HIDDEN,
            eflags::SYSTEM,
            eflags::READONLY,
            eflags::COMPRESSED,
        ] {
            assert_ne!(e.flags & bit, 0, "missing flag bit {bit:#x}");
        }
    }

    #[test]
    fn hard_links_are_flagged() {
        let rec = RecordBuilder::new(1024)
            .hard_links(3)
            .file_name(FileRef(5), "linked.dll", namespace::WIN32)
            .resident_data(b"x")
            .build();
        assert_ne!(entry_of(&rec, 110).flags & eflags::HARD_LINK, 0);
    }

    #[test]
    fn metadata_records_are_flagged() {
        let rec = RecordBuilder::new(1024)
            .file_name(FileRef(5), "$MFT", namespace::WIN32)
            .non_resident_data(&[0x21, 0x10, 0x00, 0x02, 0x00], 1 << 30, 1 << 30)
            .build();
        assert_ne!(entry_of(&rec, 0).flags & eflags::METADATA, 0);
        // A user record at the same shape is not metadata.
        assert_eq!(entry_of(&rec, 4096).flags & eflags::METADATA, 0);
    }

    #[test]
    fn parse_chunk_applies_fixups_and_survives_corruption() {
        let good = RecordBuilder::new(1024)
            .file_name(FileRef(5), "alpha.txt", namespace::WIN32)
            .resident_data(&[0xCD; 700]);
        let also_good = RecordBuilder::new(1024)
            .file_name(FileRef(5), "beta.txt", namespace::WIN32)
            .resident_data(&[0xEF; 700]);

        let mut chunk = Vec::new();
        chunk.extend_from_slice(&good.build_raw(0x1234));
        // A record whose fixup is wrong must be skipped, not abort the sweep.
        let mut torn = also_good.build_raw(0x1234);
        torn[510..512].copy_from_slice(&0xFFFFu16.to_le_bytes());
        chunk.extend_from_slice(&torn);
        chunk.extend_from_slice(&also_good.build_raw(0x1234));

        let parser = MftParser::new(512, 1024);
        let mut batch = EntryBatch::default();
        parser.parse_chunk(&mut chunk, 500, &mut batch);

        assert_eq!(batch.len(), 2);
        assert_eq!(batch.name(0), "alpha.txt");
        assert_eq!(batch.record_no[0], 500);
        assert_eq!(batch.name(1), "beta.txt");
        assert_eq!(batch.record_no[1], 502, "record numbers track slot index");
    }

    #[test]
    fn batches_merge_with_correct_name_offsets() {
        let mut a = EntryBatch::default();
        let mut b = EntryBatch::default();
        let ra = RecordBuilder::new(1024)
            .file_name(FileRef(5), "first.txt", namespace::WIN32)
            .resident_data(b"1")
            .build();
        let rb = RecordBuilder::new(1024)
            .file_name(FileRef(5), "second.txt", namespace::WIN32)
            .resident_data(b"22")
            .build();
        a.push(&entry_of(&ra, 1000));
        b.push(&entry_of(&rb, 1001));
        b.spill.push((7, 1, 2));

        a.merge(b);
        assert_eq!(a.len(), 2);
        assert_eq!(a.name(0), "first.txt");
        assert_eq!(a.name(1), "second.txt");
        assert_eq!(a.spill, vec![(7, 1, 2)]);
    }

    #[test]
    fn transcodes_unicode_names() {
        let rec = RecordBuilder::new(1024)
            .file_name(FileRef(5), "café_日本語.txt", namespace::WIN32)
            .resident_data(b"x")
            .build();
        let mut batch = EntryBatch::default();
        batch.push(&entry_of(&rec, 111));
        assert_eq!(batch.name(0), "café_日本語.txt");
    }

    #[test]
    fn transcodes_names_outside_the_bmp() {
        let rec = RecordBuilder::new(1024)
            .file_name(FileRef(5), "emoji_🦀.rs", namespace::WIN32)
            .resident_data(b"x")
            .build();
        let mut batch = EntryBatch::default();
        batch.push(&entry_of(&rec, 112));
        assert_eq!(batch.name(0), "emoji_🦀.rs");
    }
}
