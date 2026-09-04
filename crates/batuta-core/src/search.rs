//! Search over the name arena.
//!
//! There is deliberately no inverted index. The arena is one contiguous block
//! of roughly 45 MB for this machine's ~3.2M files, and a SIMD substring scan
//! runs at multiple GB/s, so a full pass costs single-digit milliseconds on one
//! core and well under a millisecond across all of them. An inverted index
//! would add tens of megabytes of resident memory to save time that is already
//! imperceptible.
//!
//! The behaviour that makes typing feel instant is incremental narrowing: when
//! a query extends the previous one, only the previous (tiny) result set is
//! rescanned. The expensive full pass happens once per fresh query.

use std::cmp::Ordering;
use std::time::{Duration, Instant};

use memchr::memmem;
use rayon::prelude::*;

use crate::index::{flags, Index, NO_NODE};

/// The whitespace-separated terms of a substring query.
///
/// One term is matched against names, as it always was. Several are ANDed
/// against the *whole path*: `sso updates` finds
/// `D:\Downloads\SSO Updates\SSO 0.1.0.zip` because one term matches the file
/// and the other an ancestor directory. Matching names alone could never find
/// that — no single name contains "sso updates" — yet it is the shape of most
/// real searches, where you remember roughly where a thing lives as well as
/// roughly what it is called.
pub fn terms(text: &str) -> Vec<&str> {
    text.split_whitespace().collect()
}

/// Terms carried in the prefilter's bitmask. Beyond this the mask is a
/// superset and the authoritative walk finishes the job.
const MASK_TERMS: usize = 8;

/// Does any name on this node's path contain every term?
///
/// The authoritative answer, and deliberately the only one: the fast pass
/// below is a prefilter whose survivors all come back through here, so the two
/// cannot drift apart into disagreeing about what matches.
fn path_matches(idx: &Index, n: u32, terms: &[&str], case_sensitive: bool) -> bool {
    if terms.is_empty() {
        return true;
    }
    let all: u32 = if terms.len() >= 32 {
        u32::MAX
    } else {
        (1u32 << terms.len()) - 1
    };
    let mut got = 0u32;
    let mut cur = n;

    // A corrupt parent chain must not hang the UI; no real path is this deep.
    for _ in 0..4096 {
        let name = idx.name_bytes(cur);
        for (i, t) in terms.iter().enumerate().take(32) {
            if got & (1 << i) == 0 && name_contains(name, t.as_bytes(), case_sensitive) {
                got |= 1 << i;
            }
        }
        if got == all {
            return true;
        }
        if idx.is_root(cur) {
            return false;
        }
        let parent = idx.parent[cur as usize];
        if parent == cur || parent as usize >= idx.len() {
            return false;
        }
        cur = parent;
    }
    false
}

fn name_contains(name: &[u8], needle: &[u8], case_sensitive: bool) -> bool {
    if case_sensitive {
        memmem::find(name, needle).is_some()
    } else {
        contains_ascii_ci(name, needle)
    }
}

/// How a query text is interpreted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MatchMode {
    #[default]
    Substring,
    Glob,
    /// The name starts with the text. Used when browsing a folder: the text
    /// is the last, still-being-typed component of a path.
    NamePrefix,
}

/// What to order results by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortBy {
    #[default]
    Name,
    SizeDesc,
    ModifiedDesc,
}

#[derive(Debug, Clone, Default)]
pub struct Query {
    pub text: String,
    pub mode: MatchMode,
    pub case_sensitive: bool,
    pub min_size: Option<u64>,
    pub max_size: Option<u64>,
    /// Extension without the dot, matched case-insensitively.
    pub ext: Option<String>,
    /// Restrict results to this subtree.
    pub under: Option<u32>,
    /// Restrict results to the direct children of this node, turning a
    /// search into a folder listing. Combined with [`MatchMode::NamePrefix`]
    /// this is how a typed path browses the folder it names.
    pub children_of: Option<u32>,
    pub dirs_only: bool,
    pub files_only: bool,
    /// Include nodes pruned by configuration, and NTFS metadata files.
    pub include_excluded: bool,
    pub sort: SortBy,
    /// How many of the sorted matches to skip. Lets a UI scroll a large
    /// result set without ever materialising all of it.
    pub offset: usize,
    pub limit: usize,
}

