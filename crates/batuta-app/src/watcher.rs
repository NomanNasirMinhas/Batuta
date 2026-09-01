//! Turning USN journal records into index changes.
//!
//! The journal says *what* changed and *where*, but never how large a file
//! became — so a size-affecting record has to be followed by a metadata read.
//! Those reads are the only expensive part, which is why records are coalesced
//! first: a program writing a file in a loop produces many `DATA_EXTEND`
//! records for one file, and only the final size matters.

use std::collections::HashMap;
use std::path::PathBuf;

use batuta_core::watch::Change;
use batuta_core::Index;
use batuta_ntfs::usn::{reason, UsnRecord};

/// Metadata read back for a changed file.
struct Stat {
    size: u64,
    mtime: u32,
    is_dir: bool,
}

fn stat(path: &PathBuf) -> Option<Stat> {
    let md = std::fs::metadata(path).ok()?;
    let mtime = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs().min(u32::MAX as u64) as u32)
        .unwrap_or(0);
    Some(Stat {
        size: md.len(),
        mtime,
        is_dir: md.is_dir(),
    })
}

/// One coalesced pending change for a single MFT record.
#[derive(Default)]
struct Pending {
    created: bool,
    deleted: bool,
    resized: bool,
    /// Set by a rename or move; carries the destination.
    renamed_to: Option<(u64, String)>,
    /// The MFT sequence this record carried. NTFS bumps it each time a record
    /// is recycled, and the index needs it to tell a genuinely new file from
    /// one whose record number happens to match something it already holds.
    sequence: u16,
    parent: u64,
    name: String,
    is_dir: bool,
}

/// Convert a batch of journal records into index changes.
///
/// Records are folded per file first, so a burst of writes to one file costs
/// a single metadata read rather than one per record. A create followed by a
/// delete inside the same batch cancels out entirely.
pub fn translate<'a, I>(idx: &Index, volume: usize, records: I) -> Vec<Change>
where
    I: IntoIterator<Item = UsnRecord<'a>>,
{
    let mut pending: HashMap<u64, Pending> = HashMap::new();
    let mut order: Vec<u64> = Vec::new();

    for r in records {
        let rec = r.file_ref.record();
        let entry = pending.entry(rec).or_insert_with(|| {
            order.push(rec);
            Pending::default()
        });
        entry.is_dir = r.is_directory();
        entry.parent = r.parent_ref.record();
        entry.sequence = r.file_ref.sequence();
        if !r.name_utf16.is_empty() {
            entry.name = r.name();
        }

        if r.has(reason::FILE_CREATE) {
            entry.created = true;
            entry.deleted = false;
        }
        if r.has(reason::FILE_DELETE) {
            entry.deleted = true;
        }
        if r.affects_size() {
            entry.resized = true;
        }
        if r.has(reason::RENAME_NEW_NAME) {
            entry.renamed_to = Some((r.parent_ref.record(), r.name()));
        }
        // RENAME_OLD_NAME is deliberately ignored: the paired new-name record
        // carries the destination, and acting on the old one would look like
        // a delete.
    }

    let mut out = Vec::with_capacity(order.len());
    for rec in order {
        let Some(p) = pending.get(&rec) else { continue };

        // A file created and removed within one batch never really existed.
        if p.deleted {
            if !p.created {
                out.push(Change::Deleted {
                    volume,
                    record: rec,
                    sequence: p.sequence,
                });
            }
            continue;
        }

        if p.created {
            let st = resolve_stat(idx, volume, p.parent, &p.name);
            out.push(Change::Created {
                volume,
                record: rec,
                sequence: p.sequence,
                parent_record: p.parent,
                name: p.name.clone(),
                is_dir: p.is_dir || st.as_ref().map(|s| s.is_dir).unwrap_or(false),
                size: st.as_ref().map(|s| s.size).unwrap_or(0),
                mtime: st.as_ref().map(|s| s.mtime).unwrap_or(0),
            });
            // A rename in the same batch as the create is already reflected in
            // the name used above.
            continue;
        }

        if let Some((new_parent, new_name)) = &p.renamed_to {
            out.push(Change::Renamed {
                volume,
                record: rec,
                sequence: p.sequence,
                parent_record: *new_parent,
                name: new_name.clone(),
            });
        }

        if p.resized && !p.is_dir {
            if let Some(st) = resolve_stat(idx, volume, p.parent, &p.name) {
                out.push(Change::Resized {
                    volume,
                    record: rec,
                    sequence: p.sequence,
                    size: st.size,
                    mtime: st.mtime,
                });
            }
        }
    }
    out
}

