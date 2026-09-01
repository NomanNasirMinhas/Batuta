//! Enumerating the volumes Quick Setup can offer.

/// One volume, as shown in step 2.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriveInfo {
    pub letter: char,
    pub label: String,
    pub filesystem: String,
    pub total: u64,
    pub free: u64,
    /// Whether the change journal is active, when known.
    pub journal: Journal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Journal {
    Active,
    /// No journal; real-time tracking needs one created first.
    Absent,
    Unknown,
}

impl DriveInfo {
    /// Only NTFS can be indexed: everything here reads the MFT.
    pub fn selectable(&self) -> bool {
        self.filesystem.eq_ignore_ascii_case("NTFS")
    }

    /// The line shown in the picker.
    ///
    /// Volumes that cannot be indexed are still listed, with the reason. A
    /// drive the user can see in Explorer silently missing from the list would
    /// look like a bug rather than a limitation.
    pub fn describe(&self) -> String {
        let name = if self.label.is_empty() {
            String::new()
        } else {
            format!(" {}", self.label)
        };
        let mut line = format!(
            "{}:{name}  {} free of {}  {}",
            self.letter,
            crate::fmt::bytes(self.free),
            crate::fmt::bytes(self.total),
            self.filesystem,
        );
        if !self.selectable() {
            line.push_str("  — not NTFS, cannot be indexed");
        } else if self.journal == Journal::Absent {
            line.push_str("  — no change journal yet, will be created for live tracking");
        }
        line
    }
}

#[cfg(windows)]
pub use imp::list;

#[cfg(windows)]
mod imp {
    use super::*;
    use std::ptr;

    use windows_sys::Win32::Storage::FileSystem::{
        GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW,
    };

    /// `GetDriveTypeW` returns a bare u32; windows-sys does not name this one.
    const DRIVE_FIXED: u32 = 3;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn utf16_to_string(buf: &[u16]) -> String {
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..end])
    }

    /// Every fixed local volume, in drive-letter order.
    pub fn list() -> Vec<DriveInfo> {
        let mask = unsafe { GetLogicalDrives() };
        let mut out = Vec::new();

        for i in 0..26u32 {
            if mask & (1 << i) == 0 {
                continue;
            }
            let letter = (b'A' + i as u8) as char;
            let root = format!("{letter}:\\");
            let root_w = wide(&root);

            // Removable and network volumes are out of scope: the index is
            // built from a raw volume handle and kept live by that volume's
            // change journal, neither of which survives the drive going away.
            if unsafe { GetDriveTypeW(root_w.as_ptr()) } != DRIVE_FIXED {
                continue;
            }

            let mut label = [0u16; 256];
            let mut fs = [0u16; 64];
            let ok = unsafe {
                GetVolumeInformationW(
                    root_w.as_ptr(),
                    label.as_mut_ptr(),
                    label.len() as u32,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                    fs.as_mut_ptr(),
                    fs.len() as u32,
                )
            };
            if ok == 0 {
                continue;
            }

            let (mut free, mut total) = (0u64, 0u64);
            unsafe {
                GetDiskFreeSpaceExW(root_w.as_ptr(), ptr::null_mut(), &mut total, &mut free);
            }

            let filesystem = utf16_to_string(&fs);
            let journal = if filesystem.eq_ignore_ascii_case("NTFS") {
                match batuta_ntfs::usn::query(letter) {
                    Ok(batuta_ntfs::usn::JournalStatus::Active(_)) => Journal::Active,
                    Ok(batuta_ntfs::usn::JournalStatus::NotActive) => Journal::Absent,
                    _ => Journal::Unknown,
                }
            } else {
                Journal::Unknown
            };

            out.push(DriveInfo {
                letter,
                label: utf16_to_string(&label),
                filesystem,
                total,
                free,
                journal,
            });
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drive(letter: char, fs: &str, journal: Journal) -> DriveInfo {
        DriveInfo {
            letter,
            label: "Data".into(),
            filesystem: fs.into(),
            total: 1_292_500_000_000,
            free: 674_300_000_000,
            journal,
        }
    }

    #[test]
    fn only_ntfs_can_be_indexed() {
        assert!(drive('C', "NTFS", Journal::Active).selectable());
        assert!(drive('C', "ntfs", Journal::Active).selectable());
        assert!(!drive('E', "exFAT", Journal::Unknown).selectable());
        assert!(!drive('F', "FAT32", Journal::Unknown).selectable());
        assert!(!drive('G', "ReFS", Journal::Unknown).selectable());
    }

    #[test]
    fn unusable_volumes_are_described_with_the_reason() {
        // Listing them but saying why beats omitting them, which would read as
        // a missing drive rather than an unsupported one.
        let d = drive('E', "exFAT", Journal::Unknown).describe();
        assert!(d.contains("E:"), "{d}");
        assert!(d.contains("exFAT"), "{d}");
        assert!(d.contains("cannot be indexed"), "{d}");
    }

    #[test]
    fn a_missing_journal_is_called_out() {
        // D: on this machine has no journal, so live tracking needs one made.
        let d = drive('D', "NTFS", Journal::Absent).describe();
        assert!(d.contains("no change journal"), "{d}");

        let d = drive('C', "NTFS", Journal::Active).describe();
        assert!(!d.contains("no change journal"), "{d}");
    }

    #[test]
    fn the_description_carries_sizes_and_label() {
        let d = drive('D', "NTFS", Journal::Active).describe();
        assert!(d.contains("Data"), "{d}");
        assert!(d.contains("free of"), "{d}");
    }

    #[test]
    fn a_volume_with_no_label_still_renders() {
        let mut d = drive('C', "NTFS", Journal::Active);
        d.label = String::new();
        let line = d.describe();
        assert!(line.starts_with("C:  "), "{line}");
    }
}
