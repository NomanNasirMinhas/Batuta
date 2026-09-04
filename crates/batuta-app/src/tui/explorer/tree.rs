//! The directory tree.
//!
//! ## Why this reads the disk instead of the index
//!
//! Every other view in Batuta is served from the in-memory index, and there is
//! no `read_dir` anywhere else in the workspace. The tree is the exception, for
//! three reasons:
//!
//! - The index is **configured to exclude** `C:\Windows`, `C:\Program Files`
//!   and `C:\Program Files (x86)`. A file explorer that cannot show you
//!   `C:\Windows` is broken.
//! - Without a running daemon the index is a snapshot, as stale as its last
//!   scan. A tree is exactly where that is noticed.
//! - `Index::children` is a full scan of a multi-million-entry array *per
//!   directory*, and the cheap alternative allocates a map over the entire
//!   index. Listing one real directory is a handful of syscalls.
//!
//! Listing goes through [`Lister`] rather than calling `read_dir` directly, so
//! the ordering, expansion and flattening logic here is tested against a fake
//! filesystem — including the cases that are awkward to arrange on a real one,
//! like a directory that refuses to be read.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub is_dir: bool,
}

/// Where a directory's contents come from.
pub trait Lister {
    fn list(&self, dir: &Path) -> Result<Vec<Entry>, String>;
}

/// The real filesystem.
pub struct Disk;

impl Lister for Disk {
    fn list(&self, dir: &Path) -> Result<Vec<Entry>, String> {
        let read = std::fs::read_dir(dir).map_err(|e| friendly(&e))?;
        let mut out = Vec::new();
        for entry in read.flatten() {
            // `file_type` avoids a second stat, and a symlink whose target has
            // gone is reported as what it is rather than failing the listing.
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            out.push(Entry {
                name: entry.file_name().to_string_lossy().into_owned(),
                is_dir,
            });
        }
        Ok(out)
    }
}

/// Turn an I/O error into something worth putting on a row.
fn friendly(e: &std::io::Error) -> String {
    match e.kind() {
        std::io::ErrorKind::PermissionDenied => "access denied".into(),
        std::io::ErrorKind::NotFound => "no longer here".into(),
        _ => e.to_string(),
    }
}

/// Directories first, then by name, case-insensitively.
///
/// Matches how the result list already orders names, and matches Explorer, so
/// the tree does not read as a different program.
fn order(entries: &mut [Entry]) {
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| {
                a.name
                    .to_ascii_lowercase()
                    .cmp(&b.name.to_ascii_lowercase())
            })
            .then_with(|| a.name.cmp(&b.name))
    });
}

/// One visible line of the tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub path: PathBuf,
    pub name: String,
    pub depth: usize,
    pub is_dir: bool,
    pub expanded: bool,
    /// Set when this row stands in for a directory that could not be read.
    /// An unreadable directory drawn as an empty one is a lie: `C:\System
    /// Volume Information` is not empty, you just cannot see into it.
    pub error: Option<String>,
}

pub struct Tree {
    root: PathBuf,
    expanded: BTreeSet<PathBuf>,
    listing: HashMap<PathBuf, Result<Vec<Entry>, String>>,
    pub selected: usize,
    pub window_start: usize,
}

