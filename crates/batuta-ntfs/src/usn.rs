//! The NTFS change journal (USN journal).
//!
//! This is what makes folder sizes live. After the MFT snapshot records a
//! starting USN, a reader thread blocks inside `FSCTL_READ_USN_JOURNAL` with
//! `BytesToWaitFor` set, so it parks in the kernel until something changes
//! rather than polling: idle cost is genuinely zero.
//!
//! Record *parsing* is separated from the Windows calls so it can be tested on
//! synthetic buffers without a volume or elevation.
//!
//! Probing this machine established the access rules, which differ per
//! operation: `FSCTL_QUERY_USN_JOURNAL` succeeds unelevated through a
//! root-directory handle, while `FSCTL_READ_USN_JOURNAL` and
//! `FSCTL_ENUM_USN_DATA` return `ACCESS_DENIED` through either handle. So
//! status reporting works for any user; actually following changes needs
//! Administrator.

use crate::error::{FileRef, NtfsError, ReadLe, Result};

/// Reasons a USN record can be emitted.
pub mod reason {
    pub const DATA_OVERWRITE: u32 = 0x0000_0001;
    pub const DATA_EXTEND: u32 = 0x0000_0002;
    pub const DATA_TRUNCATION: u32 = 0x0000_0004;
    pub const FILE_CREATE: u32 = 0x0000_0100;
    pub const FILE_DELETE: u32 = 0x0000_0200;
    pub const RENAME_OLD_NAME: u32 = 0x0000_1000;
    pub const RENAME_NEW_NAME: u32 = 0x0000_2000;
    pub const BASIC_INFO_CHANGE: u32 = 0x0000_8000;
    pub const HARD_LINK_CHANGE: u32 = 0x0001_0000;
    pub const STREAM_CHANGE: u32 = 0x0020_0000;
    pub const CLOSE: u32 = 0x8000_0000;

    /// Everything that can change a name, a parent, or a size. Filtering at
    /// the kernel boundary keeps security-descriptor and access-time churn
    /// from ever reaching us.
    pub const INTERESTING: u32 = DATA_OVERWRITE
        | DATA_EXTEND
        | DATA_TRUNCATION
        | FILE_CREATE
        | FILE_DELETE
        | RENAME_OLD_NAME
        | RENAME_NEW_NAME
        | HARD_LINK_CHANGE
        | CLOSE;
}

/// One change record, with its name still borrowed from the read buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UsnRecord<'a> {
    pub usn: i64,
    pub file_ref: FileRef,
    pub parent_ref: FileRef,
    pub reason: u32,
    pub attributes: u32,
    /// Raw UTF-16LE name.
    pub name_utf16: &'a [u8],
}

impl UsnRecord<'_> {
    #[inline]
    pub fn is_directory(&self) -> bool {
        self.attributes & 0x10 != 0
    }

    #[inline]
    pub fn has(&self, mask: u32) -> bool {
        self.reason & mask != 0
    }

    /// A change that could alter the file's size.
    #[inline]
    pub fn affects_size(&self) -> bool {
        self.has(reason::DATA_EXTEND | reason::DATA_TRUNCATION | reason::DATA_OVERWRITE)
    }

    pub fn name(&self) -> String {
        crate::record::decode_utf16(self.name_utf16)
    }
}

/// Iterate the records in a `FSCTL_READ_USN_JOURNAL` output buffer.
///
/// The buffer begins with the USN to resume from, followed by variable-length
/// records. A malformed length terminates iteration rather than looping or
/// reading past the end.
pub struct UsnRecords<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> UsnRecords<'a> {
    /// `buf` is the raw output buffer, including the leading next-USN field.
    pub fn new(buf: &'a [u8]) -> (i64, Self) {
        let next = buf.u64_at(0).unwrap_or(0) as i64;
        (next, UsnRecords { buf, pos: 8 })
    }
}

impl<'a> Iterator for UsnRecords<'a> {
    type Item = UsnRecord<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.pos + 4 > self.buf.len() {
                return None;
            }
            let base = self.pos;
            let len = self.buf.u32_at(base)? as usize;
            // A zero or unaligned length would spin forever.
            if len < 0x3C || !len.is_multiple_of(8) || base + len > self.buf.len() {
                return None;
            }
            self.pos = base + len;

