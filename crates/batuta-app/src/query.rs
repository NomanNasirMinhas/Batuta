//! Executing requests against an index, and rendering the results.
//!
//! Both the local path (load a snapshot, answer here) and the daemon path
//! (send over the pipe) produce the same [`Response`], so there is exactly one
//! implementation of each query and one renderer. A discrepancy between
//! "answered locally" and "answered by the daemon" is therefore not possible.

use std::time::Instant;

use batuta_core::search::{largest_dirs, own_bytes, MatchMode, Query, Searcher, SortBy};
use batuta_core::{ContentSource, Index};
use batuta_ipc::{Request, Response, Row, SearchArgs, VolumeStatus};

use crate::fmt;

fn row(idx: &Index, n: u32) -> Row {
    Row {
        path: idx.path(n),
        size: idx.size[n as usize],
        mtime: idx.mtime[n as usize],
        is_dir: idx.is_dir(n),
        files: idx.subtree_files[n as usize],
        own: 0,
    }
}

fn to_query(a: &SearchArgs, idx: &Index) -> Query {
    Query {
        text: a.query.clone(),
        mode: if a.glob {
            MatchMode::Glob
        } else {
            MatchMode::Substring
        },
        case_sensitive: a.case_sensitive,
        min_size: a.min_size,
        max_size: a.max_size,
        ext: a.ext.clone(),
        under: a.under.as_deref().and_then(|p| idx.lookup(p)),
        children_of: None,
        offset: a.offset as usize,
        dirs_only: a.dirs_only,
        files_only: a.files_only,
        include_excluded: a.include_excluded,
        sort: match a.sort {
            1 => SortBy::SizeDesc,
            2 => SortBy::ModifiedDesc,
            _ => SortBy::Name,
        },
        limit: a.limit as usize,
    }
}

/// Does this text name a location rather than a fragment of a name?
///
/// A drive spec (`C:`, `C:\`) or any separator says the user is typing a path
/// and expects the folder it names to be listed, not a name search — which
/// could never match anyway, since stored names contain no separators.
pub fn looks_like_path(text: &str) -> bool {
    has_drive(text) || text.contains('\\') || text.contains('/')
}

/// Does this text start with a drive spec, as a real path always does?
///
/// The difference between text somebody *pasted* and text somebody *typed*.
/// `C:\Users\Hacker\report` is a path that failed to resolve; `hacker\report`
/// is two words with a separator between them, and was never a path at all.
/// They deserve different treatment when nothing matches.
fn has_drive(text: &str) -> bool {
    let b = text.trim().as_bytes();
    b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic()
}

/// Split a path-like query into the directory to list and the partial name
/// still being typed.
///
/// The directory part must resolve exactly; the final component matches
/// children by name prefix, so every keystroke narrows the listing the way
/// typing narrows a search. Returns `None` when the path names no indexed
/// directory.
fn browse_target(idx: &Index, text: &str) -> Option<(u32, String)> {
    let text = text.trim();
    if !looks_like_path(text) {
        return None;
    }
    let (dir_text, partial) = match text.rsplit_once(['\\', '/']) {
        Some((d, p)) => (d, p),
        None => (text, ""),
    };
    let dir = idx.lookup(dir_text)?;
    // A trailing separator on a file names nothing listable.
    if !idx.is_dir(dir) {
        return None;
    }
    Some((dir, partial.to_string()))
}

/// The last non-empty path component, for the fallback search when a path does
/// not resolve: `C:\Nope\readme` becomes a plain search for `readme`.
pub fn last_segment(text: &str) -> &str {
    text.rsplit(['\\', '/'])
        .find(|s| !s.is_empty())
        .unwrap_or(text)
}

