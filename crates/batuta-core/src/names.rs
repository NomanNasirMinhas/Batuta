//! One contiguous UTF-8 arena holding every base name in the index.
//!
//! Names are appended in node order, which matters: the search engine scans
//! this arena as a single block and maps a match offset back to a node with a
//! binary search over `name_off`. That only works while the arena is sorted by
//! node id, so nothing may reorder or rewrite it in place.

/// Growable byte arena of names.
#[derive(Default, Debug, Clone)]
pub struct NameArena {
    buf: Vec<u8>,
}

impl NameArena {
    pub fn with_capacity(bytes: usize) -> Self {
        NameArena {
            buf: Vec::with_capacity(bytes),
        }
    }

    /// Wrap bytes read back from a snapshot.
    pub fn from_bytes(buf: Vec<u8>) -> Self {
        NameArena { buf }
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// The whole arena, for bulk scanning.
    #[inline]
    pub fn as_bytes(&self) -> &[u8] {
        &self.buf
    }

    /// Append a name, returning its offset and byte length.
    pub fn push(&mut self, name: &str) -> (u32, u16) {
        let off = self.buf.len() as u32;
        // NTFS caps names at 255 UTF-16 units, so this cannot overflow u16 in
        // practice; truncate rather than panic if something upstream lies.
        let bytes = name.as_bytes();
        let len = bytes.len().min(u16::MAX as usize);
        self.buf.extend_from_slice(&bytes[..len]);
        (off, len as u16)
    }

    /// The raw bytes of a name, skipping UTF-8 validation.
    ///
    /// Every name in the arena was written from a `&str`, so the bytes are
    /// already valid. Comparators use this because revalidating on each access
    /// dominates the cost of a large sort.
    #[inline]
    pub fn bytes(&self, off: u32, len: u16) -> &[u8] {
        let (a, b) = (off as usize, off as usize + len as usize);
        self.buf.get(a..b).unwrap_or(&[])
    }

    #[inline]
    pub fn get(&self, off: u32, len: u16) -> &str {
        let (a, b) = (off as usize, off as usize + len as usize);
        match self.buf.get(a..b) {
            Some(s) => std::str::from_utf8(s).unwrap_or(""),
            None => "",
        }
    }

    /// Repoint a name at an empty slice, used for volume roots whose stored
    /// name is "." and would otherwise show up in results.
    pub fn overwrite_empty(&self, off: &mut u32, len: &mut u16) {
        *off = 0;
        *len = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_names_in_order() {
        let mut a = NameArena::default();
        let x = a.push("alpha.txt");
        let y = a.push("beta");
        let z = a.push("café_日本.md");
        assert_eq!(a.get(x.0, x.1), "alpha.txt");
        assert_eq!(a.get(y.0, y.1), "beta");
        assert_eq!(a.get(z.0, z.1), "café_日本.md");
        // Offsets must increase strictly: the search engine binary searches
        // them to map a match back to its node.
        assert!(x.0 < y.0 && y.0 < z.0);
        assert_eq!(a.as_bytes().len(), a.len());
    }

    #[test]
    fn out_of_range_reads_are_empty_not_panics() {
        let a = NameArena::default();
        assert_eq!(a.get(50, 10), "");
    }
}