impl Query {
    pub fn new(text: impl Into<String>) -> Self {
        Query {
            text: text.into(),
            limit: 100,
            ..Default::default()
        }
    }

    /// Filters that change which nodes are eligible, ignoring the text. Two
    /// queries may only share narrowing state if these agree.
    fn filter_key(&self) -> impl PartialEq + Clone + std::fmt::Debug {
        (
            self.mode,
            self.case_sensitive,
            self.min_size,
            self.max_size,
            self.ext.clone(),
            self.under,
            self.children_of,
            self.dirs_only,
            self.files_only,
            self.include_excluded,
        )
    }
}

#[derive(Debug, Clone)]
pub struct SearchResult {
    pub nodes: Vec<u32>,
    /// Total matches before `limit` was applied.
    pub total: usize,
    pub elapsed: Duration,
    /// True when the previous result set was narrowed rather than rescanned.
    pub narrowed: bool,
    /// True when this page came straight from the previous ordering, with no
    /// scan and no sort. Scrolling should always hit this.
    pub cached: bool,
}

/// Holds the previous query so consecutive keystrokes can narrow, and the
/// previous ordering so scrolling costs nothing.
#[derive(Default)]
pub struct Searcher {
    last_text: String,
    last_key: Option<String>,
    /// Matches in node order. Narrowing filters this.
    last_matches: Vec<u32>,
    /// The same matches in display order.
    last_sorted: Vec<u32>,
    last_sort: Option<SortBy>,
    /// How many leading entries of `last_sorted` are genuinely ordered. Beyond
    /// this the array is only partitioned, so a further page needs a re-sort.
    last_ordered: usize,
    /// Index generation the cached matches came from.
    last_generation: u64,
    /// Whether the cached matches came from a multi-term (path) query.
    last_multi: bool,
}

impl Searcher {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn reset(&mut self) {
        self.last_text.clear();
        self.last_key = None;
        self.last_matches.clear();
        self.last_sorted.clear();
        self.last_sort = None;
        self.last_ordered = 0;
        self.last_generation = 0;
        self.last_multi = false;
    }

