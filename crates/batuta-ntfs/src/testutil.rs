//! Builders for synthetic MFT records.
//!
//! The MFT parser is the highest-risk component in the project and it normally
//! only sees bytes from a raw volume, which requires elevation and is not
//! reproducible. These builders let the whole parser be exercised against
//! hand-constructed records covering fixups, resident and non-resident data,
//! attribute-list spill, hard links and DOS-namespace names.

use crate::error::FileRef;
use crate::record::attr_type;

/// Namespace values for `$FILE_NAME`.
pub mod namespace {
    pub const POSIX: u8 = 0;
    pub const WIN32: u8 = 1;
    pub const DOS: u8 = 2;
    pub const WIN32_AND_DOS: u8 = 3;
}

const USA_OFFSET: usize = 0x30;

fn align8(n: usize) -> usize {
    (n + 7) & !7
}

fn utf16(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect()
}

/// Builds a single MFT file record.
pub struct RecordBuilder {
    size: usize,
    sector_size: usize,
    flags: u16,
    sequence: u16,
    hard_links: u16,
    base_record: FileRef,
    record_number: u32,
    attrs: Vec<Vec<u8>>,
    next_id: u16,
}

impl RecordBuilder {
    pub fn new(size: usize) -> Self {
        RecordBuilder {
            size,
            sector_size: 512,
            flags: crate::record::FLAG_IN_USE,
            sequence: 1,
            hard_links: 1,
            base_record: FileRef(0),
            record_number: 0,
            attrs: Vec::new(),
            next_id: 0,
        }
    }

    pub fn sector_size(mut self, s: usize) -> Self {
        self.sector_size = s;
        self
    }

    pub fn directory(mut self, yes: bool) -> Self {
        if yes {
            self.flags |= crate::record::FLAG_DIRECTORY;
        } else {
            self.flags &= !crate::record::FLAG_DIRECTORY;
        }
        self
    }

    pub fn in_use(mut self, yes: bool) -> Self {
        if yes {
            self.flags |= crate::record::FLAG_IN_USE;
        } else {
            self.flags &= !crate::record::FLAG_IN_USE;
        }
        self
    }

    pub fn sequence(mut self, seq: u16) -> Self {
        self.sequence = seq;
        self
    }

    pub fn hard_links(mut self, n: u16) -> Self {
        self.hard_links = n;
        self
    }

    /// Mark this record as an extension of `base`.
    pub fn extension_of(mut self, base: FileRef) -> Self {
        self.base_record = base;
        self
    }

    pub fn record_number(mut self, n: u32) -> Self {
        self.record_number = n;
        self
    }

    fn take_id(&mut self) -> u16 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Build a resident attribute with an optional name.
    fn resident(&mut self, ty: u32, name: Option<&str>, value: &[u8], flags: u16) -> Vec<u8> {
        let id = self.take_id();
        let name_u16 = name.map(utf16).unwrap_or_default();
        let name_chars = name.map(|n| n.encode_utf16().count()).unwrap_or(0);
        let name_off = 0x18usize;
        let value_off = align8(name_off + name_u16.len());
        let total = align8(value_off + value.len());

        let mut a = vec![0u8; total];
        a[0x00..0x04].copy_from_slice(&ty.to_le_bytes());
        a[0x04..0x08].copy_from_slice(&(total as u32).to_le_bytes());
        a[0x08] = 0; // resident
        a[0x09] = name_chars as u8;
        a[0x0A..0x0C].copy_from_slice(&(name_off as u16).to_le_bytes());
        a[0x0C..0x0E].copy_from_slice(&flags.to_le_bytes());
        a[0x0E..0x10].copy_from_slice(&id.to_le_bytes());
        a[0x10..0x14].copy_from_slice(&(value.len() as u32).to_le_bytes());
        a[0x14..0x16].copy_from_slice(&(value_off as u16).to_le_bytes());
        a[name_off..name_off + name_u16.len()].copy_from_slice(&name_u16);
        a[value_off..value_off + value.len()].copy_from_slice(value);
        a
    }