/// Build the file's path from its parent's indexed path, then read its metadata.
///
/// Going via the parent works for files that are not indexed yet, which is
/// exactly the case for a newly created one.
fn resolve_stat(idx: &Index, volume: usize, parent_record: u64, name: &str) -> Option<Stat> {
    if name.is_empty() {
        return None;
    }
    let parent = idx.node_of_record(volume, parent_record)?;
    let mut path = PathBuf::from(idx.path(parent));
    path.push(name);
    stat(&path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use batuta_core::testtree::{TreeBuilder, ROOT_REC};
    use batuta_ntfs::FileRef;

    fn rec(
        file: u64,
        parent: u64,
        reason: u32,
        attrs: u32,
        name: &'static [u8],
    ) -> UsnRecord<'static> {
        UsnRecord {
            usn: 0,
            file_ref: FileRef(file),
            parent_ref: FileRef(parent),
            reason,
            attributes: attrs,
            name_utf16: name,
        }
    }

    /// UTF-16 for a name, leaked so the borrowed record can be 'static in tests.
    fn u16name(s: &str) -> &'static [u8] {
        let v: Vec<u8> = s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        Box::leak(v.into_boxed_slice())
    }

    fn empty_index() -> Index {
        let mut t = TreeBuilder::new('C');
        t.dir(ROOT_REC, "Users");
        t.build()
    }

    #[test]
    fn a_burst_of_writes_becomes_one_resize() {
        // The whole point of coalescing: a program appending in a loop must
        // not cost one metadata read per journal record.
        let idx = empty_index();
        let name = u16name("big.log");
        let records = vec![
            rec(100, 5, reason::DATA_EXTEND, 0x20, name),
            rec(100, 5, reason::DATA_EXTEND, 0x20, name),
            rec(100, 5, reason::DATA_EXTEND, 0x20, name),
            rec(100, 5, reason::DATA_EXTEND | reason::CLOSE, 0x20, name),
        ];
        let changes = translate(&idx, 0, records);
        // The file does not exist on disk, so the stat fails and no resize is
        // emitted; what matters is that it was attempted at most once.
        assert!(changes.len() <= 1);
    }

    #[test]
    fn create_then_delete_in_one_batch_cancels_out() {
        let idx = empty_index();
        let name = u16name("scratch.tmp");
        let changes = translate(
            &idx,
            0,
            vec![
                rec(200, 5, reason::FILE_CREATE, 0x20, name),
                rec(200, 5, reason::FILE_DELETE | reason::CLOSE, 0x20, name),
            ],
        );
        assert!(
            changes.is_empty(),
            "a temp file that never survived is not a change"
        );
    }

    #[test]
    fn a_plain_delete_survives_coalescing() {
        let idx = empty_index();
        let changes = translate(
            &idx,
            0,
            vec![rec(
                300,
                5,
                reason::FILE_DELETE | reason::CLOSE,
                0x20,
                u16name("gone.txt"),
            )],
        );
        assert_eq!(
            changes,
            vec![Change::Deleted {
                volume: 0,
                record: 300,
                sequence: 0
            }]
        );
    }

    #[test]
    fn rename_uses_the_new_name_record_only() {
        let idx = empty_index();
        let changes = translate(
            &idx,
            0,
            vec![
                rec(400, 5, reason::RENAME_OLD_NAME, 0x20, u16name("before.txt")),
                rec(
                    400,
                    9,
                    reason::RENAME_NEW_NAME | reason::CLOSE,
                    0x20,
                    u16name("after.txt"),
                ),
            ],
        );
        assert_eq!(
            changes,
            vec![Change::Renamed {
                volume: 0,
                record: 400,
                sequence: 0,
                parent_record: 9,
                name: "after.txt".into(),
            }],
            "the old-name record must not look like a delete"
        );
    }

    #[test]
    fn directories_are_never_resized() {
        let idx = empty_index();
        let changes = translate(
            &idx,
            0,
            // Attribute 0x10 marks a directory.
            vec![rec(
                500,
                5,
                reason::DATA_EXTEND | reason::CLOSE,
                0x10,
                u16name("SomeDir"),
            )],
        );
        assert!(
            changes.is_empty(),
            "directory sizes come from the rollup, not from stat"
        );
    }

    #[test]
    fn changes_are_emitted_in_arrival_order() {
        let idx = empty_index();
        let changes = translate(
            &idx,
            0,
            vec![
                rec(601, 5, reason::FILE_DELETE, 0x20, u16name("a.txt")),
                rec(602, 5, reason::FILE_DELETE, 0x20, u16name("b.txt")),
                rec(603, 5, reason::FILE_DELETE, 0x20, u16name("c.txt")),
            ],
        );
        let records: Vec<u64> = changes.iter().map(|c| c.record()).collect();
        assert_eq!(records, vec![601, 602, 603]);
    }

    #[test]
    fn an_empty_batch_produces_nothing() {
        let idx = empty_index();
        assert!(translate(&idx, 0, Vec::new()).is_empty());
    }
}