impl Tree {
    pub fn new(root: PathBuf) -> Self {
        Tree {
            root,
            expanded: BTreeSet::new(),
            listing: HashMap::new(),
            selected: 0,
            window_start: 0,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Move the tree to a different directory, keeping nothing.
    pub fn set_root(&mut self, root: PathBuf) {
        self.root = root;
        self.expanded.clear();
        self.listing.clear();
        self.selected = 0;
        self.window_start = 0;
    }

    /// Drop every cached listing so the next draw re-reads the disk.
    pub fn refresh(&mut self) {
        self.listing.clear();
    }

    fn children(&mut self, dir: &Path, lister: &dyn Lister) -> &Result<Vec<Entry>, String> {
        // Read once and remember: a directory is re-visited on every frame
        // while it is expanded, and `read_dir` on a network path is not free.
        self.listing.entry(dir.to_path_buf()).or_insert_with(|| {
            lister.list(dir).map(|mut es| {
                order(&mut es);
                es
            })
        })
    }

    pub fn is_expanded(&self, path: &Path) -> bool {
        self.expanded.contains(path)
    }

    pub fn expand(&mut self, path: &Path) {
        self.expanded.insert(path.to_path_buf());
    }

    pub fn collapse(&mut self, path: &Path) {
        self.expanded.remove(path);
    }

    pub fn toggle(&mut self, path: &Path) {
        if self.is_expanded(path) {
            self.collapse(path);
        } else {
            self.expand(path);
        }
    }

    /// Expand every directory on the way to `path`, so it is visible.
    pub fn reveal(&mut self, path: &Path) {
        let mut at = path;
        while let Some(parent) = at.parent() {
            if !parent.starts_with(&self.root) && parent != self.root {
                break;
            }
            self.expanded.insert(parent.to_path_buf());
            at = parent;
        }
    }

    /// The tree flattened into the lines to draw, in order.
    pub fn rows(&mut self, lister: &dyn Lister) -> Vec<Row> {
        let mut out = Vec::new();
        let root = self.root.clone();
        self.walk(&root, 0, lister, &mut out);
        out
    }

    fn walk(&mut self, dir: &Path, depth: usize, lister: &dyn Lister, out: &mut Vec<Row>) {
        let entries = match self.children(dir, lister) {
            Ok(entries) => entries.clone(),
            Err(why) => {
                out.push(Row {
                    path: dir.to_path_buf(),
                    name: String::new(),
                    depth,
                    is_dir: true,
                    expanded: true,
                    error: Some(why.clone()),
                });
                return;
            }
        };

        for entry in entries {
            let path = dir.join(&entry.name);
            let expanded = entry.is_dir && self.is_expanded(&path);
            out.push(Row {
                path: path.clone(),
                name: entry.name,
                depth,
                is_dir: entry.is_dir,
                expanded,
                error: None,
            });
            if expanded {
                self.walk(&path, depth + 1, lister, out);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A filesystem made of nothing, so the awkward cases are easy to arrange.
    struct Fake {
        dirs: HashMap<PathBuf, Result<Vec<Entry>, String>>,
    }

    impl Fake {
        fn new(entries: &[(&str, &[(&str, bool)])]) -> Self {
            let mut dirs = HashMap::new();
            for (dir, kids) in entries {
                dirs.insert(
                    PathBuf::from(dir),
                    Ok(kids
                        .iter()
                        .map(|(n, d)| Entry {
                            name: (*n).to_string(),
                            is_dir: *d,
                        })
                        .collect()),
                );
            }
            Fake { dirs }
        }

        fn deny(mut self, dir: &str) -> Self {
            self.dirs
                .insert(PathBuf::from(dir), Err("access denied".into()));
            self
        }
    }

    impl Lister for Fake {
        fn list(&self, dir: &Path) -> Result<Vec<Entry>, String> {
            self.dirs
                .get(dir)
                .cloned()
                .unwrap_or_else(|| Ok(Vec::new()))
        }
    }

    fn names(rows: &[Row]) -> Vec<String> {
        rows.iter()
            .map(|r| format!("{}{}", "  ".repeat(r.depth), r.name))
            .collect()
    }

    #[test]
    fn directories_come_first_then_names_case_insensitively() {
        let fs = Fake::new(&[(
            r"C:\p",
            &[
                ("zeta.txt", false),
                ("Alpha", true),
                ("beta.txt", false),
                ("alpha", true),
            ],
        )]);
        let mut t = Tree::new(r"C:\p".into());
        assert_eq!(
            names(&t.rows(&fs)),
            vec!["Alpha", "alpha", "beta.txt", "zeta.txt"]
        );
    }

    #[test]
    fn only_expanded_directories_show_their_contents() {
        let fs = Fake::new(&[
            (r"C:\p", &[("src", true), ("readme.md", false)]),
            (r"C:\p\src", &[("main.rs", false)]),
        ]);
        let mut t = Tree::new(r"C:\p".into());
        assert_eq!(names(&t.rows(&fs)), vec!["src", "readme.md"]);

        t.expand(Path::new(r"C:\p\src"));
        assert_eq!(names(&t.rows(&fs)), vec!["src", "  main.rs", "readme.md"]);

        t.collapse(Path::new(r"C:\p\src"));
        assert_eq!(names(&t.rows(&fs)), vec!["src", "readme.md"]);
    }

    #[test]
    fn nesting_carries_the_depth_for_indenting() {
        let fs = Fake::new(&[
            (r"C:\p", &[("a", true)]),
            (r"C:\p\a", &[("b", true)]),
            (r"C:\p\a\b", &[("deep.txt", false)]),
        ]);
        let mut t = Tree::new(r"C:\p".into());
        t.expand(Path::new(r"C:\p\a"));
        t.expand(Path::new(r"C:\p\a\b"));
        assert_eq!(names(&t.rows(&fs)), vec!["a", "  b", "    deep.txt"]);
    }

    #[test]
    fn an_unreadable_directory_says_why_instead_of_looking_empty() {
        // Drawing it as empty is a lie, and the kind that sends someone
        // hunting for files that are right there.
        let fs = Fake::new(&[(r"C:\p", &[("locked", true)])]).deny(r"C:\p\locked");
        let mut t = Tree::new(r"C:\p".into());
        t.expand(Path::new(r"C:\p\locked"));

        let rows = t.rows(&fs);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].error.as_deref(), Some("access denied"));
        assert_eq!(rows[1].depth, 1, "the reason sits under its directory");
    }

    #[test]
    fn an_unreadable_root_is_reported_rather_than_drawn_blank() {
        let fs = Fake::new(&[]).deny(r"C:\nope");
        let mut t = Tree::new(r"C:\nope".into());
        let rows = t.rows(&fs);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].error.is_some());
    }

    #[test]
    fn revealing_a_path_opens_every_directory_above_it() {
        let fs = Fake::new(&[
            (r"C:\p", &[("a", true)]),
            (r"C:\p\a", &[("b", true)]),
            (r"C:\p\a\b", &[("target.txt", false)]),
        ]);
        let mut t = Tree::new(r"C:\p".into());
        t.reveal(Path::new(r"C:\p\a\b\target.txt"));

        assert_eq!(
            names(&t.rows(&fs)),
            vec!["a", "  b", "    target.txt"],
            "the file it was asked to reveal has to be on screen"
        );
    }

    #[test]
    fn a_listing_is_read_once_and_remembered() {
        // Every frame redraws the tree; re-reading a network directory each
        // time would be felt.
        struct Counting {
            inner: Fake,
            reads: std::cell::Cell<usize>,
        }
        impl Lister for Counting {
            fn list(&self, dir: &Path) -> Result<Vec<Entry>, String> {
                self.reads.set(self.reads.get() + 1);
                self.inner.list(dir)
            }
        }

        let fs = Counting {
            inner: Fake::new(&[(r"C:\p", &[("a.txt", false)])]),
            reads: std::cell::Cell::new(0),
        };
        let mut t = Tree::new(r"C:\p".into());
        t.rows(&fs);
        t.rows(&fs);
        t.rows(&fs);
        assert_eq!(fs.reads.get(), 1);

        t.refresh();
        t.rows(&fs);
        assert_eq!(fs.reads.get(), 2, "F5 must actually go back to the disk");
    }

    #[test]
    fn changing_root_forgets_everything_about_the_old_one() {
        let fs = Fake::new(&[
            (r"C:\p", &[("a", true)]),
            (r"C:\p\a", &[("x.txt", false)]),
            (r"D:\q", &[("other.txt", false)]),
        ]);
        let mut t = Tree::new(r"C:\p".into());
        t.expand(Path::new(r"C:\p\a"));
        t.selected = 1;
        t.rows(&fs);

        t.set_root(r"D:\q".into());
        assert_eq!(names(&t.rows(&fs)), vec!["other.txt"]);
        assert_eq!(t.selected, 0, "a stale selection would point at nothing");
        assert!(!t.is_expanded(Path::new(r"C:\p\a")));
    }

    #[test]
    fn an_empty_directory_produces_no_rows_and_does_not_panic() {
        let fs = Fake::new(&[(r"C:\p", &[])]);
        let mut t = Tree::new(r"C:\p".into());
        assert!(t.rows(&fs).is_empty());
    }

    #[test]
    fn expanding_a_file_shows_nothing_under_it() {
        // Nothing stops the state from being set; the flattening must not act
        // on it.
        let fs = Fake::new(&[(r"C:\p", &[("a.txt", false)])]);
        let mut t = Tree::new(r"C:\p".into());
        t.expand(Path::new(r"C:\p\a.txt"));
        assert_eq!(names(&t.rows(&fs)), vec!["a.txt"]);
    }
}
