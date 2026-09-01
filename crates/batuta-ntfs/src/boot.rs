//! NTFS boot sector (BPB) parsing.

use crate::error::{NtfsError, ReadLe, Result};

/// Geometry of an NTFS volume, read from its boot sector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BootSector {
    pub bytes_per_sector: u32,
    pub sectors_per_cluster: u32,
    pub total_sectors: u64,
    /// Logical cluster number where `$MFT` begins.
    pub mft_lcn: u64,
    pub mft_mirror_lcn: u64,
    /// Size of one MFT file record, in bytes (almost always 1024).
    pub file_record_size: u32,
    pub index_record_size: u32,
    pub volume_serial: u64,
}

impl BootSector {
    #[inline]
    pub const fn cluster_size(&self) -> u64 {
        self.bytes_per_sector as u64 * self.sectors_per_cluster as u64
    }

    #[inline]
    pub const fn mft_offset(&self) -> u64 {
        self.mft_lcn * self.cluster_size()
    }

    /// Parse the 512-byte boot sector at the start of an NTFS volume.
    pub fn parse(buf: &[u8]) -> Result<Self> {
        if buf.len() < 512 {
            return Err(NtfsError::Truncated {
                need: 512,
                have: buf.len(),
            });
        }
        if &buf[3..11] != b"NTFS    " {
            return Err(NtfsError::NotNtfs);
        }

        let bytes_per_sector = buf.u16_at(0x0B).unwrap_or(0) as u32;
        if !matches!(bytes_per_sector, 256 | 512 | 1024 | 2048 | 4096) {
            return Err(NtfsError::BadBootSector("bytes per sector"));
        }

        // Values above 0x80 encode a power of two: 2^(256 - value) sectors.
        // This shows up on volumes formatted with very large clusters.
        let raw_spc = buf.u8_at(0x0D).unwrap_or(0);
        let sectors_per_cluster = match raw_spc {
            0 => return Err(NtfsError::BadBootSector("sectors per cluster is zero")),
            v if v <= 0x80 => {
                if !v.is_power_of_two() {
                    return Err(NtfsError::BadBootSector(
                        "sectors per cluster not a power of two",
                    ));
                }
                v as u32
            }
            v => {
                let shift = 256u32 - v as u32;
                if shift >= 32 {
                    return Err(NtfsError::BadBootSector(
                        "sectors per cluster shift too large",
                    ));
                }
                1u32 << shift
            }
        };

        let total_sectors = buf.u64_at(0x28).unwrap_or(0);
        let mft_lcn = buf.u64_at(0x30).unwrap_or(0);
        let mft_mirror_lcn = buf.u64_at(0x38).unwrap_or(0);
        let cluster_size = bytes_per_sector as u64 * sectors_per_cluster as u64;

        let file_record_size = decode_record_size(buf.i8_at(0x40).unwrap_or(0), cluster_size)
            .ok_or(NtfsError::BadBootSector("file record size"))?;
        let index_record_size = decode_record_size(buf.i8_at(0x44).unwrap_or(0), cluster_size)
            .ok_or(NtfsError::BadBootSector("index record size"))?;

        // A record must hold at least a header and be sector-aligned, otherwise
        // fixup application below would be meaningless.
        if file_record_size < 48 || file_record_size % bytes_per_sector != 0 {
            return Err(NtfsError::BadBootSector(
                "file record size not sector aligned",
            ));
        }

        Ok(BootSector {
            bytes_per_sector,
            sectors_per_cluster,
            total_sectors,
            mft_lcn,
            mft_mirror_lcn,
            file_record_size,
            index_record_size,
            volume_serial: buf.u64_at(0x48).unwrap_or(0),
        })
    }
}

/// `clusters_per_record` is signed: positive means "this many clusters",
/// negative means "2^-value bytes".
fn decode_record_size(raw: i8, cluster_size: u64) -> Option<u32> {
    if raw > 0 {
        u32::try_from(raw as u64 * cluster_size).ok()
    } else {
        let shift = raw.unsigned_abs() as u32;
        if shift >= 32 {
            return None;
        }
        Some(1u32 << shift)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A boot sector matching a typical 4K-cluster NTFS volume.
    fn sample() -> Vec<u8> {
        let mut b = vec![0u8; 512];
        b[0..3].copy_from_slice(&[0xEB, 0x52, 0x90]);
        b[3..11].copy_from_slice(b"NTFS    ");
        b[0x0B..0x0D].copy_from_slice(&512u16.to_le_bytes());
        b[0x0D] = 8; // 8 * 512 = 4096 byte clusters
        b[0x28..0x30].copy_from_slice(&1_000_000u64.to_le_bytes());
        b[0x30..0x38].copy_from_slice(&786_432u64.to_le_bytes());
        b[0x38..0x40].copy_from_slice(&2u64.to_le_bytes());
        b[0x40] = (-10i8) as u8; // 2^10 = 1024 byte records
        b[0x44] = (-12i8) as u8; // 2^12 = 4096 byte index records
        b[0x48..0x50].copy_from_slice(&0xDEAD_BEEF_CAFE_1234u64.to_le_bytes());
        b[510] = 0x55;
        b[511] = 0xAA;
        b
    }

    #[test]
    fn parses_typical_volume() {
        let bs = BootSector::parse(&sample()).unwrap();
        assert_eq!(bs.bytes_per_sector, 512);
        assert_eq!(bs.sectors_per_cluster, 8);
        assert_eq!(bs.cluster_size(), 4096);
        assert_eq!(bs.file_record_size, 1024);
        assert_eq!(bs.index_record_size, 4096);
        assert_eq!(bs.mft_lcn, 786_432);
        assert_eq!(bs.mft_offset(), 786_432 * 4096);
        assert_eq!(bs.volume_serial, 0xDEAD_BEEF_CAFE_1234);
    }

    #[test]
    fn positive_record_size_means_clusters() {
        let mut b = sample();
        b[0x0D] = 1; // 512 byte clusters
        b[0x40] = 2; // 2 clusters = 1024 bytes
        let bs = BootSector::parse(&b).unwrap();
        assert_eq!(bs.cluster_size(), 512);
        assert_eq!(bs.file_record_size, 1024);
    }

    #[test]
    fn large_cluster_encoding() {
        let mut b = sample();
        b[0x0D] = 0xF4; // 2^(256-244) = 4096 sectors per cluster
        let bs = BootSector::parse(&b).unwrap();
        assert_eq!(bs.sectors_per_cluster, 4096);
    }

    #[test]
    fn rejects_non_ntfs() {
        let mut b = sample();
        b[3..11].copy_from_slice(b"FAT32   ");
        assert!(matches!(BootSector::parse(&b), Err(NtfsError::NotNtfs)));
    }

    #[test]
    fn rejects_bad_geometry() {
        let mut b = sample();
        b[0x0B..0x0D].copy_from_slice(&777u16.to_le_bytes());
        assert!(BootSector::parse(&b).is_err());

        let mut b = sample();
        b[0x0D] = 0;
        assert!(BootSector::parse(&b).is_err());

        // Record size smaller than a record header must be rejected.
        let mut b = sample();
        b[0x40] = (-4i8) as u8; // 16 bytes
        assert!(BootSector::parse(&b).is_err());
    }

    #[test]
    fn rejects_short_buffer() {
        assert!(matches!(
            BootSector::parse(&[0u8; 100]),
            Err(NtfsError::Truncated { .. })
        ));
    }
}
