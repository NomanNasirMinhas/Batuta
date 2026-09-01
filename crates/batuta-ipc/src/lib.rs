//! Wire protocol between the `batuta` CLI and the resident daemon.
//!
//! The daemon runs privileged and clients do not, so this decoder sits on a
//! privilege boundary: every length is bounds-checked against what actually
//! arrived, every frame is capped, and no input can drive an allocation
//! larger than the bytes backing it. Decoding returns an error rather than
//! panicking, whatever the input.
//!
//! The codec is written out by hand rather than derived. It is a small, fixed
//! set of messages, and being explicit about each field keeps the trust
//! boundary readable.

use std::io::{self, Read, Write};

/// Largest frame accepted in either direction.
pub const MAX_FRAME: u32 = 8 * 1024 * 1024;

/// Longest string accepted in a message.
const MAX_STRING: u32 = 64 * 1024;

/// Most rows a single response may carry.
const MAX_ROWS: u32 = 1_000_000;

pub const PIPE_NAME: &str = r"\\.\pipe\batuta";

#[derive(Debug, thiserror::Error)]
pub enum IpcError {
    #[error("message is truncated")]
    Truncated,
    #[error("unknown message tag {0}")]
    UnknownTag(u8),
    #[error("value exceeds its limit: {0}")]
    TooLarge(&'static str),
    #[error("string is not valid utf-8")]
    BadUtf8,
    #[error("io error: {0}")]
    Io(#[from] io::Error),
}

pub type Result<T> = std::result::Result<T, IpcError>;

// ---------------------------------------------------------------- primitives

#[derive(Default)]
pub struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }
    pub fn u8(&mut self, v: u8) -> &mut Self {
        self.buf.push(v);
        self
    }
    pub fn bool(&mut self, v: bool) -> &mut Self {
        self.u8(v as u8)
    }
    pub fn u32(&mut self, v: u32) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn u64(&mut self, v: u64) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn str(&mut self, s: &str) -> &mut Self {
        let b = s.as_bytes();
        let n = b.len().min(MAX_STRING as usize);
        self.u32(n as u32);
        self.buf.extend_from_slice(&b[..n]);
        self
    }
    pub fn opt_str(&mut self, s: Option<&str>) -> &mut Self {
        match s {
            Some(v) => {
                self.bool(true);
                self.str(v)
            }
            None => self.bool(false),
        }
    }
    pub fn opt_u64(&mut self, v: Option<u64>) -> &mut Self {
        match v {
            Some(x) => {
                self.bool(true);
                self.u64(x)
            }
            None => self.bool(false),
        }
    }
}

pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Reader { buf, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.pos.checked_add(n).ok_or(IpcError::Truncated)?;
        let s = self.buf.get(self.pos..end).ok_or(IpcError::Truncated)?;
        self.pos = end;
        Ok(s)
    }

    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    pub fn bool(&mut self) -> Result<bool> {
        Ok(self.u8()? != 0)
    }
    pub fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    pub fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    pub fn str(&mut self) -> Result<String> {
        let n = self.u32()?;
        if n > MAX_STRING {
            return Err(IpcError::TooLarge("string"));
        }
        // Bounded by the remaining bytes, so a huge declared length cannot
        // make us allocate more than actually arrived.
        let b = self.take(n as usize)?;
        std::str::from_utf8(b)
            .map(str::to_owned)
            .map_err(|_| IpcError::BadUtf8)
    }

    pub fn opt_str(&mut self) -> Result<Option<String>> {
        if self.bool()? {
            Ok(Some(self.str()?))
        } else {
            Ok(None)
        }
    }

    pub fn opt_u64(&mut self) -> Result<Option<u64>> {
        if self.bool()? {
            Ok(Some(self.u64()?))
        } else {
            Ok(None)
        }
    }

    /// Read a count, refusing anything that could not possibly be backed by
    /// the bytes still available.
    fn count(&mut self, min_bytes_each: usize, what: &'static str) -> Result<usize> {
        let n = self.u32()?;
        if n > MAX_ROWS {
            return Err(IpcError::TooLarge(what));
        }
        let remaining = self.buf.len() - self.pos;
        if (n as usize).saturating_mul(min_bytes_each) > remaining {
            return Err(IpcError::Truncated);
        }
        Ok(n as usize)
    }
}