/// A path that resolved to nothing, rewritten as the terms it is made of.
///
/// `hacker\downloads` becomes `hacker downloads` — the same search a space
/// would have given, and matched against the whole path.
///
/// This used to keep only the last segment, on the grounds that a name can
/// never contain a separator so the full text could not match. That was true
/// when names were all that got searched. Since terms are matched against the
/// whole path, every segment the user typed is usable, and throwing all but
/// the last one away silently answered a different question from the one asked:
/// `hacker\downloads` returned the same rows as plain `downloads`.
///
/// A drive spec is dropped rather than kept as a term. `C:` is not a word
/// anybody is searching for, and as a term it would match nothing and take the
/// whole query down with it.
pub fn path_terms(text: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for seg in text.split(['\\', '/']) {
        let seg = seg.trim();
        if seg.is_empty() {
            continue;
        }
        let b = seg.as_bytes();
        if b.len() == 2 && b[1] == b':' && b[0].is_ascii_alphabetic() {
            continue;
        }
        out.push(seg);
    }
    out.join(" ")
}

/// Answer one request.
///
/// The `Searcher` is owned by the caller and reused across calls, which is
/// what keeps incremental narrowing alive: a client typing one character at a
/// time re-filters the previous result set instead of rescanning the arena.
/// Constructing a fresh one per request would silently make every keystroke a
/// full cold scan.
pub fn execute(
    idx: &Index,
    req: &Request,
    searcher: &mut Searcher,
    watching: bool,
    changes: u64,
) -> Response {
    match req {
        Request::Search(args) => {
            // A scope that resolves to nothing would otherwise silently widen
            // to the whole index, which is worse than an explicit error.
            if let Some(path) = &args.under {
                if idx.lookup(path).is_none() {
                    return Response::Error {
                        message: format!("no indexed directory matches {path}"),
                    };
                }
            }
            let mut q = to_query(args, idx);
            // A query shaped like a path browses the folder it names rather
            // than searching names, which could never contain a separator.
            // The decision is made here, server-side, so a thin client that
            // just forwards keystrokes gets the behaviour for free.
            let pathish = !args.glob && looks_like_path(&args.query);
            let browse = if pathish {
                browse_target(idx, &args.query)
            } else {
                None
            };

            let r = match &browse {
                Some((dir, partial)) => {
                    q.mode = MatchMode::NamePrefix;
                    q.children_of = Some(*dir);
                    q.text = partial.clone();
                    searcher.search(idx, &q)
                }
                // The path names no indexed folder — a typo, a drive the index
                // does not cover, or text that was never a path at all. Its
                // segments are still what the user asked for, so they become
                // AND terms rather than being thrown away.
                None if pathish => {
                    q.text = path_terms(&args.query);
                    searcher.search(idx, &q)
                }
                None => searcher.search(idx, &q),
            };

            // Nothing matched, and the query was path-shaped. The last
            // component is worth a plain name search — but only for the two
            // cases where dropping the rest is defensible:
            //
            // - the folder *did* resolve and the partial matched no child, so
            //   this widens a listing rather than discarding anything typed;
            // - the query names a drive, so it is a path somebody pasted that
            //   this index does not cover, and the file name is still a
            //   reasonable guess at what they wanted.
            //
            // Not for `hacker\downloads`, which was never a path. Retrying that
            // with fewer terms is exactly the behaviour that made it return the
            // rows of a plain `downloads` search.
            let retry = browse.is_some() || has_drive(&args.query);
            if r.nodes.is_empty() && retry {
                q.mode = MatchMode::Substring;
                q.children_of = None;
                q.text = last_segment(&args.query).to_string();
                let r = searcher.search(idx, &q);
                return Response::Rows {
                    rows: r.nodes.iter().map(|&n| row(idx, n)).collect(),
                    total: r.total as u64,
                    elapsed_us: r.elapsed.as_micros() as u64,
                };
            }
            Response::Rows {
                rows: r.nodes.iter().map(|&n| row(idx, n)).collect(),
                total: r.total as u64,
                elapsed_us: r.elapsed.as_micros() as u64,
            }
        }

        Request::Dupes {
            min_size,
            top,
            under,
        } => {
            let scope = match under {
                Some(p) => match idx.lookup(p) {
                    Some(n) => Some(n),
                    None => {
                        return Response::Error {
                            message: format!("no indexed directory matches {p}"),
                        }
                    }
                },
                None => None,
            };
            let started = Instant::now();
            let src = crate::content::FsContent::new(idx);
            let mut resp = dupes_response(idx, &src, *min_size, *top as usize, scope);
            if let Response::Dupes { elapsed_us, .. } = &mut resp {
                *elapsed_us = started.elapsed().as_micros() as u64;
            }
            resp
        }

        Request::Size { path, top } => {
            let Some(node) = idx.lookup(path) else {
                return Response::Error {
                    message: format!("no indexed directory matches {path}"),
                };
            };
            let mut children: Vec<u32> = idx
                .children(node)
                .into_iter()
                .filter(|&k| !idx.is_excluded(k) && !idx.is_deleted(k))
                .collect();
            children.sort_unstable_by_key(|&k| (std::cmp::Reverse(idx.size[k as usize]), k));
            children.truncate(*top as usize);

            Response::SizeInfo {
                path: idx.path(node),
                total: idx.size[node as usize],
                alloc: idx.alloc[node as usize],
                files: idx.subtree_files[node as usize] as u64,
                children: children.iter().map(|&k| row(idx, k)).collect(),
            }
        }

        Request::Bloat { top, under } => {
            let scope = match under {
                Some(p) => match idx.lookup(p) {
                    Some(n) => Some(n),
                    None => {
                        return Response::Error {
                            message: format!("no indexed directory matches {p}"),
                        }
                    }
                },
                None => None,
            };
            let dirs = largest_dirs(idx, scope, *top as usize);
            let kids = idx.children_map();
            Response::Rows {
                rows: dirs
                    .iter()
                    .map(|&n| {
                        // Own-bytes distinguishes a directory that is big in
                        // itself from one that merely contains something big.
                        let mut r = row(idx, n);
                        r.own = own_bytes(idx, n, &kids);
                        r
                    })
                    .collect(),
                total: dirs.len() as u64,
                elapsed_us: 0,
            }
        }

        Request::Status => Response::Status {
            nodes: idx.len() as u64,
            memory: idx.memory_bytes() as u64,
            watching,
            changes_applied: changes,
            volumes: idx
                .volumes
                .iter()
                .map(|v| VolumeStatus {
                    drive: v.drive.to_string(),
                    files: idx.subtree_files[v.root as usize] as u64,
                    size: idx.size[v.root as usize],
                    next_usn: v.next_usn.max(0) as u64,
                    journal_active: v.next_usn > 0,
                })
                .collect(),
        },

        Request::Rescan => Response::Error {
            message: "rescan is handled by the daemon".into(),
        },
    }
}

