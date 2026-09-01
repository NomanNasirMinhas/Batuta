//! MFT file record parsing: header, update-sequence fixups, attribute walking.

use crate::error::{FileRef, NtfsError, ReadLe, Result};

pub mod attr_type {
    pub const STANDARD_INFORMATION: u32 = 0x10;
    pub const ATTRIBUTE_LIST: u32 = 0x20;
    pub const FILE_NAME: u32 = 0x30;
    pub const OBJECT_ID: u32 = 0x40;
    pub const DATA: u32 = 0x80;
    pub const INDEX_ROOT: u32 = 0x90;
    pub const INDEX_ALLOCATION: u32 = 0xA0;
    pub const BITMAP: u32 = 0xB0;
    pub const REPARSE_POINT: u32 = 0xC0;
    pub const END: u32 = 0xFFFF_FFFF;
}

pub mod attr_flags {
    pub const COMPRESSED: u16 = 0x0001;
    pub const ENCRYPTED: u16 = 0x4000;
    pub const SPARSE: u16 = 0x8000;
}

/// Record header flags.
pub const FLAG_IN_USE: u16 = 0x0001;
pub const FLAG_DIRECTORY: u16 = 0x0002;

const HEADER_MIN: usize = 0x30;

/// Apply the update sequence array to a raw file record, in place.
///
/// NTFS overwrites the last two bytes of every sector in a record with a
/// sequence number so torn writes are detectable, stashing the real bytes in
/// the update sequence array. They must be put back before the record can be
/// parsed. Skipping this silently corrupts the final two bytes of each sector,
/// which is the single most common NTFS parser bug.
pub fn apply_fixups(buf: &mut [u8], bytes_per_sector: u32) -> Result<()> {
    if buf.len() < HEADER_MIN {
        return Err(NtfsError::Truncated {
            need: HEADER_MIN,
            have: buf.len(),
        });
    }
    if &buf[0..4] != b"FILE" {
        return Err(NtfsError::NotAFileRecord);
    }

    let usa_offset = buf.u16_at(0x04).unwrap_or(0) as usize;
    let usa_count = buf.u16_at(0x06).unwrap_or(0) as usize;
    if usa_count == 0 {
        return Err(NtfsError::BadFixup("zero update sequence count"));
    }

    let sector_size = bytes_per_sector as usize;
    if sector_size < 4 {
        return Err(NtfsError::BadFixup("sector size too small"));
    }

    let fixup_count = usa_count - 1;
    if fixup_count.saturating_mul(sector_size) > buf.len() {
        return Err(NtfsError::BadFixup(
            "update sequence array covers more than the record",
        ));
    }
    // The array itself must sit inside the record.
    if usa_offset < 0x2A || usa_offset + usa_count * 2 > buf.len() {
        return Err(NtfsError::BadFixup("update sequence array out of bounds"));
    }

    let usn = buf
        .u16_at(usa_offset)
        .ok_or(NtfsError::BadFixup("missing usn"))?;

    for i in 0..fixup_count {
        let saved = buf
            .u16_at(usa_offset + 2 + i * 2)
            .ok_or(NtfsError::BadFixup("truncated update sequence array"))?;
        let tail = (i + 1) * sector_size - 2;
        let current = buf
            .u16_at(tail)
            .ok_or(NtfsError::BadFixup("record shorter than its sectors"))?;
        if current != usn {
            return Err(NtfsError::BadFixup("sector sequence number mismatch"));
        }
        buf[tail..tail + 2].copy_from_slice(&saved.to_le_bytes());
    }

    Ok(())
}

/// Parsed MFT record header.
#[derive(Debug, Clone, Copy)]
pub struct FileRecord {
    pub sequence: u16,
    pub hard_link_count: u16,
    pub first_attr_offset: u16,
    pub flags: u16,
    pub used_size: u32,
    pub allocated_size: u32,
    /// Zero for a base record; otherwise points at the base record this
    /// extension belongs to.
    pub base_record: FileRef,
    /// Self record number as stored in the record (XP and later); may be zero
    /// on older volumes, so callers should prefer the slot index they read from.
    pub record_number: u32,
}

impl FileRecord {
    #[inline]
    pub fn is_in_use(&self) -> bool {
        self.flags & FLAG_IN_USE != 0
    }

    #[inline]
    pub fn is_directory(&self) -> bool {
        self.flags & FLAG_DIRECTORY != 0
    }