// ------------------------------------------------------------------ messages

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SearchArgs {
    pub query: String,
    pub glob: bool,
    pub case_sensitive: bool,
    pub ext: Option<String>,
    pub min_size: Option<u64>,
    pub max_size: Option<u64>,
    pub under: Option<String>,
    pub dirs_only: bool,
    pub files_only: bool,
    pub include_excluded: bool,
    /// 0 name, 1 size, 2 modified.
    pub sort: u8,
    /// Sorted matches to skip, so a UI can scroll without fetching everything.
    pub offset: u32,
    pub limit: u32,
}

/// One duplicate group on the wire: `size` bytes each, `wasted` reclaimable by
/// keeping a single copy, and one row per duplicate path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DupeGroupRows {
    pub size: u64,
    pub wasted: u64,
    pub rows: Vec<Row>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    Search(SearchArgs),
    Size {
        path: String,
        top: u32,
    },
    Bloat {
        top: u32,
        under: Option<String>,
    },
    /// Exact duplicate detection. Reads file contents, so it is by far the
    /// most expensive request; only sent when a user explicitly asks for it.
    Dupes {
        /// Ignore files smaller than this.
        min_size: u64,
        /// Stop after this many groups, ranked by reclaimable bytes.
        top: u32,
        /// Restrict to this subtree.
        under: Option<String>,
    },
    Status,
    Rescan,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Row {
    pub path: String,
    pub size: u64,
    pub mtime: u32,
    pub is_dir: bool,
    /// For directories: files anywhere beneath. Zero for files.
    pub files: u32,
    /// For directories: bytes held directly in this directory's own files,
    /// as opposed to anywhere below it. Zero for files.
    pub own: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct VolumeStatus {
    pub drive: String,
    pub files: u64,
    pub size: u64,
    pub next_usn: u64,
    pub journal_active: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Response {
    Rows {
        rows: Vec<Row>,
        total: u64,
        elapsed_us: u64,
    },
    Dupes {
        groups: Vec<DupeGroupRows>,
        /// Total reclaimable bytes across all reported groups.
        wasted_total: u64,
        elapsed_us: u64,
    },
    SizeInfo {
        path: String,
        total: u64,
        alloc: u64,
        files: u64,
        children: Vec<Row>,
    },
    Status {
        nodes: u64,
        memory: u64,
        watching: bool,
        changes_applied: u64,
        volumes: Vec<VolumeStatus>,
    },
    Error {
        message: String,
    },
}

mod tag {
    pub const SEARCH: u8 = 1;
    pub const SIZE: u8 = 2;
    pub const BLOAT: u8 = 3;
    pub const STATUS: u8 = 4;
    pub const RESCAN: u8 = 5;
    pub const DUPES: u8 = 6;

    pub const ROWS: u8 = 20;
    pub const SIZE_INFO: u8 = 21;
    pub const STATUS_INFO: u8 = 22;
    pub const ERROR: u8 = 23;
    pub const DUPES_INFO: u8 = 24;
}

/// Minimum bytes any encoded `Row` occupies: a 4-byte string length plus
/// size, mtime and the two flags.
const ROW_MIN_BYTES: usize = 4 + 8 + 4 + 1 + 4 + 8;
/// Minimum bytes any encoded `DupeGroupRows` occupies: size, wasted and a
/// row count, before the rows themselves.
const GROUP_MIN_BYTES: usize = 8 + 8 + 4;
const VOL_MIN_BYTES: usize = 4 + 8 + 8 + 8 + 1;

impl Request {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        match self {
            Request::Search(a) => {
                w.u8(tag::SEARCH)
                    .str(&a.query)
                    .bool(a.glob)
                    .bool(a.case_sensitive)
                    .opt_str(a.ext.as_deref())
                    .opt_u64(a.min_size)
                    .opt_u64(a.max_size)
                    .opt_str(a.under.as_deref())
                    .bool(a.dirs_only)
                    .bool(a.files_only)
                    .bool(a.include_excluded)
                    .u8(a.sort)
                    .u32(a.offset)
                    .u32(a.limit);
            }
            Request::Size { path, top } => {
                w.u8(tag::SIZE).str(path).u32(*top);
            }
            Request::Bloat { top, under } => {
                w.u8(tag::BLOAT).u32(*top).opt_str(under.as_deref());
            }
            Request::Dupes {
                min_size,
                top,
                under,
            } => {
                w.u8(tag::DUPES)
                    .u64(*min_size)
                    .u32(*top)
                    .opt_str(under.as_deref());
            }
            Request::Status => {
                w.u8(tag::STATUS);
            }
            Request::Rescan => {
                w.u8(tag::RESCAN);
            }
        }
        w.into_bytes()
    }

    pub fn decode(buf: &[u8]) -> Result<Self> {
        let mut r = Reader::new(buf);
        match r.u8()? {
            tag::SEARCH => Ok(Request::Search(SearchArgs {
                query: r.str()?,
                glob: r.bool()?,
                case_sensitive: r.bool()?,
                ext: r.opt_str()?,
                min_size: r.opt_u64()?,
                max_size: r.opt_u64()?,
                under: r.opt_str()?,
                dirs_only: r.bool()?,
                files_only: r.bool()?,
                include_excluded: r.bool()?,
                sort: r.u8()?,
                offset: r.u32()?,
                limit: r.u32()?,
            })),
            tag::SIZE => Ok(Request::Size {
                path: r.str()?,
                top: r.u32()?,
            }),
            tag::BLOAT => Ok(Request::Bloat {
                top: r.u32()?,
                under: r.opt_str()?,
            }),
            tag::DUPES => Ok(Request::Dupes {
                min_size: r.u64()?,
                top: r.u32()?,
                under: r.opt_str()?,
            }),
            tag::STATUS => Ok(Request::Status),
            tag::RESCAN => Ok(Request::Rescan),
            other => Err(IpcError::UnknownTag(other)),
        }
    }
}

fn write_row(w: &mut Writer, row: &Row) {
    w.str(&row.path)
        .u64(row.size)
        .u32(row.mtime)
        .bool(row.is_dir)
        .u32(row.files)
        .u64(row.own);
}

fn read_row(r: &mut Reader<'_>) -> Result<Row> {
    Ok(Row {
        path: r.str()?,
        size: r.u64()?,
        mtime: r.u32()?,
        is_dir: r.bool()?,
        files: r.u32()?,
        own: r.u64()?,
    })
}

impl Response {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        match self {
            Response::Rows {
                rows,
                total,
                elapsed_us,
            } => {
                w.u8(tag::ROWS)
                    .u64(*total)
                    .u64(*elapsed_us)
                    .u32(rows.len() as u32);
                for row in rows {
                    write_row(&mut w, row);
                }
            }
            Response::Dupes {
                groups,
                wasted_total,
                elapsed_us,
            } => {
                w.u8(tag::DUPES_INFO)
                    .u64(*wasted_total)
                    .u64(*elapsed_us)
                    .u32(groups.len() as u32);
                for g in groups {
                    w.u64(g.size).u64(g.wasted).u32(g.rows.len() as u32);
                    for row in &g.rows {
                        write_row(&mut w, row);
                    }
                }
            }
            Response::SizeInfo {
                path,
                total,
                alloc,
                files,
                children,
            } => {
                w.u8(tag::SIZE_INFO)
                    .str(path)
                    .u64(*total)
                    .u64(*alloc)
                    .u64(*files)
                    .u32(children.len() as u32);
                for row in children {
                    write_row(&mut w, row);
                }
            }
            Response::Status {
                nodes,
                memory,
                watching,
                changes_applied,
                volumes,
            } => {
                w.u8(tag::STATUS_INFO)
                    .u64(*nodes)
                    .u64(*memory)
                    .bool(*watching)
                    .u64(*changes_applied)
                    .u32(volumes.len() as u32);
                for v in volumes {
                    w.str(&v.drive)
                        .u64(v.files)
                        .u64(v.size)
                        .u64(v.next_usn)
                        .bool(v.journal_active);
                }
            }
            Response::Error { message } => {
                w.u8(tag::ERROR).str(message);
            }
        }
        w.into_bytes()
    }

    pub fn decode(buf: &[u8]) -> Result<Self> {
        let mut r = Reader::new(buf);
        match r.u8()? {
            tag::ROWS => {
                let total = r.u64()?;
                let elapsed_us = r.u64()?;
                let n = r.count(ROW_MIN_BYTES, "rows")?;
                let mut rows = Vec::with_capacity(n);
                for _ in 0..n {
                    rows.push(read_row(&mut r)?);
                }
                Ok(Response::Rows {
                    rows,
                    total,
                    elapsed_us,
                })
            }
            tag::DUPES_INFO => {
                let wasted_total = r.u64()?;
                let elapsed_us = r.u64()?;
                let n = r.count(GROUP_MIN_BYTES, "groups")?;
                let mut groups = Vec::with_capacity(n);
                for _ in 0..n {
                    let size = r.u64()?;
                    let wasted = r.u64()?;
                    let rows = r.count(ROW_MIN_BYTES, "copies")?;
                    let mut copies = Vec::with_capacity(rows);
                    for _ in 0..rows {
                        copies.push(read_row(&mut r)?);
                    }
                    groups.push(DupeGroupRows {
                        size,
                        wasted,
                        rows: copies,
                    });
                }
                Ok(Response::Dupes {
                    groups,
                    wasted_total,
                    elapsed_us,
                })
            }
            tag::SIZE_INFO => {
                let path = r.str()?;
                let total = r.u64()?;
                let alloc = r.u64()?;
                let files = r.u64()?;
                let n = r.count(ROW_MIN_BYTES, "children")?;
                let mut children = Vec::with_capacity(n);
                for _ in 0..n {
                    children.push(read_row(&mut r)?);
                }
                Ok(Response::SizeInfo {
                    path,
                    total,
                    alloc,
                    files,
                    children,
                })
            }
            tag::STATUS_INFO => {
                let nodes = r.u64()?;
                let memory = r.u64()?;
                let watching = r.bool()?;
                let changes_applied = r.u64()?;
                let n = r.count(VOL_MIN_BYTES, "volumes")?;
                let mut volumes = Vec::with_capacity(n);
                for _ in 0..n {
                    volumes.push(VolumeStatus {
                        drive: r.str()?,
                        files: r.u64()?,
                        size: r.u64()?,
                        next_usn: r.u64()?,
                        journal_active: r.bool()?,
                    });
                }
                Ok(Response::Status {
                    nodes,
                    memory,
                    watching,
                    changes_applied,
                    volumes,
                })
            }
            tag::ERROR => Ok(Response::Error { message: r.str()? }),
            other => Err(IpcError::UnknownTag(other)),
        }
    }
}

