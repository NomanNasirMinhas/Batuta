//! Tests for index construction, rollup, pruning and search.

use crate::index::flags;
use crate::search::{
    glob_match, largest_dirs, orphans, own_bytes, MatchMode, Query, Searcher, SortBy,
};
use crate::testtree::{build_multi, TreeBuilder, ROOT_REC};
use crate::Index;

const KB: u64 = 1024;
const MB: u64 = 1024 * KB;

/// C:\ with a small but realistically shaped user profile.
fn sample_tree() -> Index {
    let mut t = TreeBuilder::new('C');
    let users = t.dir(ROOT_REC, "Users");
    let hacker = t.dir(users, "Hacker");
    let downloads = t.dir(hacker, "Downloads");
    let appdata = t.dir(hacker, "AppData");
    let local = t.dir(appdata, "Local");
    let cache = t.dir(local, "Cache");
    let windows = t.dir(ROOT_REC, "Windows");

    t.file(hacker, "notes.txt", 100);
    t.file(downloads, "installer.exe", 50 * MB);
    t.file(downloads, "Report.PDF", 2 * MB);
    t.file(cache, "blob1.bin", 10 * MB);
    t.file(cache, "blob2.bin", 5 * MB);
    t.file(windows, "kernel32.dll", MB);
    t.build()
}

fn node(idx: &Index, path: &str) -> u32 {
    idx.lookup(path)
        .unwrap_or_else(|| panic!("no such path: {path}"))
}

fn names_of(idx: &Index, nodes: &[u32]) -> Vec<String> {
    nodes.iter().map(|&n| idx.name(n).to_string()).collect()
}

#[test]
fn rollup_sums_sizes_up_the_tree() {
    let idx = sample_tree();

    assert_eq!(
        idx.size[node(&idx, r"C:\Users\Hacker\Downloads") as usize],
        52 * MB
    );
    assert_eq!(
        idx.size[node(&idx, r"C:\Users\Hacker\AppData\Local\Cache") as usize],
        15 * MB
    );
    assert_eq!(
        idx.size[node(&idx, r"C:\Users\Hacker\AppData\Local") as usize],
        15 * MB
    );
    assert_eq!(
        idx.size[node(&idx, r"C:\Users\Hacker\AppData") as usize],
        15 * MB
    );
    // 52 MB downloads + 15 MB appdata + 100 bytes of notes.
    assert_eq!(
        idx.size[node(&idx, r"C:\Users\Hacker") as usize],
        67 * MB + 100
    );
    assert_eq!(idx.size[node(&idx, r"C:\Users") as usize], 67 * MB + 100);
    // Root also carries Windows.
    assert_eq!(idx.size[idx.volumes[0].root as usize], 68 * MB + 100);
}

#[test]
fn rollup_counts_files_not_directories() {
    let idx = sample_tree();
    assert_eq!(
        idx.subtree_files[node(&idx, r"C:\Users\Hacker") as usize],
        5
    );
    assert_eq!(
        idx.subtree_files[node(&idx, r"C:\Users\Hacker\Downloads") as usize],
        2
    );
    assert_eq!(idx.subtree_files[idx.volumes[0].root as usize], 6);
    // A file is not a subtree of itself.
    assert_eq!(
        idx.subtree_files[node(&idx, r"C:\Users\Hacker\notes.txt") as usize],
        0
    );
}

#[test]
fn depths_are_assigned_from_the_volume_root() {
    let idx = sample_tree();
    assert_eq!(idx.depth[idx.volumes[0].root as usize], 0);
    assert_eq!(idx.depth[node(&idx, r"C:\Users") as usize], 1);
    assert_eq!(idx.depth[node(&idx, r"C:\Users\Hacker") as usize], 2);
    assert_eq!(
        idx.depth[node(&idx, r"C:\Users\Hacker\AppData\Local\Cache") as usize],
        5
    );
}