    /// Build a non-resident attribute carrying a run list.
    #[allow(clippy::too_many_arguments)] // a test fixture mirroring the on-disk layout
    fn non_resident(
        &mut self,
        ty: u32,
        name: Option<&str>,
        runs: &[u8],
        real_size: u64,
        alloc_size: u64,
        start_vcn: u64,
        flags: u16,
    ) -> Vec<u8> {
        let id = self.take_id();
        let name_u16 = name.map(utf16).unwrap_or_default();
        let name_chars = name.map(|n| n.encode_utf16().count()).unwrap_or(0);
        let name_off = 0x40usize;
        let runs_off = align8(name_off + name_u16.len());
        let total = align8(runs_off + runs.len());

        let last_vcn = if alloc_size > 0 { alloc_size / 4096 } else { 0 };

        let mut a = vec![0u8; total];
        a[0x00..0x04].copy_from_slice(&ty.to_le_bytes());
        a[0x04..0x08].copy_from_slice(&(total as u32).to_le_bytes());
        a[0x08] = 1; // non-resident
        a[0x09] = name_chars as u8;
        a[0x0A..0x0C].copy_from_slice(&(name_off as u16).to_le_bytes());
        a[0x0C..0x0E].copy_from_slice(&flags.to_le_bytes());
        a[0x0E..0x10].copy_from_slice(&id.to_le_bytes());
        a[0x10..0x18].copy_from_slice(&start_vcn.to_le_bytes());
        a[0x18..0x20].copy_from_slice(&last_vcn.saturating_sub(1).to_le_bytes());
        a[0x20..0x22].copy_from_slice(&(runs_off as u16).to_le_bytes());
        a[0x28..0x30].copy_from_slice(&alloc_size.to_le_bytes());
        a[0x30..0x38].copy_from_slice(&real_size.to_le_bytes());
        a[0x38..0x40].copy_from_slice(&real_size.to_le_bytes());
        a[name_off..name_off + name_u16.len()].copy_from_slice(&name_u16);
        a[runs_off..runs_off + runs.len()].copy_from_slice(runs);
        a
    }

    pub fn standard_information(mut self, dos_attrs: u32, mtime: u64) -> Self {
        let mut v = vec![0u8; 72];
        v[0x00..0x08].copy_from_slice(&mtime.to_le_bytes()); // creation
        v[0x08..0x10].copy_from_slice(&mtime.to_le_bytes()); // modification
        v[0x10..0x18].copy_from_slice(&mtime.to_le_bytes()); // mft change
        v[0x18..0x20].copy_from_slice(&mtime.to_le_bytes()); // access
        v[0x20..0x24].copy_from_slice(&dos_attrs.to_le_bytes());
        let a = self.resident(attr_type::STANDARD_INFORMATION, None, &v, 0);
        self.attrs.push(a);
        self
    }

    pub fn file_name(self, parent: FileRef, name: &str, ns: u8) -> Self {
        self.file_name_sized(parent, name, ns, 0, 0)
    }

    pub fn file_name_sized(
        mut self,
        parent: FileRef,
        name: &str,
        ns: u8,
        real_size: u64,
        alloc_size: u64,
    ) -> Self {
        let name_u16 = utf16(name);
        let mut v = vec![0u8; 0x42 + name_u16.len()];
        v[0x00..0x08].copy_from_slice(&parent.0.to_le_bytes());
        v[0x28..0x30].copy_from_slice(&alloc_size.to_le_bytes());
        v[0x30..0x38].copy_from_slice(&real_size.to_le_bytes());
        v[0x40] = name.encode_utf16().count() as u8;
        v[0x41] = ns;
        v[0x42..].copy_from_slice(&name_u16);
        let a = self.resident(attr_type::FILE_NAME, None, &v, 0);
        self.attrs.push(a);
        self
    }

    pub fn resident_data(mut self, value: &[u8]) -> Self {
        let a = self.resident(attr_type::DATA, None, value, 0);
        self.attrs.push(a);
        self
    }

    pub fn named_resident_data(mut self, name: &str, value: &[u8]) -> Self {
        let a = self.resident(attr_type::DATA, Some(name), value, 0);
        self.attrs.push(a);
        self
    }

    pub fn non_resident_data(mut self, runs: &[u8], real_size: u64, alloc_size: u64) -> Self {
        let a = self.non_resident(attr_type::DATA, None, runs, real_size, alloc_size, 0, 0);
        self.attrs.push(a);
        self
    }

    /// A continuation fragment: starts at a non-zero VCN and reports no size.
    pub fn non_resident_data_fragment(mut self, runs: &[u8], start_vcn: u64) -> Self {
        let a = self.non_resident(attr_type::DATA, None, runs, 0, 0, start_vcn, 0);
        self.attrs.push(a);
        self
    }

    pub fn sparse_data(mut self, runs: &[u8], real_size: u64, alloc_size: u64) -> Self {
        let a = self.non_resident(
            attr_type::DATA,
            None,
            runs,
            real_size,
            alloc_size,
            0,
            crate::record::attr_flags::SPARSE,
        );
        self.attrs.push(a);
        self
    }

    pub fn named_non_resident_data(mut self, name: &str, runs: &[u8], real_size: u64) -> Self {
        let a = self.non_resident(
            attr_type::DATA,
            Some(name),
            runs,
            real_size,
            real_size,
            0,
            0,
        );
        self.attrs.push(a);
        self
    }

    /// An `$ATTRIBUTE_LIST` naming the records that hold spilled attributes.
    pub fn attribute_list(mut self, entries: &[(u32, u64, FileRef)]) -> Self {
        let mut v = Vec::new();
        for &(ty, start_vcn, reference) in entries {
            let len = 0x20usize;
            let mut e = vec![0u8; len];
            e[0x00..0x04].copy_from_slice(&ty.to_le_bytes());
            e[0x04..0x06].copy_from_slice(&(len as u16).to_le_bytes());
            e[0x06] = 0; // name length
            e[0x07] = 0x1A; // name offset
            e[0x08..0x10].copy_from_slice(&start_vcn.to_le_bytes());
            e[0x10..0x18].copy_from_slice(&reference.0.to_le_bytes());
            v.extend_from_slice(&e);
        }
        let a = self.resident(attr_type::ATTRIBUTE_LIST, None, &v, 0);
        self.attrs.push(a);
        self
    }

