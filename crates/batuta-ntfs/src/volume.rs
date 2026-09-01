//! Raw volume access and MFT streaming. Windows only, and requires elevation.
//!
//! Opening `\\.\C:` for read needs Administrator: probing this machine while
//! planning showed `CreateFile` returning `ERROR_ACCESS_DENIED` unelevated via
//! both a volume handle and a root-directory handle, so there is no
//! unprivileged path to the MFT.

use std::ffi::c_void;
use std::io;
use std::ptr;

use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::OVERLAPPED;

use crate::boot::BootSector;
use crate::error::{NtfsError, Result};
use crate::mft::{EntryBatch, MftParser};
use crate::record::{apply_fixups, attr_type, AttrBody, FileRecord};
use crate::runlist::RunList;

const GENERIC_READ: u32 = 0x8000_0000;

/// How much of the MFT to pull in a single read. Large enough to keep an NVMe
/// queue busy, small enough to stay friendly to the page cache.
const CHUNK_BYTES: usize = 4 * 1024 * 1024;

/// Refuse absurd MFT sizes rather than trying to allocate for them.
const MAX_MFT_BYTES: u64 = 64 * 1024 * 1024 * 1024;

fn last_error() -> io::Error {
    io::Error::from_raw_os_error(unsafe { GetLastError() } as i32)
}

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Is the current process running elevated?
pub fn is_elevated() -> bool {
    use windows_sys::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_QUERY};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    unsafe {
        let mut token: HANDLE = ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return false;
        }
        // TOKEN_ELEVATION is a single u32.
        let mut elevation: u32 = 0;
        let mut size: u32 = 0;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            &mut elevation as *mut u32 as *mut c_void,
            std::mem::size_of::<u32>() as u32,
            &mut size,
        );
        CloseHandle(token);
        ok != 0 && elevation != 0
    }
}

/// An open handle to a raw NTFS volume.
pub struct Volume {
    handle: HANDLE,
    boot: BootSector,
}

// The handle is only used for positioned reads via OVERLAPPED, which do not
// rely on the shared file pointer.
unsafe impl Send for Volume {}
unsafe impl Sync for Volume {}

impl Drop for Volume {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.handle);
        }
    }
}

impl Volume {
    /// Open a volume by drive letter, e.g. `'C'`.
    pub fn open(drive: char) -> Result<Self> {
        let path = format!(r"\\.\{}:", drive.to_ascii_uppercase());
        let wide = to_wide(&path);
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_READ,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                ptr::null(),
                OPEN_EXISTING,
                0,
                ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            let e = last_error();
            return Err(NtfsError::Io(if e.raw_os_error() == Some(5) {
                io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!(
                        "cannot open {path}: reading the MFT requires Administrator. \
                         Run batuta elevated, or install the service."
                    ),
                )
            } else {
                e
            }));
        }

        let mut vol = Volume {
            handle,
            boot: unsafe { std::mem::zeroed() },
        };
        let mut sector = vec![0u8; 512];
        vol.read_at(0, &mut sector)?;
        vol.boot = BootSector::parse(&sector)?;
        Ok(vol)
    }

    pub fn boot(&self) -> &BootSector {
        &self.boot
    }

    /// Positioned read. Offset and length must be sector-aligned, which every
    /// caller here satisfies by working in whole clusters.
    pub fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let mut done = 0usize;
        while done < buf.len() {
            let want = (buf.len() - done).min(u32::MAX as usize) as u32;
            let pos = offset + done as u64;

            let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
            ov.Anonymous.Anonymous.Offset = pos as u32;
            ov.Anonymous.Anonymous.OffsetHigh = (pos >> 32) as u32;

            let mut got: u32 = 0;
            let ok = unsafe {
                ReadFile(
                    self.handle,
                    buf[done..].as_mut_ptr(),
                    want,
                    &mut got,
                    &mut ov,
                )
            };
            if ok == 0 {
                return Err(NtfsError::Io(last_error()));
            }
            if got == 0 {
                return Err(NtfsError::Io(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    format!("short read at offset {pos}"),
                )));
            }
            done += got as usize;
        }
        Ok(())
    }

    /// Read the `$MFT` record and decode its own cluster map.
    ///
    /// The MFT describes where it lives, so this bootstrap read is what makes
    /// a fragmented MFT tractable.
    pub fn mft_runs(&self) -> Result<RunList> {
        let rs = self.boot.file_record_size as usize;
        let mut rec = vec![0u8; rs.max(self.boot.cluster_size() as usize)];
        self.read_at(self.boot.mft_offset(), &mut rec)?;
        rec.truncate(rs);

        apply_fixups(&mut rec, self.boot.bytes_per_sector)?;
        let hdr = FileRecord::parse(&rec)?;

        let max_clusters = self.boot.total_sectors / self.boot.sectors_per_cluster as u64;
        for attr in hdr.attributes(&rec).flatten() {
            if attr.ty == attr_type::DATA && attr.is_unnamed() {
                if let AttrBody::NonResident { runs, .. } = attr.body {
                    return RunList::parse(runs, Some(max_clusters.max(1)));
                }
            }
        }
        Err(NtfsError::BadBootSector("$MFT has no non-resident $DATA"))
    }
}