    pub fn search(&mut self, idx: &Index, q: &Query) -> SearchResult {
        let started = Instant::now();
        let key = format!("{:?}", q.filter_key());

        // Scrolling changes only `offset`: the matches and their order are
        // unchanged, so the answer is already sitting in `last_sorted`. Without
        // this a scroll would repeat the whole scan and sort, which on a
        // multi-million-row result is seconds of work per keypress.
        let same_set = self.last_key.as_deref() == Some(key.as_str())
            && self.last_text == q.text
            && self.last_generation == idx.generation();
        let wanted = q
            .offset
            .saturating_add(if q.limit > 0 { q.limit } else { usize::MAX });

        if same_set
            && self.last_sort == Some(q.sort)
            && (wanted <= self.last_ordered || self.last_ordered >= self.last_sorted.len())
        {
            return SearchResult {
                nodes: page(&self.last_sorted, q.offset, q.limit),
                total: self.last_matches.len(),
                elapsed: started.elapsed(),
                narrowed: false,
                cached: true,
            };
        }

        // Several terms mean a different question is being asked — of the
        // path rather than of the name — so a cached set from the other kind
        // cannot be narrowed into this one.
        let multi = matches!(q.mode, MatchMode::Substring) && terms(&q.text).len() > 1;

        // Narrowing is only sound when three things hold: the filters are
        // unchanged, the new text extends the old (so any match for the longer
        // needle is necessarily a match for the shorter one), and the index has
        // not been mutated. Without the generation check a file created between
        // keystrokes would never appear, because it was not in the previous
        // result set to be narrowed from. A name prefix narrows the same way a
        // substring does: a longer prefix cannot admit a name a shorter one
        // rejected.
        // Going from one term to several widens what counts as a match from
        // names to whole paths, and a node matching only through an ancestor
        // was never in the single-term set to be narrowed from.
        let can_narrow = matches!(q.mode, MatchMode::Substring | MatchMode::NamePrefix)
            && self.last_multi == multi
            && self.last_key.as_deref() == Some(key.as_str())
            && !self.last_text.is_empty()
            && q.text.len() >= self.last_text.len()
            && self.last_generation == idx.generation()
            && starts_with(&q.text, &self.last_text, q.case_sensitive);

        let mut matches: Vec<u32> = if q.text.is_empty() {
            all_eligible(idx, q)
        } else if can_narrow {
            let prev = std::mem::take(&mut self.last_matches);
            prev.into_iter()
                .filter(|&n| text_matches(idx, n, q))
                .collect()
        } else {
            full_scan(idx, q)
        };

        matches.sort_unstable();
        self.last_text = q.text.clone();
        self.last_key = Some(key);
        self.last_generation = idx.generation();
        self.last_multi = multi;
        let total = matches.len();

        // Display order is a second copy: `last_matches` has to stay in node
        // order, because that is what narrowing filters and what dedup relies
        // on. Copying a few million u32s is a memcpy, unlike re-scanning.
        let mut sorted = matches.clone();
        self.last_matches = matches;

        self.last_ordered = sort_results(idx, &mut sorted, q.sort, q.offset, q.limit);
        let nodes = page(&sorted, q.offset, q.limit);
        self.last_sorted = sorted;
        self.last_sort = Some(q.sort);

        SearchResult {
            nodes,
            total,
            elapsed: started.elapsed(),
            narrowed: can_narrow,
            cached: false,
        }
    }
}

fn starts_with(text: &str, prefix: &str, case_sensitive: bool) -> bool {
    if case_sensitive {
        text.starts_with(prefix)
    } else {
        text.len() >= prefix.len()
            && text.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
    }
}

/// Every node passing the non-text filters, used for an empty query.
fn all_eligible(idx: &Index, q: &Query) -> Vec<u32> {
    (0..idx.len() as u32)
        .filter(|&n| passes_filters(idx, n, q))
        .collect()
}

/// Does this node's name match the query text (and the filters)?
fn text_matches(idx: &Index, n: u32, q: &Query) -> bool {
    if !passes_filters(idx, n, q) {
        return false;
    }
    let name = idx.name(n);
    match q.mode {
        MatchMode::Substring => {
            // Several terms are a question about the path, so they are asked
            // of the path. One term stays a question about the name, which is
            // both what people expect and the fast path.
            let parts = terms(&q.text);
            match parts.len() {
                // Only whitespace: nothing has been asked yet.
                0 => true,
                // The term, not the raw text. Now that whitespace separates
                // terms, "sso " has to keep behaving like "sso" while the
                // second word is still being typed, rather than matching
                // nothing because no name contains a trailing space.
                1 => name_contains(name.as_bytes(), parts[0].as_bytes(), q.case_sensitive),
                _ => path_matches(idx, n, &parts, q.case_sensitive),
            }
        }
        MatchMode::NamePrefix => {
            let (name, needle) = (name.as_bytes(), q.text.as_bytes());
            name.get(..needle.len()).is_some_and(|head| {
                if q.case_sensitive {
                    head == needle
                } else {
                    head.eq_ignore_ascii_case(needle)
                }
            })
        }
        MatchMode::Glob => glob_match(q.text.as_bytes(), name.as_bytes(), !q.case_sensitive),
    }
}

