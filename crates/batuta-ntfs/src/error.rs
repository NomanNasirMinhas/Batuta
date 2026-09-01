use std::fmt;

/// Errors produced while reading or parsing NTFS structures.
#[derive(Debug, thiserror::Error)]
pub enum NtfsError {
    #[error("not an NTFS volume (bad OEM id)")]
    NotNtfs,

    #[error("invalid boot sector: {0}")]
    BadBootSector(&'static str),

    /// The record did not start with the `FILE` magic. Usually means the slot
    /// has never been used, which is normal when sweeping the MFT.
    #[error("not a FILE record")]
    NotAFileRecord,

    #[error("update sequence array is malformed: {0}")]
    BadFixup(&'static str),

    #[error("attribute is truncated or malformed at offset {0}")]
    BadAttribute(usize),

    #[error("data run list is malformed at offset {0}")]
    BadRunList(usize),

    #[error("buffer too small: need {need} bytes, have {have}")]
    Truncated { need: usize, have: usize },

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, NtfsError>;

/// Little-endian primitive reads that return `None` instead of panicking.
///
/// Every parser in this crate runs over bytes read straight off a raw volume,
/// so a corrupt or hostile structure must never be able to cause a panic in
/// what will be a LocalSystem process.
pub(crate) trait ReadLe {
    fn u8_at(&self, off: usize) -> Option<u8>;
    fn u16_at(&self, off: usize) -> Option<u16>;
    fn u32_at(&self, off: usize) -> Option<u32>;
    fn u64_at(&self, off: usize) -> Option<u64>;
    fn i8_at(&self, off: usize) -> Option<i8>;
}

impl ReadLe for [u8] {
    #[inline]
    fn u8_at(&self, off: usize) -> Option<u8> {
        self.get(off).copied()
    }
    #[inline]
    fn u16_at(&self, off: usize) -> Option<u16> {
        Some(u16::from_le_bytes(self.get(off..off + 2)?.try_into().ok()?))
    }
    #[inline]
    fn u32_at(&self, off: usize) -> Option<u32> {
        Some(u32::from_le_bytes(self.get(off..off + 4)?.try_into().ok()?))
    }
    #[inline]
    fn u64_at(&self, off: usize) -> Option<u64> {
        Some(u64::from_le_bytes(self.get(off..off + 8)?.try_into().ok()?))
    }
    #[inline]
    fn i8_at(&self, off: usize) -> Option<i8> {
        self.get(off).map(|b| *b as i8)
    }
}

/// A 64-bit NTFS file reference: low 48 bits are the MFT record number,
/// high 16 bits are the sequence number.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct FileRef(pub u64);

impl FileRef {
    pub const ROOT: FileRef = FileRef(5);

    #[inline]
    pub const fn record(self) -> u64 {
        self.0 & 0x0000_FFFF_FFFF_FFFF
    }

    #[inline]
    pub const fn sequence(self) -> u16 {
        (self.0 >> 48) as u16
    }
}

impl fmt::Debug for FileRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FileRef({}#{})", self.record(), self.sequence())
    }
}