            let major = self.buf.u16_at(base + 4)?;
            // Version 2 carries 64-bit file references, which match MFT record
            // numbers directly. Version 3's 128-bit ids only appear on ReFS,
            // which this tool does not index; skip rather than misread them.
            if major != 2 {
                continue;
            }

            let rec = self.buf.get(base..base + len)?;
            let name_len = rec.u16_at(0x38)? as usize;
            let name_off = rec.u16_at(0x3A)? as usize;
            let name = match rec.get(name_off..name_off + name_len) {
                Some(n) if name_off >= 0x3C => n,
                _ => &[],
            };

            return Some(UsnRecord {
                usn: rec.u64_at(0x18)? as i64,
                file_ref: FileRef(rec.u64_at(0x08)?),
                parent_ref: FileRef(rec.u64_at(0x10)?),
                reason: rec.u32_at(0x28)?,
                attributes: rec.u32_at(0x34)?,
                name_utf16: name,
            });
        }
    }
}

/// State of a volume's change journal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JournalInfo {
    pub id: u64,
    /// Oldest USN still retained. A stored USN below this means the journal
    /// wrapped and the volume must be rescanned.
    pub first_usn: i64,
    pub next_usn: i64,
    pub max_size: u64,
    pub allocation_delta: u64,
}

impl JournalInfo {
    pub fn parse(buf: &[u8]) -> Result<Self> {
        if buf.len() < 56 {
            return Err(NtfsError::Truncated {
                need: 56,
                have: buf.len(),
            });
        }
        Ok(JournalInfo {
            id: buf.u64_at(0).unwrap_or(0),
            first_usn: buf.u64_at(8).unwrap_or(0) as i64,
            next_usn: buf.u64_at(16).unwrap_or(0) as i64,
            max_size: buf.u64_at(40).unwrap_or(0),
            allocation_delta: buf.u64_at(48).unwrap_or(0),
        })
    }

    /// Can we resume from `usn`, or has the journal moved past it?
    pub fn can_resume_from(&self, usn: i64) -> bool {
        usn >= self.first_usn && usn <= self.next_usn
    }
}

#[cfg(windows)]
pub use imp::*;

#[cfg(windows)]
mod imp {
    use super::*;
    use std::ffi::c_void;
    use std::io;
    use std::ptr;

    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::IO::DeviceIoControl;

    const GENERIC_READ: u32 = 0x8000_0000;
    const FSCTL_QUERY_USN_JOURNAL: u32 = 0x0009_00F4;
    const FSCTL_READ_USN_JOURNAL: u32 = 0x0009_00BB;
    const FSCTL_CREATE_USN_JOURNAL: u32 = 0x0009_00E7;

    const ERROR_JOURNAL_NOT_ACTIVE: i32 = 1179;
    const ERROR_JOURNAL_ENTRY_DELETED: i32 = 1181;

    /// Buffer for one journal read. Large enough to absorb a burst without
    /// many round trips, small enough to stay off the large-object path.
    const READ_BUFFER: usize = 256 * 1024;

    fn last_error() -> io::Error {
        io::Error::from_raw_os_error(unsafe { GetLastError() } as i32)
    }