fn passes_filters(idx: &Index, n: u32, q: &Query) -> bool {
    let i = n as usize;
    let f = idx.flags[i];

    // Tombstones are never results, whatever the filters say.
    if f & flags::DELETED != 0 {
        return false;
    }
    if !q.include_excluded && f & (flags::EXCLUDED | flags::METADATA) != 0 {
        return false;
    }
    let is_dir = f & flags::DIRECTORY != 0;
    if let Some(parent) = q.children_of {
        if idx.parent[i] != parent || i as u32 == parent {
            return false;
        }
    }
    if q.dirs_only && !is_dir {
        return false;
    }
    if q.files_only && is_dir {
        return false;
    }
    // A volume root is never a useful result; its stored name is ".".
    if idx.is_root(n) {
        return false;
    }
    if let Some(min) = q.min_size {
        if idx.size[i] < min {
            return false;
        }
    }
    if let Some(max) = q.max_size {
        if idx.size[i] > max {
            return false;
        }
    }
    if let Some(ext) = &q.ext {
        let name = idx.name(n);
        match name.rsplit_once('.') {
            Some((_, e)) if e.eq_ignore_ascii_case(ext) => {}
            _ => return false,
        }
    }
    if let Some(under) = q.under {
        if !idx.is_under(n, under) {
            return false;
        }
    }
    true
}

/// Full pass over the name arena.
///
/// The arena is split across the thread pool. Each chunk is extended by
/// `needle.len() - 1` bytes so a match straddling a chunk boundary is still
/// found, and matches are attributed to the chunk that contains their start.
fn full_scan(idx: &Index, q: &Query) -> Vec<u32> {
    // Whitespace separates terms, so what gets scanned for is the term, not
    // the text as typed: padding must not be part of the needle.
    let mut single = q.text.as_str();
    if matches!(q.mode, MatchMode::Substring) {
        let parts = terms(&q.text);
        match parts.len() {
            0 => return all_eligible(idx, q),
            1 => single = parts[0],
            _ => return multi_term_scan(idx, q, &parts),
        }
    }
    if matches!(q.mode, MatchMode::Glob | MatchMode::NamePrefix) {
        // Globs are anchored to whole names, so there is nothing to scan for
        // in the arena; test names directly. Prefix matches go the same way:
        // they are scoped to one directory's children, so the parent test in
        // the filters rejects almost every node before the name is touched.
        return (0..idx.len() as u32)
            .into_par_iter()
            .filter(|&n| text_matches(idx, n, q))
            .collect();
    }

    let hay = idx.names.as_bytes();
    let needle = single.as_bytes();
    if needle.is_empty() || hay.is_empty() {
        return all_eligible(idx, q);
    }
    if needle.len() > hay.len() {
        return Vec::new();
    }

    let threads = rayon::current_num_threads().max(1);
    let chunk = (hay.len() / threads).max(1 << 20);
    let overlap = needle.len() - 1;

    let mut nodes: Vec<u32> = (0..hay.len())
        .step_by(chunk)
        .collect::<Vec<_>>()
        .into_par_iter()
        .flat_map_iter(|start| {
            let end = (start + chunk).min(hay.len());
            let scan_end = (end + overlap).min(hay.len());
            let window = &hay[start..scan_end];

            let positions: Vec<usize> = if q.case_sensitive {
                memmem::find_iter(window, needle)
                    .map(|p| start + p)
                    .collect()
            } else {
                find_iter_ascii_ci(window, needle)
                    .map(|p| start + p)
                    .collect()
            };

            positions
                .into_iter()
                // Attribute each match to exactly one chunk.
                .filter(move |&p| p >= start && p < end)
                .filter_map(|p| node_containing(idx, p, needle.len()))
        })
        .collect();

    // A renamed node still has its old name sitting in the arena, so a hit
    // there is stale and must not count. Such nodes are matched separately
    // against their real names below.
    nodes.retain(|&n| idx.flags[n as usize] & flags::NAME_OVERRIDDEN == 0);
    nodes.extend(idx.overrides().nodes().filter(|&n| text_matches(idx, n, q)));

    nodes.sort_unstable();
    nodes.dedup();
    nodes.retain(|&n| passes_filters(idx, n, q));
    nodes
}