#[test]
fn paths_round_trip_through_lookup() {
    let idx = sample_tree();
    for p in [
        r"C:\Users",
        r"C:\Users\Hacker\notes.txt",
        r"C:\Users\Hacker\AppData\Local\Cache\blob1.bin",
        r"C:\Windows\kernel32.dll",
    ] {
        let n = node(&idx, p);
        assert_eq!(idx.path(n), p, "path reconstruction mismatch");
    }
    assert_eq!(idx.path(idx.volumes[0].root), r"C:\");
}

#[test]
fn lookup_is_case_insensitive_and_accepts_forward_slashes() {
    let idx = sample_tree();
    let want = node(&idx, r"C:\Users\Hacker\notes.txt");
    assert_eq!(idx.lookup(r"c:\users\hacker\NOTES.TXT"), Some(want));
    assert_eq!(idx.lookup("C:/Users/Hacker/notes.txt"), Some(want));
    assert_eq!(idx.lookup(r"C:\Users\Nobody"), None);
    assert_eq!(idx.lookup("not a path"), None);
}

#[test]
fn excluding_a_subtree_removes_its_bytes_from_ancestors() {
    let mut idx = sample_tree();
    let root_before = idx.size[idx.volumes[0].root as usize];
    let windows = node(&idx, r"C:\Windows");
    let win_size = idx.size[windows as usize];

    idx.exclude_subtree(windows);

    assert!(idx.is_excluded(windows));
    assert!(
        idx.is_excluded(node(&idx, r"C:\Windows\kernel32.dll")),
        "children are marked too"
    );
    assert_eq!(
        idx.size[idx.volumes[0].root as usize],
        root_before - win_size
    );
    // A sibling subtree is untouched.
    assert_eq!(
        idx.size[node(&idx, r"C:\Users\Hacker") as usize],
        67 * MB + 100
    );
}

#[test]
fn excluded_nodes_disappear_from_search() {
    let mut idx = sample_tree();
    idx.exclude_subtree(node(&idx, r"C:\Windows"));

    let mut s = Searcher::new();
    let hits = s.search(&idx, &Query::new("kernel32"));
    assert!(hits.nodes.is_empty(), "excluded files must not surface");

    let mut q = Query::new("kernel32");
    q.include_excluded = true;
    assert_eq!(s.search(&idx, &q).nodes.len(), 1, "opt-in still finds them");
}

/// The shape from the bug report: a file whose name carries one term and
/// whose directory carries the other.
fn sso_tree() -> Index {
    let mut t = TreeBuilder::new('D');
    let downloads = t.dir(ROOT_REC, "Downloads");
    let updates = t.dir(downloads, "SSO Updates");
    t.file(updates, "SSO 0.1.0.zip", 15 * MB);
    t.file(updates, "release notes.txt", 1024);
    let other = t.dir(downloads, "Invoices");
    t.file(other, "SSO 0.0.9.zip", 15 * MB);
    t.build()
}

#[test]
fn several_terms_match_across_the_whole_path() {
    // "sso" alone finds the zips; "sso updates" found nothing, because no
    // single *name* contains it — the directory carries half the query.
    let idx = sso_tree();
    let mut s = Searcher::new();

    let one = s.search(&idx, &Query::new("sso"));
    assert!(one.total >= 2, "the single term still works: {}", one.total);

    s.reset();
    let both = s.search(&idx, &Query::new("sso updates"));
    let names = names_of(&idx, &both.nodes);
    assert!(
        names.contains(&"SSO 0.1.0.zip".to_string()),
        "the file under SSO Updates must be found: {names:?}"
    );
    assert!(
        !names.contains(&"SSO 0.0.9.zip".to_string()),
        "the one under Invoices must not be: {names:?}"
    );
}

#[test]
fn a_term_may_be_carried_entirely_by_an_ancestor() {
    // Neither term is in this file's own name; both are on its path.
    let idx = sso_tree();
    let mut s = Searcher::new();
    let hits = s.search(&idx, &Query::new("sso notes"));
    assert_eq!(
        names_of(&idx, &hits.nodes),
        vec!["release notes.txt"],
        "a file inherits its directory's terms"
    );
}

#[test]
fn term_order_does_not_matter() {
    let idx = sso_tree();
    let mut a = Searcher::new();
    let mut b = Searcher::new();
    let forward = a.search(&idx, &Query::new("sso updates")).total;
    let backward = b.search(&idx, &Query::new("updates sso")).total;
    assert_eq!(forward, backward);
    assert!(forward > 0);
}

#[test]
fn every_term_has_to_match() {
    let idx = sso_tree();
    let mut s = Searcher::new();
    assert_eq!(
        s.search(&idx, &Query::new("sso nonexistent")).total,
        0,
        "one unmatched term must reject the whole query"
    );
}

#[test]
fn typing_a_second_term_does_not_narrow_away_ancestor_matches() {
    // The subtle one. Typing "sso" then " notes" narrows from the previous
    // result set, but "release notes.txt" was never in it: its own name has
    // no "sso". Narrowing across that change would silently lose it.
    let idx = sso_tree();
    let mut s = Searcher::new();

    s.search(&idx, &Query::new("sso"));
    let after = s.search(&idx, &Query::new("sso notes"));
    assert_eq!(
        names_of(&idx, &after.nodes),
        vec!["release notes.txt"],
        "a fresh scan must replace the narrowed set"
    );
    assert!(!after.narrowed, "this transition cannot be a narrowing");
}

#[test]
fn extra_whitespace_is_not_a_term() {
    let idx = sso_tree();
    let mut s = Searcher::new();
    let plain = s.search(&idx, &Query::new("sso")).total;
    s.reset();
    let padded = s.search(&idx, &Query::new("  sso  ")).total;
    assert_eq!(
        plain, padded,
        "padding must not turn this into a path query"
    );
}

#[test]
fn substring_search_is_case_insensitive_by_default() {
    let idx = sample_tree();
    let mut s = Searcher::new();

    let hits = s.search(&idx, &Query::new("report"));
    assert_eq!(names_of(&idx, &hits.nodes), vec!["Report.PDF"]);

    s.reset();
    let hits = s.search(&idx, &Query::new("BLOB"));
    assert_eq!(hits.total, 2);

    s.reset();
    let mut q = Query::new("report");
    q.case_sensitive = true;
    assert_eq!(
        s.search(&idx, &q).total,
        0,
        "case-sensitive must not match Report.PDF"
    );
}

/// The arena stores names back to back with no separators, so a naive scan
/// would match across a boundary. This is the regression test for that.
#[test]
fn matches_never_span_two_adjacent_names() {
    let mut t = TreeBuilder::new('C');
    t.file(ROOT_REC, "abc", 1);
    t.file(ROOT_REC, "def", 1);
    let idx = t.build();

    let mut s = Searcher::new();
    // The arena contains "...abcdef...", but "cd" belongs to no single name.
    assert_eq!(s.search(&idx, &Query::new("cd")).total, 0);
    s.reset();
    assert_eq!(s.search(&idx, &Query::new("bcde")).total, 0);
    s.reset();
    assert_eq!(s.search(&idx, &Query::new("abc")).total, 1);
    s.reset();
    assert_eq!(s.search(&idx, &Query::new("def")).total, 1);
}

/// Browsing a folder: `children_of` restricts the result set to one
/// directory, and `NamePrefix` matches the still-being-typed component.
#[test]
fn children_of_lists_a_directory_filtered_by_name_prefix() {
    let idx = sample_tree();
    let mut s = Searcher::new();

    // An empty prefix is the whole folder listing.
    let mut q = Query::new("");
    q.children_of = Some(node(&idx, r"C:\Users\Hacker\Downloads"));
    let mut names = names_of(&idx, &s.search(&idx, &q).nodes);
    names.sort();
    assert_eq!(names, vec!["Report.PDF", "installer.exe"]);

    // A partial component narrows to the names it could become.
    s.reset();
    let mut q = Query::new("rep");
    q.mode = MatchMode::NamePrefix;
    q.children_of = Some(node(&idx, r"C:\Users\Hacker\Downloads"));
    assert_eq!(
        names_of(&idx, &s.search(&idx, &q).nodes),
        vec!["Report.PDF"]
    );

    // The prefix is anchored, so "report" is not matched by "port".
    s.reset();
    let mut q = Query::new("port");
    q.mode = MatchMode::NamePrefix;
    q.children_of = Some(node(&idx, r"C:\Users\Hacker\Downloads"));
    assert_eq!(s.search(&idx, &q).total, 0);
}

#[test]
fn prefix_browsing_narrows_across_keystrokes() {
    // Typing deeper into a path one character at a time must narrow like a
    // substring search does, and still agree with a cold scan.
    let idx = sample_tree();
    let mut warm = Searcher::new();
    for text in ["i", "in", "ins"] {
        let mut q = Query::new(text);
        q.mode = MatchMode::NamePrefix;
        q.children_of = Some(node(&idx, r"C:\Users\Hacker\Downloads"));
        warm.search(&idx, &q);
    }
    let mut q = Query::new("inst");
    q.mode = MatchMode::NamePrefix;
    q.children_of = Some(node(&idx, r"C:\Users\Hacker\Downloads"));
    let narrowed = warm.search(&idx, &q);
    assert!(
        narrowed.narrowed,
        "a longer prefix should narrow, not rescan"
    );
    assert_eq!(names_of(&idx, &narrowed.nodes), vec!["installer.exe"]);
}

#[test]
fn children_of_composes_with_the_other_filters() {
    let idx = sample_tree();
    let mut s = Searcher::new();

    let mut q = Query::new("");
    q.children_of = Some(node(&idx, r"C:\Users\Hacker"));
    q.min_size = Some(10 * MB);
    q.files_only = true;
    assert_eq!(
        s.search(&idx, &q).total,
        0,
        "Hacker's own files are all small"
    );

    s.reset();
    let mut q = Query::new("");
    q.children_of = Some(idx.volumes[0].root);
    q.dirs_only = true;
    let mut names = names_of(&idx, &s.search(&idx, &q).nodes);
    names.sort();
    assert_eq!(
        names,
        vec!["Users", "Windows"],
        "only the root's own children"
    );
}

#[test]
fn incremental_narrowing_matches_a_cold_scan() {
    let idx = sample_tree();

    let mut warm = Searcher::new();
    warm.search(&idx, &Query::new("b"));
    warm.search(&idx, &Query::new("bl"));
    warm.search(&idx, &Query::new("blo"));
    let narrowed = warm.search(&idx, &Query::new("blob"));
    assert!(
        narrowed.narrowed,
        "typing forward should narrow, not rescan"
    );

    let mut cold = Searcher::new();
    let fresh = cold.search(&idx, &Query::new("blob"));

    assert!(!fresh.narrowed);
    assert_eq!(
        narrowed.nodes, fresh.nodes,
        "narrowing must not change results"
    );
}

#[test]
fn narrowing_is_abandoned_when_the_query_shrinks_or_filters_change() {
    let idx = sample_tree();
    let mut s = Searcher::new();

    s.search(&idx, &Query::new("blob"));
    let shorter = s.search(&idx, &Query::new("blo"));
    assert!(
        !shorter.narrowed,
        "deleting a character must trigger a rescan"
    );
    assert_eq!(shorter.total, 2);

    s.search(&idx, &Query::new("blo"));
    let mut q = Query::new("blob");
    q.min_size = Some(8 * MB);
    let filtered = s.search(&idx, &q);
    assert!(
        !filtered.narrowed,
        "changing filters must invalidate narrowing"
    );
    assert_eq!(names_of(&idx, &filtered.nodes), vec!["blob1.bin"]);
}

#[test]
fn filters_compose() {
    let idx = sample_tree();
    let mut s = Searcher::new();

    let mut q = Query::new("");
    q.ext = Some("bin".into());
    assert_eq!(s.search(&idx, &q).total, 2);

    s.reset();
    let mut q = Query::new("");
    q.min_size = Some(20 * MB);
    q.files_only = true;
    assert_eq!(
        names_of(&idx, &s.search(&idx, &q).nodes),
        vec!["installer.exe"]
    );

    s.reset();
    let mut q = Query::new("");
    q.dirs_only = true;
    q.under = Some(node(&idx, r"C:\Users\Hacker\AppData"));
    let mut got = names_of(&idx, &s.search(&idx, &q).nodes);
    got.sort();
    assert_eq!(got, vec!["AppData", "Cache", "Local"]);

    s.reset();
    let mut q = Query::new("");
    q.max_size = Some(1000);
    q.files_only = true;
    assert_eq!(names_of(&idx, &s.search(&idx, &q).nodes), vec!["notes.txt"]);
}

#[test]
fn scope_filter_restricts_to_a_subtree() {
    let idx = sample_tree();
    let mut s = Searcher::new();

    let mut q = Query::new("");
    q.files_only = true;
    q.under = Some(node(&idx, r"C:\Users\Hacker\Downloads"));
    let mut got = names_of(&idx, &s.search(&idx, &q).nodes);
    got.sort();
    assert_eq!(got, vec!["Report.PDF", "installer.exe"]);
}

#[test]
fn glob_mode_anchors_to_the_whole_name() {
    let idx = sample_tree();
    let mut s = Searcher::new();

    let mut q = Query::new("*.bin");
    q.mode = MatchMode::Glob;
    assert_eq!(s.search(&idx, &q).total, 2);

    s.reset();
    let mut q = Query::new("blob?.bin");
    q.mode = MatchMode::Glob;
    assert_eq!(s.search(&idx, &q).total, 2);

    s.reset();
    let mut q = Query::new("blob");
    q.mode = MatchMode::Glob;
    assert_eq!(s.search(&idx, &q).total, 0, "glob is not a substring match");
}

#[test]
fn glob_matcher_handles_stars_and_backtracking() {
    assert!(glob_match(b"*.txt", b"notes.txt", true));
    assert!(glob_match(b"*", b"anything", true));
    assert!(glob_match(b"a*b*c", b"axxbyyc", true));
    assert!(glob_match(b"*.TXT", b"notes.txt", true));
    assert!(!glob_match(b"*.TXT", b"notes.txt", false));
    assert!(!glob_match(b"a*b", b"axxc", true));
    assert!(glob_match(b"???", b"abc", true));
    assert!(!glob_match(b"???", b"abcd", true));
    // Pathological pattern must terminate rather than blow up.
    assert!(!glob_match(
        b"a*a*a*a*a*b",
        b"aaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        true
    ));
}

#[test]
fn sorting_and_limits() {
    let idx = sample_tree();
    let mut s = Searcher::new();

    let mut q = Query::new("");
    q.files_only = true;
    q.sort = SortBy::SizeDesc;
    q.limit = 3;
    let r = s.search(&idx, &q);
    assert_eq!(r.total, 6, "total counts all matches, not just the page");
    assert_eq!(
        names_of(&idx, &r.nodes),
        vec!["installer.exe", "blob1.bin", "blob2.bin"]
    );

    s.reset();
    let mut q = Query::new("");
    q.files_only = true;
    q.sort = SortBy::Name;
    q.limit = 2;
    assert_eq!(
        names_of(&idx, &s.search(&idx, &q).nodes),
        vec!["blob1.bin", "blob2.bin"]
    );
}

#[test]
fn offset_pages_through_a_result_set_without_gaps_or_repeats() {
    // What a scrolling UI needs: consecutive pages must tile the full result
    // set exactly, with no row appearing twice or going missing.
    let mut t = TreeBuilder::new('C');
    let d = t.dir(ROOT_REC, "data");
    for i in 0..250 {
        t.file(d, &format!("item_{i:03}.bin"), i as u64 + 1);
    }
    let idx = t.build();

    let mut collected = Vec::new();
    let mut s = Searcher::new();
    for page in 0..5 {
        let mut q = Query::new("item_");
        q.sort = SortBy::SizeDesc;
        q.offset = page * 50;
        q.limit = 50;
        let r = s.search(&idx, &q);
        assert_eq!(r.total, 250, "total counts all matches, not just the page");
        assert_eq!(r.nodes.len(), 50);
        collected.extend(r.nodes);
    }

    assert_eq!(collected.len(), 250);
    let mut unique = collected.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), 250, "pages must not overlap");

    // And the paged order matches one unpaged sort.
    let mut all = Query::new("item_");
    all.sort = SortBy::SizeDesc;
    all.limit = 250;
    s.reset();
    assert_eq!(s.search(&idx, &all).nodes, collected);
}