    /// True when this record is an extension of another record rather than a
    /// file in its own right. Extension records must not become index nodes.
    #[inline]
    pub fn is_extension(&self) -> bool {
        self.base_record.0 != 0
    }

    /// Parse the header of a record whose fixups have already been applied.
    pub fn parse(buf: &[u8]) -> Result<Self> {
        if buf.len() < HEADER_MIN {
            return Err(NtfsError::Truncated {
                need: HEADER_MIN,
                have: buf.len(),
            });
        }
        if &buf[0..4] != b"FILE" {
            return Err(NtfsError::NotAFileRecord);
        }

        let first_attr_offset = buf.u16_at(0x14).unwrap_or(0);
        if (first_attr_offset as usize) < HEADER_MIN || first_attr_offset as usize >= buf.len() {
            return Err(NtfsError::BadAttribute(first_attr_offset as usize));
        }

        Ok(FileRecord {
            sequence: buf.u16_at(0x10).unwrap_or(0),
            hard_link_count: buf.u16_at(0x12).unwrap_or(0),
            first_attr_offset,
            flags: buf.u16_at(0x16).unwrap_or(0),
            used_size: buf.u32_at(0x18).unwrap_or(0),
            allocated_size: buf.u32_at(0x1C).unwrap_or(0),
            base_record: FileRef(buf.u64_at(0x20).unwrap_or(0)),
            record_number: buf.u32_at(0x2C).unwrap_or(0),
        })
    }

    /// Iterate the attributes of this record.
    pub fn attributes<'a>(&self, buf: &'a [u8]) -> AttrIter<'a> {
        // `used_size` bounds the meaningful part of the record, but a corrupt
        // value must not let us read past the buffer.
        let limit = (self.used_size as usize).min(buf.len());
        AttrIter {
            buf,
            pos: self.first_attr_offset as usize,
            limit,
            done: false,
        }
    }
}

/// The body of an attribute: either stored inline or described by a run list.
#[derive(Debug, Clone, Copy)]
pub enum AttrBody<'a> {
    Resident(&'a [u8]),
    NonResident {
        start_vcn: u64,
        last_vcn: u64,
        allocated_size: u64,
        real_size: u64,
        initialized_size: u64,
        runs: &'a [u8],
    },
}

#[derive(Debug, Clone, Copy)]
pub struct Attribute<'a> {
    pub ty: u32,
    pub flags: u16,
    pub id: u16,
    /// Raw UTF-16LE attribute name; empty for the unnamed (default) attribute.
    pub name_utf16: &'a [u8],
    pub body: AttrBody<'a>,
}

impl<'a> Attribute<'a> {
    /// True for the default unnamed stream. Named `$DATA` attributes are
    /// alternate data streams, excluded from size totals by default.
    #[inline]
    pub fn is_unnamed(&self) -> bool {
        self.name_utf16.is_empty()
    }

    #[inline]
    pub fn is_sparse_or_compressed(&self) -> bool {
        self.flags & (attr_flags::SPARSE | attr_flags::COMPRESSED) != 0
    }

    /// Logical size of the attribute data.
    pub fn data_size(&self) -> u64 {
        match self.body {
            AttrBody::Resident(v) => v.len() as u64,
            AttrBody::NonResident { real_size, .. } => real_size,
        }
    }

    /// Space actually occupied on disk.
    pub fn allocated_size(&self) -> u64 {
        match self.body {
            AttrBody::Resident(v) => v.len() as u64,
            AttrBody::NonResident { allocated_size, .. } => allocated_size,
        }
    }

    /// Only the first fragment of a multi-part attribute carries the real size;
    /// continuations start at a non-zero VCN and report zero.
    #[inline]
    pub fn is_first_fragment(&self) -> bool {
        match self.body {
            AttrBody::Resident(_) => true,
            AttrBody::NonResident { start_vcn, .. } => start_vcn == 0,
        }
    }
}

pub struct AttrIter<'a> {
    buf: &'a [u8],
    pos: usize,
    limit: usize,
    done: bool,
}