/// Upper bound on copy rows in one dupes response.
///
/// The pipe caps a frame at 8 MB; without a bound, a few enormous groups
/// (thousands of copies with long paths) could outgrow it. Groups are ranked
/// by reclaimable bytes, so truncating drops the least valuable ones.
const MAX_DUPE_ROWS: usize = 10_000;

/// Run duplicate detection and shape the response.
///
/// Generic over the content source so the tiering can be exercised against
/// in-memory data; over the pipe the daemon reads the real files.
fn dupes_response<S: ContentSource>(
    idx: &Index,
    src: &S,
    min_size: u64,
    top: usize,
    under: Option<u32>,
) -> Response {
    let (groups, stats) = batuta_core::dupes::find_duplicates(
        idx,
        src,
        &batuta_core::dupes::DupeOptions {
            min_size,
            under,
            limit: top,
            ..Default::default()
        },
    );

    let mut out = Vec::new();
    let mut rows = 0usize;
    for g in &groups {
        // A group is kept whole or skipped: half a group would understate the
        // copies and overstate the savings of deleting them.
        if rows + g.nodes.len() > MAX_DUPE_ROWS {
            break;
        }
        rows += g.nodes.len();
        out.push(batuta_ipc::DupeGroupRows {
            size: g.size,
            wasted: g.wasted(),
            rows: g.nodes.iter().map(|&n| row(idx, n)).collect(),
        });
    }
    Response::Dupes {
        groups: out,
        wasted_total: stats.wasted_bytes,
        elapsed_us: 0,
    }
}

// ------------------------------------------------------------------ renderers