#[test]
fn scrolling_reuses_the_previous_ordering_instead_of_rescanning() {
    // Scrolling changes only the offset. Re-running the scan and the sort for
    // each new window is what made the UI stall for seconds per keypress on a
    // multi-million-row result.
    let mut t = TreeBuilder::new('C');
    let d = t.dir(ROOT_REC, "data");
    for i in 0..5_000 {
        t.file(d, &format!("item_{i:05}.bin"), i as u64 + 1);
    }
    let idx = t.build();

    let mut s = Searcher::new();
    let mut q = Query::new("item_");
    q.limit = 40;

    let first = s.search(&idx, &q);
    assert!(!first.cached, "the first query has to do the work");

    // Every subsequent window inside the ordered prefix comes from the cache.
    let mut pages = Vec::new();
    for step in 1..40 {
        q.offset = step * 40;
        let r = s.search(&idx, &q);
        assert!(
            r.cached,
            "scroll to offset {} should reuse the ordering",
            q.offset
        );
        assert_eq!(r.total, 5_000);
        pages.push(r.nodes);
    }

    // Cached pages must be identical to what a cold searcher produces.
    for (i, cached) in pages.iter().enumerate() {
        let mut cold = Searcher::new();
        let mut cq = Query::new("item_");
        cq.limit = 40;
        cq.offset = (i + 1) * 40;
        assert_eq!(
            &cold.search(&idx, &cq).nodes,
            cached,
            "page {i} differs from a cold scan"
        );
    }
}