/// Nodes whose own name contains `needle`, ignoring every filter.
///
/// The arena scan, extracted so each term of a multi-term query can use it.
fn scan_names(idx: &Index, needle: &[u8], case_sensitive: bool) -> Vec<u32> {
    let hay = idx.names.as_bytes();
    if needle.is_empty() || hay.is_empty() || needle.len() > hay.len() {
        return Vec::new();
    }

    let threads = rayon::current_num_threads().max(1);
    let chunk = (hay.len() / threads).max(1 << 20);
    let overlap = needle.len() - 1;

    let mut nodes: Vec<u32> = (0..hay.len())
        .step_by(chunk)
        .collect::<Vec<_>>()
        .into_par_iter()
        .flat_map_iter(|start| {
            let end = (start + chunk).min(hay.len());
            let scan_end = (end + overlap).min(hay.len());
            let window = &hay[start..scan_end];

            let positions: Vec<usize> = if case_sensitive {
                memmem::find_iter(window, needle)
                    .map(|p| start + p)
                    .collect()
            } else {
                find_iter_ascii_ci(window, needle)
                    .map(|p| start + p)
                    .collect()
            };

            positions
                .into_iter()
                .filter(move |&p| p >= start && p < end)
                .filter_map(|p| node_containing(idx, p, needle.len()))
        })
        .collect();

    // A renamed node still has its old name in the arena, so a hit there is
    // stale; those nodes are tested against their real names instead.
    nodes.retain(|&n| idx.flags[n as usize] & flags::NAME_OVERRIDDEN == 0);
    nodes.extend(
        idx.overrides()
            .nodes()
            .filter(|&n| name_contains(idx.name_bytes(n), needle, case_sensitive)),
    );
    nodes.sort_unstable();
    nodes.dedup();
    nodes
}

/// Match several terms against whole paths.
///
/// Testing every node's whole path directly would mean millions of ancestor
/// walks. Instead each term is scanned for once across the arena — the same
/// cheap SIMD pass a single-term search uses — and the per-name hits are
/// pushed down the tree in one linear pass, so a node inherits every term its
/// ancestors matched. That leaves a small candidate set, which the
/// authoritative walk then confirms.
fn multi_term_scan(idx: &Index, q: &Query, parts: &[&str]) -> Vec<u32> {
    let n = idx.len();
    if n == 0 {
        return Vec::new();
    }

    let capped = parts.len().min(MASK_TERMS);
    let mut own = vec![0u8; n];
    for (i, term) in parts.iter().take(capped).enumerate() {
        for node in scan_names(idx, term.as_bytes(), q.case_sensitive) {
            own[node as usize] |= 1 << i;
        }
    }
    let all: u8 = if capped >= MASK_TERMS {
        u8::MAX
    } else {
        (1u8 << capped) - 1
    };

    // Push each node's terms down to its descendants. Walking up from every
    // node and memoising as we go visits each node once overall, which a
    // naive per-node walk would not.
    let mut acc = vec![0u8; n];
    let mut done = vec![false; n];
    let mut stack: Vec<u32> = Vec::new();

    for start in 0..n as u32 {
        if done[start as usize] {
            continue;
        }
        stack.clear();
        let mut cur = start;
        let inherited = loop {
            if done[cur as usize] {
                break acc[cur as usize];
            }
            stack.push(cur);
            let parent = idx.parent[cur as usize];
            // Root, or a parent chain too broken or too deep to trust.
            if parent == cur || parent as usize >= n || stack.len() > 4096 {
                break 0;
            }
            cur = parent;
        };

        let mut carry = inherited;
        while let Some(node) = stack.pop() {
            carry |= own[node as usize];
            acc[node as usize] = carry;
            done[node as usize] = true;
        }
    }

    (0..n as u32)
        .into_par_iter()
        .filter(|&node| acc[node as usize] == all)
        .filter(|&node| passes_filters(idx, node, q))
        .filter(|&node| path_matches(idx, node, parts, q.case_sensitive))
        .collect()
}

