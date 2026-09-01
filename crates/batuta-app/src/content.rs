//! Filesystem-backed content access for duplicate detection.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};

use batuta_core::{ContentSource, Index};

/// Reads file content by reconstructing each node's path on demand.
///
/// Only the small candidate set that survives the size and sample tiers ever
/// reaches here, so opening files one at a time is not the bottleneck.
pub struct FsContent<'a> {
    idx: &'a Index,
}

impl<'a> FsContent<'a> {
    pub fn new(idx: &'a Index) -> Self {
        FsContent { idx }
    }
}

impl ContentSource for FsContent<'_> {
    fn read_at(&self, node: u32, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        let mut f = File::open(self.idx.path(node))?;
        f.seek(SeekFrom::Start(offset))?;

        // `read` may legitimately return short; loop until the buffer is full
        // or the file ends, so the sample hash covers a deterministic range.
        let mut done = 0;
        while done < buf.len() {
            match f.read(&mut buf[done..]) {
                Ok(0) => break,
                Ok(n) => done += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(done)
    }

    fn hash_full(&self, node: u32) -> io::Result<[u8; 32]> {
        let path = self.idx.path(node);
        let mut hasher = blake3::Hasher::new();
        // Memory-mapped and multi-threaded above blake3's internal threshold,
        // which matters because this tier is the expensive one.
        hasher.update_mmap_rayon(&path)?;
        Ok(*hasher.finalize().as_bytes())
    }
}