    fn to_wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Open a handle suitable for journal operations.
    ///
    /// `root_only` opens `C:\` as a directory, which is enough for querying
    /// and works without elevation; otherwise `\\.\C:` is opened, which reads
    /// require and which needs Administrator.
    fn open_handle(drive: char, root_only: bool) -> io::Result<HANDLE> {
        let (path, flags) = if root_only {
            (
                format!(r"{}:\", drive.to_ascii_uppercase()),
                FILE_FLAG_BACKUP_SEMANTICS,
            )
        } else {
            (format!(r"\\.\{}:", drive.to_ascii_uppercase()), 0)
        };
        let wide = to_wide(&path);
        let h = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_READ,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                ptr::null(),
                OPEN_EXISTING,
                flags,
                ptr::null_mut(),
            )
        };
        if h == INVALID_HANDLE_VALUE {
            Err(last_error())
        } else {
            Ok(h)
        }
    }

    fn ioctl(h: HANDLE, code: u32, input: &[u8], output: &mut [u8]) -> io::Result<usize> {
        let mut returned: u32 = 0;
        let ok = unsafe {
            DeviceIoControl(
                h,
                code,
                if input.is_empty() {
                    ptr::null()
                } else {
                    input.as_ptr() as *const c_void
                },
                input.len() as u32,
                output.as_mut_ptr() as *mut c_void,
                output.len() as u32,
                &mut returned,
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            Err(last_error())
        } else {
            Ok(returned as usize)
        }
    }

    /// Why a journal is unavailable.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum JournalStatus {
        Active(JournalInfo),
        /// No journal exists on this volume; one must be created.
        NotActive,
        /// The query itself was denied.
        AccessDenied,
    }

    /// Query a volume's journal without needing elevation.
    pub fn query(drive: char) -> io::Result<JournalStatus> {
        let h = match open_handle(drive, true) {
            Ok(h) => h,
            Err(e) if e.raw_os_error() == Some(5) => return Ok(JournalStatus::AccessDenied),
            Err(e) => return Err(e),
        };
        let mut out = [0u8; 96];
        let res = ioctl(h, FSCTL_QUERY_USN_JOURNAL, &[], &mut out);
        unsafe { CloseHandle(h) };

        match res {
            Ok(_) => match JournalInfo::parse(&out) {
                Ok(info) => Ok(JournalStatus::Active(info)),
                Err(_) => Ok(JournalStatus::NotActive),
            },
            Err(e) if e.raw_os_error() == Some(ERROR_JOURNAL_NOT_ACTIVE) => {
                Ok(JournalStatus::NotActive)
            }
            Err(e) if e.raw_os_error() == Some(5) => Ok(JournalStatus::AccessDenied),
            Err(e) => Err(e),
        }
    }

    /// Create a journal on a volume that has none.
    ///
    /// This modifies the volume, so callers must confirm with the user first
    /// rather than doing it silently. `D:` on this machine has no journal.
    pub fn create(drive: char, max_size: u64, allocation_delta: u64) -> io::Result<()> {
        let h = open_handle(drive, false)?;
        let mut input = [0u8; 16];
        input[0..8].copy_from_slice(&max_size.to_le_bytes());
        input[8..16].copy_from_slice(&allocation_delta.to_le_bytes());
        let mut out = [0u8; 8];
        let res = ioctl(h, FSCTL_CREATE_USN_JOURNAL, &input, &mut out);
        unsafe { CloseHandle(h) };
        res.map(|_| ())
    }

    /// A live reader over one volume's change journal.
    pub struct JournalReader {
        handle: HANDLE,
        journal_id: u64,
        next_usn: i64,
        buf: Vec<u8>,
        /// Bytes the last read actually returned.
        returned: usize,
        /// The requested start position was no longer in the journal, so
        /// history was skipped to catch up.
        gap: bool,
    }

    // The handle is used only from the thread that owns the reader.
    unsafe impl Send for JournalReader {}

    impl Drop for JournalReader {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.handle);
            }
        }
    }

    /// Outcome of one journal read.
    pub enum Poll {
        /// Records were returned; the buffer holds them.
        Records,
        /// The journal was recreated or wrapped past our position. The volume
        /// must be rescanned; resuming would silently lose changes.
        Desynchronised,
    }

    impl JournalReader {
        /// Open a reader positioned at `start_usn`.
        ///
        /// Pass the USN recorded when the index snapshot was taken. If it is
        /// older than the journal's retained window the reader reports
        /// desynchronisation instead of quietly skipping the gap.
        pub fn open(drive: char, start_usn: i64) -> Result<Self> {
            let handle = open_handle(drive, false).map_err(|e| {
                NtfsError::Io(if e.raw_os_error() == Some(5) {
                    io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        format!("reading the change journal on {drive}: requires Administrator"),
                    )
                } else {
                    e
                })
            })?;

            let mut out = [0u8; 96];
            let res = ioctl(handle, FSCTL_QUERY_USN_JOURNAL, &[], &mut out);
            if let Err(e) = res {
                unsafe { CloseHandle(handle) };
                return Err(NtfsError::Io(e));
            }
            let info = JournalInfo::parse(&out)?;

            // Clamp forward rather than backward: starting before `first_usn`
            // would read a partial history and pretend it was complete.
            // Never start before `first_usn`: that would read a partial
            // history and present it as complete. But jumping forward loses
            // whatever happened in between, so record that it happened. A
            // caller that ignores this would show a stale index as healthy.
            let stale = start_usn > 0 && !info.can_resume_from(start_usn);
            let next_usn = if start_usn <= 0 || stale {
                info.next_usn
            } else {
                start_usn
            };

            Ok(JournalReader {
                handle,
                journal_id: info.id,
                next_usn,
                buf: vec![0u8; READ_BUFFER],
                returned: 0,
                gap: stale,
            })
        }

        pub fn next_usn(&self) -> i64 {
            self.next_usn
        }

        /// True when the requested resume point had already aged out of the
        /// journal, so changes between it and now were never seen. The index
        /// is stale and only a rescan can fix it.
        pub fn skipped_history(&self) -> bool {
            self.gap
        }

        pub fn journal_id(&self) -> u64 {
            self.journal_id
        }

        /// Read the next batch of changes.
        ///
        /// With `wait_bytes` greater than zero the call blocks in the kernel
        /// until that many bytes of journal accumulate, which is what keeps
        /// the watcher thread at zero CPU while idle. Pass zero to drain
        /// whatever is already there and return immediately.
        pub fn read(&mut self, wait_bytes: u64) -> Result<Poll> {
            // READ_USN_JOURNAL_DATA_V1
            let mut input = [0u8; 44];
            input[0..8].copy_from_slice(&self.next_usn.to_le_bytes());
            input[8..12].copy_from_slice(&reason::INTERESTING.to_le_bytes());
            input[12..16].copy_from_slice(&0u32.to_le_bytes()); // ReturnOnlyOnClose
            input[16..24].copy_from_slice(&0u64.to_le_bytes()); // Timeout
            input[24..32].copy_from_slice(&wait_bytes.to_le_bytes());
            input[32..40].copy_from_slice(&self.journal_id.to_le_bytes());
            input[40..42].copy_from_slice(&2u16.to_le_bytes()); // MinMajorVersion
            input[42..44].copy_from_slice(&2u16.to_le_bytes()); // MaxMajorVersion

            match ioctl(self.handle, FSCTL_READ_USN_JOURNAL, &input, &mut self.buf) {
                Ok(n) => {
                    self.returned = n;
                    let (next, _) = UsnRecords::new(&self.buf[..n.max(8)]);
                    self.next_usn = next;
                    Ok(Poll::Records)
                }
                Err(e)
                    if matches!(
                        e.raw_os_error(),
                        Some(ERROR_JOURNAL_NOT_ACTIVE) | Some(ERROR_JOURNAL_ENTRY_DELETED)
                    ) =>
                {
                    Ok(Poll::Desynchronised)
                }
                Err(e) => Err(NtfsError::Io(e)),
            }
        }

        /// The raw bytes of the most recent read, including the leading
        /// next-USN field. Useful for handing a batch to another thread.
        pub fn raw(&self) -> &[u8] {
            let n = self.returned.max(8).min(self.buf.len());
            &self.buf[..n]
        }

        /// The records returned by the most recent [`read`](Self::read).
        pub fn records(&self) -> UsnRecords<'_> {
            let n = self.returned.max(8).min(self.buf.len());
            UsnRecords::new(&self.buf[..n]).1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a synthetic USN_RECORD_V2.
    fn record(usn: i64, file: u64, parent: u64, reason: u32, attrs: u32, name: &str) -> Vec<u8> {
        let name_u16: Vec<u8> = name.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        let name_off = 0x3Cusize;
        let len = (name_off + name_u16.len() + 7) & !7;

        let mut r = vec![0u8; len];
        r[0x00..0x04].copy_from_slice(&(len as u32).to_le_bytes());
        r[0x04..0x06].copy_from_slice(&2u16.to_le_bytes()); // major
        r[0x06..0x08].copy_from_slice(&0u16.to_le_bytes());
        r[0x08..0x10].copy_from_slice(&file.to_le_bytes());
        r[0x10..0x18].copy_from_slice(&parent.to_le_bytes());
        r[0x18..0x20].copy_from_slice(&usn.to_le_bytes());
        r[0x28..0x2C].copy_from_slice(&reason.to_le_bytes());
        r[0x34..0x38].copy_from_slice(&attrs.to_le_bytes());
        r[0x38..0x3A].copy_from_slice(&(name_u16.len() as u16).to_le_bytes());
        r[0x3A..0x3C].copy_from_slice(&(name_off as u16).to_le_bytes());
        r[name_off..name_off + name_u16.len()].copy_from_slice(&name_u16);
        r
    }

    fn buffer(next: i64, records: &[Vec<u8>]) -> Vec<u8> {
        let mut b = next.to_le_bytes().to_vec();
        for r in records {
            b.extend_from_slice(r);
        }
        b
    }

    #[test]
    fn parses_a_batch_of_records() {
        let buf = buffer(
            9_000,
            &[
                record(100, 42, 5, reason::FILE_CREATE, 0x20, "new.txt"),
                record(200, 43, 5, reason::DATA_EXTEND, 0x20, "grown.bin"),
                record(300, 44, 5, reason::FILE_DELETE, 0x10, "gone"),
            ],
        );
        let (next, records) = UsnRecords::new(&buf);
        assert_eq!(next, 9_000);

        let got: Vec<_> = records.collect();
        assert_eq!(got.len(), 3);

        assert_eq!(got[0].name(), "new.txt");
        assert_eq!(got[0].file_ref, FileRef(42));
        assert_eq!(got[0].parent_ref, FileRef(5));
        assert!(got[0].has(reason::FILE_CREATE));
        assert!(!got[0].is_directory());

        assert_eq!(got[1].name(), "grown.bin");
        assert!(got[1].affects_size());

        assert_eq!(got[2].name(), "gone");
        assert!(got[2].is_directory(), "attribute 0x10 marks a directory");
        assert!(!got[2].affects_size());
    }

    #[test]
    fn handles_an_empty_batch() {
        let buf = buffer(1234, &[]);
        let (next, records) = UsnRecords::new(&buf);
        assert_eq!(next, 1234);
        assert_eq!(records.count(), 0);
    }

    #[test]
    fn parses_unicode_names() {
        let buf = buffer(
            1,
            &[record(
                10,
                7,
                5,
                reason::FILE_CREATE,
                0x20,
                "café_日本_🦀.txt",
            )],
        );
        let got: Vec<_> = UsnRecords::new(&buf).1.collect();
        assert_eq!(got[0].name(), "café_日本_🦀.txt");
    }

    #[test]
    fn skips_version_3_records_rather_than_misreading_them() {
        // V3 uses 128-bit references; reading it as V2 would produce garbage
        // file ids that could be applied to the wrong node.
        let mut v3 = record(10, 7, 5, reason::FILE_CREATE, 0x20, "refs.txt");
        v3[0x04..0x06].copy_from_slice(&3u16.to_le_bytes());
        let v2 = record(20, 8, 5, reason::FILE_CREATE, 0x20, "ntfs.txt");

        let buf = buffer(1, &[v3, v2]);
        let got: Vec<_> = UsnRecords::new(&buf).1.collect();
        assert_eq!(got.len(), 1, "the v3 record must be skipped, not decoded");
        assert_eq!(got[0].name(), "ntfs.txt");
    }

    #[test]
    fn malformed_lengths_terminate_iteration() {
        // Zero length would otherwise loop forever.
        let mut bad = record(10, 7, 5, reason::FILE_CREATE, 0x20, "x.txt");
        bad[0..4].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(UsnRecords::new(&buffer(1, &[bad])).1.count(), 0);

        // A length past the end of the buffer.
        let mut over = record(10, 7, 5, reason::FILE_CREATE, 0x20, "x.txt");
        over[0..4].copy_from_slice(&100_000u32.to_le_bytes());
        assert_eq!(UsnRecords::new(&buffer(1, &[over])).1.count(), 0);

        // An unaligned length.
        let mut odd = record(10, 7, 5, reason::FILE_CREATE, 0x20, "x.txt");
        odd[0..4].copy_from_slice(&77u32.to_le_bytes());
        assert_eq!(UsnRecords::new(&buffer(1, &[odd])).1.count(), 0);

        // A truncated buffer.
        let buf = buffer(1, &[record(10, 7, 5, reason::FILE_CREATE, 0x20, "x.txt")]);
        assert_eq!(UsnRecords::new(&buf[..20]).1.count(), 0);
        // And one too short to even hold the next-USN field.
        assert_eq!(UsnRecords::new(&[0u8; 3]).1.count(), 0);
    }

    #[test]
    fn rename_pairs_are_visible_as_two_records() {
        // NTFS reports a rename as an old-name record and a new-name record,
        // which the watcher must handle as a move rather than a delete.
        let buf = buffer(
            50,
            &[
                record(10, 42, 5, reason::RENAME_OLD_NAME, 0x20, "before.txt"),
                record(
                    20,
                    42,
                    9,
                    reason::RENAME_NEW_NAME | reason::CLOSE,
                    0x20,
                    "after.txt",
                ),
            ],
        );
        let got: Vec<_> = UsnRecords::new(&buf).1.collect();
        assert_eq!(got.len(), 2);
        assert!(got[0].has(reason::RENAME_OLD_NAME));
        assert_eq!(got[0].name(), "before.txt");
        assert!(got[1].has(reason::RENAME_NEW_NAME));
        assert_eq!(got[1].parent_ref, FileRef(9), "the new parent moved");
        // Same file throughout.
        assert_eq!(got[0].file_ref, got[1].file_ref);
    }

    #[test]
    fn journal_info_parses_and_detects_wrap() {
        // Shaped like the real C: journal read during planning.
        let mut buf = [0u8; 80];
        buf[0..8].copy_from_slice(&0x01db_0ee0_adbc_3a1eu64.to_le_bytes());
        buf[8..16].copy_from_slice(&0x0000_000b_2492_0000u64.to_le_bytes());
        buf[16..24].copy_from_slice(&0x0000_000b_26b7_74a0u64.to_le_bytes());
        buf[40..48].copy_from_slice(&(32 * 1024 * 1024u64).to_le_bytes());
        buf[48..56].copy_from_slice(&(8 * 1024 * 1024u64).to_le_bytes());

        let info = JournalInfo::parse(&buf).unwrap();
        assert_eq!(info.id, 0x01db_0ee0_adbc_3a1e);
        assert_eq!(info.max_size, 32 * 1024 * 1024);
        assert_eq!(info.allocation_delta, 8 * 1024 * 1024);

        assert!(info.can_resume_from(info.first_usn));
        assert!(info.can_resume_from(info.next_usn));
        assert!(info.can_resume_from((info.first_usn + info.next_usn) / 2));
        // A USN older than the retained window means the journal wrapped.
        assert!(!info.can_resume_from(info.first_usn - 1));
        assert!(!info.can_resume_from(0));
        // And one from the future is equally unusable.
        assert!(!info.can_resume_from(info.next_usn + 1));
    }

    #[test]
    fn journal_info_rejects_a_short_buffer() {
        assert!(JournalInfo::parse(&[0u8; 20]).is_err());
    }

    #[test]
    fn interesting_mask_covers_size_and_name_changes_only() {
        use reason::*;
        for bit in [
            DATA_EXTEND,
            DATA_TRUNCATION,
            FILE_CREATE,
            FILE_DELETE,
            RENAME_NEW_NAME,
        ] {
            assert_ne!(INTERESTING & bit, 0);
        }
        // Security and timestamp churn is filtered out in the kernel.
        assert_eq!(INTERESTING & BASIC_INFO_CHANGE, 0);
        assert_eq!(INTERESTING & STREAM_CHANGE, 0);
    }
}