    pub fn index_root(mut self) -> Self {
        let a = self.resident(attr_type::INDEX_ROOT, Some("$I30"), &[0u8; 16], 0);
        self.attrs.push(a);
        self
    }

    /// Produce the record with fixups already applied (as the parser sees it
    /// after `apply_fixups`).
    pub fn build(&self) -> Vec<u8> {
        let mut rec = vec![0u8; self.size];
        let sectors = self.size / self.sector_size;
        let usa_count = sectors + 1;
        let first_attr = align8(USA_OFFSET + usa_count * 2);

        rec[0x00..0x04].copy_from_slice(b"FILE");
        rec[0x04..0x06].copy_from_slice(&(USA_OFFSET as u16).to_le_bytes());
        rec[0x06..0x08].copy_from_slice(&(usa_count as u16).to_le_bytes());
        rec[0x10..0x12].copy_from_slice(&self.sequence.to_le_bytes());
        rec[0x12..0x14].copy_from_slice(&self.hard_links.to_le_bytes());
        rec[0x14..0x16].copy_from_slice(&(first_attr as u16).to_le_bytes());
        rec[0x16..0x18].copy_from_slice(&self.flags.to_le_bytes());
        rec[0x20..0x28].copy_from_slice(&self.base_record.0.to_le_bytes());
        rec[0x28..0x2A].copy_from_slice(&self.next_id.to_le_bytes());
        rec[0x2C..0x30].copy_from_slice(&self.record_number.to_le_bytes());

        let mut pos = first_attr;
        for a in &self.attrs {
            assert!(
                pos + a.len() + 8 <= self.size,
                "synthetic record overflowed"
            );
            rec[pos..pos + a.len()].copy_from_slice(a);
            pos += a.len();
        }
        // End-of-attributes marker.
        rec[pos..pos + 4].copy_from_slice(&attr_type::END.to_le_bytes());
        let used = pos + 8;

        rec[0x18..0x1C].copy_from_slice(&(used as u32).to_le_bytes());
        rec[0x1C..0x20].copy_from_slice(&(self.size as u32).to_le_bytes());
        rec
    }

    /// Produce the record in raw on-disk form, with each sector tail replaced
    /// by the update sequence number so `apply_fixups` has real work to do.
    pub fn build_raw(&self, usn: u16) -> Vec<u8> {
        let mut rec = self.build();
        let sectors = self.size / self.sector_size;
        rec[USA_OFFSET..USA_OFFSET + 2].copy_from_slice(&usn.to_le_bytes());
        for i in 0..sectors {
            let tail = (i + 1) * self.sector_size - 2;
            let original = [rec[tail], rec[tail + 1]];
            // Stash the real bytes in the array, stamp the USN into the tail.
            let slot = USA_OFFSET + 2 + i * 2;
            rec[slot..slot + 2].copy_from_slice(&original);
            rec[tail..tail + 2].copy_from_slice(&usn.to_le_bytes());
        }
        rec
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::{apply_fixups, FileRecord};

    /// Blank the update sequence array, which is record metadata rather than
    /// file data: `build` leaves it zeroed while `build_raw` fills it in, so
    /// only the bytes outside it are meaningful to compare.
    fn without_usa(rec: &[u8]) -> Vec<u8> {
        let mut v = rec.to_vec();
        let count = u16::from_le_bytes([rec[0x06], rec[0x07]]) as usize;
        v[USA_OFFSET..USA_OFFSET + count * 2].fill(0);
        v
    }

    #[test]
    fn build_raw_round_trips_through_apply_fixups() {
        // Fill the record far enough that both sector tails land inside real
        // attribute data, so a skipped fixup would corrupt something visible.
        let builder = RecordBuilder::new(1024)
            .file_name(FileRef::ROOT, "roundtrip.txt", namespace::WIN32)
            .resident_data(&[0xAB; 600]);

        let clean = builder.build();
        let mut raw = builder.build_raw(0x5A5A);

        // On disk the tails hold the sequence number, not the real bytes.
        assert_eq!(&raw[510..512], &0x5A5Au16.to_le_bytes());
        assert_eq!(&raw[1022..1024], &0x5A5Au16.to_le_bytes());
        assert_ne!(without_usa(&clean), without_usa(&raw));

        apply_fixups(&mut raw, 512).unwrap();

        assert_eq!(
            without_usa(&clean),
            without_usa(&raw),
            "fixups must restore every data byte"
        );
        assert_eq!(&raw[510..512], &clean[510..512], "first sector tail");
        assert_eq!(&raw[1022..1024], &clean[1022..1024], "second sector tail");
        assert!(FileRecord::parse(&raw).unwrap().is_in_use());
    }
}