impl<'a> Iterator for AttrIter<'a> {
    type Item = Result<Attribute<'a>>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done || self.pos.saturating_add(8) > self.limit {
            return None;
        }
        let base = self.pos;
        let ty = self.buf.u32_at(base)?;
        if ty == attr_type::END {
            self.done = true;
            return None;
        }
        let len = self.buf.u32_at(base + 4).unwrap_or(0) as usize;
        // A zero or unaligned length would loop forever or step off the record.
        if len < 16 || !len.is_multiple_of(8) || base + len > self.limit {
            self.done = true;
            return Some(Err(NtfsError::BadAttribute(base)));
        }
        self.pos = base + len;

        let non_resident = self.buf.u8_at(base + 0x08).unwrap_or(0) != 0;
        let name_len = self.buf.u8_at(base + 0x09).unwrap_or(0) as usize * 2;
        let name_off = self.buf.u16_at(base + 0x0A).unwrap_or(0) as usize;
        let flags = self.buf.u16_at(base + 0x0C).unwrap_or(0);
        let id = self.buf.u16_at(base + 0x0E).unwrap_or(0);

        let name_utf16: &[u8] = if name_len == 0 {
            &[]
        } else if name_off + name_len <= len {
            match self.buf.get(base + name_off..base + name_off + name_len) {
                Some(s) => s,
                None => {
                    self.done = true;
                    return Some(Err(NtfsError::BadAttribute(base)));
                }
            }
        } else {
            self.done = true;
            return Some(Err(NtfsError::BadAttribute(base)));
        };

        let body = if non_resident {
            let runs_off = self.buf.u16_at(base + 0x20).unwrap_or(0) as usize;
            if runs_off < 0x40 || runs_off > len {
                self.done = true;
                return Some(Err(NtfsError::BadAttribute(base)));
            }
            AttrBody::NonResident {
                start_vcn: self.buf.u64_at(base + 0x10).unwrap_or(0),
                last_vcn: self.buf.u64_at(base + 0x18).unwrap_or(0),
                allocated_size: self.buf.u64_at(base + 0x28).unwrap_or(0),
                real_size: self.buf.u64_at(base + 0x30).unwrap_or(0),
                initialized_size: self.buf.u64_at(base + 0x38).unwrap_or(0),
                runs: &self.buf[base + runs_off..base + len],
            }
        } else {
            let val_len = self.buf.u32_at(base + 0x10).unwrap_or(0) as usize;
            let val_off = self.buf.u16_at(base + 0x14).unwrap_or(0) as usize;
            if val_off.saturating_add(val_len) > len {
                self.done = true;
                return Some(Err(NtfsError::BadAttribute(base)));
            }
            match self.buf.get(base + val_off..base + val_off + val_len) {
                Some(v) => AttrBody::Resident(v),
                None => {
                    self.done = true;
                    return Some(Err(NtfsError::BadAttribute(base)));
                }
            }
        };

        Some(Ok(Attribute {
            ty,
            flags,
            id,
            name_utf16,
            body,
        }))
    }
}