// ------------------------------------------------------------------- framing

/// Write a length-prefixed frame.
pub fn write_frame<W: Write>(w: &mut W, payload: &[u8]) -> Result<()> {
    if payload.len() as u64 > MAX_FRAME as u64 {
        return Err(IpcError::TooLarge("frame"));
    }
    w.write_all(&(payload.len() as u32).to_le_bytes())?;
    w.write_all(payload)?;
    w.flush()?;
    Ok(())
}

/// Read a length-prefixed frame.
///
/// The declared length is checked against `MAX_FRAME` *before* allocating, so
/// a client cannot ask the daemon to reserve an arbitrary buffer.
pub fn read_frame<R: Read>(r: &mut R) -> Result<Vec<u8>> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len);
    if len > MAX_FRAME {
        return Err(IpcError::TooLarge("frame"));
    }
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_search() -> Request {
        Request::Search(SearchArgs {
            query: "café_日本.txt".into(),
            glob: true,
            case_sensitive: false,
            ext: Some("rs".into()),
            min_size: Some(1024),
            max_size: None,
            under: Some(r"C:\Users\Hacker".into()),
            dirs_only: false,
            files_only: true,
            include_excluded: false,
            sort: 1,
            offset: 100,
            limit: 250,
        })
    }

    #[test]
    fn requests_round_trip() {
        let cases = vec![
            sample_search(),
            Request::Search(SearchArgs::default()),
            Request::Size {
                path: r"D:\Projects".into(),
                top: 20,
            },
            Request::Bloat {
                top: 30,
                under: None,
            },
            Request::Bloat {
                top: 5,
                under: Some(r"C:\Users".into()),
            },
            Request::Dupes {
                min_size: 1 << 20,
                top: 500,
                under: None,
            },
            Request::Dupes {
                min_size: 1024,
                top: 25,
                under: Some(r"D:\Code".into()),
            },
            Request::Status,
            Request::Rescan,
        ];
        for c in cases {
            let bytes = c.encode();
            assert_eq!(
                Request::decode(&bytes).unwrap(),
                c,
                "round trip failed for {c:?}"
            );
        }
    }

    #[test]
    fn responses_round_trip() {
        let rows = vec![
            Row {
                path: r"C:\Users\Hacker\notes.txt".into(),
                size: 100,
                mtime: 1_788_134_400,
                is_dir: false,
                files: 0,
                own: 0,
            },
            Row {
                path: r"C:\Users\Hacker\Downloads".into(),
                size: 52 * 1024 * 1024,
                mtime: 0,
                is_dir: true,
                files: 2,
                own: 4096,
            },
        ];
        let cases = vec![
            Response::Rows {
                rows: rows.clone(),
                total: 2,
                elapsed_us: 1234,
            },
            Response::Rows {
                rows: Vec::new(),
                total: 0,
                elapsed_us: 7,
            },
            Response::Dupes {
                groups: Vec::new(),
                wasted_total: 0,
                elapsed_us: 42,
            },
            Response::Dupes {
                groups: vec![
                    DupeGroupRows {
                        size: 20_000,
                        wasted: 20_000,
                        rows: rows.clone(),
                    },
                    DupeGroupRows {
                        size: 5,
                        wasted: 5,
                        rows: vec![
                            Row {
                                path: r"C:\one.txt".into(),
                                size: 5,
                                mtime: 0,
                                is_dir: false,
                                files: 0,
                                own: 0,
                            },
                            Row {
                                path: r"C:\two.txt".into(),
                                size: 5,
                                mtime: 0,
                                is_dir: false,
                                files: 0,
                                own: 0,
                            },
                        ],
                    },
                ],
                wasted_total: 20_000,
                elapsed_us: 9_000_000,
            },
            Response::SizeInfo {
                path: r"C:\Users".into(),
                total: 1 << 40,
                alloc: 1 << 40,
                files: 1_516_374,
                children: rows.clone(),
            },
            Response::Status {
                nodes: 3_200_000,
                memory: 140 << 20,
                watching: true,
                changes_applied: 42,
                volumes: vec![
                    VolumeStatus {
                        drive: "C".into(),
                        files: 1_516_374,
                        size: 158 << 30,
                        next_usn: 47_928_925_696,
                        journal_active: true,
                    },
                    VolumeStatus {
                        drive: "D".into(),
                        files: 1_412_839,
                        size: 622 << 30,
                        next_usn: 0,
                        journal_active: false,
                    },
                ],
            },
            Response::Error {
                message: "no such path".into(),
            },
        ];
        for c in cases {
            let bytes = c.encode();
            assert_eq!(Response::decode(&bytes).unwrap(), c);
        }
    }

    #[test]
    fn frames_round_trip() {
        let payload = sample_search().encode();
        let mut buf = Vec::new();
        write_frame(&mut buf, &payload).unwrap();
        assert_eq!(buf.len(), payload.len() + 4);

        let mut cursor = std::io::Cursor::new(buf);
        let back = read_frame(&mut cursor).unwrap();
        assert_eq!(back, payload);
        assert_eq!(Request::decode(&back).unwrap(), sample_search());
    }

    #[test]
    fn an_oversized_frame_header_is_refused_before_allocating() {
        // The whole point: a client must not be able to make a privileged
        // daemon reserve gigabytes by lying about a length.
        let mut buf = (MAX_FRAME + 1).to_le_bytes().to_vec();
        buf.extend_from_slice(b"short");
        let mut cursor = std::io::Cursor::new(buf);
        assert!(matches!(
            read_frame(&mut cursor),
            Err(IpcError::TooLarge("frame"))
        ));
    }

    #[test]
    fn a_truncated_frame_is_an_error_not_a_hang() {
        let mut buf = 1000u32.to_le_bytes().to_vec();
        buf.extend_from_slice(b"only a few bytes");
        let mut cursor = std::io::Cursor::new(buf);
        assert!(read_frame(&mut cursor).is_err());
    }

    #[test]
    fn unknown_tags_are_rejected() {
        assert!(matches!(
            Request::decode(&[99]),
            Err(IpcError::UnknownTag(99))
        ));
        assert!(matches!(
            Response::decode(&[99]),
            Err(IpcError::UnknownTag(99))
        ));
        assert!(matches!(Request::decode(&[]), Err(IpcError::Truncated)));
        assert!(matches!(Response::decode(&[]), Err(IpcError::Truncated)));
    }

    #[test]
    fn a_lying_string_length_cannot_over_allocate() {
        // Claim a 60 KB string but supply two bytes.
        let mut buf = vec![tag::SIZE];
        buf.extend_from_slice(&60_000u32.to_le_bytes());
        buf.extend_from_slice(b"hi");
        assert!(matches!(Request::decode(&buf), Err(IpcError::Truncated)));

        // And a length beyond the hard cap is refused outright.
        let mut buf = vec![tag::SIZE];
        buf.extend_from_slice(&(MAX_STRING + 1).to_le_bytes());
        assert!(matches!(
            Request::decode(&buf),
            Err(IpcError::TooLarge("string"))
        ));
    }

    #[test]
    fn a_lying_row_count_cannot_over_allocate() {
        // A response claiming a million rows but carrying none must be
        // rejected on the count, not by trying to reserve for them.
        let mut buf = vec![tag::ROWS];
        buf.extend_from_slice(&5u64.to_le_bytes()); // total
        buf.extend_from_slice(&0u64.to_le_bytes()); // elapsed
        buf.extend_from_slice(&900_000u32.to_le_bytes()); // row count
        assert!(matches!(Response::decode(&buf), Err(IpcError::Truncated)));

        let mut buf = vec![tag::ROWS];
        buf.extend_from_slice(&0u64.to_le_bytes());
        buf.extend_from_slice(&0u64.to_le_bytes());
        buf.extend_from_slice(&(MAX_ROWS + 1).to_le_bytes());
        assert!(matches!(
            Response::decode(&buf),
            Err(IpcError::TooLarge("rows"))
        ));
    }

    #[test]
    fn a_lying_group_count_cannot_over_allocate() {
        // A response claiming many groups but carrying none must be rejected
        // on the count, not by reserving for them. Same for the copies within
        // a group.
        let mut buf = vec![tag::DUPES_INFO];
        buf.extend_from_slice(&0u64.to_le_bytes()); // wasted total
        buf.extend_from_slice(&0u64.to_le_bytes()); // elapsed
        buf.extend_from_slice(&900_000u32.to_le_bytes()); // group count
        assert!(matches!(Response::decode(&buf), Err(IpcError::Truncated)));

        // A group that claims more copies than the frame could hold.
        let mut buf = vec![tag::DUPES_INFO];
        buf.extend_from_slice(&0u64.to_le_bytes());
        buf.extend_from_slice(&0u64.to_le_bytes());
        buf.extend_from_slice(&1u32.to_le_bytes()); // one group
        buf.extend_from_slice(&100u64.to_le_bytes()); // size
        buf.extend_from_slice(&100u64.to_le_bytes()); // wasted
        buf.extend_from_slice(&800_000u32.to_le_bytes()); // copies
        assert!(matches!(Response::decode(&buf), Err(IpcError::Truncated)));
    }

    #[test]
    fn invalid_utf8_is_rejected() {
        let mut buf = vec![tag::SIZE];
        buf.extend_from_slice(&2u32.to_le_bytes());
        buf.extend_from_slice(&[0xFF, 0xFE]);
        buf.extend_from_slice(&0u32.to_le_bytes());
        assert!(matches!(Request::decode(&buf), Err(IpcError::BadUtf8)));
    }

    #[test]
    fn every_truncation_of_a_valid_message_is_handled() {
        // Feed the decoder every prefix of a well-formed message. None may
        // panic; each must simply fail.
        for msg in [sample_search().encode(), Request::Status.encode()] {
            for cut in 0..msg.len() {
                let _ = Request::decode(&msg[..cut]);
            }
        }
        let responses = [
            Response::Dupes {
                groups: vec![DupeGroupRows {
                    size: 7,
                    wasted: 7,
                    rows: vec![Row {
                        path: "x".into(),
                        size: 7,
                        mtime: 2,
                        is_dir: false,
                        files: 0,
                        own: 0,
                    }],
                }],
                wasted_total: 7,
                elapsed_us: 0,
            }
            .encode(),
            Response::Rows {
                rows: vec![Row {
                    path: "x".into(),
                    size: 1,
                    mtime: 2,
                    is_dir: true,
                    files: 3,
                    own: 4,
                }],
                total: 1,
                elapsed_us: 0,
            }
            .encode(),
            Response::Error {
                message: "boom".into(),
            }
            .encode(),
        ];
        for msg in responses {
            for cut in 0..msg.len() {
                let _ = Response::decode(&msg[..cut]);
            }
        }
    }

    #[test]
    fn arbitrary_bytes_never_panic() {
        // A crude fuzz sweep over the decoders, which run inside a privileged
        // process on input a normal user controls.
        let mut seed = 0x12345678u32;
        let mut rand = move || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed
        };
        for _ in 0..20_000 {
            let len = (rand() % 64) as usize;
            let buf: Vec<u8> = (0..len).map(|_| (rand() % 256) as u8).collect();
            let _ = Request::decode(&buf);
            let _ = Response::decode(&buf);
        }
    }

    #[test]
    fn strings_longer_than_the_cap_are_truncated_on_write() {
        let long = "a".repeat(MAX_STRING as usize + 100);
        let req = Request::Size { path: long, top: 1 };
        let bytes = req.encode();
        match Request::decode(&bytes).unwrap() {
            Request::Size { path, .. } => assert_eq!(path.len(), MAX_STRING as usize),
            other => panic!("unexpected {other:?}"),
        }
    }
}