#[test]
fn changing_the_sort_or_the_query_invalidates_the_cached_ordering() {
    let idx = sample_tree();
    let mut s = Searcher::new();

    let mut q = Query::new("");
    q.limit = 5;
    s.search(&idx, &q);
    assert!(
        s.search(&idx, &q).cached,
        "an identical repeat is served from cache"
    );

    q.sort = SortBy::SizeDesc;
    assert!(!s.search(&idx, &q).cached, "a different sort must re-order");

    let mut q2 = Query::new("blob");
    q2.limit = 5;
    q2.sort = SortBy::SizeDesc;
    assert!(!s.search(&idx, &q2).cached, "a different query must rescan");
}

#[test]
fn a_mutated_index_invalidates_the_cached_ordering() {
    use crate::watch::Change;

    let mut t = TreeBuilder::new('C');
    let d = t.dir(ROOT_REC, "data");
    t.file(d, "one.log", 10);
    let mut idx = t.build();
    let dir_rec = (0..64u64)
        .find(|&r| idx.node_of_record(0, r) == Some(idx.lookup(r"C:\data").unwrap()))
        .unwrap();

    let mut s = Searcher::new();
    let mut q = Query::new(".log");
    q.limit = 10;
    s.search(&idx, &q);
    assert!(s.search(&idx, &q).cached);

    idx.apply(&Change::Created {
        volume: 0,
        record: 4242,
        sequence: 1,
        parent_record: dir_rec,
        name: "two.log".into(),
        is_dir: false,
        size: 20,
        mtime: 0,
    });

    let r = s.search(&idx, &q);
    assert!(
        !r.cached,
        "a changed index must not be answered from a stale ordering"
    );
    assert_eq!(r.total, 2, "the new file must appear");
}