/// Map an arena offset to the node whose name contains it.
///
/// Names are stored in node order, so `name_off` is sorted and a binary search
/// finds the owner. The length check rejects a match that started inside one
/// name and ran past its end into the next, which the arena's lack of
/// separators would otherwise allow.
fn node_containing(idx: &Index, pos: usize, needle_len: usize) -> Option<u32> {
    let offs = &idx.name_off;
    let i = match offs.binary_search(&(pos as u32)) {
        Ok(i) => i,
        Err(0) => return None,
        Err(i) => i - 1,
    };
    let start = offs[i] as usize;
    let end = start + idx.name_len[i] as usize;
    if pos >= start && pos + needle_len <= end {
        Some(i as u32)
    } else {
        None
    }
}

/// Case-insensitive ASCII substring search over bytes.
///
/// Uses `memchr2` on both cases of the needle's first byte as a SIMD
/// prefilter, then verifies candidates, which keeps the common path close to
/// plain `memmem` speed without needing a second lowercased copy of the arena.
pub fn find_iter_ascii_ci<'a>(hay: &'a [u8], needle: &'a [u8]) -> impl Iterator<Item = usize> + 'a {
    let first = needle[0];
    let lo = first.to_ascii_lowercase();
    let up = first.to_ascii_uppercase();

    let mut pos = 0usize;
    std::iter::from_fn(move || {
        while pos + needle.len() <= hay.len() {
            let rel = if lo == up {
                memchr::memchr(lo, &hay[pos..])
            } else {
                memchr::memchr2(lo, up, &hay[pos..])
            }?;
            let cand = pos + rel;
            if cand + needle.len() > hay.len() {
                return None;
            }
            pos = cand + 1;
            if hay[cand..cand + needle.len()].eq_ignore_ascii_case(needle) {
                return Some(cand);
            }
        }
        None
    })
}

fn contains_ascii_ci(hay: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    if needle.len() > hay.len() {
        return false;
    }
    find_iter_ascii_ci(hay, needle).next().is_some()
}