/// Render a search or bloat response. `bloat` switches to the wider layout
/// that shows own-bytes alongside the rolled-up total.
pub fn render_rows(resp: &Response, bloat: bool) -> bool {
    match resp {
        Response::Rows {
            rows,
            total,
            elapsed_us,
        } => {
            if bloat {
                println!("{:>10}  {:>10}  {:>9}  PATH", "TOTAL", "OWN", "FILES");
                for r in rows {
                    println!(
                        "{:>10}  {:>10}  {:>9}  {}",
                        fmt::bytes(r.size),
                        fmt::bytes(r.own),
                        fmt::count(r.files as u64),
                        r.path
                    );
                }
                println!();
                println!("OWN is bytes held directly in that directory's own files.");
            } else {
                for r in rows {
                    let marker = if r.is_dir { "d" } else { " " };
                    println!(
                        "{marker} {:>10}  {}  {}",
                        fmt::bytes(r.size),
                        fmt::timestamp(r.mtime),
                        r.path
                    );
                }
                println!();
                println!(
                    "{} match{} in {:.2}ms{}",
                    fmt::count(*total),
                    if *total == 1 { "" } else { "es" },
                    *elapsed_us as f64 / 1000.0,
                    if *total > rows.len() as u64 {
                        format!(" (showing {})", rows.len())
                    } else {
                        String::new()
                    }
                );
            }
            true
        }
        Response::Error { message } => {
            eprintln!("error: {message}");
            false
        }
        other => {
            eprintln!("error: unexpected response {other:?}");
            false
        }
    }
}