#[test]
fn name_ordering_is_case_insensitive_and_stable() {
    let mut t = TreeBuilder::new('C');
    let d = t.dir(ROOT_REC, "x");
    for n in ["Banana.txt", "apple.txt", "Cherry.txt", "APRICOT.txt"] {
        t.file(d, n, 1);
    }
    let idx = t.build();

    let mut s = Searcher::new();
    let mut q = Query::new(".txt");
    q.limit = 10;
    assert_eq!(
        names_of(&idx, &s.search(&idx, &q).nodes),
        vec!["apple.txt", "APRICOT.txt", "Banana.txt", "Cherry.txt"],
        "ordering must ignore case, not sort uppercase first"
    );
}

#[test]
fn an_offset_past_the_end_yields_nothing() {
    let idx = sample_tree();
    let mut s = Searcher::new();
    let mut q = Query::new("blob");
    q.offset = 100;
    q.limit = 50;
    let r = s.search(&idx, &q);
    assert_eq!(r.total, 2, "the total is still reported");
    assert!(r.nodes.is_empty());
}

#[test]
fn narrowing_is_abandoned_when_the_index_changes() {
    // The subtle one. Narrowing filters the *previous* result set, so a file
    // created between keystrokes was never in it and could never appear. The
    // generation guard forces a fresh scan instead.
    use crate::watch::Change;

    let mut t = TreeBuilder::new('C');
    let d = t.dir(ROOT_REC, "data");
    t.file(d, "alpha.txt", 10);
    let mut idx = t.build();
    let dir_rec = (0..64u64)
        .find(|&r| idx.node_of_record(0, r) == Some(idx.lookup(r"C:\data").unwrap()))
        .unwrap();

    let mut s = Searcher::new();
    assert_eq!(s.search(&idx, &Query::new("al")).total, 1);

    // A second matching file appears.
    idx.apply(&Change::Created {
        volume: 0,
        record: 5000,
        sequence: 1,
        parent_record: dir_rec,
        name: "alphabet.txt".into(),
        is_dir: false,
        size: 20,
        mtime: 0,
    });

    let r = s.search(&idx, &Query::new("alp"));
    assert!(
        !r.narrowed,
        "a mutated index must invalidate the cached matches"
    );
    assert_eq!(r.total, 2, "the newly created file must be found");

    // With the index steady again, narrowing resumes.
    let r = s.search(&idx, &Query::new("alph"));
    assert!(r.narrowed);
    assert_eq!(r.total, 2);
}

#[test]
fn narrowing_still_matches_a_cold_scan_after_a_deletion() {
    use crate::watch::Change;

    let mut t = TreeBuilder::new('C');
    let d = t.dir(ROOT_REC, "data");
    t.file(d, "keep_me.log", 10);
    let doomed = t.file(d, "delete_me.log", 20);
    let mut idx = t.build();

    let mut s = Searcher::new();
    assert_eq!(s.search(&idx, &Query::new("me")).total, 2);

    idx.apply(&Change::Deleted {
        volume: 0,
        record: doomed,
        sequence: 1,
    });

    let warm = s.search(&idx, &Query::new("me.log"));
    let mut cold = Searcher::new();
    let fresh = cold.search(&idx, &Query::new("me.log"));
    assert_eq!(warm.nodes, fresh.nodes);
    assert_eq!(
        warm.total, 1,
        "the deleted file must not survive in the cache"
    );
}