/// Glob matcher supporting `*` and `?`, anchored to the whole name.
pub fn glob_match(pattern: &[u8], text: &[u8], ci: bool) -> bool {
    // Iterative backtracking: linear in the common case, and unlike a
    // recursive version it cannot blow the stack on a hostile pattern.
    let (mut p, mut t) = (0usize, 0usize);
    let (mut star, mut mark) = (usize::MAX, 0usize);

    let eq = |a: u8, b: u8| {
        if ci {
            a.eq_ignore_ascii_case(&b)
        } else {
            a == b
        }
    };

    while t < text.len() {
        if p < pattern.len() && (pattern[p] == b'?' || eq(pattern[p], text[t])) {
            p += 1;
            t += 1;
        } else if p < pattern.len() && pattern[p] == b'*' {
            star = p;
            mark = t;
            p += 1;
        } else if star != usize::MAX {
            p = star + 1;
            mark += 1;
            t = mark;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == b'*' {
        p += 1;
    }
    p == pattern.len()
}

/// Case-insensitive ASCII name ordering that allocates nothing.
///
/// The obvious form lowercases both sides with `to_ascii_lowercase()`, which
/// allocates two `String`s for every comparison. Sorting a few million matches
/// is tens of millions of comparisons, so that turns an ordering pass into a
/// multi-second stall — long enough that an interactive UI looks frozen.
fn name_cmp(x: &[u8], y: &[u8]) -> Ordering {
    let n = x.len().min(y.len());
    for i in 0..n {
        let (l, r) = (x[i].to_ascii_lowercase(), y[i].to_ascii_lowercase());
        if l != r {
            return l.cmp(&r);
        }
    }
    x.len().cmp(&y.len())
}

/// Order `nodes` far enough to serve the page at `offset..offset + limit`.
///
/// Returns how many leading entries are actually in order. Beyond that the
/// array is only partitioned, so a later page cannot be served from it without
/// sorting again.
fn sort_results(
    idx: &Index,
    nodes: &mut [u32],
    sort: SortBy,
    offset: usize,
    limit: usize,
) -> usize {
    // Only the rows up to `offset + limit` need ordering, so partial selection
    // avoids fully sorting what could be a million matches.
    //
    // Order a generous prefix rather than exactly one page: the selection pass
    // is linear in the whole array whatever `k` is, so extending it costs
    // almost nothing, and it lets a UI scroll many screens before another sort
    // is needed.
    const MIN_ORDERED: usize = 4096;
    let want = offset
        .saturating_add(limit)
        .max(MIN_ORDERED)
        .min(nodes.len());
    let partial = limit > 0 && want < nodes.len();
    match sort {
        SortBy::Name => {
            let cmp =
                |a: &u32, b: &u32| name_cmp(idx.name_bytes(*a), idx.name_bytes(*b)).then(a.cmp(b));
            if partial {
                nodes.select_nth_unstable_by(want, cmp);
                nodes[..want].sort_unstable_by(cmp);
            } else {
                nodes.sort_unstable_by(cmp);
            }
        }
        SortBy::SizeDesc => {
            // Node id breaks ties so equal-sized entries order deterministically
            // rather than depending on the unstable sort's internal choices.
            let key = |n: &u32| (std::cmp::Reverse(idx.size[*n as usize]), *n);
            if partial {
                nodes.select_nth_unstable_by_key(want, key);
                nodes[..want].sort_unstable_by_key(key);
            } else {
                nodes.sort_unstable_by_key(key);
            }
        }
        SortBy::ModifiedDesc => {
            let key = |n: &u32| (std::cmp::Reverse(idx.mtime[*n as usize]), *n);
            if partial {
                nodes.select_nth_unstable_by_key(want, key);
                nodes[..want].sort_unstable_by_key(key);
            } else {
                nodes.sort_unstable_by_key(key);
            }
        }
    }
    if partial {
        want
    } else {
        nodes.len()
    }
}

/// Slice one page out of an ordered match list.
fn page(sorted: &[u32], offset: usize, limit: usize) -> Vec<u32> {
    let start = offset.min(sorted.len());
    let end = if limit > 0 {
        start.saturating_add(limit).min(sorted.len())
    } else {
        sorted.len()
    };
    sorted[start..end].to_vec()
}

/// Directories ranked by rolled-up size: the answer to "what is eating my disk".
pub fn largest_dirs(idx: &Index, under: Option<u32>, top: usize) -> Vec<u32> {
    let mut dirs: Vec<u32> = (0..idx.len() as u32)
        .filter(|&n| {
            idx.is_dir(n)
                && !idx.is_excluded(n)
                && !idx.is_root(n)
                && under.is_none_or(|u| idx.is_under(n, u))
        })
        .collect();

    let key = |n: &u32| (std::cmp::Reverse(idx.size[*n as usize]), *n);
    if top > 0 && top < dirs.len() {
        dirs.select_nth_unstable_by_key(top, key);
        dirs.truncate(top);
    }
    dirs.sort_unstable_by_key(key);
    dirs
}

/// Bytes held directly in a directory's own files, excluding subdirectories.
///
/// A directory that is large because of one child is usually not the
/// interesting one; this separates "big because of me" from "big because of
/// something below me".
pub fn own_bytes(idx: &Index, dir: u32, children: &rustc_hash::FxHashMap<u32, Vec<u32>>) -> u64 {
    children
        .get(&dir)
        .map(|kids| {
            kids.iter()
                .filter(|&&c| !idx.is_dir(c) && !idx.is_excluded(c))
                .map(|&c| idx.size[c as usize])
                .sum()
        })
        .unwrap_or(0)
}

/// Nodes whose parent chain is broken, for diagnostics.
pub fn orphans(idx: &Index) -> Vec<u32> {
    (0..idx.len() as u32)
        .filter(|&n| {
            idx.flags[n as usize] & flags::ORPHAN != 0 || idx.parent[n as usize] == NO_NODE
        })
        .collect()
}