/// Decode a UTF-16LE name, replacing unpaired surrogates rather than failing.
pub fn decode_utf16(raw: &[u8]) -> String {
    let units: Vec<u16> = raw
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    char::decode_utf16(units)
        .map(|r| r.unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::RecordBuilder;

    #[test]
    fn fixups_restore_sector_tails() {
        let mut rec = vec![0u8; 1024];
        rec[0..4].copy_from_slice(b"FILE");
        rec[0x04..0x06].copy_from_slice(&0x30u16.to_le_bytes());
        rec[0x06..0x08].copy_from_slice(&3u16.to_le_bytes()); // usn + 2 sectors
        let usn: u16 = 0xBEEF;
        rec[0x30..0x32].copy_from_slice(&usn.to_le_bytes());
        rec[0x32..0x34].copy_from_slice(&0x1111u16.to_le_bytes());
        rec[0x34..0x36].copy_from_slice(&0x2222u16.to_le_bytes());
        // Sector tails currently hold the USN, as they do on disk.
        rec[510..512].copy_from_slice(&usn.to_le_bytes());
        rec[1022..1024].copy_from_slice(&usn.to_le_bytes());

        apply_fixups(&mut rec, 512).unwrap();
        assert_eq!(&rec[510..512], &0x1111u16.to_le_bytes());
        assert_eq!(&rec[1022..1024], &0x2222u16.to_le_bytes());
    }

    #[test]
    fn fixup_mismatch_is_rejected() {
        let mut rec = vec![0u8; 1024];
        rec[0..4].copy_from_slice(b"FILE");
        rec[0x04..0x06].copy_from_slice(&0x30u16.to_le_bytes());
        rec[0x06..0x08].copy_from_slice(&3u16.to_le_bytes());
        rec[0x30..0x32].copy_from_slice(&0xBEEFu16.to_le_bytes());
        rec[510..512].copy_from_slice(&0xDEADu16.to_le_bytes()); // torn write
        assert!(matches!(
            apply_fixups(&mut rec, 512),
            Err(NtfsError::BadFixup(_))
        ));
    }

    #[test]
    fn rejects_non_file_record() {
        let mut rec = vec![0u8; 1024];
        rec[0..4].copy_from_slice(b"BAAD");
        assert!(matches!(
            apply_fixups(&mut rec, 512),
            Err(NtfsError::NotAFileRecord)
        ));
    }

    #[test]
    fn walks_resident_attributes() {
        let rec = RecordBuilder::new(1024)
            .standard_information(0x0020, 130_000_000_000_000_000)
            .file_name(FileRef(5), "hello.txt", 1)
            .resident_data(b"abcdef")
            .build();

        let hdr = FileRecord::parse(&rec).unwrap();
        assert!(hdr.is_in_use());
        assert!(!hdr.is_directory());
        assert!(!hdr.is_extension());

        let attrs: Vec<_> = hdr.attributes(&rec).map(|a| a.unwrap()).collect();
        assert_eq!(attrs.len(), 3);
        assert_eq!(attrs[0].ty, attr_type::STANDARD_INFORMATION);
        assert_eq!(attrs[1].ty, attr_type::FILE_NAME);
        assert_eq!(attrs[2].ty, attr_type::DATA);
        assert_eq!(attrs[2].data_size(), 6);
        assert!(attrs[2].is_unnamed());
    }

    #[test]
    fn walks_non_resident_data_and_exposes_runs() {
        let rec = RecordBuilder::new(1024)
            .file_name(FileRef(5), "big.bin", 1)
            .non_resident_data(&[0x21, 0x18, 0x33, 0x02, 0x00], 100_000, 98_304)
            .build();

        let hdr = FileRecord::parse(&rec).unwrap();
        let data = hdr
            .attributes(&rec)
            .map(|a| a.unwrap())
            .find(|a| a.ty == attr_type::DATA)
            .unwrap();
        assert_eq!(data.data_size(), 100_000);
        assert_eq!(data.allocated_size(), 98_304);
        assert!(data.is_first_fragment());
        match data.body {
            AttrBody::NonResident { runs, .. } => {
                let rl = crate::runlist::RunList::parse(runs, None).unwrap();
                assert_eq!(rl.runs.len(), 1);
                assert_eq!(rl.runs[0].lcn, Some(0x0233));
            }
            _ => panic!("expected non-resident"),
        }
    }

    #[test]
    fn named_data_stream_is_distinguishable_from_default() {
        let rec = RecordBuilder::new(1024)
            .file_name(FileRef(5), "doc.txt", 1)
            .resident_data(b"main")
            .named_resident_data("Zone.Identifier", b"[ZoneTransfer]")
            .build();
        let hdr = FileRecord::parse(&rec).unwrap();
        let datas: Vec<_> = hdr
            .attributes(&rec)
            .map(|a| a.unwrap())
            .filter(|a| a.ty == attr_type::DATA)
            .collect();
        assert_eq!(datas.len(), 2);
        assert!(datas[0].is_unnamed());
        assert!(!datas[1].is_unnamed());
        assert_eq!(decode_utf16(datas[1].name_utf16), "Zone.Identifier");
    }

    #[test]
    fn malformed_attribute_length_terminates_iteration() {
        let mut rec = RecordBuilder::new(1024)
            .file_name(FileRef(5), "x", 1)
            .resident_data(b"y")
            .build();
        let first_off = FileRecord::parse(&rec).unwrap().first_attr_offset as usize;
        // A zero length would otherwise spin forever.
        rec[first_off + 4..first_off + 8].copy_from_slice(&0u32.to_le_bytes());
        let hdr = FileRecord::parse(&rec).unwrap();
        let results: Vec<_> = hdr.attributes(&rec).collect();
        assert_eq!(results.len(), 1);
        assert!(results[0].is_err());
    }

    #[test]
    fn decodes_utf16_names() {
        let raw: Vec<u8> = "café.txt"
            .encode_utf16()
            .flat_map(|u| u.to_le_bytes())
            .collect();
        assert_eq!(decode_utf16(&raw), "café.txt");
    }
}