/// Streams the MFT of one volume and parses it into index rows.
pub struct MftScanner {
    volume: Volume,
    runs: RunList,
}

/// Summary of one MFT scan.
#[derive(Debug, Clone, Copy, Default)]
pub struct ScanStats {
    pub bytes_read: u64,
    pub records_swept: u64,
    pub entries: u64,
}

impl MftScanner {
    pub fn open(drive: char) -> Result<Self> {
        let volume = Volume::open(drive)?;
        let runs = volume.mft_runs()?;
        Ok(MftScanner { volume, runs })
    }

    pub fn boot(&self) -> &BootSector {
        self.volume.boot()
    }

    /// Total bytes the MFT occupies, sparse holes included.
    pub fn mft_bytes(&self) -> u64 {
        self.runs.cluster_count() * self.volume.boot.cluster_size()
    }

    /// How many extents the MFT is split across. A high count means a badly
    /// fragmented MFT, which is worth surfacing in verbose output.
    pub fn run_count(&self) -> usize {
        self.runs.runs.len()
    }

    /// Highest record number the MFT can hold. Used to size the
    /// record-number lookup table.
    pub fn max_records(&self) -> u64 {
        self.mft_bytes() / self.volume.boot.file_record_size as u64
    }

    /// Read and parse the entire MFT.
    ///
    /// I/O is sequential while parsing fans out across every core, which is
    /// the right split: the planning benchmark showed this workload is bound
    /// by per-record CPU work, not by the disk.
    pub fn scan(&self) -> Result<(EntryBatch, ScanStats)> {
        use rayon::prelude::*;

        let boot = &self.volume.boot;
        let cluster = boot.cluster_size();
        let rs = boot.file_record_size as u64;
        if cluster == 0 || rs == 0 {
            return Err(NtfsError::BadBootSector("zero cluster or record size"));
        }
        if self.mft_bytes() > MAX_MFT_BYTES {
            return Err(NtfsError::BadBootSector("implausible $MFT size"));
        }

        let parser = MftParser::new(boot.bytes_per_sector, boot.file_record_size);
        let mut out = EntryBatch::default();
        let mut stats = ScanStats::default();

        // Keep chunks a whole number of both clusters and records so every
        // chunk starts exactly on a record boundary.
        let chunk_bytes = {
            let step = lcm(cluster, rs);
            let n = (CHUNK_BYTES as u64 / step).max(1);
            (n * step) as usize
        };
        let mut buf = vec![0u8; chunk_bytes];

        for run in &self.runs.runs {
            let Some(lcn) = run.lcn else {
                continue; // sparse region of the MFT holds no records
            };
            let run_bytes = run.length * cluster;
            let virtual_start = run.vcn * cluster;

            // A record must not straddle a run boundary, or its two halves
            // would be non-adjacent on disk.
            if !virtual_start.is_multiple_of(rs) {
                continue;
            }

            let mut done = 0u64;
            while done < run_bytes {
                let n = ((run_bytes - done) as usize).min(chunk_bytes);
                let n = n - (n % rs as usize); // whole records only
                if n == 0 {
                    break;
                }
                let slice = &mut buf[..n];
                self.volume.read_at(lcn * cluster + done, slice)?;

                let first_record = (virtual_start + done) / rs;
                stats.bytes_read += n as u64;
                stats.records_swept += n as u64 / rs;

                // Split the chunk across the pool; each thread fills its own
                // batch so there is no contention on a shared arena.
                let sub = (n / rayon::current_num_threads().max(1)).max(rs as usize);
                let sub = sub - (sub % rs as usize);
                let batches: Vec<EntryBatch> = slice
                    .par_chunks_mut(sub)
                    .enumerate()
                    .map(|(i, part)| {
                        let mut b = EntryBatch::default();
                        let base = first_record + (i * sub) as u64 / rs;
                        parser.parse_chunk(part, base, &mut b);
                        b
                    })
                    .collect();
                for b in batches {
                    out.merge(b);
                }

                done += n as u64;
            }
        }

        out.apply_spill();
        stats.entries = out.len() as u64;
        Ok((out, stats))
    }
}

fn gcd(a: u64, b: u64) -> u64 {
    if b == 0 {
        a
    } else {
        gcd(b, a % b)
    }
}

fn lcm(a: u64, b: u64) -> u64 {
    if a == 0 || b == 0 {
        1
    } else {
        a / gcd(a, b) * b
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lcm_keeps_chunks_on_record_boundaries() {
        assert_eq!(lcm(4096, 1024), 4096);
        assert_eq!(lcm(512, 1024), 1024);
        assert_eq!(lcm(4096, 4096), 4096);
        // 64K clusters with 1K records.
        assert_eq!(lcm(65536, 1024), 65536);
    }

    #[test]
    fn elevation_probe_does_not_panic() {
        // The value depends on how the tests were launched; only the call
        // itself is under test.
        let _ = is_elevated();
    }
}
