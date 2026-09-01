//! Decoding of NTFS data runs (the cluster map of a non-resident attribute).
//!
//! A run list is a sequence of variable-width entries. Each begins with a
//! header byte whose low nibble gives the byte-width of the run *length* and
//! whose high nibble gives the byte-width of the run *offset*. The offset is
//! signed and relative to the previous run's LCN; a zero-width offset marks a
//! sparse run (a hole with no backing clusters). A zero header terminates.

use crate::error::{NtfsError, Result};

/// One contiguous extent of a non-resident attribute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Run {
    /// Virtual cluster number where this extent starts within the attribute.
    pub vcn: u64,
    /// Logical cluster number on the volume, or `None` for a sparse hole.
    pub lcn: Option<u64>,
    pub length: u64,
}

/// The full cluster map of a non-resident attribute.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunList {
    pub runs: Vec<Run>,
}

impl RunList {
    /// Total number of clusters covered, sparse holes included.
    pub fn cluster_count(&self) -> u64 {
        self.runs.iter().map(|r| r.length).sum()
    }

    /// Walk the runs, yielding only the parts backed by real clusters.
    pub fn extents(&self) -> impl Iterator<Item = (u64, u64)> + '_ {
        self.runs
            .iter()
            .filter_map(|r| r.lcn.map(|lcn| (lcn, r.length)))
    }

    /// Parse a run list, stopping at the terminator or the end of `buf`.
    ///
    /// `max_clusters`, when set, caps the total decoded size; a corrupt run
    /// list must not be able to make us allocate or read unbounded amounts.
    pub fn parse(buf: &[u8], max_clusters: Option<u64>) -> Result<Self> {
        let mut runs = Vec::new();
        let mut pos = 0usize;
        let mut vcn = 0u64;
        let mut prev_lcn = 0i64;

        loop {
            let header = match buf.get(pos) {
                Some(0) | None => break,
                Some(&h) => h,
            };
            let start = pos;
            pos += 1;

            let len_size = (header & 0x0F) as usize;
            let off_size = (header >> 4) as usize;

            // Both fields are at most 8 bytes; anything wider is corruption.
            if len_size == 0 || len_size > 8 || off_size > 8 {
                return Err(NtfsError::BadRunList(start));
            }

            let length = read_uint(buf, pos, len_size).ok_or(NtfsError::BadRunList(start))?;
            pos += len_size;

            let lcn = if off_size == 0 {
                None // sparse run
            } else {
                let delta = read_int(buf, pos, off_size).ok_or(NtfsError::BadRunList(start))?;
                pos += off_size;
                prev_lcn = prev_lcn
                    .checked_add(delta)
                    .ok_or(NtfsError::BadRunList(start))?;
                if prev_lcn < 0 {
                    return Err(NtfsError::BadRunList(start));
                }
                Some(prev_lcn as u64)
            };

            if length == 0 {
                return Err(NtfsError::BadRunList(start));
            }
            vcn = vcn
                .checked_add(length)
                .ok_or(NtfsError::BadRunList(start))?;
            if let Some(max) = max_clusters {
                if vcn > max {
                    return Err(NtfsError::BadRunList(start));
                }
            }

            runs.push(Run {
                vcn: vcn - length,
                lcn,
                length,
            });
        }

        Ok(RunList { runs })
    }
}

/// Read an unsigned little-endian integer of `size` bytes (size <= 8).
fn read_uint(buf: &[u8], off: usize, size: usize) -> Option<u64> {
    let bytes = buf.get(off..off + size)?;
    let mut v = 0u64;
    for (i, &b) in bytes.iter().enumerate() {
        v |= (b as u64) << (i * 8);
    }
    Some(v)
}

/// Read a signed little-endian integer of `size` bytes, sign-extending from
/// the top bit of the final byte.
fn read_int(buf: &[u8], off: usize, size: usize) -> Option<i64> {
    let raw = read_uint(buf, off, size)?;
    if size == 8 {
        return Some(raw as i64);
    }
    let sign_bit = 1u64 << (size * 8 - 1);
    Some(if raw & sign_bit != 0 {
        (raw | !((1u64 << (size * 8)) - 1)) as i64
    } else {
        raw as i64
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_run() {
        // 0x21: 1 length byte, 2 offset bytes. length=0x18, offset=0x0233.
        let rl = RunList::parse(&[0x21, 0x18, 0x33, 0x02, 0x00], None).unwrap();
        assert_eq!(
            rl.runs,
            vec![Run {
                vcn: 0,
                lcn: Some(0x0233),
                length: 0x18
            }]
        );
        assert_eq!(rl.cluster_count(), 0x18);
    }

    #[test]
    fn offsets_are_relative_and_signed() {
        // Second run steps backwards on disk via a negative delta.
        let data = [
            0x21, 0x10, 0x00, 0x01, // len 0x10 @ lcn 0x0100
            0x11, 0x08, 0xF0, // len 0x08, delta -0x10 -> lcn 0x00F0
            0x00,
        ];
        let rl = RunList::parse(&data, None).unwrap();
        assert_eq!(
            rl.runs,
            vec![
                Run {
                    vcn: 0,
                    lcn: Some(0x0100),
                    length: 0x10
                },
                Run {
                    vcn: 0x10,
                    lcn: Some(0x00F0),
                    length: 0x08
                },
            ]
        );
    }

    #[test]
    fn sparse_run_has_no_lcn_and_does_not_move_prev() {
        let data = [
            0x21, 0x10, 0x00, 0x01, // len 0x10 @ lcn 0x0100
            0x01, 0x20, // sparse hole, len 0x20, no offset field
            0x11, 0x08, 0x10, // len 0x08, delta +0x10 from 0x0100 -> 0x0110
            0x00,
        ];
        let rl = RunList::parse(&data, None).unwrap();
        assert_eq!(
            rl.runs[1],
            Run {
                vcn: 0x10,
                lcn: None,
                length: 0x20
            }
        );
        assert_eq!(
            rl.runs[2],
            Run {
                vcn: 0x30,
                lcn: Some(0x0110),
                length: 0x08
            }
        );
        // Sparse holes contribute clusters but no readable extents.
        assert_eq!(rl.cluster_count(), 0x38);
        assert_eq!(rl.extents().count(), 2);
    }

    #[test]
    fn terminates_on_zero_and_on_buffer_end() {
        assert!(RunList::parse(&[0x00], None).unwrap().runs.is_empty());
        assert!(RunList::parse(&[], None).unwrap().runs.is_empty());
        // Trailing bytes after the terminator are ignored.
        let rl = RunList::parse(&[0x11, 0x04, 0x02, 0x00, 0xFF, 0xFF], None).unwrap();
        assert_eq!(rl.runs.len(), 1);
    }

    #[test]
    fn rejects_corruption() {
        // Length field wider than 8 bytes.
        assert!(RunList::parse(&[0x09, 1, 2, 3, 4, 5, 6, 7, 8, 9], None).is_err());
        // Header promises more bytes than the buffer holds.
        assert!(RunList::parse(&[0x21, 0x18], None).is_err());
        // Zero-length run.
        assert!(RunList::parse(&[0x11, 0x00, 0x02, 0x00], None).is_err());
        // Negative resulting LCN.
        assert!(RunList::parse(&[0x11, 0x04, 0xF0, 0x00], None).is_err());
    }

    #[test]
    fn enforces_cluster_cap() {
        let data = [0x21, 0xFF, 0xFF, 0x00, 0x00];
        assert!(RunList::parse(&data, Some(100)).is_err());
        assert!(RunList::parse(&data, Some(0xFFFF)).is_ok());
    }
}