#[test]
fn the_journal_position_advances_and_never_goes_backwards() {
    // The checkpointed USN is what a restarting daemon resumes from. If it
    // never moved, it would age out of the journal's retained window and the
    // restart would silently skip everything since the original scan.
    let mut idx = sample_tree();
    assert_eq!(idx.volumes[0].next_usn, 0);

    idx.set_next_usn(0, 5_000);
    assert_eq!(idx.volumes[0].next_usn, 5_000);

    idx.set_next_usn(0, 9_000);
    assert_eq!(idx.volumes[0].next_usn, 9_000);

    // Batches can be handled out of order; the high-water mark must hold, or
    // a restart would re-read journal it had already consumed.
    idx.set_next_usn(0, 6_000);
    assert_eq!(idx.volumes[0].next_usn, 9_000, "position must not regress");

    // An unknown volume is ignored rather than panicking.
    idx.set_next_usn(99, 1_000);
    assert_eq!(idx.volumes[0].next_usn, 9_000);
}

#[test]
fn the_journal_position_survives_a_snapshot() {
    let mut idx = sample_tree();
    idx.set_next_usn(0, 47_928_925_696);

    let mut path = std::env::temp_dir();
    path.push(format!("batuta-usn-{}.idx", std::process::id()));
    idx.save(&path).unwrap();
    let back = Index::load(&path).unwrap();

    assert_eq!(
        back.volumes[0].next_usn, 47_928_925_696,
        "a restart must resume where the last checkpoint left off"
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn sorting_by_modified_time() {
    let mut t = TreeBuilder::new('C');
    // FILETIME ticks; larger is newer.
    t.file_at(ROOT_REC, "old.txt", 1, 130_000_000_000_000_000);
    t.file_at(ROOT_REC, "new.txt", 1, 133_000_000_000_000_000);
    t.file_at(ROOT_REC, "mid.txt", 1, 131_000_000_000_000_000);
    let idx = t.build();

    let mut s = Searcher::new();
    let mut q = Query::new(".txt");
    q.sort = SortBy::ModifiedDesc;
    assert_eq!(
        names_of(&idx, &s.search(&idx, &q).nodes),
        vec!["new.txt", "mid.txt", "old.txt"]
    );
}

#[test]
fn largest_dirs_ranks_by_rolled_up_size() {
    let idx = sample_tree();
    let top = largest_dirs(&idx, None, 3);
    assert_eq!(names_of(&idx, &top), vec!["Users", "Hacker", "Downloads"]);

    let scoped = largest_dirs(&idx, Some(node(&idx, r"C:\Users\Hacker\AppData")), 2);
    assert_eq!(names_of(&idx, &scoped), vec!["AppData", "Local"]);
}

#[test]
fn own_bytes_separates_local_weight_from_subtree_weight() {
    let idx = sample_tree();
    let kids = idx.children_map();

    // Hacker is huge, but almost none of that is its own files.
    let hacker = node(&idx, r"C:\Users\Hacker");
    assert_eq!(own_bytes(&idx, hacker, &kids), 100);
    assert_eq!(idx.size[hacker as usize], 67 * MB + 100);

    // Downloads holds its weight directly.
    let downloads = node(&idx, r"C:\Users\Hacker\Downloads");
    assert_eq!(own_bytes(&idx, downloads, &kids), 52 * MB);
}

#[test]
fn live_delta_propagates_to_every_ancestor() {
    let mut idx = sample_tree();
    let cache = node(&idx, r"C:\Users\Hacker\AppData\Local\Cache");
    let hacker = node(&idx, r"C:\Users\Hacker");
    let root = idx.volumes[0].root;
    let blob = node(&idx, r"C:\Users\Hacker\AppData\Local\Cache\blob1.bin");

    let (c0, h0, r0) = (
        idx.size[cache as usize],
        idx.size[hacker as usize],
        idx.size[root as usize],
    );

    // The file grows by 1 MB.
    idx.size[blob as usize] += MB;
    idx.propagate_delta(blob, MB as i64, MB as i64);

    assert_eq!(idx.size[cache as usize], c0 + MB);
    assert_eq!(idx.size[hacker as usize], h0 + MB);
    assert_eq!(idx.size[root as usize], r0 + MB);

    // And shrinking again restores the original totals.
    idx.size[blob as usize] -= MB;
    idx.propagate_delta(blob, -(MB as i64), -(MB as i64));
    assert_eq!(idx.size[cache as usize], c0);
    assert_eq!(idx.size[hacker as usize], h0);
    assert_eq!(idx.size[root as usize], r0);
}

#[test]
fn orphans_attach_to_the_root_and_still_count() {
    let mut t = TreeBuilder::new('C');
    t.file(ROOT_REC, "normal.txt", 100);
    t.orphan("lost.bin", 4 * MB);
    let idx = t.build();

    let root = idx.volumes[0].root;
    assert_eq!(
        idx.size[root as usize],
        4 * MB + 100,
        "orphan bytes are not lost"
    );
    assert_eq!(orphans(&idx).len(), 1);
    assert_eq!(idx.name(orphans(&idx)[0]), "lost.bin");
}

#[test]
fn multiple_volumes_keep_separate_record_spaces() {
    let mut c = TreeBuilder::new('C');
    let cu = c.dir(ROOT_REC, "Users");
    c.file(cu, "shared_name.txt", 10);

    let mut d = TreeBuilder::new('D');
    let dp = d.dir(ROOT_REC, "Projects");
    d.file(dp, "shared_name.txt", 20);

    let idx = build_multi(vec![c, d]);
    assert_eq!(idx.volumes.len(), 2);

    let mut s = Searcher::new();
    let hits = s.search(&idx, &Query::new("shared_name"));
    assert_eq!(hits.total, 2);

    let mut paths: Vec<String> = hits.nodes.iter().map(|&n| idx.path(n)).collect();
    paths.sort();
    assert_eq!(
        paths,
        vec![r"C:\Users\shared_name.txt", r"D:\Projects\shared_name.txt"]
    );

    // Sizes roll up within their own volume only.
    assert_eq!(idx.size[idx.volumes[0].root as usize], 10);
    assert_eq!(idx.size[idx.volumes[1].root as usize], 20);
}

#[test]
fn reparse_points_do_not_recurse_or_double_count() {
    let mut t = TreeBuilder::new('C');
    let users = t.dir(ROOT_REC, "Users");
    // A junction is its own record with no children beneath it.
    t.flagged(users, "OneDrive", flags::DIRECTORY | flags::REPARSE, 0);
    t.file(users, "real.txt", 1000);
    let idx = t.build();

    assert_eq!(idx.size[node(&idx, r"C:\Users") as usize], 1000);
    let junction = node(&idx, r"C:\Users\OneDrive");
    assert_ne!(idx.flags[junction as usize] & flags::REPARSE, 0);
    assert_eq!(idx.size[junction as usize], 0);
}

#[test]
fn deep_nesting_does_not_overflow_the_stack() {
    let mut t = TreeBuilder::new('C');
    let mut parent = ROOT_REC;
    for i in 0..400 {
        parent = t.dir(parent, &format!("level{i}"));
    }
    t.file(parent, "deep.txt", 777);
    let idx = t.build();

    assert_eq!(idx.size[idx.volumes[0].root as usize], 777);
    let deep = idx.lookup(r"C:\level0\level1\level2").unwrap();
    assert_eq!(idx.depth[deep as usize], 3);
    // Path building walks the whole chain without recursion.
    assert!(idx.path(node(&idx, r"C:\level0")).starts_with(r"C:\level0"));
}

#[test]
fn empty_index_is_harmless() {
    let idx = TreeBuilder::new('C').build();
    let mut s = Searcher::new();
    assert_eq!(s.search(&idx, &Query::new("anything")).total, 0);
    assert!(largest_dirs(&idx, None, 10).is_empty());
    assert_eq!(idx.len(), 1, "just the root");
}

#[test]
fn unicode_names_are_searchable() {
    let mut t = TreeBuilder::new('C');
    t.file(ROOT_REC, "café_日本語.txt", 10);
    t.file(ROOT_REC, "emoji_🦀.rs", 20);
    let idx = t.build();

    let mut s = Searcher::new();
    assert_eq!(s.search(&idx, &Query::new("日本")).total, 1);
    s.reset();
    assert_eq!(s.search(&idx, &Query::new("🦀")).total, 1);
    s.reset();
    // ASCII portions of a Unicode name still match case-insensitively.
    assert_eq!(s.search(&idx, &Query::new("CAF")).total, 1);
}

#[test]
fn memory_footprint_stays_within_budget() {
    // Roughly 50k files: extrapolate to the ~3.2M on this machine and check
    // the per-node cost is in the range the design targets (<150 MB total).
    let mut t = TreeBuilder::new('C');
    let d = t.dir(ROOT_REC, "data");
    for i in 0..50_000 {
        t.file(d, &format!("file_{i:06}.bin"), i as u64 * 512);
    }
    let idx = t.build();

    let per_node = idx.memory_bytes() as f64 / idx.len() as f64;
    assert!(
        per_node < 90.0,
        "per-node cost {per_node:.1} B would exceed the memory budget at scale"
    );
}

/// A tree shaped like the one that exposed the ranking problem: several names
/// containing "downloads", one of them actually called it, and one sorting
/// ahead of it alphabetically because it starts with an underscore.
fn downloads_tree() -> Index {
    let mut t = TreeBuilder::new('C');
    let users = t.dir(ROOT_REC, "Users");
    let hacker = t.dir(users, "Hacker");
    let python = t.dir(ROOT_REC, "Python313");
    let doc = t.dir(python, "Doc");

    t.dir(hacker, "Downloads");
    t.dir(doc, "_downloads");
    t.dir(hacker, "DownloadsFolder");
    t.dir(python, "zzz_downloads_cache");
    t.build()
}

#[test]
fn a_name_that_is_the_query_sorts_above_one_that_merely_contains_it() {
    // `_downloads` beats `Downloads` alphabetically, because `_` is 0x5F and
    // `d` is 0x64. Alphabetically right, and the wrong answer: the folder
    // actually called Downloads is the one being looked for.
    let idx = downloads_tree();
    let mut s = Searcher::default();
    let mut q = Query::new("downloads");
    q.sort = SortBy::Name;

    let names = names_of(&idx, &s.search(&idx, &q).nodes);
    assert_eq!(names[0], "Downloads", "exact match first, got {names:?}");
}

#[test]
fn names_starting_with_the_query_come_before_ones_that_only_contain_it() {
    let idx = downloads_tree();
    let mut s = Searcher::default();
    let mut q = Query::new("downloads");
    q.sort = SortBy::Name;

    let names = names_of(&idx, &s.search(&idx, &q).nodes);
    assert_eq!(
        names,
        vec![
            "Downloads",           // is the term
            "DownloadsFolder",     // starts with it
            "_downloads",          // merely contains it, and sorts first
            "zzz_downloads_cache", // of the two, alphabetically after
        ],
        "three tiers, alphabetical within each"
    );
}

#[test]
fn ranking_uses_any_term_not_the_whole_query() {
    // A multi-term query describes a path; the node at the end of it is named
    // after one term, never the sentence. `hacker downloads` has to put the
    // folder called Downloads first even though nothing is called that.
    let idx = downloads_tree();
    let mut s = Searcher::default();
    let mut q = Query::new("hacker downloads");
    q.sort = SortBy::Name;

    let names = names_of(&idx, &s.search(&idx, &q).nodes);
    assert_eq!(names[0], "Downloads", "got {names:?}");
}

#[test]
fn ranking_reaches_the_size_and_modified_sorts_too() {
    // Otherwise the exact answer sinks the moment you change the sort, which
    // is the one thing a sort should not do.
    let mut t = TreeBuilder::new('C');
    let d = t.dir(ROOT_REC, "d");
    t.file(d, "notes", 1);
    t.file(d, "notes_backup_huge", 900 * MB);
    let idx = t.build();

    let mut s = Searcher::default();
    let mut q = Query::new("notes");
    q.sort = SortBy::SizeDesc;
    assert_eq!(
        names_of(&idx, &s.search(&idx, &q).nodes)[0],
        "notes",
        "the exact name outranks the far larger file"
    );

    s.reset();
    q.sort = SortBy::ModifiedDesc;
    assert_eq!(names_of(&idx, &s.search(&idx, &q).nodes)[0], "notes");
}

#[test]
fn ranking_is_case_insensitive_like_the_matching() {
    let mut t = TreeBuilder::new('C');
    let d = t.dir(ROOT_REC, "d");
    t.dir(d, "_DOWNLOADS");
    t.dir(d, "DOWNLOADS");
    let idx = t.build();

    let mut s = Searcher::default();
    let mut q = Query::new("downloads");
    q.sort = SortBy::Name;
    assert_eq!(names_of(&idx, &s.search(&idx, &q).nodes)[0], "DOWNLOADS");
}

#[test]
fn an_empty_query_is_ranked_by_the_sort_alone() {
    // No terms means no tiers; listing everything must stay alphabetical.
    let idx = downloads_tree();
    let mut s = Searcher::default();
    let mut q = Query::new("");
    q.dirs_only = true;
    q.sort = SortBy::Name;

    let names = names_of(&idx, &s.search(&idx, &q).nodes);
    let mut expected = names.clone();
    expected.sort_by_key(|n| n.to_ascii_lowercase());
    assert_eq!(names, expected, "still plain alphabetical");
}

#[test]
fn among_equally_exact_matches_the_shallower_path_wins() {
    // Several folders genuinely called what you searched for is the normal
    // case, and their names cannot separate them. Node id would decide it by
    // accident of scan order.
    let mut t = TreeBuilder::new('C');
    let users = t.dir(ROOT_REC, "Users");
    let hacker = t.dir(users, "Hacker");
    let cloud = t.dir(hacker, "iCloudDrive");
    let appdata = t.dir(hacker, "AppData");
    let cache = t.dir(appdata, "cache");

    // Deliberately created before the shallow one, so scan order argues for
    // the wrong answer.
    t.dir(cache, "Downloads");
    t.dir(cloud, "Downloads");
    t.dir(hacker, "Downloads");
    let idx = t.build();

    let mut s = Searcher::default();
    let mut q = Query::new("downloads");
    q.sort = SortBy::Name;

    let paths: Vec<String> = s
        .search(&idx, &q)
        .nodes
        .iter()
        .map(|&n| idx.path(n))
        .collect();
    assert_eq!(paths[0], r"C:\Users\Hacker\Downloads", "got {paths:?}");
}

#[test]
fn ranking_leaves_the_result_set_a_permutation_of_the_matches() {
    // Grouping by rank rewrites the array in three pieces; dropping or
    // duplicating one would silently lose matches or show them twice.
    let mut t = TreeBuilder::new('C');
    let d = t.dir(ROOT_REC, "d");
    for i in 0..500 {
        t.file(d, &format!("item{i:03}_x.bin"), i as u64 + 1);
    }
    t.file(d, "x", 1);
    t.dir(d, "x_prefixed");
    let idx = t.build();

    let mut s = Searcher::default();
    for sort in [SortBy::Name, SortBy::SizeDesc, SortBy::ModifiedDesc] {
        s.reset();
        let mut q = Query::new("x");
        q.sort = sort;
        q.limit = 0;
        let r = s.search(&idx, &q);

        let mut seen = r.nodes.clone();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), r.nodes.len(), "{sort:?} duplicated a row");
        assert_eq!(r.nodes.len(), r.total, "{sort:?} lost a row");
        assert_eq!(idx.name(r.nodes[0]), "x", "{sort:?} exact match first");
    }
}

#[test]
fn paging_through_a_ranked_result_set_still_tiles_it_exactly() {
    // The ordered-prefix bookkeeping is what a scrolling UI relies on, and
    // grouping by rank is where it could go wrong.
    let mut t = TreeBuilder::new('C');
    let d = t.dir(ROOT_REC, "d");
    for i in 0..300 {
        t.file(d, &format!("note{i:03}.txt"), i as u64 + 1);
    }
    t.file(d, "note", 1);
    let idx = t.build();

    let mut s = Searcher::default();
    let mut seen = Vec::new();
    for page in 0..7 {
        let mut q = Query::new("note");
        q.sort = SortBy::Name;
        q.offset = page * 50;
        q.limit = 50;
        seen.extend(s.search(&idx, &q).nodes);
    }
    let mut sorted = seen.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), seen.len(), "a row appeared on two pages");
    assert_eq!(seen.len(), 301, "every match appears exactly once");
    assert_eq!(idx.name(seen[0]), "note", "the exact match leads");
}