pub fn render_size(resp: &Response) -> bool {
    match resp {
        Response::SizeInfo {
            path,
            total,
            alloc,
            files,
            children,
        } => {
            println!("{path}");
            println!("  total    {}", fmt::bytes(*total));
            println!("  on disk  {}", fmt::bytes(*alloc));
            println!("  files    {}", fmt::count(*files));
            if !children.is_empty() {
                println!();
                for c in children {
                    let marker = if c.is_dir { "d" } else { " " };
                    let name = c.path.rsplit('\\').next().unwrap_or(&c.path);
                    println!("{marker} {:>10}  {}", fmt::bytes(c.size), name);
                }
            }
            true
        }
        Response::Error { message } => {
            eprintln!("error: {message}");
            false
        }
        other => {
            eprintln!("error: unexpected response {other:?}");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use batuta_core::testtree::{TreeBuilder, ROOT_REC};

    const MB: u64 = 1024 * 1024;

    fn sample() -> Index {
        let mut t = TreeBuilder::new('C');
        let users = t.dir(ROOT_REC, "Users");
        let hacker = t.dir(users, "Hacker");
        let dl = t.dir(hacker, "Downloads");
        t.file(hacker, "notes.txt", 100);
        t.file(dl, "installer.exe", 50 * MB);
        t.file(dl, "report.pdf", 2 * MB);
        t.build()
    }

    fn search(idx: &Index, args: SearchArgs) -> Response {
        execute(idx, &Request::Search(args), &mut Searcher::new(), false, 0)
    }

    fn exec(idx: &Index, req: &Request) -> Response {
        execute(idx, req, &mut Searcher::new(), false, 0)
    }

    // ------------------------------------------------------------ path browse

    fn search_text(idx: &Index, text: &str) -> Vec<String> {
        match search(
            idx,
            SearchArgs {
                query: text.into(),
                limit: 50,
                ..Default::default()
            },
        ) {
            Response::Rows { rows, .. } => rows.into_iter().map(|r| r.path).collect(),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn a_path_that_resolves_to_nothing_searches_all_of_its_segments() {
        // The reported bug: `hacker\downloads` returned exactly the rows of a
        // plain `downloads` search, because everything but the last segment
        // was thrown away. `hacker` names no resolvable directory — it has no
        // drive — so this goes down the fallback.
        let idx = sample();

        let both = search_text(&idx, r"hacker\downloads");
        assert!(!both.is_empty(), "the obvious answer should still be found");
        assert!(
            both.iter().all(|p| p.to_lowercase().contains("hacker")),
            "every row has to satisfy the term that used to be discarded: {both:?}"
        );
        assert!(
            both.iter().any(|p| p == r"C:\Users\Hacker\Downloads"),
            "including the folder itself: {both:?}"
        );
    }

    #[test]
    fn a_drive_letter_is_not_treated_as_a_search_term() {
        // `C:` is not a word anybody is looking for, and as a term it would
        // match nothing and take the whole query down with it.
        assert_eq!(path_terms(r"C:\Users\Hacker"), "Users Hacker");
        assert_eq!(path_terms("c:/users/hacker"), "users hacker");
        assert_eq!(path_terms(r"hacker\downloads"), "hacker downloads");
        assert_eq!(path_terms(r"\downloads"), "downloads");
        assert_eq!(path_terms(r"C:\"), "", "a bare drive has no terms at all");
    }

    #[test]
    fn a_query_that_was_never_a_path_keeps_every_segment() {
        // No drive, so this is two words with a separator, not a pasted path.
        // Retrying it with fewer terms is exactly the bug being fixed.
        let idx = sample();
        let rows = search_text(&idx, "notes.txt/installer");
        assert!(
            rows.is_empty(),
            "no segment may be dropped to manufacture a result: {rows:?}"
        );
    }

    #[test]
    fn a_folder_that_resolves_but_matches_nothing_still_widens_to_a_search() {
        // The one surviving retry, and it drops no term the user typed: the
        // folder was found, the partial matched no child in it, so the partial
        // is worth looking for elsewhere.
        let idx = sample();
        assert_eq!(
            search_text(&idx, r"C:\Users\Hacker\report"),
            vec![r"C:\Users\Hacker\Downloads\report.pdf".to_string()],
        );
    }

    #[test]
    fn a_path_that_does_resolve_still_browses_rather_than_searching() {
        // The fallback must not have taken over the case that already worked.
        let idx = sample();
        assert_eq!(
            search_text(&idx, r"C:\Users\Hacker\Downloads\"),
            vec![
                r"C:\Users\Hacker\Downloads\installer.exe".to_string(),
                r"C:\Users\Hacker\Downloads\report.pdf".to_string(),
            ]
        );
    }

    #[test]
    fn a_typed_path_lists_the_contents_of_that_folder() {
        let idx = sample();

        // The reported bug: typing `C:\` found nothing, because names never
        // contain a separator. Now it is the volume root's folder listing.
        assert_eq!(
            search_text(&idx, r"C:\"),
            vec![r"C:\Users".to_string()],
            "the root's children, and nothing else"
        );
        // Case-insensitive, and a forward slash is still a separator.
        assert_eq!(search_text(&idx, "c:/"), vec![r"C:\Users".to_string()]);

        assert_eq!(
            search_text(&idx, r"C:\Users\Hacker\Downloads\"),
            vec![
                r"C:\Users\Hacker\Downloads\installer.exe".to_string(),
                r"C:\Users\Hacker\Downloads\report.pdf".to_string(),
            ],
            "sorted by name, like any other result"
        );
    }

    #[test]
    fn a_partial_path_component_narrows_the_listing() {
        let idx = sample();

        // Each keystroke of `C:\Users\Hacker\rep` narrows within the parent.
        assert_eq!(
            search_text(&idx, r"C:\Users\Hacker\n"),
            vec![r"C:\Users\Hacker\notes.txt".to_string()]
        );
        // A prefix, not a substring: `zz` matches no child of Downloads and
        // no name anywhere, so the browse and its fallback both come up empty.
        assert!(search_text(&idx, r"C:\Users\Hacker\Downloads\zz").is_empty());
    }

    #[test]
    fn a_path_that_resolves_to_a_file_shows_that_file() {
        let idx = sample();
        assert_eq!(
            search_text(&idx, r"C:\Users\Hacker\notes.txt"),
            vec![r"C:\Users\Hacker\notes.txt".to_string()]
        );
    }

    #[test]
    fn an_unresolvable_path_falls_back_to_a_name_search() {
        let idx = sample();

        // `C:\Nope\report` names no indexed folder, so the last component is
        // searched as a plain name instead of answering nothing.
        assert_eq!(
            search_text(&idx, r"C:\Nope\Report"),
            vec![r"C:\Users\Hacker\Downloads\report.pdf".to_string()]
        );
        // A trailing separator makes the whole path a dead end, but the final
        // component is still worth searching.
        assert_eq!(
            search_text(&idx, r"C:\Users\Hacker\Report\"),
            vec![r"C:\Users\Hacker\Downloads\report.pdf".to_string()]
        );
    }

    #[test]
    fn a_browse_scopes_to_the_requested_under() {
        // An explicit --under still applies on top of the browse: the root's
        // children are listed, minus anything outside the scope.
        let idx = sample();
        let r = search(
            &idx,
            SearchArgs {
                query: r"C:\".into(),
                under: Some(r"C:\Users".into()),
                limit: 10,
                ..Default::default()
            },
        );
        match r {
            Response::Rows { rows, .. } => {
                let paths: Vec<_> = rows.iter().map(|r| r.path.as_str()).collect();
                assert_eq!(paths, vec![r"C:\Users"], "the scope intersects the browse");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn plain_text_queries_still_search_names() {
        let idx = sample();
        // No drive spec, no separator: the old behaviour, unchanged.
        assert_eq!(
            search_text(&idx, "installer"),
            vec![r"C:\Users\Hacker\Downloads\installer.exe".to_string()]
        );
        assert_eq!(
            search_text(&idx, "report"),
            vec![r"C:\Users\Hacker\Downloads\report.pdf".to_string()]
        );
    }

    // ------------------------------------------------------------------ dupes

    /// In-memory content, so duplicate detection is testable without a disk.
    #[derive(Default)]
    struct MemContent(std::collections::HashMap<u32, Vec<u8>>);

    impl batuta_core::ContentSource for MemContent {
        fn read_at(&self, node: u32, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
            let d = self.0.get(&node).unwrap();
            let start = (offset as usize).min(d.len());
            let n = buf.len().min(d.len() - start);
            buf[..n].copy_from_slice(&d[start..start + n]);
            Ok(n)
        }
        fn hash_full(&self, node: u32) -> std::io::Result<[u8; 32]> {
            Ok(*blake3::hash(self.0.get(&node).unwrap()).as_bytes())
        }
    }

    #[test]
    fn dupes_maps_files_against_their_duplicated_paths() {
        let mut t = TreeBuilder::new('C');
        let d = t.dir(ROOT_REC, "data");
        let other = t.dir(ROOT_REC, "other");
        t.file(d, "same.bin", 5000);
        t.file(other, "copy of same.bin", 5000);
        t.file(d, "unique.bin", 5000);
        let idx = t.build();

        let mut src = MemContent::default();
        for p in [r"C:\data\same.bin", r"C:\other\copy of same.bin"] {
            src.0
                .insert(idx.lookup(p).unwrap(), b"identical bytes".to_vec());
        }
        src.0.insert(
            idx.lookup(r"C:\data\unique.bin").unwrap(),
            b"different bytes".to_vec(),
        );

        let resp = dupes_response(&idx, &src, 1, 10, None);
        match resp {
            Response::Dupes {
                groups,
                wasted_total,
                ..
            } => {
                assert_eq!(groups.len(), 1);
                assert_eq!(groups[0].size, 5000);
                assert_eq!(groups[0].wasted, 5000);
                let paths: Vec<_> = groups[0].rows.iter().map(|r| r.path.clone()).collect();
                assert_eq!(
                    paths,
                    vec![
                        r"C:\data\same.bin".to_string(),
                        r"C:\other\copy of same.bin".to_string()
                    ],
                    "both copies, in node order"
                );
                assert_eq!(wasted_total, 5000);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn dupes_honours_min_size_scope_and_top() {
        let mut t = TreeBuilder::new('C');
        let d = t.dir(ROOT_REC, "data");
        t.file(d, "big1.bin", 9000);
        t.file(d, "big2.bin", 9000);
        t.file(d, "small1.bin", 10);
        t.file(d, "small2.bin", 10);
        let idx = t.build();

        let mut src = MemContent::default();
        for p in [r"C:\data\big1.bin", r"C:\data\big2.bin"] {
            src.0.insert(idx.lookup(p).unwrap(), b"big".to_vec());
        }
        for p in [r"C:\data\small1.bin", r"C:\data\small2.bin"] {
            src.0.insert(idx.lookup(p).unwrap(), b"small".to_vec());
        }

        match dupes_response(&idx, &src, 100, 10, None) {
            Response::Dupes { groups, .. } => {
                assert_eq!(groups.len(), 1, "min_size drops only the small pair");
                assert_eq!(groups[0].size, 9000);
            }
            other => panic!("unexpected {other:?}"),
        }
        match dupes_response(&idx, &src, 10_000, 10, None) {
            Response::Dupes { groups, .. } => assert!(
                groups.iter().all(|g| g.size >= 10_000),
                "min_size filters every pair out"
            ),
            other => panic!("unexpected {other:?}"),
        }
        match dupes_response(&idx, &src, 1, 1, None) {
            Response::Dupes { groups, .. } => assert_eq!(groups.len(), 1, "top caps the groups"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn dupes_requests_survive_the_wire() {
        let idx = sample();
        let req = Request::Dupes {
            min_size: 1 << 20,
            top: 500,
            under: None,
        };
        let bytes = req.encode();
        assert_eq!(Request::decode(&bytes).unwrap(), req);

        // Whatever execute produces must encode and decode unchanged.
        let resp = execute(&idx, &req, &mut Searcher::new(), false, 0);
        let bytes = resp.encode();
        assert_eq!(
            Response::decode(&bytes).unwrap(),
            resp,
            "response changed on the wire"
        );
    }

    #[test]
    fn search_returns_rows_with_full_paths() {
        let idx = sample();
        let r = search(
            &idx,
            SearchArgs {
                query: "installer".into(),
                limit: 10,
                ..Default::default()
            },
        );
        match r {
            Response::Rows { rows, total, .. } => {
                assert_eq!(total, 1);
                assert_eq!(rows[0].path, r"C:\Users\Hacker\Downloads\installer.exe");
                assert_eq!(rows[0].size, 50 * MB);
                assert!(!rows[0].is_dir);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn search_honours_filters_and_sorting() {
        let idx = sample();
        let r = search(
            &idx,
            SearchArgs {
                query: String::new(),
                files_only: true,
                min_size: Some(MB),
                sort: 1, // size, descending
                limit: 10,
                ..Default::default()
            },
        );
        match r {
            Response::Rows { rows, .. } => {
                let names: Vec<_> = rows.iter().map(|r| r.path.as_str()).collect();
                assert_eq!(
                    names,
                    vec![
                        r"C:\Users\Hacker\Downloads\installer.exe",
                        r"C:\Users\Hacker\Downloads\report.pdf"
                    ]
                );
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn an_unresolvable_scope_is_an_error_not_a_silent_widening() {
        let idx = sample();
        let r = search(
            &idx,
            SearchArgs {
                query: "notes".into(),
                under: Some(r"C:\Nowhere".into()),
                limit: 10,
                ..Default::default()
            },
        );
        // Falling back to searching everything would quietly answer a
        // different question than the one asked.
        assert!(matches!(r, Response::Error { .. }));
    }

    #[test]
    fn size_reports_rolled_up_totals_and_children() {
        let idx = sample();
        match exec(
            &idx,
            &Request::Size {
                path: r"C:\Users\Hacker".into(),
                top: 10,
            },
        ) {
            Response::SizeInfo {
                path,
                total,
                files,
                children,
                ..
            } => {
                assert_eq!(path, r"C:\Users\Hacker");
                assert_eq!(total, 52 * MB + 100);
                assert_eq!(files, 3);
                // Largest child first.
                assert_eq!(children[0].path, r"C:\Users\Hacker\Downloads");
                assert_eq!(children[0].size, 52 * MB);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn size_of_a_missing_path_is_an_error() {
        let idx = sample();
        let r = exec(
            &idx,
            &Request::Size {
                path: r"C:\Nope".into(),
                top: 5,
            },
        );
        assert!(matches!(r, Response::Error { .. }));
    }

    #[test]
    fn bloat_ranks_directories_and_reports_own_bytes() {
        let idx = sample();
        match exec(
            &idx,
            &Request::Bloat {
                top: 3,
                under: None,
            },
        ) {
            Response::Rows { rows, .. } => {
                assert_eq!(rows[0].path, r"C:\Users");
                assert_eq!(rows[1].path, r"C:\Users\Hacker");
                assert_eq!(rows[2].path, r"C:\Users\Hacker\Downloads");
                // Hacker is big because of Downloads, not its own files.
                assert_eq!(rows[1].own, 100);
                assert_eq!(rows[1].files, 3, "the file count must survive too");
                // Downloads holds its weight directly.
                assert_eq!(rows[2].own, 52 * MB);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn status_summarises_the_index() {
        let idx = sample();
        match execute(&idx, &Request::Status, &mut Searcher::new(), true, 17) {
            Response::Status {
                nodes,
                watching,
                changes_applied,
                volumes,
                ..
            } => {
                assert_eq!(nodes as usize, idx.len());
                assert!(watching);
                assert_eq!(changes_applied, 17);
                assert_eq!(volumes.len(), 1);
                assert_eq!(volumes[0].drive, "C");
                assert_eq!(volumes[0].files, 3);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn a_reused_searcher_narrows_across_requests() {
        // This is the property a typing UI depends on. Each keystroke is a
        // separate request, so the Searcher has to be owned by the caller and
        // carried between them; building one per request would silently turn
        // every keystroke into a full cold scan.
        let mut t = TreeBuilder::new('C');
        let d = t.dir(ROOT_REC, "data");
        for i in 0..2000 {
            t.file(d, &format!("document_{i:04}.txt"), i as u64);
        }
        t.file(d, "installer.exe", 1);
        let idx = t.build();

        let mut searcher = Searcher::new();
        let mut totals = Vec::new();
        for prefix in ["i", "in", "ins", "inst"] {
            let req = Request::Search(SearchArgs {
                query: prefix.into(),
                limit: 10,
                ..Default::default()
            });
            match execute(&idx, &req, &mut searcher, false, 0) {
                Response::Rows { total, .. } => totals.push(total),
                other => panic!("unexpected {other:?}"),
            }
        }
        // Narrowing must not change the answers.
        assert_eq!(totals.last(), Some(&1));

        // And a fresh searcher reaches the same result for the final query.
        let req = Request::Search(SearchArgs {
            query: "inst".into(),
            limit: 10,
            ..Default::default()
        });
        let cold = execute(&idx, &req, &mut Searcher::new(), false, 0);
        let warm = execute(&idx, &req, &mut searcher, false, 0);
        match (cold, warm) {
            (Response::Rows { rows: a, .. }, Response::Rows { rows: b, .. }) => assert_eq!(a, b),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn offset_lets_a_client_page_through_results() {
        let mut t = TreeBuilder::new('C');
        let d = t.dir(ROOT_REC, "data");
        for i in 0..100 {
            t.file(d, &format!("row_{i:03}.bin"), i as u64 + 1);
        }
        let idx = t.build();

        let page = |offset: u32| -> Vec<String> {
            let req = Request::Search(SearchArgs {
                query: "row_".into(),
                sort: 1,
                offset,
                limit: 10,
                ..Default::default()
            });
            match execute(&idx, &req, &mut Searcher::new(), false, 0) {
                Response::Rows { rows, total, .. } => {
                    assert_eq!(total, 100, "total is the full match count, not the page");
                    rows.into_iter().map(|r| r.path).collect()
                }
                other => panic!("unexpected {other:?}"),
            }
        };

        let first = page(0);
        let second = page(10);
        assert_eq!(first.len(), 10);
        assert_eq!(second.len(), 10);
        assert!(
            first.iter().all(|p| !second.contains(p)),
            "pages must not overlap"
        );
        // Past the end is empty, not an error.
        assert!(page(500).is_empty());
    }

    #[test]
    fn every_response_survives_the_wire() {
        // The local and daemon paths must agree, so anything execute produces
        // has to encode and decode unchanged.
        let idx = sample();
        let requests = vec![
            Request::Search(SearchArgs {
                query: "e".into(),
                limit: 50,
                ..Default::default()
            }),
            Request::Size {
                path: r"C:\Users".into(),
                top: 5,
            },
            Request::Bloat {
                top: 5,
                under: None,
            },
            Request::Status,
        ];
        for req in requests {
            let encoded = req.encode();
            assert_eq!(Request::decode(&encoded).unwrap(), req);

            let resp = execute(&idx, &req, &mut Searcher::new(), false, 0);
            let bytes = resp.encode();
            assert_eq!(
                Response::decode(&bytes).unwrap(),
                resp,
                "response changed on the wire"
            );
        }
    }
}
