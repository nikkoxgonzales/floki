//! Evaluation: rayon over shards, `prev` narrowing, sort (SPEC section 4).
//!
//! `prev` narrowing rule: the caller (service) may pass the previous result
//! set as `prev` **only** when [`prev_reusable`] says so, i.e. the new query
//! string starts with the previous query string, neither query has OR / NOT,
//! the new query contains no `"`, `(`, `<` or `|`, **and** the previous result
//! set is complete (not truncated by `max_results`). The `prev` ids must also
//! come from the current [`epoch`](crate::Index::epoch): drop them after any
//! `compact()` / `load`. Out-of-range or tombstoned ids in `prev` simply do
//! not match (never a panic / wrong hit). `search` itself always narrows when
//! `prev` is `Some`.
//!
//! Sort note: `NameAsc`/`NameDesc` run in two phases. Phase 1 scans `entries`
//! sequentially (cache-friendly, rayon `par_chunks`), counting `total` and
//! collecting the global first [`MID_CAP`] ids (atomic-gated, bounded).
//! Totals up to [`PHASE1_CAP`] go straight to fold-keys + windowed page
//! (`select_nth` + window sort = the exact full-sort page). Totals up to
//! `MID_CAP` probe the phase-2 merge walk with a small budget and fall back
//! to the window reusing phase 1's complete collection (no rescan); beyond
//! `MID_CAP` the merge walk (or its budget bail with a rescan) takes over.
//! Phase 2 merge-walks the sorted `by_name` snapshot with the sorted
//! `pending` list (two-pointer merge on `(fold(name), id)`, reversed for
//! `Desc`), stopping after `offset + max_results` hits — expected cost
//! `n*(offset+max)/total`, tiny for broad queries. Deep pages
//! (`offset + max > DEEP_PAGE_BOUND`), unlimited pages, `prev` narrowing, and
//! scan-dirty indexes use collect+sort instead. `PathAsc`/`PathDesc` sort all
//! matches by rebuilt path, then page (exact total ordering; pays one path
//! rebuild per match, with a `select_nth` window instead of a full sort once
//! the match count exceeds [`PATH_SORT_FULL_BOUND`]). `Modified*`/`Created*`
//! work the same way but key on a filesystem stat per match — the index
//! stores no timestamps, so these orders pay one `metadata` call per match.
//!
//! Hot path: names are scanned as arena bytes without UTF-8 validation;
//! case-insensitive substrings run a SIMD first-byte prefilter with
//! lowercase-compare verify (folded needle, hoisted), `path:` ancestor walks
//! are memoized per query (tri-state chain verdicts shared lock-free across
//! rayon chunks, plus a thread-local sibling resolve cache), and `And`
//! children run cheap name-only terms before glob / regex / path terms.
//! Globs, regexes and `case:` terms carry required-literal prefilters so the
//! expensive matcher only sees candidates.

use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::OnceLock;

use memchr::memmem;
use rayon::prelude::*;
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::entry::{Entry, EntryId, DIRECTORY, TOMBSTONE};
use crate::fold::{
    cmp_folded_bytes, contains_insensitive, contains_insensitive_prefolded, eq_insensitive, fold,
    fold_bytes, fold_bytes_into,
};
use crate::index::Index;
use crate::index::PathVerdictKey;
use crate::index::ARENA_BLOCK_BITS;
use crate::query::{Node, Query, TermKind};

/// Global rayon pool for all parallel search/count/path-window work, so the
/// caller (the service) can cap search CPU independently of rayon's global
/// pool. First [`set_search_threads`] call wins; otherwise lazily created
/// with [`default_search_threads`] threads.
static SEARCH_POOL: OnceLock<rayon::ThreadPool> = OnceLock::new();

/// Default search threads: half the machine, at least one.
fn default_search_threads() -> usize {
    std::thread::available_parallelism()
        .map(|n| (n.get() / 2).max(1))
        .unwrap_or(1)
}

/// Install the global search thread pool used by [`search`], [`count`] and
/// the path-window sorts. Call once at startup with the service's policy;
/// later calls are ignored (returns `false`). When never called, the pool is
/// created on first use with [`default_search_threads`] threads.
pub fn set_search_threads(n: usize) -> bool {
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(n.max(1))
        .thread_name(|i| format!("floki-search-{i}"))
        .build();
    match pool {
        Ok(pool) => SEARCH_POOL.set(pool).is_ok(),
        Err(_) => false,
    }
}

/// The search pool, creating it with defaults on first use.
fn search_pool() -> &'static rayon::ThreadPool {
    SEARCH_POOL.get_or_init(|| {
        rayon::ThreadPoolBuilder::new()
            .num_threads(default_search_threads())
            .thread_name(|i| format!("floki-search-{i}"))
            .build()
            .expect("search pool builds with valid config")
    })
}

/// Required literal byte-substring, extracted heuristically from a glob,
/// regex, `case:` or `path:` term at build time: any match MUST contain it,
/// so it runs first via memchr and the expensive matcher only sees
/// candidates. `folded` selects the test: pre-folded bytes via
/// [`contains_insensitive_prefolded`] (case-insensitive terms) or raw
/// [`memmem::find`] (case-sensitive terms).
#[derive(Clone, Debug)]
struct RequiredLit {
    bytes: Vec<u8>,
    folded: bool,
    ascii: bool,
}

impl RequiredLit {
    /// Compile `s`: raw bytes for case-sensitive terms, folded bytes
    /// otherwise.
    fn new(s: &str, case_sensitive: bool) -> Self {
        if case_sensitive {
            Self {
                ascii: s.is_ascii(),
                bytes: s.as_bytes().to_vec(),
                folded: false,
            }
        } else {
            let folded = fold(s);
            Self {
                ascii: folded.is_ascii(),
                bytes: folded.into_bytes(),
                folded: true,
            }
        }
    }

    fn test(&self, haystack: &[u8]) -> bool {
        if self.folded {
            contains_insensitive_prefolded(haystack, &self.bytes, self.ascii)
        } else {
            memmem::find(haystack, &self.bytes).is_some()
        }
    }
}

/// Per-query ancestor-match memo for one `path:` term: tri-state verdict per
/// entry id (0 unknown, 1 no-match, 2 match) answering "does this entry's own
/// name or any ancestor's name contain the segment". Atomics so rayon chunks
/// share one table lock-free; races are benign (verdicts are deterministic —
/// last writer wins with the same value). Lazily sized on first use, so
/// queries without path terms pay nothing.
struct PathMemo {
    state: Box<[AtomicU8]>,
}

impl PathMemo {
    fn new(n: usize) -> Self {
        let mut state = Vec::with_capacity(n);
        state.extend((0..n).map(|_| AtomicU8::new(0)));
        Self {
            state: state.into_boxed_slice(),
        }
    }

    /// Known verdict, or `None` when unvisited (or out of range, which only
    /// a concurrently-grown index could produce — searches hold `&`).
    fn get(&self, id: EntryId) -> Option<bool> {
        match self.state.get(id as usize)?.load(Ordering::Relaxed) {
            0 => None,
            1 => Some(false),
            _ => Some(true),
        }
    }

    fn set(&self, id: EntryId, verdict: bool) {
        if let Some(slot) = self.state.get(id as usize) {
            slot.store(u8::from(verdict) + 1, Ordering::Relaxed);
        }
    }
}

/// Thread-local parent resolve cache: consecutive entries (scan/MFT order)
/// usually share a parent, so the binary search runs once per directory run
/// instead of once per file. Generous on purpose: round-robin parent
/// patterns (and production interleave) still hit; masked indexing needs a
/// power of two (see assert). Boxed behind [`PathCtx`] so non-path queries
/// and small evaluations never pay the 64 KB memset.
const SIB_SLOTS: usize = 4096;
const _: () = assert!(SIB_SLOTS & (SIB_SLOTS - 1) == 0);

/// `(parent_frn, vol, parent_id)` slots; `u64::MAX` is an impossible FRN and
/// marks empty slots.
struct SibCache {
    slots: [(u64, u8, EntryId); SIB_SLOTS],
}

impl SibCache {
    fn new() -> Self {
        Self {
            slots: [(u64::MAX, 0, EntryId::MAX); SIB_SLOTS],
        }
    }

    /// Cached parent id, or `None` on a cold slot (caller resolves + fills
    /// via [`resolve`](SibCache::resolve)).
    fn peek(&self, vol: u8, frn: u64) -> Option<EntryId> {
        let slot = &self.slots[(frn as usize ^ vol as usize) & (SIB_SLOTS - 1)];
        (slot.0 == frn && slot.1 == vol).then_some(slot.2)
    }

    fn resolve(&mut self, index: &Index, vol: u8, frn: u64) -> Option<EntryId> {
        let slot = &mut self.slots[(frn as usize ^ vol as usize) & (SIB_SLOTS - 1)];
        if slot.0 == frn && slot.1 == vol {
            return Some(slot.2);
        }
        let id = index.resolve(vol, frn)?;
        *slot = (frn, vol, id);
        Some(id)
    }
}

/// Per-evaluation scratch for path terms: the thread-local sibling cache
/// (boxed + lazy, so only path-term evaluations allocate it) plus a reusable
/// ancestor-walk stack (cold chains used to allocate one `Vec` per entry —
/// millions per query). The shared chain memo lives in the matcher (one
/// [`PathMemo`] per `path:` term, lazily sized). Fresh per chunk /
/// sequential pass — never shared across threads.
struct PathCtx {
    sib: Option<Box<SibCache>>,
    stack: Vec<EntryId>,
}

impl PathCtx {
    fn new() -> Self {
        Self {
            sib: None,
            stack: Vec::new(),
        }
    }

    fn sib(&mut self) -> &mut SibCache {
        self.sib.get_or_insert_with(|| Box::new(SibCache::new()))
    }
}

impl Default for PathCtx {
    fn default() -> Self {
        Self::new()
    }
}

/// Ancestor verdict with memoization: does `id`'s own name or any ancestor's
/// name satisfy `seg`? Fast path (no allocation, no walk): verdict already
/// memoized, or parent id + verdict both cached — then the verdict is the
/// parent's, or the parent's OR the own name. Slow path (first touch of a
/// cold chain): walk up to the nearest memoized link (or root), then unwind,
/// memoizing every visited id — each distinct directory's chain runs once
/// per query instead of once per file underneath it. Corrupt parent cycles
/// terminate via [`MAX_PATH_HOPS`](crate::MAX_PATH_HOPS) with best-effort
/// verdicts (same class as the unmemoized walk).
fn chain_match(
    index: &Index,
    id: EntryId,
    seg: &RequiredLit,
    memo: &PathMemo,
    pcx: &mut PathCtx,
) -> bool {
    if let Some(verdict) = memo.get(id) {
        return verdict;
    }
    let Some(entry) = index.entries.get(id as usize) else {
        return false;
    };
    let vol = index.volume_of(id).unwrap_or(0);
    // Fast path: parent id cached and its verdict already memoized (the
    // common case once chains have filled). A hot parent decides without
    // any name check; a cold one needs only the own name.
    if entry.frn != entry.parent_frn {
        if let Some(pid) = pcx.sib().peek(vol, entry.parent_frn) {
            if let Some(parent_verdict) = memo.get(pid) {
                let hit = parent_verdict || seg.test(index.name_bytes_of(entry));
                memo.set(id, hit);
                return hit;
            }
        }
    }
    // Slow path: walk to the nearest memoized link, unwind memoizing. The
    // stack buffer is reused across calls (taken from the thread-local
    // context, returned on every exit below).
    let mut stack = std::mem::take(&mut pcx.stack);
    stack.clear();
    let mut cur = id;
    let inherited = loop {
        if let Some(verdict) = memo.get(cur) {
            break verdict;
        }
        let Some(entry) = index.entries.get(cur as usize) else {
            break false;
        };
        if stack.len() >= crate::MAX_PATH_HOPS as usize {
            break false;
        }
        stack.push(cur);
        if entry.frn == entry.parent_frn {
            break false; // root has no ancestors; its name is checked below
        }
        let vol = index.volume_of(cur).unwrap_or(0);
        match pcx.sib().resolve(index, vol, entry.parent_frn) {
            Some(pid) if pid != cur => cur = pid,
            _ => break false,
        }
    };
    // Unwind top-down: M(link) = name_match(link) || M(parent(link)). A hot
    // inherited verdict marks the whole stack without name checks.
    if inherited {
        for cid in stack.drain(..) {
            memo.set(cid, true);
        }
        pcx.stack = stack;
        return true;
    }
    let mut acc = false;
    while let Some(cid) = stack.pop() {
        let own = index
            .entries
            .get(cid as usize)
            .map(|e| index.name_bytes_of(e))
            .is_some_and(|nb| seg.test(nb));
        let hit = acc || own;
        memo.set(cid, hit);
        acc = hit;
    }
    pcx.stack = stack;
    acc
}

/// Replay a cached single-segment `path:` verdict set: the exact total via
/// popcount plus the global first-`cap` ids in id order. Identical to what
/// the walking scan would collect: scan chunks tile ids in ascending order
/// (so truncation is the same prefix), and tombstoned ids never set bits
/// (the walk skips them). Returns `None` on a cache miss — the caller walks
/// and stores via [`store_path_bits`].
fn replay_path_bits(index: &Index, seg: &RequiredLit, cap: usize) -> Option<(u64, Vec<EntryId>)> {
    let key = PathVerdictKey {
        seg: seg.bytes.clone(),
        folded: seg.folded,
        mutation: index.mutation,
        len: index.entries.len(),
    };
    let bits = index
        .path_verdicts
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .lookup(&key, index.entries.len())?;
    let mut total = 0u64;
    let mut hits = Vec::new();
    for (wi, &w) in bits.iter().enumerate() {
        total += w.count_ones() as u64;
        let mut word = w;
        while word != 0 && hits.len() < cap {
            let b = word.trailing_zeros();
            hits.push((wi * 64 + b as usize) as EntryId);
            word &= word - 1;
        }
    }
    Some((total, hits))
}

/// Store the verdict bitset of one cold single-segment `path:` walk for
/// repeat queries. `bits` holds one `1` per hit id (zeros read back as
/// non-hits); keyed by the same era the walk observed. Lock poison (a prior
/// search panicking mid-store) only drops the store — the next query walks.
fn store_path_bits(index: &Index, seg: &RequiredLit, bits: Vec<u64>) {
    let key = PathVerdictKey {
        seg: seg.bytes.clone(),
        folded: seg.folded,
        mutation: index.mutation,
        len: index.entries.len(),
    };
    index
        .path_verdicts
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .store(key, std::sync::Arc::new(bits));
}

/// Tight single-segment separator-free `path:` verdict: [`chain_match`]'s
/// fast path inlined (caller threads the chunk-local [`PathCtx`]; cold
/// chains delegate to [`chain_match`]'s slow walk). For directories the
/// verdict is memoized exactly as [`chain_match`] would; files skip the
/// memo write — sound because the memo is a pure cache (an unwritten file
/// verdict recomputes to the same value, or is filled later by a slow walk
/// that visits the file as an ancestor).
#[inline(always)]
#[allow(clippy::too_many_arguments)]
fn path_single_hit(
    index: &Index,
    id: EntryId,
    entry: &Entry,
    name: &[u8],
    seg: &RequiredLit,
    memo: &PathMemo,
    pcx: &mut PathCtx,
) -> bool {
    if let Some(verdict) = memo.get(id) {
        return verdict;
    }
    let vol = index.volume_of(id).unwrap_or(0);
    if entry.frn != entry.parent_frn {
        if let Some(pid) = pcx.sib().peek(vol, entry.parent_frn) {
            if let Some(parent_verdict) = memo.get(pid) {
                let hit = parent_verdict || seg.test(name);
                if entry.flags & DIRECTORY != 0 {
                    memo.set(id, hit);
                }
                return hit;
            }
        }
    }
    chain_match(index, id, seg, memo, pcx)
}

/// Sort order for results.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Sort {
    /// By folded name, ascending.
    #[default]
    NameAsc,
    /// By folded name, descending.
    NameDesc,
    /// By rebuilt path, ascending (all matches sorted, then paged).
    PathAsc,
    /// By rebuilt path, descending (all matches sorted, then paged).
    PathDesc,
    /// By filesystem last-modified time, ascending (stats every match).
    ModifiedAsc,
    /// By filesystem last-modified time, descending (stats every match).
    ModifiedDesc,
    /// By filesystem creation time, ascending (stats every match).
    CreatedAsc,
    /// By filesystem creation time, descending (stats every match).
    CreatedDesc,
}

/// Paging + sort options.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchOptions {
    /// Max hits returned; `0` means unlimited.
    pub max_results: u32,
    /// Hits to skip.
    pub offset: u32,
    /// Sort order.
    pub sort: Sort,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            max_results: 1000,
            offset: 0,
            sort: Sort::NameAsc,
        }
    }
}

impl SearchOptions {
    /// Build options explicitly.
    #[must_use]
    pub fn new(max_results: u32, offset: u32, sort: Sort) -> Self {
        Self {
            max_results,
            offset,
            sort,
        }
    }
}

/// One match: entry id plus its volume index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hit {
    /// Entry id in the index.
    pub id: EntryId,
    /// Volume index (`Index::volumes` position).
    pub vol: u8,
}

/// A result page plus the total match count.
#[derive(Debug, Clone)]
pub struct SearchResult {
    /// The requested page.
    pub hits: Vec<Hit>,
    /// All matches, even when `max_results` truncated the page.
    pub total: u64,
}

/// SPEC search entry point: page of hits for `q`.
pub fn search(index: &Index, q: &Query, opts: &SearchOptions, prev: Option<&[Hit]>) -> Vec<Hit> {
    search_paged(index, q, opts, prev).hits
}

/// Like [`search`], but also reports `total` (all matches pre-truncation).
pub fn search_paged(
    index: &Index,
    q: &Query,
    opts: &SearchOptions,
    prev: Option<&[Hit]>,
) -> SearchResult {
    let matcher = Matcher::build(&q.root);
    let offset = opts.offset as usize;
    let max = if opts.max_results == 0 {
        usize::MAX
    } else {
        opts.max_results as usize
    };
    match opts.sort {
        Sort::NameAsc | Sort::NameDesc => {
            let desc = opts.sort == Sort::NameDesc;
            // No usable global order without a full pass: explicit `prev`
            // sets, scan-dirty `by_name` (unsorted push tail), unlimited
            // pages, and very deep pages all go through collect+sort.
            if prev.is_some()
                || !index.by_name_is_fresh()
                || max == usize::MAX
                || offset.saturating_add(max) > DEEP_PAGE_BOUND
            {
                let mut matches = collect(index, &matcher, prev);
                let total = matches.len() as u64;
                sort_by_name(index, &mut matches, desc);
                return SearchResult {
                    hits: matches.into_iter().skip(offset).take(max).collect(),
                    total,
                };
            }
            // Phase 1 through the routed scan (arena sweep with fused key
            // folding, disjunction union, or generic): the fold fallbacks
            // below page without re-reading names cold.
            let (total, first, parts) = match scan_route(index, &matcher) {
                ScanRoute::Arena(p) => {
                    let (t, h, fp) = scan_arena(index, &p, &matcher, MID_CAP, false);
                    (t, h, Some(fp))
                }
                // Whole-name totals are tiny; refold below costs nothing.
                ScanRoute::Whole(needle, cs) => {
                    let (t, h, _) = whole_lookup(index, needle, cs, MID_CAP, true);
                    (t, h, None)
                }
                ScanRoute::Or => {
                    let (t, h, fp) = scan_or_union(index, &matcher, MID_CAP, false);
                    (t, h, fp)
                }
                // Whole-name equality resolves inside `scan_entries` (no
                // scan when fresh); its totals are tiny, so the refold
                // below costs nothing.
                ScanRoute::Generic => {
                    let (t, h) = scan_entries(index, &matcher, MID_CAP);
                    (t, h, None)
                }
            };
            // Fused page when the scan folded keys, refold otherwise. Both
            // need complete keys (`total <= cap`); the debug assert in
            // `page_from_parts` guards the contract. `parts` moves into the
            // single fallback that runs (branches are exclusive).
            let mut parts = parts;
            let page_fallback =
                |index: &Index, first: &[EntryId], parts: Option<FoldParts>| match parts {
                    Some(fp) => page_from_parts(index, fp, offset, max, total, desc),
                    None => page_ids_arena(index, first, offset, max, total, desc),
                };
            if total <= PHASE1_CAP as u64 {
                // `first` holds every match: order only the requested window.
                page_fallback(index, &first, parts.take())
            } else if total <= MID_CAP as u64 {
                // `first` is already complete (collection stops only past
                // the cap): probe the merge walk with a small budget,
                // fall back to the window without rescanning.
                match search_name_merge(
                    index,
                    &matcher,
                    total,
                    offset,
                    max,
                    desc,
                    MERGE_PROBE_BUDGET,
                ) {
                    Some(page) => page,
                    None => page_fallback(index, &first, parts.take()),
                }
            } else if let Some(page) =
                search_name_merge(index, &matcher, total, offset, max, desc, MERGE_STEP_BUDGET)
            {
                page
            } else {
                // Clustered matches exhausted the walk budget: collect all
                // ids and order only the requested window (exact page).
                match scan_route(index, &matcher) {
                    ScanRoute::Arena(p) => {
                        let (_, _, fp) = scan_arena(index, &p, &matcher, usize::MAX, false);
                        page_from_parts(index, fp, offset, max, total, desc)
                    }
                    ScanRoute::Whole(needle, cs) => {
                        let (_, all, _) = whole_lookup(index, needle, cs, usize::MAX, true);
                        page_ids_arena(index, &all, offset, max, total, desc)
                    }
                    ScanRoute::Or => {
                        // Broad union would collect everything fused;
                        // generic rescan bounds the transient instead.
                        let (_, all) = scan_entries_generic(index, &matcher, usize::MAX);
                        page_ids_arena(index, &all, offset, max, total, desc)
                    }
                    ScanRoute::Generic => {
                        let (_, all) = scan_entries(index, &matcher, usize::MAX);
                        page_ids_arena(index, &all, offset, max, total, desc)
                    }
                }
            }
        }
        Sort::PathAsc | Sort::PathDesc => {
            // Sort-then-page: one path rebuild per match (accepted cost) so
            // paging is globally ordered, not page-local.
            let desc = opts.sort == Sort::PathDesc;
            let matches = collect(index, &matcher, prev);
            let total = matches.len() as u64;
            let keyed: Vec<(String, EntryId, u8)> = paths_of(index, &matches)
                .into_iter()
                .zip(&matches)
                .map(|(path, h)| (path, h.id, h.vol))
                .collect();
            let cmp = |a: &(String, EntryId, u8), b: &(String, EntryId, u8)| {
                let ord = a.0.cmp(&b.0).then(a.1.cmp(&b.1));
                if desc {
                    ord.reverse()
                } else {
                    ord
                }
            };
            page_keyed(keyed, cmp, offset, max, total)
        }
        Sort::ModifiedAsc | Sort::ModifiedDesc | Sort::CreatedAsc | Sort::CreatedDesc => {
            // Same sort-then-page shape as `PathAsc`, keyed on a filesystem
            // stat per match: the index stores no timestamps, so ordering by
            // time pays one `metadata` call per match (parallel on the
            // search pool past `PAR_THRESHOLD`). Entries that cannot be
            // statted sort last in both directions.
            let (field, desc) = match opts.sort {
                Sort::ModifiedAsc => (TimeField::Modified, false),
                Sort::ModifiedDesc => (TimeField::Modified, true),
                Sort::CreatedAsc => (TimeField::Created, false),
                _ => (TimeField::Created, true),
            };
            let matches = collect(index, &matcher, prev);
            let total = matches.len() as u64;
            let keyed: Vec<(Option<i64>, EntryId, u8)> = if matches.len() < PAR_THRESHOLD {
                matches
                    .iter()
                    .map(|h| (time_key(&index.path(h.id), field), h.id, h.vol))
                    .collect()
            } else {
                search_pool().install(|| {
                    matches
                        .par_iter()
                        .map(|h| (time_key(&index.path(h.id), field), h.id, h.vol))
                        .collect()
                })
            };
            let cmp = |a: &(Option<i64>, EntryId, u8), b: &(Option<i64>, EntryId, u8)| {
                cmp_time_key(a.0, b.0, desc).then(a.1.cmp(&b.1))
            };
            page_keyed(keyed, cmp, offset, max, total)
        }
    }
}

/// Which filesystem timestamp [`Sort::ModifiedAsc`]/[`Sort::CreatedAsc`]
/// (and their `Desc` twins) key on.
#[derive(Clone, Copy)]
enum TimeField {
    Modified,
    Created,
}

/// `SystemTime` as Unix epoch milliseconds; `None` before the epoch.
fn unix_ms(t: std::time::SystemTime) -> Option<i64> {
    t.duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// Stat `path` for `field`; `None` when the stat or the field fails.
fn time_key(path: &str, field: TimeField) -> Option<i64> {
    let md = std::fs::metadata(path).ok()?;
    let t = match field {
        TimeField::Modified => md.modified().ok(),
        TimeField::Created => md.created().ok(),
    }?;
    unix_ms(t)
}

/// Time-sort comparator: `None` (unstattable) always sorts last in both
/// directions; `desc` reverses only the `Some`/`Some` arm.
fn cmp_time_key(a: Option<i64>, b: Option<i64>, desc: bool) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a, b) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(x), Some(y)) => {
            let ord = x.cmp(&y);
            if desc {
                ord.reverse()
            } else {
                ord
            }
        }
    }
}

/// Sort-then-page over pre-keyed matches, shared by the path and time
/// orders: a full sort below [`PATH_SORT_FULL_BOUND`], two `select_nth`
/// partitions plus a window sort past it (yields exactly the same page as a
/// full sort without ordering the whole set).
fn page_keyed<K>(
    mut keyed: Vec<(K, EntryId, u8)>,
    cmp: impl Fn(&(K, EntryId, u8), &(K, EntryId, u8)) -> std::cmp::Ordering,
    offset: usize,
    max: usize,
    total: u64,
) -> SearchResult {
    let end = (offset as u64).saturating_add(max as u64).min(total) as usize;
    if offset >= keyed.len() {
        return SearchResult {
            hits: Vec::new(),
            total,
        };
    }
    if total <= PATH_SORT_FULL_BOUND as u64 || max == usize::MAX {
        keyed.sort_by(&cmp);
        SearchResult {
            hits: keyed
                .into_iter()
                .skip(offset)
                .take(max)
                .map(|(_, id, vol)| Hit { id, vol })
                .collect(),
            total,
        }
    } else {
        // end > offset here (offset < total and end = min(offset+max, total)).
        keyed.select_nth_unstable_by(end - 1, &cmp);
        keyed[..end].select_nth_unstable_by(offset, &cmp);
        keyed[offset..end].sort_by(&cmp);
        SearchResult {
            hits: keyed[offset..end]
                .iter()
                .map(|&(_, id, vol)| Hit { id, vol })
                .collect(),
            total,
        }
    }
}

/// Below this many entries the rayon dispatch costs more than the scan, so
/// `collect` / `count` run sequentially.
const PAR_THRESHOLD: usize = 4096;

/// One `entries` chunk in the phase-1 sequential scan (cache-friendly order,
/// not name order). Each chunk keeps its match count plus its first
/// `cap` hit ids; the reduce truncates to the global first `cap`.
const SCAN_CHUNK: usize = 65_536;

/// One id chunk when folding page keys: mid-band lists are only tens of
/// thousands of ids, so scan-sized chunks would idle most of the pool.
/// Small (L1-resident buffers, many concurrent miss streams for the
/// strided arena reads — measured faster than 4k/16k chunks).
const FOLD_CHUNK: usize = 1_024;

/// Phase-1 hit cap: at most this many ids collected (first in entry order).
/// Queries matching fewer go straight to fold-keys + windowed page;
/// broader queries take the mid band or the `by_name + pending` merge walk.
/// Sized to cover common queries without ever sorting large sets.
const PHASE1_CAP: usize = 32_768;

/// Mid-band ceiling: totals in `(PHASE1_CAP, MID_CAP]` probe the merge walk
/// first and fall back to fold-keys + windowed page on budget exhaustion —
/// reusing phase 1's already-complete collection, never rescanning. Clustered
/// matches in this band defeat the walk (near-full random-order pass) while
/// uniform ones finish in hundreds of steps, so probing picks the cheaper
/// path per query automatically. Sized to cover the live 200k–500k class
/// (e.g. 387k-hit substring queries) without a broad rescan: phase 1 holds
/// up to ~2 MB of ids plus fused keys transiently. Beyond it the merge walk
/// (or its budget bail with a rescan) takes over.
const MID_CAP: usize = 500_000;

/// Merge-walk step budget (matcher evaluations) for huge totals: clustered
/// matches can turn the walk into a full random-order pass, so on exhaustion
/// the search bails to collect+window instead. Uniform broad queries finish
/// in hundreds of steps and never notice.
const MERGE_STEP_BUDGET: usize = 50_000;

/// Probe budget for the mid band (`(PHASE1_CAP, MID_CAP]` totals): uniform
/// queries in the band finish in hundreds of steps; clustered ones bail fast
/// onto the exact window path (which reuses phase 1's complete collection).
/// Deep pages in the band false-bail into the same exact window path.
const MERGE_PROBE_BUDGET: usize = 10_000;

/// Zero-hit abort floor inside the merge walk: with no match at all and too
/// few steps left to fill the page even at a 100% hit rate from there, the
/// window fallback is cheaper than walking on. The walk computes the exact
/// trip point per call (`budget - need`, floored here): clustered queries
/// whose fertility starts late but still inside the budget (e.g. hundreds of
/// thousands of substring hits spread past the first few thousand
/// name-ordered entries) walk on to success, while genuinely late clusters
/// still bail fast onto the exact window path. Results are identical either
/// way — both paths return the exact full-sort page.
const MERGE_ZERO_HIT_ABORT: usize = 3_000;

/// Past this `offset + max_results`, name-ordered search falls back to
/// collect+sort: a merge walk that deep approaches a full pass anyway, and
/// the fallback is exact.
const DEEP_PAGE_BOUND: usize = 100_000;

/// Past this many matches the `PathAsc`/`PathDesc` sorts order only the
/// requested page window (`select_nth` + window sort) instead of the full
/// match set. The page is identical; only oversized throwaway work is saved.
const PATH_SORT_FULL_BOUND: usize = 250_000;

/// Fold `(fold(name), id)` keys for `ids` (parallel past [`PAR_THRESHOLD`]).
/// Folds arena bytes directly: single pass, no per-name UTF-8 validation.
/// Folded sort key into a per-chunk bump buffer (`chunk/start/len` over the
/// chunk's `Vec<u8>`, byte-identical content to [`fold_bytes`]): orders
/// exactly like the `(fold(name), id)` snapshot key with none of the
/// per-key `String` allocations and no buffer concatenation (each key names
/// its own chunk's buffer).
#[derive(Clone, Copy)]
struct ArenaKey {
    chunk: u32,
    start: u32,
    len: u32,
    id: EntryId,
}

/// Folded keys for one page: per-chunk buffers plus per-chunk keys in scan
/// order (one key per match, chunk-stamped). Built either by folding an id
/// list ([`fold_parts`]) or as a byproduct of the arena scan ([`scan_arena`],
/// where the swept names are still hot — nearly free). Keys stay chunked so
/// the page selects the window hierarchically (per-chunk top-K in L1, then
/// one small final select) instead of one giant select.
struct FoldParts {
    bufs: Vec<Vec<u8>>,
    chunk_keys: Vec<Vec<ArenaKey>>,
}

/// Fold one chunk of ids into a private buffer (no per-key allocation;
/// missing ids fold to the empty key, as before). `chunk_idx` stamps every
/// key so buffers never need concatenating.
fn fold_keys_chunk(index: &Index, chunk: &[EntryId], chunk_idx: u32) -> (Vec<u8>, Vec<ArenaKey>) {
    let mut buf = Vec::with_capacity(chunk.len() * 32);
    let mut keys = Vec::with_capacity(chunk.len());
    for &id in chunk {
        let start = buf.len() as u32;
        if let Some(e) = index.entries.get(id as usize) {
            fold_bytes_into(index.name_bytes_of(e), &mut buf);
        }
        keys.push(ArenaKey {
            chunk: chunk_idx,
            start,
            len: buf.len() as u32 - start,
            id,
        });
    }
    (buf, keys)
}

/// Fold every id's name into per-chunk buffers on the pool (small chunks:
/// mid-band lists are only tens of thousands of ids, and one chunk per
/// thread would idle most of the pool). Keys stay chunked for the
/// hierarchical page select.
fn fold_parts(index: &Index, ids: &[EntryId]) -> FoldParts {
    let parts: Vec<(Vec<u8>, Vec<ArenaKey>)> = if ids.len() < PAR_THRESHOLD {
        vec![fold_keys_chunk(index, ids, 0)]
    } else {
        search_pool().install(|| {
            ids.par_chunks(FOLD_CHUNK)
                .enumerate()
                .map(|(ci, chunk)| fold_keys_chunk(index, chunk, ci as u32))
                .collect()
        })
    };
    let mut bufs = Vec::with_capacity(parts.len());
    let mut chunk_keys = Vec::with_capacity(parts.len());
    for (b, ks) in parts {
        bufs.push(b);
        chunk_keys.push(ks);
    }
    FoldParts { bufs, chunk_keys }
}

/// Order folded keys and return the page `[offset, offset + max)`:
/// hierarchical select — the many small sweep-chunk key vecs flatten into
/// one list, coarse thread-sized lanes keep their top-`end` keys in
/// parallel, then one small final select + window sort over the union
/// yields exactly the full-sort page. Exact: the global top-`end` is a
/// subset of the lane top-`end` union (any element missing from its lane's
/// top-`end` has `end` lane-mates ahead of it, hence rank `>= end`
/// globally). Consumes `parts`. Caller guarantees a bounded page
/// (`max != usize::MAX`; the name branch sends unlimited pages through
/// collect+sort) and complete keys (total key count `== total`). `total` is
/// the exact match count.
fn page_from_parts(
    index: &Index,
    parts: FoldParts,
    offset: usize,
    max: usize,
    total: u64,
    desc: bool,
) -> SearchResult {
    let FoldParts { bufs, chunk_keys } = parts;
    let nkeys: usize = chunk_keys.iter().map(Vec::len).sum();
    debug_assert_eq!(nkeys as u64, total);
    let end = (offset as u64 + max as u64).min(nkeys as u64) as usize;
    if offset >= nkeys {
        return SearchResult {
            hits: Vec::new(),
            total,
        };
    }
    // end > offset here (offset < len, max >= 1).
    let key_of =
        |k: &ArenaKey| &bufs[k.chunk as usize][k.start as usize..(k.start + k.len) as usize];
    let cmp = |a: &ArenaKey, b: &ArenaKey| {
        let ord = key_of(a).cmp(key_of(b)).then(a.id.cmp(&b.id));
        if desc {
            ord.reverse()
        } else {
            ord
        }
    };
    // Flatten the many small sweep-chunk key vecs (thousands of 32 KiB
    // chunks) into one key list first: the old per-chunk top-`end` union
    // fed a single-threaded final select over chunks×`end` candidates
    // (500k+ for broad queries). Coarse lanes below shrink that union to
    // threads×`end` instead — one small final select + window sort.
    let mut flat: Vec<ArenaKey> = chunk_keys.into_iter().flatten().collect();
    // Small lists skip the lanes (pool + scatter overhead dominates):
    // one flat select + window sort.
    if flat.len() < PAR_THRESHOLD {
        flat.select_nth_unstable_by(end - 1, &cmp);
        flat[..end].sort_by(&cmp);
        return SearchResult {
            hits: flat[offset..end]
                .iter()
                .map(|k| Hit {
                    id: k.id,
                    vol: index.volume_of(k.id).unwrap_or(0),
                })
                .collect(),
            total,
        };
    }
    // Per-lane top-`end` (short lanes taken whole), then the exact window
    // out of the union.
    let lanes = search_pool().current_num_threads().max(1);
    let per_lane = flat.len().div_ceil(lanes).max(1);
    let parts: Vec<Vec<ArenaKey>> = search_pool().install(|| {
        flat.par_chunks_mut(per_lane)
            .map(|lane| {
                if lane.len() > end {
                    lane.select_nth_unstable_by(end - 1, &cmp);
                    lane[..end].to_vec()
                } else {
                    lane.to_vec()
                }
            })
            .collect()
    });
    let mut cand: Vec<ArenaKey> = parts.into_iter().flatten().collect();
    // end <= cand.len() here (every lane contributed min(end, len), and at
    // least one lane is non-empty since offset < nkeys).
    cand.select_nth_unstable_by(end - 1, &cmp);
    cand[..end].sort_by(&cmp);
    SearchResult {
        hits: cand[offset..end]
            .iter()
            .map(|k| Hit {
                id: k.id,
                vol: index.volume_of(k.id).unwrap_or(0),
            })
            .collect(),
        total,
    }
}

/// Order `ids` (every match) and return the page `[offset, offset + max)`
/// via [`fold_parts`] + [`page_from_parts`].
fn page_ids_arena(
    index: &Index,
    ids: &[EntryId],
    offset: usize,
    max: usize,
    total: u64,
    desc: bool,
) -> SearchResult {
    if offset >= ids.len() {
        return SearchResult {
            hits: Vec::new(),
            total,
        };
    }
    let parts = fold_parts(index, ids);
    page_from_parts(index, parts, offset, max, total, desc)
}

/// Phase 1 of name-ordered search (and all of [`count`]): one sequential
/// pass over `entries` in index order — prefetch-friendly, unlike a
/// `by_name`-order walk. Returns the exact total plus the global first `cap`
/// match ids in entry order. Collection stops past `cap` via a shared atomic
/// (batched, uncontended): chunks stop only on a loaded value `>= cap`, so
/// when `total <= cap` nothing ever stops early and the list is complete.
/// `cap == 0` collects nothing (the [`count`] path, zero atomics);
/// `cap == usize::MAX` never stops (the bail-out rescan).
/// Single-byte case-insensitive substring test: exactly
/// [`contains_insensitive_prefolded`](crate::fold::contains_insensitive_prefolded)
/// for a 1-byte ASCII needle (the only shape reaching here). One inline pass
/// per name — either casing hits immediately with no `is_ascii` pre-scan and
/// no `memchr` per-name setup (which dominates at 8.5M entries) — while a
/// name with non-ASCII bytes and no ASCII hit takes the exact folded fallback
/// (covers e.g. U+212A folding to `k`), so verdicts are identical.
#[inline(always)]
fn contains_single_ci(name: &[u8], n: u8, u: u8, needle: &[u8]) -> bool {
    debug_assert!(n.is_ascii() && needle.len() == 1);
    let mut nonascii = false;
    for &b in name {
        if b >= 0x80 {
            nonascii = true;
        } else if b == n || b == u {
            return true;
        }
    }
    nonascii && contains_insensitive_prefolded(name, needle, true)
}

/// Entry-point scan: single-leaf matchers run the monomorphized
/// [`scan_loop`] driver (the leaf predicate inlines — no per-entry
/// `Matcher` dispatch, no `PathCtx` unless the leaf uses it); compound
/// matchers and multi-segment / separator `path:` queries keep the generic
/// loop. Result semantics are identical: `total` counts every match and
/// `hits` holds the global first `cap` ids in scan order.
fn scan_entries(index: &Index, matcher: &Matcher, cap: usize) -> (u64, Vec<EntryId>) {
    match scan_route(index, matcher) {
        ScanRoute::Whole(needle, cs) => {
            let (total, hits, _) = whole_lookup(index, needle, cs, cap, true);
            (total, hits)
        }
        ScanRoute::Arena(plan) => {
            let (total, hits, _) = scan_arena(index, &plan, matcher, cap, true);
            (total, hits)
        }
        ScanRoute::Or => {
            let (total, hits, _) = scan_or_union(index, matcher, cap, true);
            (total, hits)
        }
        ScanRoute::Generic => match matcher {
            Matcher::SubFold { needle, ascii } if *ascii && needle.len() == 1 => {
                // Single-byte case-insensitive substring (`a`, `e`, `x`):
                // the inline pass above replaces an `is_ascii` pre-scan
                // plus a `memchr` call per name (see `contains_single_ci`).
                let n = needle.as_bytes()[0];
                let u = n.to_ascii_uppercase();
                let needle = needle.clone();
                scan_loop(index, cap, &move |_, _, _, name, _| {
                    contains_single_ci(name, n, u, needle.as_bytes())
                })
            }
            Matcher::SubFold { needle, ascii } => {
                let needle = needle.as_bytes();
                let ascii = *ascii;
                scan_loop(index, cap, &move |_, _, _, name, _| {
                    contains_insensitive_prefolded(name, needle, ascii)
                })
            }
            Matcher::SubRaw { needle, .. } if needle.len() == 1 => {
                // Single-byte case-sensitive substring: one exact-byte
                // scan decides — an exact hit implies the folded
                // prefilter, an exact miss fails the confirm — replacing
                // the prefilter + `memmem` pair per name.
                let b = needle.as_bytes()[0];
                scan_loop(index, cap, &move |_, _, _, name, _| {
                    memchr::memchr(b, name).is_some()
                })
            }
            Matcher::SubRaw { needle, pre } => scan_loop(index, cap, &move |_, _, _, name, _| {
                pre.test(name) && memmem::find(name, needle.as_bytes()).is_some()
            }),
            Matcher::Whole { raw, folded } => {
                scan_loop(index, cap, &move |_, _, _, name, _| match folded {
                    Some(f) => eq_insensitive(name_str(name), raw, f),
                    None => name_str(name) == raw,
                })
            }
            Matcher::Ext(list) => {
                scan_loop(index, cap, &move |_, _, _, name, _| ext_match(name, list))
            }
            Matcher::GlobFast {
                shape,
                lit,
                lit2,
                regex,
            } => {
                let shape = *shape;
                scan_loop(index, cap, &move |_, _, _, name, _| {
                    glob_fast_match(name, shape, lit, lit2.as_ref(), regex)
                })
            }
            Matcher::RegexAffix {
                kind,
                guard_nl,
                regex,
                case_sensitive,
            } => {
                let guard_nl = *guard_nl;
                let case_sensitive = *case_sensitive;
                scan_loop(index, cap, &move |_, _, _, name, _| {
                    regex_fast_match(name, kind, guard_nl, case_sensitive, regex)
                })
            }
            Matcher::True => scan_loop(index, cap, &|_, _, _, _, _| true),
            Matcher::IsDir => scan_loop(index, cap, &|_, _, entry, _, _| {
                entry.flags & DIRECTORY != 0
            }),
            Matcher::IsFile => scan_loop(index, cap, &|_, _, entry, _, _| {
                entry.flags & DIRECTORY == 0
            }),
            Matcher::PathSub {
                segs,
                has_separator: false,
                memo,
                ..
            } if segs.len() == 1 => {
                let memo = memo.get_or_init(|| PathMemo::new(index.entries.len()));
                let seg = &segs[0];
                // Repeat queries replay the cached verdict bitset (exact
                // total + first-`cap` page) without walking; the first query
                // of an era walks and stores it.
                if let Some(cached) = replay_path_bits(index, seg, cap) {
                    return cached;
                }
                let words = index.entries.len().div_ceil(64);
                let bits: Vec<AtomicU64> = (0..words).map(|_| AtomicU64::new(0)).collect();
                let out = scan_loop(index, cap, &|index, id, entry, name, pcx| {
                    let hit = path_single_hit(index, id, entry, name, seg, memo, pcx);
                    if hit {
                        bits[(id as usize) / 64]
                            .fetch_or(1 << ((id as usize) % 64), Ordering::Relaxed);
                    }
                    hit
                });
                store_path_bits(
                    index,
                    seg,
                    bits.iter().map(|w| w.load(Ordering::Relaxed)).collect(),
                );
                out
            }
            _ => scan_entries_generic(index, matcher, cap),
        },
    }
}

/// Literal driver for the contiguous arena scan: the byte pattern plus its
/// casing. `Exact` runs one `memmem::Finder` (byte-exact — sound on any
/// bytes, no ASCII gate; covers case-sensitive literals). `Ci` runs a
/// `memchr2` first-byte search over ASCII-verified chunks (sound: in an
/// all-ASCII chunk every true match starts at a first-byte position).
/// Non-ASCII needles never drive (they fall back to the per-entry scan).
/// `substr` marks pure-substring leaves whose candidate check is definitive
/// (bounds + tombstone + literal occurrence), skipping the full matcher;
/// every other shape re-verifies candidates with the full matcher, so
/// verdicts are identical to the per-entry scan either way.
struct ArenaPlan {
    bytes: Vec<u8>,
    ci: bool,
    substr: bool,
    /// Sweep anchor: rarest byte and its offset in `bytes` (case-insensitive
    /// substr leaves), else the first byte at offset 0. The sweep iterates
    /// anchor positions and verifies the full literal around them — fewer
    /// candidates than first-byte iteration when the rarest byte sits
    /// inside, identical verdicts either way.
    a0: u8,
    a1: u8,
    aoff: usize,
}

/// TEMP-MEASURE chunk census counters (counts only, no names).
static ARENA_SWEPT: AtomicU64 = AtomicU64::new(0);
static ARENA_FALLBACK: AtomicU64 = AtomicU64::new(0);
static ARENA_SKIPPED: AtomicU64 = AtomicU64::new(0);
static OR_SWEPT: AtomicU64 = AtomicU64::new(0);
static OR_FALLBACK: AtomicU64 = AtomicU64::new(0);
static OR_SKIPPED: AtomicU64 = AtomicU64::new(0);

/// Rarity rank for anchor selection (index = rare first): space, then
/// letters rare→common in filenames (live 8.55M arena histogram — e.g. `c`
/// is English-rare but filename-common at 4.4% of name bytes, while `h`
/// sits at 1.6%), then everything else (digits, punctuation — common in
/// filenames: versions, extensions, separators). Wrong guesses only cost
/// speed (correctness never depends on it).
fn anchor_rank(b: u8) -> usize {
    const ORDER: &[u8] = b" qzkvxwjgubhfmlydrinopscate";
    ORDER.iter().position(|&x| x == b).unwrap_or(ORDER.len())
}

impl ArenaPlan {
    /// Usable literal: at least 2 bytes (single bytes generate near
    /// every-name candidates on real data — the per-entry scan stays
    /// cheaper there; measured: 1-char arena 120ms+ vs 45ms per-entry at
    /// 8.5M) and, when folded, pure ASCII (non-ASCII folding can move
    /// match starts off first-byte positions).
    fn lit(bytes: &[u8], folded: bool, ascii: bool, substr: bool) -> Option<ArenaPlan> {
        if bytes.len() < 2 || (folded && !ascii) {
            return None;
        }
        // Case-insensitive substr leaves drive on the rarest byte (fewer
        // candidates); everything else anchors on the first byte.
        let (a0, aoff) = if folded && substr {
            bytes
                .iter()
                .enumerate()
                .min_by_key(|(_, &b)| anchor_rank(b))
                .map(|(i, &b)| (b, i))
                .unwrap_or((bytes[0], 0))
        } else {
            (bytes[0], 0)
        };
        let a1 = if folded { a0.to_ascii_uppercase() } else { a0 };
        Some(ArenaPlan {
            bytes: bytes.to_vec(),
            ci: folded,
            substr,
            a0,
            a1,
            aoff,
        })
    }

    /// Single bytes that stay on the arena sweep instead of the per-entry
    /// scan: filename-rare letters (live 8.55M arena histogram: each under
    /// 0.7% of name bytes — `x` 0.64%, `q` 0.12%, `z` 0.14%, `j` 0.68%,
    /// `k` 0.53%, `v` 0.61%, `w` 0.64%). Dense singles (`a` 4.6%, `e`
    /// 5.9%) keep the per-entry scan — sweeping them enumerates tens of
    /// millions of anchor positions. Fold-edge names (e.g. U+212A folding
    /// to `k` with no `k` byte) stay exact: their non-ascii bytes dirty
    /// their chunk's block mask, forcing the exact per-entry fallback for
    /// that chunk. The needle here is already folded (lowercase), so one
    /// match arm covers both casings.
    fn single_arena_byte(b: u8) -> bool {
        matches!(b, b'q' | b'z' | b'x' | b'j' | b'k' | b'v' | b'w')
    }

    /// Usable single-byte literal for the arena sweep (see
    /// [`single_arena_byte`](ArenaPlan::single_arena_byte)): the byte
    /// itself is the anchor at offset 0.
    fn lit1(b: u8) -> ArenaPlan {
        ArenaPlan {
            bytes: vec![b],
            ci: true,
            substr: true,
            a0: b,
            a1: b.to_ascii_uppercase(),
            aoff: 0,
        }
    }

    /// Longer of two usable literals (length proxies rarity for multi-part
    /// filters where either part is a necessary condition).
    fn best(a: Option<ArenaPlan>, b: Option<ArenaPlan>) -> Option<ArenaPlan> {
        match (a, b) {
            (Some(x), Some(y)) => Some(if x.bytes.len() >= y.bytes.len() { x } else { y }),
            (x, None) => x,
            (None, y) => y,
        }
    }

    fn for_matcher(m: &Matcher) -> Option<ArenaPlan> {
        match m {
            Matcher::SubFold { needle, ascii } if *ascii && needle.len() == 1 => {
                // Rare single bytes sweep the arena (see `single_arena_byte`);
                // dense ones fall through to the per-entry scan (`None`).
                let b = needle.as_bytes()[0];
                Self::single_arena_byte(b).then(|| Self::lit1(b))
            }
            Matcher::SubFold { needle, ascii } => Self::lit(needle.as_bytes(), true, *ascii, true),
            Matcher::SubRaw { needle, .. } => Self::lit(needle.as_bytes(), false, true, true),
            Matcher::Whole { raw, folded } => match folded {
                None => Self::lit(raw.as_bytes(), false, true, false),
                Some(f) => Self::lit(f.as_bytes(), true, f.is_ascii(), false),
            },
            Matcher::GlobFast { lit, lit2, .. } => {
                // Glob shape checks re-scan the literal per candidate, so
                // filtering buys nothing when the first byte is
                // case-invariant (every occurrence is a candidate): fall
                // through to the tight per-entry pred. Measured: "*.log"
                // 3.6 scan-loop vs 6.0 arena; regex affixes keep the arena
                // (their per-entry checks are expensive).
                if lit.folded
                    && lit
                        .bytes
                        .first()
                        .is_some_and(|&b| b.to_ascii_uppercase() == b)
                {
                    return None;
                }
                let a = Self::lit(&lit.bytes, lit.folded, lit.ascii, false);
                let b = lit2
                    .as_ref()
                    .and_then(|l| Self::lit(&l.bytes, l.folded, l.ascii, false));
                Self::best(a, b)
            }
            Matcher::RegexAffix {
                kind,
                case_sensitive,
                ..
            } => {
                let cs = *case_sensitive;
                match kind {
                    RegexFastKind::Substr(l)
                    | RegexFastKind::Whole(l)
                    | RegexFastKind::Suffix(l) => Self::lit(l, !cs, l.is_ascii(), false),
                    RegexFastKind::Prefix(RegexPrefix::Lit(l)) => {
                        Self::lit(l, !cs, l.is_ascii(), false)
                    }
                    RegexFastKind::Prefix(RegexPrefix::Class { .. }) => None,
                    RegexFastKind::PreSuf(pre, suf) => {
                        let a = match pre {
                            RegexPrefix::Lit(l) => Self::lit(l, !cs, l.is_ascii(), false),
                            RegexPrefix::Class { .. } => None,
                        };
                        Self::best(a, Self::lit(suf, !cs, suf.is_ascii(), false))
                    }
                }
            }
            Matcher::RegexMatch { pre, .. } | Matcher::Glob { pre, .. } => match pre {
                Some(lit) => Self::lit(&lit.bytes, lit.folded, lit.ascii, false),
                None => None,
            },
            // Multi-term: the longest child literal drives (a necessary
            // condition), but verification always runs the whole conjunction
            // — so the inherited `substr` fast path is forced off here.
            Matcher::And(v) => v
                .iter()
                .filter_map(Self::for_matcher)
                .max_by_key(|p| p.bytes.len())
                .map(|mut p| {
                    p.substr = false;
                    p
                }),
            Matcher::True
            | Matcher::Or(_)
            | Matcher::Not(_)
            | Matcher::Ext(_)
            | Matcher::PathSub { .. }
            | Matcher::IsDir
            | Matcher::IsFile => None,
        }
    }
}

/// Aggregate block presence mask over a chunk's byte range (OR of the
/// overlapped 64 KiB bitsets) plus coverage: false when the bitset is short
/// (corrupt/short tables) — then the caller keeps the chunk and falls back
/// instead of skipping.
fn chunk_mask(index: &Index, chunk: &ArenaChunk) -> ([u64; 4], bool) {
    let mut agg = [0u64; 4];
    let bb0 = chunk.b_start >> ARENA_BLOCK_BITS as usize;
    let bb1 = chunk.b_end.saturating_sub(1) >> ARENA_BLOCK_BITS as usize;
    for bi in bb0..=bb1 {
        match index.arena_blocks.get(bi) {
            Some(m) => {
                for (a, b) in agg.iter_mut().zip(m.iter()) {
                    *a |= *b;
                }
            }
            None => return (agg, false),
        }
    }
    (agg, true)
}

/// Byte presence in a 256-bit block mask (see `arena_blocks`).
#[inline(always)]
fn block_has(mask: &[u64; 4], b: u8) -> bool {
    mask[(b >> 6) as usize] & (1 << (b & 63)) != 0
}

/// Whole-name fast path: needle bytes (folded unless case-sensitive) plus
/// casing, when the matcher is whole-name equality and the snapshot is
/// fresh (binary-searchable). Stale snapshot tails fall back to the scan.
fn whole_fast<'m>(index: &Index, matcher: &'m Matcher) -> Option<(&'m [u8], bool)> {
    match matcher {
        Matcher::Whole { raw, folded } if index.by_name_is_fresh() => match folded {
            Some(f) => Some((f.as_bytes(), false)),
            None => Some((raw.as_bytes(), true)),
        },
        _ => None,
    }
}

/// Whole-name equality without scanning: binary-search the fresh sorted
/// snapshot (`by_name`) plus the sorted `pending` list for the needle's
/// fold, collecting every live match in `(fold, id)` order — exactly the
/// scan's verdict set (fold-equality is exactly `eq_insensitive`, and the
/// case-sensitive arm filters raw equality on top; tombstones skipped in
/// both lists). Fused keys ride along for the page path (whole-name totals
/// are small, so folding is trivial).
fn whole_lookup(
    index: &Index,
    needle: &[u8],
    case_sensitive: bool,
    cap: usize,
    need_hits: bool,
) -> (u64, Vec<EntryId>, FoldParts) {
    use std::cmp::Ordering;
    // Live ids in one sorted list whose fold equals the needle.
    fn range_matches(
        index: &Index,
        list: &[EntryId],
        needle: &[u8],
        case_sensitive: bool,
    ) -> Vec<EntryId> {
        let start = list.partition_point(|&id| {
            let nb = index
                .entries
                .get(id as usize)
                .and_then(|e| index.names.get_bytes(e.name_off, e.name_len))
                .unwrap_or(b"");
            cmp_folded_bytes(nb, needle) == Ordering::Less
        });
        let mut out = Vec::new();
        for &id in &list[start..] {
            let Some(e) = index.entries.get(id as usize) else {
                continue;
            };
            let nb = index.names.get_bytes(e.name_off, e.name_len).unwrap_or(b"");
            if cmp_folded_bytes(nb, needle) != Ordering::Equal {
                break;
            }
            if e.flags & TOMBSTONE != 0 {
                continue;
            }
            if !vol_enabled(index, id) {
                continue;
            }
            if case_sensitive && nb != needle {
                continue;
            }
            out.push(id);
        }
        out
    }
    // Both runs ascend by id within the equal fold (`pending` ids are
    // always newer, so the lists are disjoint): linear merge.
    let a = range_matches(index, index.by_name(), needle, case_sensitive);
    let b = range_matches(index, index.pending(), needle, case_sensitive);
    let mut all = Vec::with_capacity(a.len() + b.len());
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        if a[i] < b[j] {
            all.push(a[i]);
            i += 1;
        } else {
            all.push(b[j]);
            j += 1;
        }
    }
    all.extend_from_slice(&a[i..]);
    all.extend_from_slice(&b[j..]);
    let total = all.len() as u64;
    let hits = if need_hits {
        all.iter().take(cap).copied().collect()
    } else {
        Vec::new()
    };
    let parts = if cap > 0 {
        let (buf, keys) = fold_keys_chunk(index, &all, 0);
        FoldParts {
            bufs: vec![buf],
            chunk_keys: vec![keys],
        }
    } else {
        FoldParts {
            bufs: Vec::new(),
            chunk_keys: Vec::new(),
        }
    };
    (total, hits, parts)
}

/// Scan route for one matcher: a literal arena sweep, a whole-name lookup,
/// a disjunction union, or the generic per-entry scan. Computed identically
/// at every dispatch site (phase 1, rescan, count) so behavior matches
/// everywhere.
enum ScanRoute<'a> {
    Arena(ArenaPlan),
    Whole(&'a [u8], bool),
    Or,
    Generic,
}

/// Route a matcher: arena sweep for usable literals, multi-sweep union for
/// disjunctions of pure-substring leaves (each child filters cheaply alone;
/// the union verifies nothing further — substr leaves are definitive), and
/// the generic scan otherwise. The arena needs a big monotonic prefix and a
/// small tail; anything else goes generic.
fn scan_route<'a>(index: &'a Index, matcher: &'a Matcher) -> ScanRoute<'a> {
    // Whole-name equality resolves via the sorted snapshot (no scan) when
    // fresh — microseconds even at 8.5M. Checked first (a Whole matcher
    // also carries an arena-usable literal, but lookup dominates it).
    if let Some((needle, cs)) = whole_fast(index, matcher) {
        return ScanRoute::Whole(needle, cs);
    }
    let prefix = (index.arena_prefix_len as usize).min(index.entries.len());
    if prefix >= PAR_THRESHOLD && index.entries.len() - prefix <= PAR_THRESHOLD {
        if let Some(plan) = ArenaPlan::for_matcher(matcher) {
            return ScanRoute::Arena(plan);
        }
        // Disjunctions route to the union (which fuses substr children and
        // falls back to generic itself when a child has no usable plan).
        if matches!(matcher, Matcher::Or(_)) {
            return ScanRoute::Or;
        }
    }
    ScanRoute::Generic
}

/// One sweep pass in [`scan_or_fused`]: case-insensitive plans sharing an
/// anchor byte pair ride a single `memchr2` pass, while case-sensitive
/// plans keep their own `Finder` pass.
enum OrSweep {
    Ci { a0: u8, a1: u8, members: Vec<usize> },
    Cs { member: usize },
}

/// Single-plan case-insensitive sweep into the chunk union bitmap `seen`
/// (confirm-first, dedupe, bounds, tombstone, substr-direct accept — the
/// same body [`scan_arena`] uses, minus folding which the caller batches).
#[allow(clippy::too_many_arguments)]
fn sweep_ci_plan(
    index: &Index,
    p: &ArenaPlan,
    a0: u8,
    a1: u8,
    chunk: &ArenaChunk,
    bytes: &[u8],
    arena: &[u8],
    seen: &mut [bool],
) {
    let nlen = p.bytes.len();
    let mut last = EntryId::MAX;
    let mut id = chunk.id_start;
    for h in memchr::memchr2_iter(a0, a1, bytes) {
        let abs = chunk.b_start + h;
        let Some(start) = abs.checked_sub(p.aoff) else {
            continue;
        };
        let cand = arena.get(start..start + nlen).unwrap_or(b"");
        if cand.len() < nlen {
            continue;
        }
        let mut ok = true;
        for (i, &b) in p.bytes.iter().enumerate() {
            if cand[i].to_ascii_lowercase() != b {
                ok = false;
                break;
            }
        }
        if !ok {
            continue;
        }
        while id + 1 < chunk.id_end && index.entries[(id + 1) as usize].name_off as usize <= start {
            id += 1;
        }
        if id == last {
            continue;
        }
        last = id;
        let e = &index.entries[id as usize];
        let end = e.name_off as usize + e.name_len as usize;
        if start + nlen > end {
            continue;
        }
        if e.flags & TOMBSTONE != 0 {
            continue;
        }
        if !vol_enabled(index, id) {
            continue;
        }
        seen[(id - chunk.id_start) as usize] = true;
    }
}

/// Shared-anchor sweep for several case-insensitive plans: one `memchr2`
/// pass over `(a0, a1)`, each member's literal verified per anchor position.
/// Per-member owner cursors advance independently (every member's starts
/// ascend with the anchor positions, so each cursor stays exact) with
/// per-member dedupe — the union written to `seen` is exactly the union of
/// the members' solo sweeps.
#[allow(clippy::too_many_arguments)]
fn sweep_ci_group(
    index: &Index,
    plans: &[ArenaPlan],
    a0: u8,
    a1: u8,
    members: &[usize],
    chunk: &ArenaChunk,
    bytes: &[u8],
    arena: &[u8],
    seen: &mut [bool],
) {
    let mut ids = vec![chunk.id_start; members.len()];
    let mut lasts = vec![EntryId::MAX; members.len()];
    for h in memchr::memchr2_iter(a0, a1, bytes) {
        let abs = chunk.b_start + h;
        for (mi, &pi) in members.iter().enumerate() {
            let p = &plans[pi];
            let nlen = p.bytes.len();
            let Some(start) = abs.checked_sub(p.aoff) else {
                continue;
            };
            let cand = arena.get(start..start + nlen).unwrap_or(b"");
            if cand.len() < nlen {
                continue;
            }
            let mut ok = true;
            for (i, &b) in p.bytes.iter().enumerate() {
                if cand[i].to_ascii_lowercase() != b {
                    ok = false;
                    break;
                }
            }
            if !ok {
                continue;
            }
            let mut id = ids[mi];
            while id + 1 < chunk.id_end
                && index.entries[(id + 1) as usize].name_off as usize <= start
            {
                id += 1;
            }
            ids[mi] = id;
            if id == lasts[mi] {
                continue;
            }
            lasts[mi] = id;
            let e = &index.entries[id as usize];
            let end = e.name_off as usize + e.name_len as usize;
            if start + nlen > end {
                continue;
            }
            if e.flags & TOMBSTONE != 0 {
                continue;
            }
            if !vol_enabled(index, id) {
                continue;
            }
            seen[(id - chunk.id_start) as usize] = true;
        }
    }
}

/// Fused multi-literal sweep for disjunctions of pure-substring leaves
/// (see [`scan_route`]): one chunking, one mask gate per chunk, then one
/// memchr pass + sweep per anchor group over L2-hot bytes (vs one full scan
/// per child). Case-insensitive plans sharing an anchor byte pair sweep
/// together ([`OrSweep::Ci`]: one `memchr2` pass, each member's literal
/// verified per anchor position — e.g. `config|settings` shares its anchor);
/// case-sensitive plans keep their own `Finder` pass. Per-plan accepts
/// merge-deduped per chunk via a small id bitmap (order-preserving, no
/// sort), then accepted in ascending order with fused folding. Candidates of
/// every plan are a superset of its matches and substr confirms are
/// definitive, so the union is exact with zero matcher calls. Returns the
/// exact total plus hits truncated to `cap` and fused parts iff complete
/// (`total <= cap`); otherwise parts is `None` and the caller falls back
/// (which recounts nothing — total is already exact).
fn scan_or_fused(
    index: &Index,
    plans: &[ArenaPlan],
    matcher: &Matcher,
    cap: usize,
    need_hits: bool,
) -> (u64, Vec<EntryId>, Option<FoldParts>) {
    let prefix = (index.arena_prefix_len as usize).min(index.entries.len());
    let arena = index.names.as_bytes();
    let mut total_bytes = 0usize;
    if prefix > 0 {
        let e = &index.entries[prefix - 1];
        total_bytes = e.name_off as usize + e.name_len as usize;
    }
    let chunks = arena_chunks(index, prefix, total_bytes);
    // Per-plan Finders (cs only; ci plans use memchr2 inline), built once.
    let finders: Vec<Option<memmem::Finder>> = plans
        .iter()
        .map(|p| {
            if p.ci {
                None
            } else {
                Some(memmem::Finder::new(&p.bytes))
            }
        })
        .collect();
    // Sweep passes: ci plans sharing an anchor pair ride one `memchr2`
    // pass (grouped here, once per query); cs plans sweep solo. A group
    // iterates the same anchor positions each member would visit alone
    // and verifies members independently into the shared union bitmap,
    // so verdicts are identical to per-plan sweeps with fewer passes.
    let mut sweeps: Vec<OrSweep> = Vec::with_capacity(plans.len());
    for (pi, p) in plans.iter().enumerate() {
        if p.ci {
            match sweeps.iter_mut().find_map(|s| match s {
                OrSweep::Ci { a0, a1, members } if *a0 == p.a0 && *a1 == p.a1 => Some(members),
                _ => None,
            }) {
                Some(members) => members.push(pi),
                None => sweeps.push(OrSweep::Ci {
                    a0: p.a0,
                    a1: p.a1,
                    members: vec![pi],
                }),
            }
        } else {
            sweeps.push(OrSweep::Cs { member: pi });
        }
    }
    let collected = AtomicU64::new(0);
    let fold = cap > 0;
    let partial: Vec<ChunkScan> = search_pool().install(|| {
        chunks
            .into_par_iter()
            .enumerate()
            .map(|(ci, chunk)| {
                let ci = ci as u32;
                let mut acc = ChunkAcc {
                    count: 0,
                    hits: Vec::new(),
                    collecting: need_hits && cap > 0,
                    since_sync: 0,
                    collected: &collected,
                    cap,
                    record: need_hits,
                    folding: fold,
                };
                let mut pcx = PathCtx::new();
                let mut buf = Vec::new();
                let mut keys = Vec::new();
                let bytes = &arena[chunk.b_start..chunk.b_end];
                let (agg, covered) = chunk_mask(index, &chunk);
                // Skip the chunk when NO plan's first byte can occur;
                // ASCII-clean (mask-derived) decides sweep vs fallback.
                // A chunk with a non-substr... (all plans here are substr
                // by route contract) — fallback evaluates the full Or.
                let mut any = false;
                for p in plans.iter() {
                    if p.ci {
                        if block_has(&agg, p.a0) || block_has(&agg, p.a1) {
                            any = true;
                            break;
                        }
                    } else if block_has(&agg, p.a0) {
                        any = true;
                        break;
                    }
                }
                if covered && !any {
                    acc.flush();
                    return (acc.count, acc.hits, buf, keys);
                }
                if fold {
                    buf.reserve(4096);
                    keys.reserve(128);
                }
                let ascii_clean = covered && agg[2] == 0 && agg[3] == 0;
                // TEMP-MEASURE: or-sweep vs fallback chunk census.
                if std::env::var_os("FLOKI_CENSUS").is_some() {
                    use std::sync::atomic::Ordering as Ord;
                    if covered && !any {
                        OR_SKIPPED.fetch_add(1, Ord::Relaxed);
                    } else if plans.iter().any(|p| p.ci) && !ascii_clean {
                        OR_FALLBACK.fetch_add(1, Ord::Relaxed);
                    } else {
                        OR_SWEPT.fetch_add(1, Ord::Relaxed);
                    }
                }
                let need_fallback = plans.iter().any(|p| p.ci) && !ascii_clean;
                if need_fallback {
                    // Non-ASCII chunk with a ci plan: per-entry evaluation
                    // of the whole disjunction (exact).
                    for id in chunk.id_start..chunk.id_end {
                        if eval_hit(index, matcher, id, &mut pcx) {
                            acc.accept(id);
                            if acc.folding {
                                fold_accept(index, id, &mut buf, &mut keys, ci);
                            }
                        }
                    }
                    acc.flush();
                    return (acc.count, acc.hits, buf, keys);
                }
                // Sweeps per anchor group (each ascending + internally
                // deduped), unioned through a chunk-local id bitmap
                // (order-preserving, no sort), then accepted + folded in
                // ascending order.
                let span = (chunk.id_end - chunk.id_start) as usize;
                let mut seen = vec![false; span];
                for sweep in sweeps.iter() {
                    match sweep {
                        OrSweep::Ci { a0, a1, members } if members.len() == 1 => {
                            sweep_ci_plan(
                                index,
                                &plans[members[0]],
                                *a0,
                                *a1,
                                &chunk,
                                bytes,
                                arena,
                                &mut seen,
                            );
                        }
                        OrSweep::Ci { a0, a1, members } => {
                            sweep_ci_group(
                                index, plans, *a0, *a1, members, &chunk, bytes, arena, &mut seen,
                            );
                        }
                        OrSweep::Cs { member } => {
                            let p = &plans[*member];
                            let nlen = p.bytes.len();
                            let mut last = EntryId::MAX;
                            let mut id = chunk.id_start;
                            let finder = finders[*member].as_ref().expect("cs plan has Finder");
                            for h in finder.find_iter(bytes) {
                                let abs = chunk.b_start + h;
                                while id + 1 < chunk.id_end
                                    && index.entries[(id + 1) as usize].name_off as usize <= abs
                                {
                                    id += 1;
                                }
                                if id == last {
                                    continue;
                                }
                                last = id;
                                let e = &index.entries[id as usize];
                                let end = e.name_off as usize + e.name_len as usize;
                                if abs + nlen > end {
                                    continue;
                                }
                                if e.flags & TOMBSTONE != 0 {
                                    continue;
                                }
                                if !vol_enabled(index, id) {
                                    continue;
                                }
                                seen[(id - chunk.id_start) as usize] = true;
                            }
                        }
                    }
                }
                for (i, s) in seen.iter().enumerate() {
                    if *s {
                        let id = chunk.id_start + i as u32;
                        acc.accept(id);
                        if acc.folding {
                            fold_accept(index, id, &mut buf, &mut keys, ci);
                        }
                    }
                }
                acc.flush();
                (acc.count, acc.hits, buf, keys)
            })
            .collect()
    });
    let mut total: u64 = partial.iter().map(|(c, _, _, _)| *c).sum();
    let mut hits = Vec::new();
    let mut bufs = Vec::with_capacity(partial.len() + 1);
    let mut chunk_keys = Vec::with_capacity(partial.len() + 1);
    for (_, h, b, ks) in partial.into_iter() {
        if need_hits && hits.len() < cap {
            let room = cap - hits.len();
            hits.extend(h.iter().take(room).copied());
        }
        bufs.push(b);
        chunk_keys.push(ks);
    }
    // Tail: ids past the monotonic prefix (normally none), evaluated
    // against the whole disjunction in scan order.
    if prefix < index.entries.len() {
        let mut pcx = PathCtx::new();
        let mut tail_buf = Vec::new();
        let mut tail_keys = Vec::new();
        let tail_chunk = bufs.len() as u32;
        for i in prefix..index.entries.len() {
            let id = i as EntryId;
            if eval_hit(index, matcher, id, &mut pcx) {
                total += 1;
                if need_hits && hits.len() < cap {
                    hits.push(id);
                }
                if fold {
                    fold_accept(index, id, &mut tail_buf, &mut tail_keys, tail_chunk);
                }
            }
        }
        bufs.push(tail_buf);
        chunk_keys.push(tail_keys);
    }
    let parts = if total <= cap as u64 {
        Some(FoldParts { bufs, chunk_keys })
    } else {
        None
    };
    // TEMP-MEASURE: census print (counts only).
    if std::env::var_os("FLOKI_CENSUS").is_some() {
        use std::sync::atomic::Ordering as Ord;
        eprintln!(
            "orcensus swept={} fallback={} skipped={}",
            OR_SWEPT.swap(0, Ord::Relaxed),
            OR_FALLBACK.swap(0, Ord::Relaxed),
            OR_SKIPPED.swap(0, Ord::Relaxed)
        );
    }
    (total, hits, parts)
}

/// Disjunction scan for [`scan_route`]: fused multi-literal sweep when
/// every child has a usable substr plan ([`scan_or_fused`]), else the
/// generic per-entry scan. Returns the exact total, hits truncated to
/// `cap`, and fused parts iff complete (`total <= cap`) — callers that
/// need parts only consume them in the complete regime.
fn scan_or_union(
    index: &Index,
    matcher: &Matcher,
    cap: usize,
    need_hits: bool,
) -> (u64, Vec<EntryId>, Option<FoldParts>) {
    let Matcher::Or(kids) = matcher else {
        let (t, h) = scan_entries_generic(index, matcher, cap);
        return (t, h, None);
    };
    let mut plans = Vec::with_capacity(kids.len());
    for k in kids {
        match ArenaPlan::for_matcher(k) {
            Some(p) if p.substr => plans.push(p),
            _ => {
                let (t, h) = scan_entries_generic(index, matcher, cap);
                return (t, h, None);
            }
        }
    }
    if plans.is_empty() {
        return (
            0,
            Vec::new(),
            Some(FoldParts {
                bufs: Vec::new(),
                chunk_keys: Vec::new(),
            }),
        );
    }
    let (total, hits, parts) = scan_or_fused(index, &plans, matcher, cap, need_hits);
    (total, hits, parts)
}

/// Byte size separating small scans (chunk count scales with threads for
/// minimal overhead) from big scans (fixed small chunks for L2-resident
/// sweep working sets). Measured crossover on 1M vs 8.5M indexes.
const BIG_SCAN_BYTES: usize = 64 << 20;
/// Sweep chunk size for big scans: L1-resident working sets win over
/// scheduling overhead at 8.5M (measured: 32 KiB beats 128 KiB/512 KiB/2 MiB
/// on dense sweeps; small scans use thread-scaled chunks instead).
const SWEEP_CHUNK_BYTES: usize = 32 << 10;

/// Split the monotonic arena prefix into chunks (boundaries snap up to name
/// starts so every in-name occurrence belongs to exactly one chunk).
/// Small scans use thread-scaled chunks (minimal overhead); big scans use
/// fixed small chunks (L2-resident sweep working sets). Shared by the
/// single- and multi-literal sweeps.
fn arena_chunks(index: &Index, prefix: usize, total_bytes: usize) -> Vec<ArenaChunk> {
    let threads = search_pool().current_num_threads().max(1);
    let nchunks = if total_bytes <= BIG_SCAN_BYTES {
        (threads * 4).max(1)
    } else {
        total_bytes.div_ceil(SWEEP_CHUNK_BYTES).max(1)
    };
    let mut chunks: Vec<ArenaChunk> = Vec::with_capacity(nchunks);
    let mut start_id: u32 = 0;
    let mut start_byte: usize = if prefix > 0 {
        index.entries[0].name_off as usize
    } else {
        0
    };
    for k in 1..=nchunks {
        let (end_id, end_byte) = if k == nchunks {
            (prefix as u32, total_bytes)
        } else {
            let b = total_bytes as u64 * k as u64 / nchunks as u64;
            let eid = index.entries[..prefix].partition_point(|e| (e.name_off as u64) < b) as u32;
            let ebyte = if (eid as usize) < prefix {
                index.entries[eid as usize].name_off as usize
            } else {
                total_bytes
            };
            (eid, ebyte)
        };
        if end_id > start_id && end_byte > start_byte {
            chunks.push(ArenaChunk {
                id_start: start_id,
                id_end: end_id,
                b_start: start_byte,
                b_end: end_byte,
            });
        }
        start_id = end_id;
        start_byte = end_byte;
    }
    chunks
}

/// One arena byte chunk: an id range tiling the monotonic prefix plus its
/// exact byte range (names tile it gaplessly, so every in-name occurrence
/// belongs to exactly one chunk).
struct ArenaChunk {
    id_start: u32,
    id_end: u32,
    b_start: usize,
    b_end: usize,
}

/// One chunk's scan output: match count, collected ids, folded-name buffer,
/// and its keys.
type ChunkScan = (u64, Vec<EntryId>, Vec<u8>, Vec<ArenaKey>);

/// Per-chunk match accumulator with the same atomic cap gating as
/// [`scan_loop`]: chunks stop collecting past `cap` (batched, uncontended),
/// `total` still counts every match.
struct ChunkAcc<'a> {
    count: u64,
    hits: Vec<EntryId>,
    collecting: bool,
    since_sync: u32,
    collected: &'a AtomicU64,
    cap: usize,
    record: bool,
    /// False once the global count reaches `cap`: further accepts are
    /// counted but neither collected nor folded (fused keys stay complete
    /// exactly when `total <= cap` — the only regime the fused page
    /// consumes: stopping implies every match was already folded, since
    /// each match is accepted exactly once and folds synchronously).
    folding: bool,
}

impl ChunkAcc<'_> {
    fn accept(&mut self, id: EntryId) {
        self.count += 1;
        if !self.record {
            // Still sync + observe the global stop so folding halts too.
            if self.cap > 0 {
                self.since_sync += 1;
                if self.since_sync >= 1024 {
                    let seen = self
                        .collected
                        .fetch_add(self.since_sync as u64, Ordering::Relaxed);
                    self.since_sync = 0;
                    if seen >= self.cap as u64 {
                        self.collecting = false;
                        self.folding = false;
                    }
                }
            }
            return;
        }
        if self.collecting {
            self.hits.push(id);
        }
        if self.cap > 0 {
            self.since_sync += 1;
            if self.since_sync >= 1024 {
                let seen = self
                    .collected
                    .fetch_add(self.since_sync as u64, Ordering::Relaxed);
                self.since_sync = 0;
                if seen >= self.cap as u64 {
                    self.collecting = false;
                }
            }
        }
    }

    fn flush(&mut self) {
        if self.since_sync > 0 {
            self.collected
                .fetch_add(self.since_sync as u64, Ordering::Relaxed);
            self.since_sync = 0;
        }
    }
}

/// Fold an accepted match's name into the chunk buffer (swept names are
/// still hot — nearly free) for the fused page path.
fn fold_accept(
    index: &Index,
    id: EntryId,
    buf: &mut Vec<u8>,
    keys: &mut Vec<ArenaKey>,
    chunk: u32,
) {
    if let Some(e) = index.entries.get(id as usize) {
        let start = buf.len() as u32;
        fold_bytes_into(index.name_bytes_of(e), buf);
        keys.push(ArenaKey {
            chunk,
            start,
            len: buf.len() as u32 - start,
            id,
        });
    }
}

/// Contiguous arena scan for matchers with a usable literal (see
/// [`ArenaPlan`]): one SIMD pass per chunk over the arena bytes, sweeping
/// ids in order to map each hit offset to its owner (bounds + tombstone
/// checked), verifying every candidate — pure-substring leaves directly,
/// every other shape with the full matcher (candidates are a superset of
/// true matches, so verdicts are identical to the per-entry scan at a
/// fraction of the memory traffic: no per-entry dereference, no dispatch
/// until a candidate). Chunks keep scan order, so truncation is the global
/// first-`cap`; ids past the monotonic prefix use the sequential per-entry
/// tail scan. Case-insensitive chunks containing non-ASCII bytes fall back
/// to per-entry evaluation (same verdicts, no silent misses).
///
/// Accepted matches also fold into per-chunk buffers ([`FoldParts`]) while
/// the names are still hot, so the page path never re-reads them cold —
/// except when `cap == 0` (the count path needs no page). When `need_hits`
/// is false, ids are counted (and folded) but not collected: the fused page
/// path orders from keys alone and never reads the hit list.
fn scan_arena(
    index: &Index,
    plan: &ArenaPlan,
    matcher: &Matcher,
    cap: usize,
    need_hits: bool,
) -> (u64, Vec<EntryId>, FoldParts) {
    let prefix = (index.arena_prefix_len as usize).min(index.entries.len());
    let arena = index.names.as_bytes();
    let nlen = plan.bytes.len();
    // Byte end of the monotonic prefix (names tile `[0, total)` exactly).
    let mut total_bytes = 0usize;
    if prefix > 0 {
        let e = &index.entries[prefix - 1];
        total_bytes = e.name_off as usize + e.name_len as usize;
    }
    // Byte-sized chunks (~128 KiB each, so the sweep working set stays
    // L2-resident): boundaries snap up to name starts so every in-name
    // occurrence belongs to exactly one chunk. factored for the fused
    // disjunction sweep below.
    let chunks = arena_chunks(index, prefix, total_bytes);
    let finder = memmem::Finder::new(&plan.bytes);
    let collected = AtomicU64::new(0);
    let fold = cap > 0;
    let partial: Vec<ChunkScan> = search_pool().install(|| {
        chunks
            .into_par_iter()
            .enumerate()
            .map(|(ci, chunk)| {
                let ci = ci as u32;
                let mut acc = ChunkAcc {
                    count: 0,
                    hits: Vec::new(),
                    collecting: need_hits && cap > 0,
                    since_sync: 0,
                    collected: &collected,
                    cap,
                    record: need_hits,
                    folding: fold,
                };
                let mut pcx = PathCtx::new();
                let mut buf = Vec::new();
                let mut keys = Vec::new();
                // Per-chunk scratch for the fused page path.
                let bytes = &arena[chunk.b_start..chunk.b_end];
                // Block presence aggregate over the chunk's 64 KiB
                // blocks (bitsets are L2-resident; no byte scan): skip
                // the chunk when the first byte cannot occur, and gate
                // case-insensitive sweeps on block ASCII-ness instead
                // of re-scanning bytes. Missing coverage (only a
                // corrupt/short bitset) keeps + falls back, never
                // skips.
                let (agg, covered) = chunk_mask(index, &chunk);
                // Skip the chunk when the sweep anchor cannot occur
                // (first byte for first-byte anchors). Missing coverage
                // keeps instead of skipping.
                let first_present = if plan.ci {
                    block_has(&agg, plan.a0) || block_has(&agg, plan.a1)
                } else {
                    block_has(&agg, plan.a0)
                };
                if covered && !first_present {
                    acc.flush();
                    return (acc.count, acc.hits, buf, keys);
                }
                if fold {
                    // Small fixed reserves: most queries accept little
                    // (no waste), busy ones skip the first realloc
                    // waves (no churn). After the skip above, so
                    // skipped chunks allocate nothing.
                    buf.reserve(4096);
                    keys.reserve(128);
                }
                let ascii_clean = covered && agg[2] == 0 && agg[3] == 0;
                // TEMP-MEASURE: sweep vs fallback chunk census.
                if std::env::var_os("FLOKI_CENSUS").is_some() {
                    use std::sync::atomic::Ordering as Ord;
                    if covered && !first_present {
                        ARENA_SKIPPED.fetch_add(1, Ord::Relaxed);
                    } else if plan.ci && !ascii_clean {
                        ARENA_FALLBACK.fetch_add(1, Ord::Relaxed);
                    } else {
                        ARENA_SWEPT.fetch_add(1, Ord::Relaxed);
                    }
                }
                if plan.ci && !ascii_clean {
                    // Non-ASCII chunk: per-entry evaluation (exact).
                    for id in chunk.id_start..chunk.id_end {
                        if eval_hit(index, matcher, id, &mut pcx) {
                            acc.accept(id);
                            if acc.folding {
                                fold_accept(index, id, &mut buf, &mut keys, ci);
                            }
                        }
                    }
                } else {
                    // Sweep: hit offsets ascend, so owner ids only move
                    // forward; contiguity makes the owner exact. Fully
                    // inlined per candidate (no helper calls in the hot
                    // path). The literal confirm runs BEFORE the owner map
                    // (it needs no owner): failures `continue` without
                    // touching the per-id dedupe, so a failed position can
                    // never hide a later true occurrence in the same name.
                    // Candidates are a superset of true matches either way,
                    // so verdicts are identical to the per-entry scan.
                    let mut last = EntryId::MAX;
                    let mut id = chunk.id_start;
                    if plan.ci {
                        for h in memchr::memchr2_iter(plan.a0, plan.a1, bytes) {
                            let abs = chunk.b_start + h;
                            let Some(start) = abs.checked_sub(plan.aoff) else {
                                continue;
                            };
                            let cand = arena.get(start..start + nlen).unwrap_or(b"");
                            if cand.len() < nlen {
                                continue;
                            }
                            let mut ok = true;
                            for (i, &b) in plan.bytes.iter().enumerate() {
                                if cand[i].to_ascii_lowercase() != b {
                                    ok = false;
                                    break;
                                }
                            }
                            if !ok {
                                continue;
                            }
                            while id + 1 < chunk.id_end
                                && index.entries[(id + 1) as usize].name_off as usize <= start
                            {
                                id += 1;
                            }
                            if id == last {
                                continue;
                            }
                            last = id;
                            let e = &index.entries[id as usize];
                            let end = e.name_off as usize + e.name_len as usize;
                            if start + nlen > end {
                                continue;
                            }
                            if e.flags & TOMBSTONE != 0 {
                                continue;
                            }
                            if !vol_enabled(index, id) {
                                continue;
                            }
                            if plan.substr {
                                acc.accept(id);
                            } else {
                                let mut path_cache = None;
                                if matcher.matches(
                                    index,
                                    id,
                                    e,
                                    index.name_bytes_of(e),
                                    &mut path_cache,
                                    &mut pcx,
                                ) {
                                    acc.accept(id);
                                } else {
                                    continue;
                                }
                            }
                            if acc.folding {
                                fold_accept(index, id, &mut buf, &mut keys, ci);
                            }
                        }
                    } else {
                        for h in finder.find_iter(bytes) {
                            let abs = chunk.b_start + h;
                            while id + 1 < chunk.id_end
                                && index.entries[(id + 1) as usize].name_off as usize <= abs
                            {
                                id += 1;
                            }
                            if id == last {
                                continue;
                            }
                            last = id;
                            let e = &index.entries[id as usize];
                            let end = e.name_off as usize + e.name_len as usize;
                            if abs + nlen > end {
                                continue;
                            }
                            if e.flags & TOMBSTONE != 0 {
                                continue;
                            }
                            if !vol_enabled(index, id) {
                                continue;
                            }
                            if plan.substr {
                                acc.accept(id);
                            } else {
                                let mut path_cache = None;
                                if matcher.matches(
                                    index,
                                    id,
                                    e,
                                    index.name_bytes_of(e),
                                    &mut path_cache,
                                    &mut pcx,
                                ) {
                                    acc.accept(id);
                                } else {
                                    continue;
                                }
                            }
                            if acc.folding {
                                fold_accept(index, id, &mut buf, &mut keys, ci);
                            }
                        }
                    }
                }
                acc.flush();
                (acc.count, acc.hits, buf, keys)
            })
            .collect()
    });
    let mut total: u64 = partial.iter().map(|(c, _, _, _)| *c).sum();
    let mut hits = Vec::new();
    let mut bufs = Vec::with_capacity(partial.len() + 1);
    let mut chunk_keys = Vec::with_capacity(partial.len() + 1);
    for (_, h, b, ks) in partial.into_iter() {
        if need_hits && hits.len() < cap {
            let room = cap - hits.len();
            hits.extend(h.iter().take(room).copied());
        }
        // Keys only stay complete while collection does (`total <= cap` —
        // the only regime the fused page consumes); the debug assert in
        // `page_from_parts` guards the contract.
        bufs.push(b);
        chunk_keys.push(ks);
    }
    // Tail: ids past the monotonic prefix (normally none) in scan order, so
    // the combined list stays the global first-`cap`.
    if prefix < index.entries.len() {
        let mut pcx = PathCtx::new();
        let mut tail_buf = Vec::new();
        let mut tail_keys = Vec::new();
        let tail_chunk = bufs.len() as u32;
        for i in prefix..index.entries.len() {
            let id = i as EntryId;
            if eval_hit(index, matcher, id, &mut pcx) {
                total += 1;
                if need_hits && hits.len() < cap {
                    hits.push(id);
                }
                if fold {
                    fold_accept(index, id, &mut tail_buf, &mut tail_keys, tail_chunk);
                }
            }
        }
        bufs.push(tail_buf);
        chunk_keys.push(tail_keys);
    }
    // TEMP-MEASURE: census print (counts only).
    if std::env::var_os("FLOKI_CENSUS").is_some() {
        use std::sync::atomic::Ordering as Ord;
        eprintln!(
            "census swept={} fallback={} skipped={}",
            ARENA_SWEPT.swap(0, Ord::Relaxed),
            ARENA_FALLBACK.swap(0, Ord::Relaxed),
            ARENA_SKIPPED.swap(0, Ord::Relaxed)
        );
    }
    (total, hits, FoldParts { bufs, chunk_keys })
}

/// Monomorphized scan driver: `pred` inlines into both the sequential loop
/// and the parallel chunk loop. Collection / cap semantics mirror
/// [`scan_entries_generic`] exactly — chunks stop collecting past `cap` via
/// a shared atomic (batched, uncontended), `collect` preserves chunk order
/// so truncation is the global first-`cap`, and `total` counts every match.
fn scan_loop<F>(index: &Index, cap: usize, pred: &F) -> (u64, Vec<EntryId>)
where
    F: Fn(&Index, EntryId, &Entry, &[u8], &mut PathCtx) -> bool + Sync,
{
    if index.entries.len() < PAR_THRESHOLD {
        let mut total = 0u64;
        let mut hits = Vec::new();
        let mut pcx = PathCtx::new();
        for (j, entry) in index.entries.iter().enumerate() {
            if entry.flags & TOMBSTONE != 0 {
                continue;
            }
            let id = j as EntryId;
            if !vol_enabled(index, id) {
                continue;
            }
            if pred(index, id, entry, index.name_bytes_of(entry), &mut pcx) {
                total += 1;
                if hits.len() < cap {
                    hits.push(id);
                }
            }
        }
        return (total, hits);
    }
    let collected = AtomicU64::new(0);
    let partial: Vec<(u64, Vec<EntryId>)> = search_pool().install(|| {
        index
            .entries
            .par_chunks(SCAN_CHUNK)
            .enumerate()
            .map(|(ci, chunk)| {
                let base = ci * SCAN_CHUNK;
                let mut count = 0u64;
                let mut hits = Vec::new();
                // One sibling cache for the chunk's consecutive entries.
                let mut pcx = PathCtx::new();
                let mut collecting = cap > 0;
                let mut since_sync = 0u32;
                for (j, entry) in chunk.iter().enumerate() {
                    if entry.flags & TOMBSTONE != 0 {
                        continue;
                    }
                    let id = (base + j) as EntryId;
                    if !vol_enabled(index, id) {
                        continue;
                    }
                    if pred(index, id, entry, index.name_bytes_of(entry), &mut pcx) {
                        count += 1;
                        if collecting {
                            hits.push(id);
                        }
                        if cap > 0 {
                            since_sync += 1;
                            if since_sync >= 1024 {
                                let seen =
                                    collected.fetch_add(since_sync as u64, Ordering::Relaxed);
                                since_sync = 0;
                                if seen >= cap as u64 {
                                    collecting = false;
                                }
                            }
                        }
                    }
                }
                if since_sync > 0 {
                    collected.fetch_add(since_sync as u64, Ordering::Relaxed);
                }
                (count, hits)
            })
            .collect()
    });
    let total: u64 = partial.iter().map(|(c, _)| *c).sum();
    let mut hits = Vec::new();
    for (_, h) in &partial {
        if hits.len() >= cap {
            break;
        }
        let room = cap - hits.len();
        hits.extend(h.iter().take(room).copied());
    }
    (total, hits)
}

/// Generic scan over [`Matcher::matches`] (per-entry dispatch); used for
/// compound matchers and path shapes without a tight loop.
fn scan_entries_generic(index: &Index, matcher: &Matcher, cap: usize) -> (u64, Vec<EntryId>) {
    if index.entries.len() < PAR_THRESHOLD {
        let mut total = 0u64;
        let mut hits = Vec::new();
        let mut pcx = PathCtx::new();
        for (j, entry) in index.entries.iter().enumerate() {
            if entry.flags & TOMBSTONE != 0 {
                continue;
            }
            let id = j as EntryId;
            if !vol_enabled(index, id) {
                continue;
            }
            let name = index.name_bytes_of(entry);
            let mut path_cache = None;
            if matcher.matches(index, id, entry, name, &mut path_cache, &mut pcx) {
                total += 1;
                if hits.len() < cap {
                    hits.push(id);
                }
            }
        }
        return (total, hits);
    }
    // Parallel over cache-friendly chunks on the dedicated search pool;
    // `collect` preserves chunk order so the truncation below is exactly the
    // global first-`cap`.
    let collected = AtomicU64::new(0);
    let partial: Vec<(u64, Vec<EntryId>)> = search_pool().install(|| {
        index
            .entries
            .par_chunks(SCAN_CHUNK)
            .enumerate()
            .map(|(ci, chunk)| {
                let base = ci * SCAN_CHUNK;
                let mut count = 0u64;
                let mut hits = Vec::new();
                // One sibling cache for the chunk's consecutive entries.
                let mut pcx = PathCtx::new();
                let mut collecting = cap > 0;
                let mut since_sync = 0u32;
                for (j, entry) in chunk.iter().enumerate() {
                    if entry.flags & TOMBSTONE != 0 {
                        continue;
                    }
                    let id = (base + j) as EntryId;
                    if !vol_enabled(index, id) {
                        continue;
                    }
                    let name = index.name_bytes_of(entry);
                    let mut path_cache = None;
                    if matcher.matches(index, id, entry, name, &mut path_cache, &mut pcx) {
                        count += 1;
                        if collecting {
                            hits.push(id);
                        }
                        if cap > 0 {
                            since_sync += 1;
                            if since_sync >= 1024 {
                                let seen =
                                    collected.fetch_add(since_sync as u64, Ordering::Relaxed);
                                since_sync = 0;
                                if seen >= cap as u64 {
                                    collecting = false;
                                }
                            }
                        }
                    }
                }
                if since_sync > 0 {
                    collected.fetch_add(since_sync as u64, Ordering::Relaxed);
                }
                (count, hits)
            })
            .collect()
    });
    let total: u64 = partial.iter().map(|(c, _)| *c).sum();
    let mut hits = Vec::new();
    for (_, h) in &partial {
        if hits.len() >= cap {
            break;
        }
        let room = cap - hits.len();
        hits.extend(h.iter().take(room).copied());
    }
    (total, hits)
}

/// Phase 2 for broad queries (`total > PHASE1_CAP`): two-pointer merge over
/// the sorted `by_name` snapshot and the sorted `pending` list on
/// `(fold(name), id)` (both directions exact-reversed for `desc`),
/// tombstones skipped on the fly, stopping after `offset + max` hits.
/// Caller guarantees `max != usize::MAX` and a shallow page.
///
/// Returns `None` when the walk exceeds `budget` matcher evaluations before
/// filling the page (clustered matches degenerate into a full random-order
/// pass): the caller falls back to fold-keys + windowed page instead. The
/// mid band probes with a small budget (uniform queries finish in hundreds
/// of steps; clustered ones bail fast onto the exact window path); huge
/// totals keep the full budget for legitimate deep walks.
fn search_name_merge(
    index: &Index,
    matcher: &Matcher,
    total: u64,
    offset: usize,
    max: usize,
    desc: bool,
    budget: usize,
) -> Option<SearchResult> {
    let need = offset.saturating_add(max);
    if need == 0 {
        return Some(SearchResult {
            hits: Vec::new(),
            total,
        });
    }
    let by_name = index.by_name();
    let pending = index.pending();
    // `at` maps a walk-order position to the id (reversed for Desc).
    let at = |ids: &[EntryId], k: usize| -> EntryId {
        if desc {
            ids[ids.len() - 1 - k]
        } else {
            ids[k]
        }
    };
    let is_dead = |id: EntryId| -> bool {
        index
            .entries
            .get(id as usize)
            .is_none_or(|e| e.flags & TOMBSTONE != 0)
            || !vol_enabled(index, id)
    };
    let mut i = 0usize;
    let mut j = 0usize;
    // Cached head keys; recomputed only when their cursor advances, so each
    // list element is folded O(1) times over the whole walk.
    let mut key_a: Option<(EntryId, String)> = None;
    let mut key_b: Option<(EntryId, String)> = None;
    let mut hits = Vec::new();
    let mut seen = 0usize;
    let mut steps = 0usize;
    // Zero-hit trip point for this walk: past it, the page cannot fill even
    // at a 100% hit rate from there (`budget - need`), so bail to the exact
    // window fallback — floored so pages deeper than the budget still bail
    // early instead of burning the whole budget.
    let zero_abort = budget.saturating_sub(need).max(MERGE_ZERO_HIT_ABORT);
    // One sibling cache for the whole walk (name-ordered, so it rarely hits
    // — the shared chain memo does the heavy lifting here).
    let mut pcx = PathCtx::new();
    loop {
        if key_a.is_none() {
            while i < by_name.len() && is_dead(at(by_name, i)) {
                i += 1;
            }
            if i < by_name.len() {
                let id = at(by_name, i);
                key_a = Some((id, fold(index.name(id).unwrap_or(""))));
            }
        }
        if key_b.is_none() {
            while j < pending.len() && is_dead(at(pending, j)) {
                j += 1;
            }
            if j < pending.len() {
                let id = at(pending, j);
                key_b = Some((id, fold(index.name(id).unwrap_or(""))));
            }
        }
        let take_a = match (&key_a, &key_b) {
            (None, None) => break,
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (Some((ida, fa)), Some((idb, fb))) => {
                if desc {
                    (fa, ida) >= (fb, idb)
                } else {
                    (fa, ida) <= (fb, idb)
                }
            }
        };
        if steps >= budget {
            return None;
        }
        if steps >= zero_abort && seen == 0 {
            return None;
        }
        steps += 1;
        let id = if take_a {
            let (id, _) = key_a.take().expect("head cached");
            i += 1;
            id
        } else {
            let (id, _) = key_b.take().expect("head cached");
            j += 1;
            id
        };
        if eval_hit(index, matcher, id, &mut pcx) {
            if seen >= offset && hits.len() < max {
                hits.push(Hit {
                    id,
                    vol: index.volume_of(id).unwrap_or(0),
                });
            }
            seen += 1;
            if seen >= need {
                break;
            }
        }
    }
    Some(SearchResult { hits, total })
}

/// Count all matches (no sorting or paging): phase 1 only — one sequential
/// pass, no hits retained.
pub fn count(index: &Index, q: &Query, prev: Option<&[Hit]>) -> u64 {
    let matcher = Matcher::build(&q.root);
    match prev {
        Some(hits) => {
            if hits.len() < PAR_THRESHOLD {
                let mut pcx = PathCtx::new();
                hits.iter()
                    .filter(|h| eval_hit(index, &matcher, h.id, &mut pcx))
                    .count() as u64
            } else {
                // Chunked (not per element): one scratch context per chunk,
                // results concatenated in chunk order, on the search pool.
                search_pool().install(|| {
                    hits.par_chunks(1024)
                        .map(|chunk| {
                            let mut pcx = PathCtx::new();
                            chunk
                                .iter()
                                .filter(|h| eval_hit(index, &matcher, h.id, &mut pcx))
                                .count()
                        })
                        .sum::<usize>() as u64
                })
            }
        }
        None => scan_entries(index, &matcher, 0).0,
    }
}

/// Can `prev` (results of `prev_q`) narrow the search for `next_q`?
///
/// True exactly when the new query string starts with the previous one and
/// neither query had OR / NOT. `next` containing `"`, `(`, `<` or `|` is
/// conservatively rejected: quotes and groups can delete or rewrite an
/// earlier term (so a string prefix need not be a semantic subset), and `|`
/// always broadens.
#[must_use]
pub fn prev_reusable(prev_q: &Query, next_q: &Query) -> bool {
    next_q.raw.starts_with(prev_q.raw.as_str())
        && !prev_q.has_or()
        && !prev_q.has_not()
        && !next_q.has_or()
        && !next_q.raw.contains(['"', '(', '<', '|'])
}

/// Every match of `q` (narrowed to `prev` when given), unsorted. For callers
/// that order matches by something the index does not hold (file times):
/// collect under the read lock, then key and sort off it.
#[must_use]
pub fn collect_matches(index: &Index, q: &Query, prev: Option<&[Hit]>) -> Vec<Hit> {
    collect(index, &Matcher::build(&q.root), prev)
}

/// Full paths of `hits`, in order. Each distinct parent folder is rebuilt
/// once and shared by its matching children, instead of one ancestor walk
/// per match (path sort over 200k+ matches spent seconds in `Index::path`).
#[must_use]
pub fn paths_of(index: &Index, hits: &[Hit]) -> Vec<String> {
    let parent_of = |h: &Hit| -> Option<EntryId> {
        let e = index.entry(h.id)?;
        if e.frn == e.parent_frn {
            return None;
        }
        let vol = index.volume_of(h.id)?;
        index.lookup(vol, e.parent_frn).filter(|&p| p != h.id)
    };
    let parents: Vec<Option<EntryId>> = if hits.len() < PAR_THRESHOLD {
        hits.iter().map(parent_of).collect()
    } else {
        search_pool().install(|| hits.par_iter().map(parent_of).collect())
    };
    let mut dirs: Vec<EntryId> = parents.iter().flatten().copied().collect();
    dirs.sort_unstable();
    dirs.dedup();
    let dir_paths: Vec<String> = if dirs.len() < PAR_THRESHOLD {
        dirs.iter().map(|&d| index.path(d)).collect()
    } else {
        search_pool().install(|| dirs.par_iter().map(|&d| index.path(d)).collect())
    };
    hits.iter()
        .zip(parents)
        .map(|(h, parent)| {
            let name = index.name(h.id).unwrap_or("");
            match parent.and_then(|p| dirs.binary_search(&p).ok()) {
                Some(i) if !name.is_empty() => {
                    let dir = &dir_paths[i];
                    if dir.ends_with('\\') {
                        format!("{dir}{name}")
                    } else {
                        format!("{dir}\\{name}")
                    }
                }
                _ => index.path(h.id),
            }
        })
        .collect()
}

fn collect(index: &Index, matcher: &Matcher, prev: Option<&[Hit]>) -> Vec<Hit> {
    match prev {
        Some(hits) => {
            if hits.len() < PAR_THRESHOLD {
                let mut pcx = PathCtx::new();
                hits.iter()
                    .filter_map(|h| {
                        if eval_hit(index, matcher, h.id, &mut pcx) {
                            Some(Hit {
                                id: h.id,
                                vol: index.volume_of(h.id).unwrap_or(h.vol),
                            })
                        } else {
                            None
                        }
                    })
                    .collect()
            } else {
                // Chunked (not per element): one scratch context per chunk,
                // results concatenated in chunk order, on the search pool.
                search_pool().install(|| {
                    hits.par_chunks(1024)
                        .map(|chunk| {
                            let mut pcx = PathCtx::new();
                            chunk
                                .iter()
                                .filter_map(|h| {
                                    if eval_hit(index, matcher, h.id, &mut pcx) {
                                        Some(Hit {
                                            id: h.id,
                                            vol: index.volume_of(h.id).unwrap_or(h.vol),
                                        })
                                    } else {
                                        None
                                    }
                                })
                                .collect::<Vec<Hit>>()
                        })
                        .collect::<Vec<Vec<Hit>>>()
                        .concat()
                })
            }
        }
        None => {
            if index.entries.len() < PAR_THRESHOLD {
                let mut pcx = PathCtx::new();
                (0..index.entries.len())
                    .filter_map(|i| {
                        let id = i as EntryId;
                        if eval_hit(index, matcher, id, &mut pcx) {
                            Some(Hit {
                                id,
                                vol: index.volume_of(id).unwrap_or(0),
                            })
                        } else {
                            None
                        }
                    })
                    .collect()
            } else {
                search_pool().install(|| {
                    (0..index.entries.len())
                        .into_par_iter()
                        .filter_map(|i| {
                            let mut pcx = PathCtx::new();
                            let id = i as EntryId;
                            if eval_hit(index, matcher, id, &mut pcx) {
                                Some(Hit {
                                    id,
                                    vol: index.volume_of(id).unwrap_or(0),
                                })
                            } else {
                                None
                            }
                        })
                        .collect()
                })
            }
        }
    }
}
/// True when `id`'s volume is search-visible (disabled volumes stay indexed
/// but never match; the volume table is tiny so this stays on every path).
#[inline]
fn vol_enabled(index: &Index, id: EntryId) -> bool {
    let vol = index.volume_of(id).unwrap_or(0) as usize;
    !index.volumes.get(vol).is_some_and(|v| !v.enabled)
}

fn eval_hit(index: &Index, matcher: &Matcher, id: EntryId, pcx: &mut PathCtx) -> bool {
    let Some(entry) = index.entries.get(id as usize) else {
        return false;
    };
    if entry.flags & TOMBSTONE != 0 {
        return false;
    }
    // Disabled volumes stay indexed but invisible: their entries never match.
    // (The scan fast paths repeat this check per-candidate so totals agree
    // with `collect` on every route.)
    if !vol_enabled(index, id) {
        return false;
    }
    // Bytes, not `&str`: the hot substring terms scan without paying UTF-8
    // validation per entry. Terms needing text re-derive it lazily.
    let name = index.name_bytes_of(entry);
    let mut path_cache: Option<String> = None;
    matcher.matches(index, id, entry, name, &mut path_cache, pcx)
}

/// Best-effort `&str` view of raw arena bytes (arena content appended from
/// `&str` is valid UTF-8; only a corrupt file can break that, and then the
/// name simply never matches text terms).
fn name_str(bytes: &[u8]) -> &str {
    std::str::from_utf8(bytes).unwrap_or("")
}

fn sort_by_name(index: &Index, matches: &mut [Hit], desc: bool) {
    // Schwartzian: one fold per hit, not one per comparison (a comparator
    // re-folds non-ASCII names on every comparison). Orders exactly like the
    // `by_name` snapshot key `(fold(name), id)`, fully reversed for Desc —
    // identical to the merge walk in both directions.
    let mut keyed: Vec<(String, Hit)> = matches
        .iter()
        .map(|&h| {
            let key = index
                .entries
                .get(h.id as usize)
                .map(|e| fold_bytes(index.name_bytes_of(e)))
                .unwrap_or_default();
            (key, h)
        })
        .collect();
    keyed.sort_by(|a, b| {
        let ord = a.0.cmp(&b.0).then(a.1.id.cmp(&b.1.id));
        if desc {
            ord.reverse()
        } else {
            ord
        }
    });
    for (slot, (_, h)) in keyed.into_iter().enumerate() {
        matches[slot] = h;
    }
}

/// Per-search compiled predicate tree (owns folded needles / regexes).
enum Matcher {
    True,
    And(Vec<Matcher>),
    Or(Vec<Matcher>),
    Not(Box<Matcher>),
    /// Case-insensitive substring; needle already folded (`ascii`
    /// precomputed so the hot path never rescans the needle).
    SubFold {
        needle: String,
        ascii: bool,
    },
    /// Case-sensitive substring: folded prefilter first (fast path), then an
    /// exact-case confirm on the raw name for candidates only.
    SubRaw {
        needle: String,
        pre: RequiredLit,
    },
    /// Whole-name glob compiled to regex, with a required-literal
    /// prefilter (`None` when the pattern has no literal run worth testing).
    Glob {
        regex: Regex,
        pre: Option<RequiredLit>,
    },
    /// Whole-name glob with an exact affix shape (no regex engine on the hot
    /// path): the byte checks below are exactly the anchored glob semantics.
    /// `regex` runs only for names with non-ASCII bytes in a compared region
    /// (byte folding is exact on pure-ASCII regions).
    GlobFast {
        shape: GlobShape,
        lit: RequiredLit,
        lit2: Option<RequiredLit>,
        regex: Regex,
    },
    /// Whole-name equality; `folded` is `Some` when case-insensitive.
    Whole {
        raw: String,
        folded: Option<String>,
    },
    /// Regex on the name, with a heuristically extracted required-literal
    /// prefilter (`None` when the pattern yields no safe literal).
    RegexMatch {
        regex: Regex,
        pre: Option<RequiredLit>,
    },
    /// Regex with an exact affix shape (no engine on the hot path): the byte
    /// checks below decide the match exactly for newline-free names; any
    /// `\n` in the name (`.` never spans it) or non-ASCII byte in a compared
    /// region falls back to `regex`, which is exact. `guard_nl` selects the
    /// newline fallback (needed exactly when the shape consumed a `.*` or a
    /// `$` anchor whose meaning depends on it).
    RegexAffix {
        kind: RegexFastKind,
        guard_nl: bool,
        regex: Regex,
        case_sensitive: bool,
    },
    /// Lowercased extension list.
    Ext(Vec<String>),
    /// Substring on the rebuilt path; `needle` folded unless `case`. `segs`
    /// are the per-component required literals (the whole needle when it has
    /// no separator; drive letter and empties dropped otherwise) tested via
    /// the memoized ancestor walk; separator needles additionally confirm
    /// against the full rebuilt path. `memo` is one chain-verdict table per
    /// term per query, sized on first use.
    PathSub {
        needle: String,
        case: bool,
        segs: Vec<RequiredLit>,
        has_separator: bool,
        memo: OnceLock<PathMemo>,
    },
    IsDir,
    IsFile,
}

impl Matcher {
    fn build(node: &Node) -> Self {
        match node {
            Node::MatchAll => Matcher::True,
            Node::And(v) => {
                let mut kids: Vec<Matcher> = v.iter().map(Matcher::build).collect();
                // Cheap name-only terms first so expensive glob / regex /
                // path terms (and path rebuilds) run only for entries that
                // pass the substring pre-filter. Pure reorder of a
                // conjunction: semantics unchanged.
                kids.sort_by_key(Matcher::cost);
                Matcher::And(kids)
            }
            Node::Or(v) => Matcher::Or(v.iter().map(Matcher::build).collect()),
            Node::Not(n) => Matcher::Not(Box::new(Matcher::build(n))),
            Node::Term(t) => match &t.kind {
                TermKind::Substring(s) if s.is_empty() => Matcher::True,
                TermKind::Substring(s) => {
                    if t.case_sensitive {
                        Matcher::SubRaw {
                            pre: RequiredLit::new(s, false),
                            needle: s.clone(),
                        }
                    } else {
                        let folded = fold(s);
                        Matcher::SubFold {
                            ascii: folded.is_ascii(),
                            needle: folded,
                        }
                    }
                }
                TermKind::Glob(p) => {
                    if !p.is_empty() && p.bytes().all(|b| b == b'*') {
                        // All-star glob (`*`, `**`, ...) matches everything.
                        Matcher::True
                    } else if let Some((shape, lit, lit2)) = analyze_glob(p) {
                        Matcher::GlobFast {
                            shape,
                            lit: RequiredLit::new(lit, t.case_sensitive),
                            lit2: lit2.map(|s| RequiredLit::new(s, t.case_sensitive)),
                            regex: glob_regex(p, t.case_sensitive),
                        }
                    } else {
                        Matcher::Glob {
                            pre: glob_required_literal(p, t.case_sensitive),
                            regex: glob_regex(p, t.case_sensitive),
                        }
                    }
                }
                TermKind::WholeName(s) => {
                    if t.case_sensitive {
                        Matcher::Whole {
                            raw: s.clone(),
                            folded: None,
                        }
                    } else {
                        Matcher::Whole {
                            raw: s.clone(),
                            folded: Some(fold(s)),
                        }
                    }
                }
                TermKind::Regex { pattern, compiled } => {
                    if let Some((kind, guard_nl)) = analyze_regex_fast(pattern, t.case_sensitive) {
                        Matcher::RegexAffix {
                            kind,
                            guard_nl,
                            regex: compiled.clone(),
                            case_sensitive: t.case_sensitive,
                        }
                    } else {
                        Matcher::RegexMatch {
                            pre: regex_required_literal(pattern, t.case_sensitive),
                            regex: compiled.clone(),
                        }
                    }
                }
                TermKind::Ext(list) => Matcher::Ext(list.clone()),
                TermKind::Path(s) => {
                    let has_separator = s.contains(['\\', '/', ':']);
                    // Per-component required literals: the whole needle when
                    // separator-free, else one per segment (empties and a
                    // leading `X:` drive prefix dropped — the full-path
                    // confirm below stays exact).
                    let mut segs: Vec<RequiredLit> = Vec::new();
                    if has_separator {
                        let mut parts: Vec<&str> = s
                            .split(['\\', '/', ':'])
                            .filter(|p| !p.is_empty())
                            .collect();
                        let nb = s.as_bytes();
                        if nb.len() >= 2
                            && nb[1] == b':'
                            && nb[0].is_ascii_alphabetic()
                            && !parts.is_empty()
                        {
                            parts.remove(0);
                        }
                        segs.extend(parts.iter().map(|p| RequiredLit::new(p, t.case_sensitive)));
                    } else {
                        segs.push(RequiredLit::new(s, t.case_sensitive));
                    }
                    Matcher::PathSub {
                        needle: if t.case_sensitive { s.clone() } else { fold(s) },
                        case: t.case_sensitive,
                        segs,
                        has_separator,
                        memo: OnceLock::new(),
                    }
                }
                TermKind::IsDir => Matcher::IsDir,
                TermKind::IsFile => Matcher::IsFile,
            },
        }
    }

    /// Evaluation cost class for `And` ordering (lower runs first).
    fn cost(&self) -> u8 {
        match self {
            Matcher::True => 0,
            Matcher::SubFold { .. } | Matcher::SubRaw { .. } => 1,
            Matcher::Whole { .. } => 1,
            Matcher::IsDir | Matcher::IsFile => 1,
            Matcher::Ext(_) => 2,
            Matcher::Glob { .. } | Matcher::GlobFast { .. } => 3,
            Matcher::RegexMatch { .. } | Matcher::RegexAffix { .. } => 4,
            Matcher::PathSub { .. } => 5,
            Matcher::Not(n) => n.cost(),
            Matcher::And(v) | Matcher::Or(v) => v.iter().map(Matcher::cost).max().unwrap_or(0),
        }
    }

    fn matches(
        &self,
        index: &Index,
        id: EntryId,
        entry: &Entry,
        name: &[u8],
        path_cache: &mut Option<String>,
        pcx: &mut PathCtx,
    ) -> bool {
        match self {
            Matcher::True => true,
            Matcher::And(v) => v
                .iter()
                .all(|m| m.matches(index, id, entry, name, path_cache, pcx)),
            Matcher::Or(v) => v
                .iter()
                .any(|m| m.matches(index, id, entry, name, path_cache, pcx)),
            Matcher::Not(m) => !m.matches(index, id, entry, name, path_cache, pcx),
            Matcher::SubFold { needle, ascii } => {
                contains_insensitive_prefolded(name, needle.as_bytes(), *ascii)
            }
            Matcher::SubRaw { needle, pre } => {
                // Folded prefilter on the fast path; exact-case confirm on
                // the raw name for candidates only.
                pre.test(name) && memmem::find(name, needle.as_bytes()).is_some()
            }
            Matcher::Glob { regex, pre } => match pre {
                Some(lit) => lit.test(name) && regex.is_match(name_str(name)),
                None => regex.is_match(name_str(name)),
            },
            Matcher::GlobFast {
                shape,
                lit,
                lit2,
                regex,
            } => glob_fast_match(name, *shape, lit, lit2.as_ref(), regex),
            Matcher::Whole { raw, folded } => match folded {
                Some(f) => eq_insensitive(name_str(name), raw, f),
                None => name_str(name) == raw,
            },
            Matcher::RegexMatch { regex, pre } => match pre {
                Some(lit) => lit.test(name) && regex.is_match(name_str(name)),
                None => regex.is_match(name_str(name)),
            },
            Matcher::RegexAffix {
                kind,
                guard_nl,
                regex,
                case_sensitive,
            } => regex_fast_match(name, kind, *guard_nl, *case_sensitive, regex),
            Matcher::Ext(list) => ext_match(name, list),
            Matcher::PathSub {
                needle,
                case,
                segs,
                has_separator,
                memo,
            } => {
                // Every segment must match some component (necessary
                // condition) via the memoized ancestor walk. Separator-free
                // needles are decided by that alone; separator needles
                // confirm against the full rebuilt path (covers matches
                // spanning a `\` boundary and the drive prefix).
                let m = memo.get_or_init(|| PathMemo::new(index.entries.len()));
                for seg in segs {
                    if !chain_match(index, id, seg, m, pcx) {
                        return false;
                    }
                }
                if !has_separator {
                    return true;
                }
                let p = match path_cache {
                    Some(s) => s,
                    None => {
                        *path_cache = Some(index.path(id));
                        path_cache.as_ref().expect("just filled")
                    }
                };
                if *case {
                    p.contains(needle.as_str())
                } else {
                    contains_insensitive(p, needle)
                }
            }
            Matcher::IsDir => entry.flags & DIRECTORY != 0,
            Matcher::IsFile => entry.flags & DIRECTORY == 0,
        }
    }
}

/// Case-insensitive extension match over the raw name bytes (shared by the
/// generic evaluator and the monomorphized scan loop): ASCII extensions take
/// a per-query-string lowercase compare, non-ASCII folds the extension once.
fn ext_match(name: &[u8], list: &[String]) -> bool {
    match ext_bytes_of(name) {
        Some(e) if e.is_ascii() => list.iter().any(|x| {
            x.is_ascii()
                && e.len() == x.len()
                && e.iter()
                    .zip(x.bytes())
                    .all(|(a, b)| a.to_ascii_lowercase() == b)
        }),
        Some(e) => {
            let ef = fold(name_str(e));
            list.iter().any(|x| x == &ef)
        }
        None => false,
    }
}

/// Longest required literal run for a glob: our glob syntax has no escapes,
/// so `*`/`?` split literal runs and every other byte is literal. A match
/// must contain the run, so it prefilters safely.
fn glob_required_literal(pattern: &str, case_sensitive: bool) -> Option<RequiredLit> {
    let best = pattern.split(['*', '?']).max_by_key(|run| run.len())?;
    if best.is_empty() {
        return None;
    }
    Some(RequiredLit::new(best, case_sensitive))
}

/// Unescape a fully-literal run (`\x` punctuation → `x`); `None` if anything
/// is not a plain literal or escaped punctuation.
fn unescape_literal(run: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(run.len());
    let mut i = 0;
    while i < run.len() {
        let c = run[i];
        if c == b'\\' {
            match run.get(i + 1) {
                Some(nc) if !nc.is_ascii_alphanumeric() => {
                    out.push(*nc);
                    i += 2;
                }
                _ => return None,
            }
        } else if is_regex_meta(c) {
            return None;
        } else {
            out.push(c);
            i += 1;
        }
    }
    Some(out)
}

/// Parse a single `[...]` class at `pat[start]` (`[` unescaped): ASCII
/// ranges, singles and negation only. Returns ranges (folded domain is
/// applied by the caller via [`fold_lit`]), negation, and the index past
/// `]`. Bails on escapes of alphanumerics (classes/backrefs), POSIX groups,
/// nesting, non-ASCII members, or a missing `]`.
fn parse_ascii_class(pat: &[u8], start: usize) -> Option<(AsciiClass, usize)> {
    debug_assert_eq!(pat[start], b'[');
    let mut i = start + 1;
    let mut neg = false;
    if pat.get(i) == Some(&b'^') {
        neg = true;
        i += 1;
    }
    let mut ranges: Vec<(u8, u8)> = Vec::new();
    let mut pending: Option<u8> = None;
    let flush = |ranges: &mut Vec<(u8, u8)>, pending: &mut Option<u8>| {
        if let Some(c) = pending.take() {
            ranges.push((c, c));
        }
    };
    loop {
        let &c = pat.get(i)?;
        if c == b']' && !ranges.is_empty() || c == b']' && pending.is_some() {
            flush(&mut ranges, &mut pending);
            i += 1;
            break;
        }
        if c == b'\\' {
            match pat.get(i + 1) {
                Some(nc) if !nc.is_ascii_alphanumeric() => {
                    if !nc.is_ascii() {
                        return None;
                    }
                    // Escaped punctuation is a literal member.
                    if pending.is_some() {
                        // Could be a range end (`a\-z`?): keep it simple —
                        // a backslash inside a range position bails.
                        return None;
                    }
                    pending = Some(*nc);
                    i += 2;
                }
                _ => return None,
            }
            continue;
        }
        if !c.is_ascii() {
            return None;
        }
        if c == b'-' && pending.is_some() {
            // Range: need `lo-hi` with a plain char after.
            let lo = pending.take().expect("checked");
            match pat.get(i + 1) {
                Some(&hi) if hi != b']' && hi.is_ascii() && hi != b'\\' => {
                    if hi < lo {
                        return None; // invalid range; engine handles it
                    }
                    ranges.push((lo, hi));
                    i += 2;
                }
                _ => {
                    // Trailing/lone `-` is literal.
                    ranges.push((lo, lo));
                    ranges.push((b'-', b'-'));
                    i += 1;
                }
            }
            continue;
        }
        if c == b'[' {
            return None; // nesting / POSIX group: bail
        }
        if let Some(p) = pending.take() {
            ranges.push((p, p));
        }
        pending = Some(c);
        i += 1;
    }
    if ranges.is_empty() {
        return None;
    }
    Some(((ranges, neg), i))
}

/// Exact affix shape of a regex: byte checks that decide the match without
/// the engine (plus engine fallback, see [`regex_fast_match`]). All literals
/// stored folded unless case-sensitive.
#[derive(Clone, Debug)]
enum RegexFastKind {
    /// Unanchored full literal: substring.
    Substr(Vec<u8>),
    /// `^lit$`: whole-name equality.
    Whole(Vec<u8>),
    /// `^pre` (+ optional trailing `.*`): byte prefix (literal or class).
    Prefix(RegexPrefix),
    /// `lit$` (+ optional leading `.*`): byte suffix.
    Suffix(Vec<u8>),
    /// `^pre.*suf$`: byte prefix plus byte suffix with a length guard.
    PreSuf(RegexPrefix, Vec<u8>),
}

/// Prefix atom: literal run or single ASCII character class (ranges/singles/
/// negation, folded domain when case-insensitive).
#[derive(Clone, Debug)]
enum RegexPrefix {
    Lit(Vec<u8>),
    Class { ranges: Vec<(u8, u8)>, neg: bool },
}

/// Fold pattern literal bytes for a case-insensitive term (identity when
/// case-sensitive). Patterns are valid UTF-8 by construction.
fn fold_lit(bytes: &[u8], case_sensitive: bool) -> Vec<u8> {
    if case_sensitive {
        bytes.to_vec()
    } else {
        fold_lit_owned(bytes)
    }
}

fn fold_lit_owned(bytes: &[u8]) -> Vec<u8> {
    match String::from_utf8(bytes.to_vec()) {
        Ok(s) => fold(&s).into_bytes(),
        Err(_) => Vec::new(), // unreachable (patterns are valid UTF-8)
    }
}

/// Fold class ranges into the folded domain (identity when case-sensitive).
/// Bails (`None`) on any non-ASCII member.
fn fold_ranges(ranges: &[(u8, u8)], case_sensitive: bool) -> Option<Vec<(u8, u8)>> {
    if case_sensitive {
        return Some(ranges.to_vec());
    }
    let mut out = Vec::with_capacity(ranges.len());
    for &(lo, hi) in ranges {
        if !lo.is_ascii() || !hi.is_ascii() {
            return None;
        }
        let (flo, fhi) = (lo.to_ascii_lowercase(), hi.to_ascii_lowercase());
        if fhi < flo {
            return None; // folding inverted the range; engine handles it
        }
        out.push((flo, fhi));
    }
    Some(out)
}

/// Analyze a regex pattern (original text, no `(?i)` wrapper) into an exact
/// affix shape, or `None` for the engine path. Accepted shapes (anything else
/// falls back, safely):
/// - fully literal `lit` (+ optional `^`/`$`): Substr/Whole/Prefix/Suffix;
/// - `^PRE.*SUF$`, `^PRE.*`, `.*SUF$` where PRE is a literal run or a single
///   unquantified ASCII class and SUF is a literal run (either side may be
///   empty only in the prefix-/suffix-only forms).
///
/// Every accepted shape is exactly the anchored pattern (empty middle and
/// affix order cover all cases; doubtful shapes yield `None`).
///
/// Returns the shape plus the `guard_nl` flag (see [`regex_fast_match`]).
type RegexFastShape = (RegexFastKind, bool);

/// Parsed ASCII character class: ranges plus negation.
type AsciiClass = (Vec<(u8, u8)>, bool);

fn analyze_regex_fast(pattern: &str, case_sensitive: bool) -> Option<RegexFastShape> {
    let pat = pattern.as_bytes();
    let mut i = 0;
    let mut j = pat.len();
    // Leading `^` (byte 0: never escaped).
    let mut caret = false;
    if pat.first() == Some(&b'^') {
        caret = true;
        i = 1;
    }
    // Trailing `$` (escape-aware).
    let mut dollar = false;
    if j > i && pat[j - 1] == b'$' && !is_escaped(pat, j - 1) {
        j -= 1;
        dollar = true;
    }
    // Strip ONE leading `.*` (sound anchored or not: `^.*R` and `.*R` both
    // mean "R matches somewhere"). The `.` at body start is preceded by `^`
    // or nothing — never a backslash — so a byte check suffices.
    let mut led_dotstar = false;
    if j - i >= 2 && pat[i] == b'.' && pat[i + 1] == b'*' {
        i += 2;
        led_dotstar = true;
    }
    // Strip ONE trailing `.*`, but ONLY when fully unanchored: `R.*` with no
    // `^`/`$` means "R matches somewhere" (the tail is vacuous for
    // existence). With either anchor the order constraint is undecomposable
    // (`ab.*$` is not a suffix check), so leave it for the fallback. The
    // `.` is escape-checked since a mid-pattern backslash can precede it.
    if !caret
        && !dollar
        && j - i >= 2
        && pat[j - 2] == b'.'
        && pat[j - 1] == b'*'
        && !is_escaped(pat, j - 2)
    {
        j -= 2;
    }
    // `guard_nl`: newline in the name forces the engine (`.` never spans it).
    // Needed exactly when a `.*` was consumed or `$` anchors: all other
    // shapes are provably newline-independent.
    let guard_nl = led_dotstar || dollar;
    if i >= j {
        // Empty body: `^$` (both anchors) matches only ""; `^`, `$` or `.*`
        // alone match everything; `^.*$` needs the guard.
        if caret && dollar {
            return Some((RegexFastKind::Whole(Vec::new()), guard_nl));
        }
        if dollar && led_dotstar {
            return Some((RegexFastKind::Suffix(Vec::new()), true));
        }
        return None; // `^`, `$`, `.*`, `^.*` — engine path (all rare)
    }
    let body = &pat[i..j];
    // Fully literal body (escape-aware unescape)?
    if let Some(lit) = unescape_literal(body) {
        let lit = fold_lit(&lit, case_sensitive);
        let kind = match (caret && !led_dotstar, dollar) {
            (true, true) => RegexFastKind::Whole(lit),
            (true, false) => RegexFastKind::Prefix(RegexPrefix::Lit(lit)),
            (false, true) => RegexFastKind::Suffix(lit),
            (false, false) => RegexFastKind::Substr(lit),
        };
        // Whole("") can't arise (empty body handled above); Substr("") can't
        // either. Prefix/Suffix with empty lit are vacuous-but-harmless...
        // actually an empty prefix/suffix literal matches trivially — but the
        // shape still needs the guard decision right. Keep, guard as computed.
        return Some((kind, guard_nl));
    }
    // Affix-with-`.*` shapes need a start anchor (elsewhere the order
    // constraint is undecomposable).
    if !caret || led_dotstar {
        return None;
    }
    // Prefix atom: literal run or single class, then literal `.*`.
    enum Pre {
        DoneLit(Vec<u8>),
        DoneClass(Vec<(u8, u8)>, bool),
    }
    let (pre, rest) = if body.first() == Some(&b'[') {
        let ((ranges, neg), k) = parse_ascii_class(body, 0)?;
        let ranges = fold_ranges(&ranges, case_sensitive)?;
        (Pre::DoneClass(ranges, neg), &body[k..])
    } else {
        // Literal run; stop AT (not consuming) the first meta/quantifier,
        // and bail outright on a quantifier (optional char breaks fixedness).
        let mut k = 0;
        let mut run = Vec::new();
        let mut ok = true;
        while k < body.len() {
            let c = body[k];
            if c == b'\\' {
                match body.get(k + 1) {
                    Some(nc) if !nc.is_ascii_alphanumeric() => {
                        run.push(*nc);
                        k += 2;
                    }
                    _ => {
                        ok = false;
                        break;
                    }
                }
            } else if is_regex_meta(c) {
                break;
            } else {
                run.push(c);
                k += 1;
            }
        }
        if !ok || run.is_empty() {
            return None;
        }
        // A quantifier met walking left... rather: the stopping char decides.
        if k < body.len() && matches!(body[k], b'*' | b'?' | b'+' | b'{') {
            return None; // quantified prefix char: not fixed
        }
        // Non-ASCII in a case-sensitive literal run is fine (exact bytes),
        // but keep the from_utf8 safety net via fold_lit on &str below.
        (Pre::DoneLit(run), &body[k..])
    };
    // Rest must be exactly `.*` + fully-literal tail (possibly empty). The
    // `.` here follows a literal run or `]` — never a backslash — so a byte
    // check is exact (no escape analysis needed).
    let tail = if rest.len() >= 2 && rest[0] == b'.' && rest[1] == b'*' {
        &rest[2..]
    } else if rest.is_empty() && !dollar {
        // No `.*`: prefix-only shape (caret guaranteed above; `$` was
        // stripped, so rest-empty means end of pattern). Unreachable in
        // practice (a fully consumed run means fully-literal body, caught
        // earlier) — kept as a defensive exact case.
        match pre {
            Pre::DoneLit(run) => {
                let lit = fold_lit(&run, case_sensitive);
                let pre = RegexPrefix::Lit(lit);
                return Some((RegexFastKind::Prefix(pre), guard_nl));
            }
            Pre::DoneClass(ranges, neg) => {
                return Some((
                    RegexFastKind::Prefix(RegexPrefix::Class { ranges, neg }),
                    guard_nl,
                ));
            }
        }
    } else {
        return None;
    };
    let lit = unescape_literal(tail)?;
    if tail.is_empty() {
        // `^pre.*$`: prefix check (+ guard iff `$`: the `.*$` tail can only
        // be vacuous on newline-free names; without `$` the trailing `.*`
        // is vacuous outright).
        let pre = match pre {
            Pre::DoneLit(run) => RegexPrefix::Lit(fold_lit(&run, case_sensitive)),
            Pre::DoneClass(ranges, neg) => RegexPrefix::Class { ranges, neg },
        };
        return Some((RegexFastKind::Prefix(pre), dollar));
    }
    // Non-empty tail without `$` is undecomposable (`^pre.*suf` needs the
    // suffix anchored; otherwise the order constraint can't be checked).
    if !dollar {
        return None;
    }
    let suf = fold_lit(&lit, case_sensitive);
    let pre = match pre {
        Pre::DoneLit(run) => RegexPrefix::Lit(fold_lit(&run, case_sensitive)),
        Pre::DoneClass(ranges, neg) => RegexPrefix::Class { ranges, neg },
    };
    Some((RegexFastKind::PreSuf(pre, suf), true))
}

/// Class membership of one byte on the folded domain.
fn class_contains(ranges: &[(u8, u8)], neg: bool, b_folded: u8) -> bool {
    let hit = ranges
        .iter()
        .any(|&(lo, hi)| b_folded >= lo && b_folded <= hi);
    hit != neg
}

/// Exact regex-affix match without the engine. `guard_nl` forces the engine
/// on any `\n` in the name (`.` never spans it); any non-ASCII byte in a
/// compared region likewise falls back (byte folding is exact only on ASCII,
/// and the analyzer guarantees ASCII-only literals/classes).
fn regex_fast_match(
    name: &[u8],
    kind: &RegexFastKind,
    guard_nl: bool,
    case_sensitive: bool,
    regex: &Regex,
) -> bool {
    let engine = || regex.is_match(name_str(name));
    if guard_nl && name.contains(&b'\n') {
        return engine();
    }
    // Fold one byte for comparison (ASCII-only path; non-ASCII bails out).
    let fold_byte = |b: u8| -> Option<u8> {
        if b >= 0x80 {
            return None;
        }
        Some(if case_sensitive {
            b
        } else {
            b.to_ascii_lowercase()
        })
    };
    let eq_lit = |hay: &[u8], lit: &[u8]| -> Option<bool> {
        if hay.len() != lit.len() {
            return Some(false);
        }
        for (&a, &b) in hay.iter().zip(lit.iter()) {
            if a >= 0x80 || b >= 0x80 {
                return None;
            }
            let a = if case_sensitive {
                a
            } else {
                a.to_ascii_lowercase()
            };
            if a != b {
                return Some(false);
            }
        }
        Some(true)
    };
    let prefix_ok = |name: &[u8], pre: &RegexPrefix| -> Option<bool> {
        match pre {
            RegexPrefix::Lit(lit) => {
                if name.len() < lit.len() {
                    return Some(false);
                }
                eq_lit(&name[..lit.len()], lit)
            }
            RegexPrefix::Class { ranges, neg } => {
                let &b0 = name.first()?;
                let fb = fold_byte(b0)?;
                Some(class_contains(ranges, *neg, fb))
            }
        }
    };
    match kind {
        RegexFastKind::Substr(lit) => {
            // No anchors/dots involved: pure substring, exact in all cases.
            if case_sensitive {
                memmem::find(name, lit).is_some()
            } else {
                contains_insensitive_prefolded(name, lit, lit.is_ascii())
            }
        }
        RegexFastKind::Whole(lit) => {
            if name.len() != lit.len() {
                return false;
            }
            match eq_lit(name, lit) {
                Some(eq) => eq,
                None => engine(),
            }
        }
        RegexFastKind::Prefix(pre) => match prefix_ok(name, pre) {
            Some(ok) => ok,
            None => engine(),
        },
        RegexFastKind::Suffix(lit) => {
            if name.len() < lit.len() {
                return false;
            }
            match eq_lit(&name[name.len() - lit.len()..], lit) {
                Some(ok) => ok,
                None => engine(),
            }
        }
        RegexFastKind::PreSuf(pre, suf) => {
            if name.len() < prefix_len(pre) + suf.len() {
                return false;
            }
            let pre_ok = prefix_ok(name, pre);
            // Suffix region (non-overlapping by the length guard above).
            let suf_ok = eq_lit(&name[name.len() - suf.len()..], suf);
            match (pre_ok, suf_ok) {
                (Some(true), Some(true)) => true,
                (Some(_), Some(_)) => false,
                _ => engine(),
            }
        }
    }
}

/// Folded length of a prefix atom (literal byte length; classes match exactly
/// one byte on the ASCII fast path).
fn prefix_len(pre: &RegexPrefix) -> usize {
    match pre {
        RegexPrefix::Lit(lit) => lit.len(),
        // A class consumes exactly one byte on the fast path (non-ASCII
        // first bytes bail to the engine before measuring).
        RegexPrefix::Class { .. } => 1,
    }
}

/// Exact affix shape of a glob with no `?` and stars only in affix position.
/// Byte indices are safe: `*` is ASCII so splits never cut a multi-byte char.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum GlobShape {
    /// No wildcards: whole-name equality.
    Whole,
    /// `lit*`: byte prefix.
    Prefix,
    /// `*lit`: byte suffix.
    Suffix,
    /// `*lit*`: byte substring (same as the required literal, exact).
    Substr,
    /// `pre*suf`: byte prefix plus byte suffix with a length guard.
    PreSuffix,
}

/// Analyze a glob into an exact affix shape, or `None` for the regex-engine
/// path (`?` anywhere, interior `*` runs, or lone stars which the caller maps
/// to match-all). Returns the shape plus literal run(s).
fn analyze_glob(pattern: &str) -> Option<(GlobShape, &str, Option<&str>)> {
    if pattern.contains('?') {
        return None;
    }
    let stars = pattern.as_bytes().iter().filter(|&&b| b == b'*').count();
    match stars {
        0 => Some((GlobShape::Whole, pattern, None)),
        1 => {
            if pattern.len() == 1 {
                return None; // lone `*`: match-all, handled at build
            }
            if let Some(lit) = pattern.strip_prefix('*') {
                Some((GlobShape::Suffix, lit, None))
            } else if let Some(lit) = pattern.strip_suffix('*') {
                Some((GlobShape::Prefix, lit, None))
            } else {
                let bar = pattern.find('*').expect("one star");
                Some((
                    GlobShape::PreSuffix,
                    &pattern[..bar],
                    Some(&pattern[bar + 1..]),
                ))
            }
        }
        2 if pattern.starts_with('*') && pattern.ends_with('*') => {
            let lit = &pattern[1..pattern.len() - 1];
            if lit.is_empty() {
                return None; // `**`: match-all, handled at build
            }
            Some((GlobShape::Substr, lit, None))
        }
        _ => None,
    }
}

/// Byte-wise folded equality over two equal-length slices: `Ok(equal)` when
/// every compared byte pair is ASCII (exact under folding); `Err(())` if any
/// byte on either side is non-ASCII (caller runs the regex engine instead).
/// `folded` selects lowercase-compare (case-insensitive terms, literal
/// already folded) vs exact compare.
fn eq_folded_region(hay: &[u8], lit: &[u8], folded: bool) -> Result<bool, ()> {
    debug_assert_eq!(hay.len(), lit.len());
    for (&a, &b) in hay.iter().zip(lit.iter()) {
        if a >= 0x80 || b >= 0x80 {
            return Err(());
        }
        let eq = if folded {
            a.to_ascii_lowercase() == b
        } else {
            a == b
        };
        if !eq {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Exact whole-name glob match without the regex engine. Suffix/prefix/whole
/// shapes compare folded bytes on pure-ASCII regions (a non-ASCII byte
/// anywhere compared falls back to `regex`, which is exact); substring shape
/// reuses the required literal (exact in all cases). Star shapes (`*` →
/// `.*`, which never spans `\n`) additionally fall back on any `\n` in the
/// name; whole-name equality has no `.` in its regex and needs no guard.
fn glob_fast_match(
    name: &[u8],
    shape: GlobShape,
    lit: &RequiredLit,
    lit2: Option<&RequiredLit>,
    regex: &Regex,
) -> bool {
    let engine = || regex.is_match(name_str(name));
    // `*` compiles to `.*`, which never spans `\n`: any affix verdict on a
    // newline-bearing name must come from the engine instead.
    let has_newline = || name.contains(&b'\n');
    match shape {
        GlobShape::Substr => {
            if has_newline() {
                return engine();
            }
            lit.test(name)
        }
        GlobShape::Whole => {
            if name.len() != lit.bytes.len() {
                return false;
            }
            match eq_folded_region(name, &lit.bytes, lit.folded) {
                Ok(eq) => eq,
                Err(()) => engine(),
            }
        }
        GlobShape::Prefix => {
            if has_newline() {
                return engine();
            }
            if name.len() < lit.bytes.len() {
                return false;
            }
            match eq_folded_region(&name[..lit.bytes.len()], &lit.bytes, lit.folded) {
                Ok(eq) => eq,
                Err(()) => engine(),
            }
        }
        GlobShape::Suffix => {
            if has_newline() {
                return engine();
            }
            if name.len() < lit.bytes.len() {
                return false;
            }
            match eq_folded_region(
                &name[name.len() - lit.bytes.len()..],
                &lit.bytes,
                lit.folded,
            ) {
                Ok(eq) => eq,
                Err(()) => engine(),
            }
        }
        GlobShape::PreSuffix => {
            if has_newline() {
                return engine();
            }
            let suf = lit2.map(|l| l.bytes.as_slice()).unwrap_or(b"");
            let pre = lit.bytes.as_slice();
            if name.len() < pre.len() + suf.len() {
                return false;
            }
            let pre_eq = eq_folded_region(&name[..pre.len()], pre, lit.folded);
            let suf_eq = eq_folded_region(&name[name.len() - suf.len()..], suf, lit.folded);
            match (pre_eq, suf_eq) {
                (Ok(true), Ok(true)) => true,
                (Ok(_), Ok(_)) => false,
                _ => engine(),
            }
        }
    }
}

/// True for regex metacharacters (all ASCII, so runs never split a
/// multi-byte char).
fn is_regex_meta(c: u8) -> bool {
    matches!(
        c,
        b'.' | b'*' | b'+' | b'?' | b'(' | b')' | b'|' | b'[' | b']' | b'{' | b'}' | b'^' | b'$'
    )
}

/// True when `pat[pos]` is escaped (odd run of preceding backslashes).
fn is_escaped(pat: &[u8], pos: usize) -> bool {
    let mut n = 0;
    let mut k = pos;
    while k > 0 && pat[k - 1] == b'\\' {
        n += 1;
        k -= 1;
    }
    n % 2 == 1
}

/// Record `run` in `best` when longer.
fn emit_run(best: &mut Vec<u8>, run: &mut Vec<u8>) {
    if run.len() > best.len() {
        best.clear();
        best.extend_from_slice(run);
    }
    run.clear();
}

/// Literal prefix after `^` (called with the index just past it):
/// accumulates literal chars, unescaping `\x` punctuation; stops at any
/// metachar, class, group, alternation, or backslash-class. A trailing char
/// consumed by `*?+` is optional, not required, so it is dropped; `{` keeps
/// the run (a following count never un-requires it) and stops.
fn scan_fwd_literal(pat: &[u8], mut i: usize) -> Vec<u8> {
    let mut out = Vec::new();
    while i < pat.len() {
        let c = pat[i];
        if c == b'\\' {
            match pat.get(i + 1) {
                Some(nc) if !nc.is_ascii_alphanumeric() => {
                    out.push(*nc);
                    i += 2;
                }
                _ => break,
            }
        } else if is_regex_meta(c) {
            if matches!(c, b'*' | b'?' | b'+') {
                out.pop();
            }
            break;
        } else {
            out.push(c);
            i += 1;
        }
    }
    out
}

/// Mirror of [`scan_fwd_literal`] from the end (exclusive `end`): accumulates
/// literal chars right-to-left; an escaped metacharacter is still literal. A
/// quantifier met walking left invalidates the char to its right (already
/// accumulated): drop it and stop. `{`/`}` stop the scan (counts are not
/// literals; the run so far stays required).
fn scan_bwd_literal(pat: &[u8], end: usize) -> Vec<u8> {
    let mut rev = Vec::new();
    let mut i = end;
    while i > 0 {
        i -= 1;
        let c = pat[i];
        if c == b'\\' {
            // The char to the right was escaped by this backslash: literal
            // punctuation stays, a class character goes (with the run).
            match pat.get(i + 1) {
                Some(nc) if !nc.is_ascii_alphanumeric() => {}
                _ => {
                    rev.pop();
                    break;
                }
            }
        } else if is_regex_meta(c) {
            if is_escaped(pat, i) {
                rev.push(c);
            } else {
                if matches!(c, b'*' | b'?' | b'+') {
                    rev.pop();
                }
                break;
            }
        } else {
            rev.push(c);
        }
    }
    rev.reverse();
    rev
}

/// Longest required literal run for an anchor-free pattern WITHOUT groups
/// or alternation (bails on unescaped `(` or `|` — no single run is required
/// across a branch). Character-class contents are skipped (alternatives, not
/// literals); a quantifier emits the run minus its last char (the quantified
/// char is optional) and restarts.
fn unanchored_literal_run(pat: &[u8]) -> Option<Vec<u8>> {
    for (idx, &c) in pat.iter().enumerate() {
        if (c == b'(' || c == b'|') && !is_escaped(pat, idx) {
            return None;
        }
    }
    let mut best = Vec::new();
    let mut run = Vec::new();
    let mut i = 0;
    let mut in_class = false;
    while i < pat.len() {
        let c = pat[i];
        if in_class {
            if c == b']' {
                in_class = false;
            }
            i += if c == b'\\' { 2 } else { 1 };
            continue;
        }
        if c == b'\\' {
            match pat.get(i + 1) {
                Some(nc) if !nc.is_ascii_alphanumeric() => {
                    run.push(*nc);
                    i += 2;
                }
                _ => {
                    emit_run(&mut best, &mut run);
                    i += 2;
                }
            }
            continue;
        }
        if c == b'[' {
            emit_run(&mut best, &mut run);
            in_class = true;
            i += 1;
            continue;
        }
        if c == b'{' {
            // Counted repetition: the run so far stays required, the count
            // is skipped (a following zero-allowing count must not merge).
            emit_run(&mut best, &mut run);
            i += 1;
            while i < pat.len() && pat[i] != b'}' {
                i += 1;
            }
            i += 1;
            continue;
        }
        if is_regex_meta(c) {
            if matches!(c, b'*' | b'?' | b'+') {
                run.pop();
            }
            emit_run(&mut best, &mut run);
            i += 1;
            continue;
        }
        run.push(c);
        i += 1;
    }
    emit_run(&mut best, &mut run);
    Some(best)
}

/// Heuristically extract a required literal substring from a regex pattern
/// (no `regex-syntax` dependency): a fully-literal pattern, a `^` literal
/// prefix, or a literal `$` suffix — longest wins. Every accepted literal is
/// a true required substring (doubtful shapes yield `None` and the regex
/// runs unfiltered, exactly as before).
fn regex_required_literal(pattern: &str, case_sensitive: bool) -> Option<RequiredLit> {
    let pat = pattern.as_bytes();
    let has_caret = pat.first() == Some(&b'^');
    let has_dollar = pat.last() == Some(&b'$') && !is_escaped(pat, pat.len() - 1);
    let mut best = Vec::new();
    if has_caret {
        let lit = scan_fwd_literal(pat, 1);
        if lit.len() > best.len() {
            best = lit;
        }
    }
    if has_dollar {
        let lit = scan_bwd_literal(pat, pat.len() - 1);
        if lit.len() > best.len() {
            best = lit;
        }
    }
    if !has_caret && !has_dollar {
        if let Some(lit) = unanchored_literal_run(pat) {
            if lit.len() > best.len() {
                best = lit;
            }
        }
    }
    if best.is_empty() {
        return None;
    }
    let lit = String::from_utf8(best).ok()?;
    Some(RequiredLit::new(&lit, case_sensitive))
}

fn glob_regex(pattern: &str, case_sensitive: bool) -> Regex {
    let mut re = String::with_capacity(pattern.len() + 6);
    re.push('^');
    for c in pattern.chars() {
        match c {
            '*' => re.push_str(".*"),
            '?' => re.push('.'),
            '.' | '+' | '(' | ')' | '|' | '^' | '$' | '[' | ']' | '{' | '}' | '\\' => {
                re.push('\\');
                re.push(c);
            }
            _ => re.push(c),
        }
    }
    re.push('$');
    let full = if case_sensitive {
        re
    } else {
        format!("(?i){re}")
    };
    Regex::new(&full).unwrap_or_else(|_| {
        // Cannot realistically fail (allMeta chars escaped); worst case match nothing.
        Regex::new("$^").expect("trivial regex")
    })
}

fn ext_bytes_of(name: &[u8]) -> Option<&[u8]> {
    let dot = name.iter().rposition(|&b| b == b'.')?;
    let ext = name.get(dot + 1..)?;
    if ext.is_empty() || ext.contains(&b'/') || ext.contains(&b'\\') {
        return None;
    }
    Some(ext)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn two_volume_index() -> Index {
        let mut ix = Index::new();
        for letter in ['C', 'D'] {
            ix.add_volume(crate::index::Volume {
                letter,
                guid: [0; 16],
                journal_id: 1,
                next_usn: 0,
                root_frn: 100,
                enabled: true,
                monitor: true,
            });
        }
        ix.push(0, 100, 100, "", DIRECTORY);
        ix.push(0, 101, 100, "shared-name.txt", 0);
        ix.push(1, 200, 200, "", DIRECTORY);
        ix.push(1, 201, 200, "shared-name.txt", 0);
        ix.finalize();
        ix.rebuild_by_name();
        ix
    }

    /// The shared-parent path builder must equal a full `Index::path` walk
    /// for every match: roots, nested folders, both volumes, any order.
    #[test]
    fn paths_of_matches_index_path() {
        let mut ix = two_volume_index();
        ix.push(0, 102, 100, "dir", DIRECTORY);
        ix.push(0, 103, 102, "deep", DIRECTORY);
        ix.push(0, 104, 103, "a.txt", 0);
        ix.push(0, 105, 103, "b.txt", 0);
        ix.push(1, 202, 200, "other.txt", 0);
        ix.finalize();
        ix.rebuild_by_name();
        let hits: Vec<Hit> = (0..ix.len() as EntryId)
            .rev()
            .map(|id| Hit {
                id,
                vol: ix.volume_of(id).unwrap_or(0),
            })
            .collect();
        let fast = paths_of(&ix, &hits);
        let slow: Vec<String> = hits.iter().map(|h| ix.path(h.id)).collect();
        assert_eq!(fast, slow);
        assert!(fast.contains(&r"C:\dir\deep\a.txt".to_owned()));
        assert!(fast.contains(&r"D:\".to_owned()));
        let all = collect_matches(&ix, &crate::query::parse("txt"), None);
        assert_eq!(all.len(), 5);
    }

    #[test]
    fn disabled_volume_is_invisible_to_search_but_kept() {
        let mut ix = two_volume_index();
        let opts = SearchOptions::new(100, 0, Sort::NameAsc);
        let q = crate::query::parse("shared-name");
        assert_eq!(count(&ix, &q, None), 2);
        ix.volumes[0].enabled = false;
        // Every scan route (collect, generic, arena, merge, whole-name)
        // must agree the disabled volume never matches.
        assert_eq!(count(&ix, &q, None), 1);
        let hits = search(&ix, &q, &opts, None);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].vol, 1);
        assert_eq!(ix.len(), 4);
    }

    fn lit(pattern: &str, case_sensitive: bool) -> Option<String> {
        regex_required_literal(pattern, case_sensitive).map(|l| String::from_utf8(l.bytes).unwrap())
    }

    #[test]
    fn regex_literal_extraction() {
        // Anchored affixes. (`.*` before an escaped dot pops the dot while
        // stopping — the shorter literal stays required and safe.)
        assert_eq!(lit("^[a-c].*\\.txt$", false), Some("txt".to_string()));
        assert_eq!(lit("^main.*", false), Some("main".to_string()));
        assert_eq!(lit(".*\\.log$", false), Some("log".to_string()));
        assert_eq!(lit("^ab.*\\.txt$", false), Some("txt".to_string()));
        assert_eq!(lit("^abc$", false), Some("abc".to_string()));
        // Fully literal patterns (the common `regex:word` shape).
        assert_eq!(lit("main", false), Some("main".to_string()));
        assert_eq!(lit("foo\\.rs", false), Some("foo.rs".to_string()));
        // Nothing safely required: leading class, alternation, groups,
        // repetitions, bare anchors.
        assert_eq!(lit("^[a-c].*", false), None);
        assert_eq!(lit("foo|bar", false), None);
        assert_eq!(lit("(ab)c", false), None);
        assert_eq!(lit("a*", false), None);
        assert_eq!(lit("^", false), None);
        assert_eq!(lit(".*", false), None);
        // Quantified literals are optional, not required.
        assert_eq!(lit("ab*c", false), Some("a".to_string()));
    }

    #[test]
    fn glob_literal_extraction() {
        let glit =
            |p: &str| glob_required_literal(p, false).map(|l| String::from_utf8(l.bytes).unwrap());
        assert_eq!(glit("*.log"), Some(".log".to_string()));
        assert_eq!(glit("foo*"), Some("foo".to_string()));
        assert_eq!(glit("a*"), Some("a".to_string()));
        assert_eq!(glit("*"), None);
        assert_eq!(glit("main.rs"), Some("main.rs".to_string()));
    }

    #[test]
    fn glob_shape_analysis() {
        use GlobShape::*;
        assert_eq!(analyze_glob("main.rs"), Some((Whole, "main.rs", None)));
        assert_eq!(analyze_glob("*.log"), Some((Suffix, ".log", None)));
        assert_eq!(analyze_glob("foo*"), Some((Prefix, "foo", None)));
        assert_eq!(analyze_glob("*mid*"), Some((Substr, "mid", None)));
        assert_eq!(analyze_glob("ab*cd"), Some((PreSuffix, "ab", Some("cd"))));
        assert_eq!(analyze_glob("*"), None); // match-all, handled at build
        assert_eq!(analyze_glob("**"), None);
        assert_eq!(analyze_glob("a?b"), None); // needs the engine
        assert_eq!(analyze_glob("a*b*c"), None);
        assert_eq!(analyze_glob(""), Some((Whole, "", None)));
    }

    #[test]
    fn glob_fast_matches_engine() {
        // Exact shapes agree with the regex engine, incl. case folding,
        // overlaps, empty names, and non-ASCII (engine fallback).
        let cases = [
            "*.log", "foo*", "*mid*", "ab*cd", "main.rs", "*.LOG", "Ab*Cd", "a*b", "*é*", "*.r*",
        ];
        let names = [
            "",
            "x.log",
            "x.logy",
            "log",
            ".log",
            "foobar",
            "foo",
            "fo",
            "amidb",
            "ab",
            "abcd",
            "abc",
            "aXcd",
            "main.rs",
            "Main.RS",
            "MAIN.rs",
            "mid",
            "xmidy",
            "naïve.log",
            "Zürich",
            "café.LOG",
            "ab\ncd",
        ];
        for pat in cases {
            for cs in [false, true] {
                let regex = glob_regex(pat, cs);
                let expect = |n: &str| regex.is_match(n);
                for n in names {
                    let nb = n.as_bytes();
                    let got = if !pat.is_empty() && pat.bytes().all(|b| b == b'*') {
                        true
                    } else if let Some((shape, lit, lit2)) = analyze_glob(pat) {
                        let l = RequiredLit::new(lit, cs);
                        let l2 = lit2.map(|s| RequiredLit::new(s, cs));
                        glob_fast_match(nb, shape, &l, l2.as_ref(), &regex)
                    } else {
                        regex.is_match(n)
                    };
                    assert_eq!(got, expect(n), "pat={pat:?} cs={cs} name={n:?}");
                }
            }
        }
    }

    #[test]
    fn required_literal_case() {
        // Case-sensitive keeps raw bytes; insensitive folds.
        let cs = RequiredLit::new("Main", true);
        assert!(!cs.folded);
        assert!(cs.test(b"Main.rs"));
        assert!(!cs.test(b"main.rs"));
        let ci = RequiredLit::new("Main", false);
        assert!(ci.folded);
        assert!(ci.test(b"Main.rs"));
        assert!(ci.test(b"main.rs"));
        assert!(!ci.test(b"other"));
    }

    #[test]
    fn dollar_matches_at_end_only() {
        // Documents the assumption behind suffix fast paths: without (?m),
        // `$` matches at the end of the haystack (if this ever flips to
        // PCRE-style trailing-newline matching, the affix guards already
        // route newline-bearing names to the engine, so results stay exact).
        assert!(Regex::new("ab$").unwrap().is_match("ab"));
        assert!(!Regex::new("ab$").unwrap().is_match("ab\n"));
        assert!(!Regex::new("^ab$").unwrap().is_match("ab\n"));
    }

    /// Build the engine regex exactly like [`Matcher::build`] does.
    fn engine_for(pattern: &str, case_sensitive: bool) -> Regex {
        let pat = if case_sensitive {
            pattern.to_string()
        } else {
            format!("(?i){pattern}")
        };
        Regex::new(&pat).unwrap()
    }

    #[test]
    fn regex_affix_extraction() {
        use RegexFastKind::*;
        let shape_of = |p: &str, cs: bool| analyze_regex_fast(p, cs).map(|(k, _)| k);
        let is_sub = |p: &str, cs: bool| matches!(shape_of(p, cs), Some(Substr(_)));
        // Anchored affixes.
        assert!(matches!(
            shape_of("^[a-c].*\\.txt$", false),
            Some(PreSuf(_, _))
        ));
        assert!(matches!(shape_of("^main.*", false), Some(Prefix(_))));
        assert!(matches!(shape_of(".*\\.log$", false), Some(Suffix(_))));
        assert!(matches!(
            shape_of("^ab.*\\.txt$", false),
            Some(PreSuf(_, _))
        ));
        assert!(matches!(shape_of("^abc$", false), Some(Whole(_))));
        // Fully literal patterns.
        assert!(is_sub("main", false));
        assert!(is_sub("foo\\.rs", false));
        assert!(is_sub("ab.*", false));
        // Nothing safely required: alternation, groups, repetitions (bare or
        // quantifying a prefix char), bare anchors, ordered middles.
        assert!(matches!(shape_of("^[a-c].*", false), Some(Prefix(_))));
        assert_eq!(shape_of("foo|bar", false).map(|_| ()), None);
        assert_eq!(shape_of("(ab)c", false).map(|_| ()), None);
        assert_eq!(shape_of("a*", false).map(|_| ()), None);
        assert_eq!(shape_of("ab.*\\.txt", false).map(|_| ()), None);
        assert_eq!(shape_of("ab.*cd.*ef$", false).map(|_| ()), None);
        assert_eq!(shape_of("^", false).map(|_| ()), None);
        assert_eq!(shape_of(".*", false).map(|_| ()), None);
        // Quantified prefix char is optional, not fixed.
        assert_eq!(shape_of("^ab*c$", false).map(|_| ()), None);
        // `.*` in the middle without end anchor is undecomposable.
        assert_eq!(shape_of("^ab.*\\.txt", false).map(|_| ()), None);
    }

    #[test]
    fn regex_affix_matches_engine() {
        // Every accepted shape agrees with the engine on tricky inputs:
        // overlaps, empty names, newlines (engine fallback), non-ASCII
        // (engine fallback), case folding, negated classes.
        let cases = [
            "^[a-c].*\\.txt$",
            "^main.*",
            ".*\\.log$",
            "^ab.*\\.txt$",
            "^abc$",
            "main",
            "foo\\.rs",
            "^Z.*",
            "^[^a].*\\.txt$",
            "^[abc].*\\.log$",
            "case:Windows",
            "^[0-9].*",
            ".*[.]tmp$",
        ];
        // `case:` is stripped by the parser before compiling; emulate that.
        let cases: Vec<(&str, bool)> = cases
            .into_iter()
            .map(|p| match p.strip_prefix("case:") {
                Some(rest) => (rest, true),
                None => (p, false),
            })
            .collect();
        let names = [
            "",
            "main",
            "main.rs",
            "MAIN.RS",
            "maint",
            "ab",
            "abc",
            "abcd",
            "abc.txt",
            "ab.txt",
            "abx.txt",
            "ab\n.txt",
            "a.txt",
            "b.txt",
            "d.txt",
            "A.TXT",
            "x.log",
            "x.logy",
            ".log",
            "log",
            "Windows",
            "windows",
            "WINDOWS",
            "Windows_sys",
            "Zebra",
            "zebra",
            "0abc",
            "9.txt",
            ".tmp",
            "x.tmp",
            "naïve.txt",
            "Zürich",
            "café.LOG",
            "ab\ncd",
            "a",
            "abcabc",
        ];
        for (pat, cs) in cases {
            let regex = engine_for(pat, cs);
            let expect = |n: &str| regex.is_match(n);
            let fast = analyze_regex_fast(pat, cs);
            for n in names {
                let nb = n.as_bytes();
                let got = match &fast {
                    Some((kind, guard)) => regex_fast_match(nb, kind, *guard, cs, &regex),
                    None => regex.is_match(n),
                };
                assert_eq!(got, expect(n), "pat={pat:?} cs={cs} name={n:?}");
            }
        }
    }

    /// Small fixture exercising every leaf shape: non-ASCII names, case
    /// variants, multi-dot extensions, dotfiles, glob chars in a name, a file
    /// as a parent (orphan-style chain), and depth-3 hierarchy.
    fn equiv_index() -> Index {
        let mut ix = Index::new();
        ix.add_volume(crate::index::Volume {
            letter: 'C',
            guid: [0; 16],
            journal_id: 1,
            next_usn: 0,
            root_frn: 100,
            enabled: true,
            monitor: true,
        });
        let dir = DIRECTORY;
        ix.push(0, 100, 100, "", dir);
        ix.push(0, 101, 100, "Docs", dir);
        ix.push(0, 102, 100, "src", dir);
        ix.push(0, 103, 100, "Ünïcödé", dir);
        ix.push(0, 200, 101, "report.txt", 0);
        ix.push(0, 201, 101, "photo.PNG", 0);
        ix.push(0, 202, 102, "main.rs", 0);
        ix.push(0, 203, 102, "Main.RS", 0);
        ix.push(0, 204, 102, "archive.tar.gz", 0);
        ix.push(0, 205, 101, "README", 0);
        ix.push(0, 206, 103, "café.log", 0);
        ix.push(0, 207, 103, "naïve.TXT", 0);
        ix.push(0, 208, 100, "we?ird.log", 0);
        ix.push(0, 209, 100, ".hidden", 0);
        ix.push(0, 210, 100, "UPPER.DLL", 0);
        ix.push(0, 211, 200, "under_file.txt", 0);
        ix.push(0, 212, 102, "star*.txt", 0);
        ix.finalize();
        ix.rebuild_by_name();
        ix
    }

    /// Production-shaped fixture above the parallel threshold (multi-chunk
    /// scans, atomic cap gating) with rare/mid tags, non-ASCII and
    /// mixed-case names, and depth-3 hierarchy.
    fn equiv_big_index() -> Index {
        let mut ix = Index::new();
        ix.add_volume(crate::index::Volume {
            letter: 'C',
            guid: [0; 16],
            journal_id: 1,
            next_usn: 0,
            root_frn: 1,
            enabled: true,
            monitor: true,
        });
        ix.push(0, 1, 1, "", DIRECTORY);
        for d in 0..50u64 {
            ix.push(0, 100 + d, 1, &format!("folder_{d:02}"), DIRECTORY);
        }
        for d in 0..200u64 {
            ix.push(
                0,
                1000 + d,
                100 + (d % 50),
                &format!("sub_{d:03}"),
                DIRECTORY,
            );
        }
        let exts = ["txt", "rs", "log", "dll", "md"];
        for i in 0..8000u32 {
            let tag = if i.is_multiple_of(1000) {
                "qw7x"
            } else if i.is_multiple_of(50) {
                "k20"
            } else if i.is_multiple_of(101) {
                "café"
            } else if i.is_multiple_of(53) {
                "MiXeD"
            } else {
                "doc"
            };
            ix.push(
                0,
                100_000 + u64::from(i),
                1000 + u64::from(i % 200),
                &format!("{tag}_file_{i:05}.{}", exts[i as usize % 5]),
                0,
            );
        }
        ix.finalize();
        ix.rebuild_by_name();
        ix
    }

    const EQUIV_QUERIES: &[&str] = &[
        "",
        "report",
        "MAIN",
        "café",
        "ÜNÏ",
        "e",
        "zz_nomatch",
        "case:Main",
        "case:main",
        "case:MAIN.RS",
        "case:café",
        "wfn:main.rs",
        "wfn:MAIN.RS",
        "case:wfn:Main.RS",
        "case:wfn:MAIN.rs",
        "*.log",
        "case:*.log",
        "case:*.LOG",
        "main.*",
        "*a*",
        "m*.rs",
        "a*b*c",
        "*",
        "we?ird.log",
        "*é*",
        "regex:^m.*\\.rs$",
        "regex:.*\\.log$",
        "regex:^main",
        "regex:^main\\.rs$",
        "regex:main",
        "regex:a|b",
        "regex:^.*$",
        "regex:café",
        "case:regex:^M",
        "ext:rs",
        "ext:txt,log",
        "ext:gz",
        "ext:DLL",
        "path:docs",
        "path:src/main",
        "path:Docs/report",
        "path:Ünï",
        "path:report/under_file",
        "case:path:Docs",
        "folder:",
        "file:",
        "folder:docs",
        "report txt",
        "report|photo",
        "!report",
        "ext:rs main",
        "path:docs ext:txt",
        "mid100_nomatch_xyz",
    ];

    /// Old-vs-new scan: the monomorphized dispatch must return exactly what
    /// the generic evaluator returns (totals and hit lists), at every cap —
    /// including `0` (no collection) and `usize::MAX` (everything).
    #[test]
    fn tight_scan_matches_generic_small() {
        let ix = equiv_index();
        for qs in EQUIV_QUERIES {
            let q = crate::query::parse(qs);
            let tight = Matcher::build(&q.root);
            let generic = Matcher::build(&q.root);
            for cap in [0usize, 1, 5, 100_000, usize::MAX] {
                let a = scan_entries(&ix, &tight, cap);
                let b = scan_entries_generic(&ix, &generic, cap);
                assert_eq!(a, b, "query {qs:?} cap {cap}");
            }
        }
    }

    /// Old-vs-new page: every `search_paged` name-branch window must equal
    /// the collect-everything + full-sort oracle window (both directions,
    /// windowed and past-the-end offsets).
    #[test]
    fn arena_page_matches_full_sort_small() {
        let ix = equiv_index();
        for qs in EQUIV_QUERIES {
            let q = crate::query::parse(qs);
            let m = Matcher::build(&q.root);
            for desc in [false, true] {
                let sort = if desc { Sort::NameDesc } else { Sort::NameAsc };
                let mut all = collect(&ix, &m, None);
                let total = all.len() as u64;
                sort_by_name(&ix, &mut all, desc);
                for (offset, max) in [(0u32, 3u32), (2, 5), (0, 100), (7, 100), (100, 10)] {
                    let end = (offset as usize + max as usize).min(all.len());
                    let want: Vec<Hit> = if offset as usize >= all.len() {
                        Vec::new()
                    } else {
                        all[offset as usize..end].to_vec()
                    };
                    let got = search_paged(&ix, &q, &SearchOptions::new(max, offset, sort), None);
                    assert_eq!(got.total, total, "query {qs:?} total");
                    assert_eq!(
                        got.hits, want,
                        "query {qs:?} desc={desc} offset={offset} max={max}"
                    );
                }
            }
        }
    }

    /// Parallel old-vs-new scan on the multi-chunk fixture (atomic cap gating
    /// live), plus the parallel fold branch of the arena pager over all ids.
    #[test]
    fn tight_scan_matches_generic_big() {
        let ix = equiv_big_index();
        assert!(ix.len() > PAR_THRESHOLD, "fixture must scan in parallel");
        let queries = [
            "qw7x",
            "k20",
            "café",
            "mixed",
            "e",
            "case:E",
            "zzz_nomatch",
            "*.log",
            "doc*",
            "*file*",
            "regex:^k.*\\.txt$",
            "regex:.*\\.md$",
            "ext:rs,log",
            "ext:dll",
            "path:folder",
            "path:sub_001/doc",
            "",
            "folder:",
            "file:",
            "k20 doc",
            "k20|doc",
            "qw7x|café",
            "!doc",
            "case:MIXED",
            "wfn:doc_file_00001.txt",
        ];
        for qs in queries {
            let q = crate::query::parse(qs);
            let tight = Matcher::build(&q.root);
            let generic = Matcher::build(&q.root);
            for cap in [0usize, 10, 5000, usize::MAX] {
                let (ta, ha) = scan_entries(&ix, &tight, cap);
                let (tb, hb) = scan_entries_generic(&ix, &generic, cap);
                assert_eq!(ta, tb, "query {qs:?} cap {cap}");
                // Past the cap the early-stop truncation is
                // schedule-dependent (production only consumes the list
                // when complete): compare lists only in that regime.
                if ta <= cap as u64 {
                    assert_eq!(ha, hb, "query {qs:?} cap {cap}");
                } else {
                    assert_eq!(ha.len(), cap, "query {qs:?} trunc");
                    assert_eq!(hb.len(), cap, "query {qs:?} trunc");
                }
            }
        }
        // Parallel fold branch: all ids (>= PAR_THRESHOLD) through the arena
        // pager must match the full-sort oracle window.
        let all_ids: Vec<EntryId> = (0..ix.len() as EntryId).collect();
        for desc in [false, true] {
            let mut all = collect(&ix, &Matcher::True, None);
            let total = all.len() as u64;
            sort_by_name(&ix, &mut all, desc);
            for (offset, max) in [(0usize, 100usize), (5000, 100), (50_000, 10)] {
                let end = (offset + max).min(all.len());
                let want: Vec<Hit> = if offset >= all.len() {
                    Vec::new()
                } else {
                    all[offset..end].to_vec()
                };
                let got = page_ids_arena(&ix, &all_ids, offset, max, total, desc);
                assert_eq!(got.total, total, "par fold total desc={desc}");
                assert_eq!(
                    got.hits, want,
                    "par fold desc={desc} offset={offset} max={max}"
                );
            }
        }
    }

    /// 1M-scale old-vs-new (`#[ignore]`d like `bench_1m`: builds a 1M tree,
    /// then compares tight vs generic scans plus paged vs oracle windows).
    #[test]
    #[ignore]
    fn tight_scan_matches_generic_1m() {
        let mut ix = Index::new();
        ix.add_volume(crate::index::Volume {
            letter: 'C',
            guid: [0; 16],
            journal_id: 1,
            next_usn: 0,
            root_frn: 5,
            enabled: true,
            monitor: true,
        });
        ix.push(0, 5, 5, "", DIRECTORY);
        ix.push(0, 1000, 5, "program_files", DIRECTORY);
        ix.push(0, 1001, 5, "Windows_sys", DIRECTORY);
        for d in 2..200u64 {
            ix.push(0, 1000 + d, 5, &format!("folder_{d:03}"), DIRECTORY);
        }
        for d in 0..200u64 {
            ix.push(
                0,
                2000 + d,
                1002 + (d % 198),
                &format!("sub_{d:03}"),
                DIRECTORY,
            );
        }
        let exts = ["txt", "rs", "log", "dll", "md"];
        let mut i = 0u32;
        while ix.len() < 1_000_000 {
            let tag = if i.is_multiple_of(1000) {
                "qw7x"
            } else if i.is_multiple_of(50) {
                "k20"
            } else if i % 10 == 7 {
                "mid100"
            } else if i.is_multiple_of(17) {
                "Windows"
            } else if i.is_multiple_of(101) {
                "café"
            } else {
                "doc"
            };
            let parent = if i.is_multiple_of(12) {
                1000
            } else {
                2000 + u64::from(i % 200)
            };
            ix.push(
                0,
                100_000 + u64::from(i),
                parent,
                &format!("{tag}_r_{i:06}.{}", exts[i as usize % 5]),
                0,
            );
            i += 1;
        }
        ix.finalize();
        ix.rebuild_by_name();
        for qs in [
            "qw7x",
            "k20",
            "e",
            "k20|doc",
            "mid100",
            "*.log",
            "regex:^[a-c].*\\.txt$",
            "case:Windows",
            "ext:rs",
            "path:program",
            "ext:dll,log,md",
            "café",
        ] {
            let q = crate::query::parse(qs);
            let (ta, ha) = scan_entries(&ix, &Matcher::build(&q.root), MID_CAP);
            let (tb, hb) = scan_entries_generic(&ix, &Matcher::build(&q.root), MID_CAP);
            assert_eq!(ta, tb, "1M scan total {qs:?}");
            // Past the cap the early-stop truncation is schedule-dependent
            // (production only consumes the list when complete, i.e. total
            // <= cap): compare lists only in the complete regime.
            if ta <= MID_CAP as u64 {
                assert_eq!(ha, hb, "1M scan hits {qs:?}");
            } else {
                assert_eq!(ha.len(), MID_CAP, "1M scan truncation {qs:?}");
                assert_eq!(hb.len(), MID_CAP, "1M scan truncation {qs:?}");
            }
        }
        for qs in ["qw7x", "k20", "path:program", "*.log"] {
            let q = crate::query::parse(qs);
            let m = Matcher::build(&q.root);
            let mut all = collect(&ix, &m, None);
            let total = all.len() as u64;
            sort_by_name(&ix, &mut all, false);
            let end = 100usize.min(all.len());
            let want = all[..end].to_vec();
            let got = search_paged(&ix, &q, &SearchOptions::new(100, 0, Sort::NameAsc), None);
            assert_eq!(got.total, total, "1M page total {qs:?}");
            assert_eq!(got.hits, want, "1M page window {qs:?}");
        }
    }

    /// Arena sweep edge cases, calling `scan_arena` directly (below the
    /// dispatcher threshold): straddling occurrences rejected, tombstones
    /// skipped, empty names safe, truncated/empty prefixes covered by the
    /// tail — all identical to the generic scan. Past-cap lists are
    /// schedule-dependent (same as `scan_loop`), so lists compare only in
    /// the complete regime; totals always compare.
    #[test]
    fn arena_sweep_edge_cases() {
        let mut ix = Index::new();
        ix.add_volume(crate::index::Volume {
            letter: 'C',
            guid: [0; 16],
            journal_id: 1,
            next_usn: 0,
            root_frn: 1,
            enabled: true,
            monitor: true,
        });
        ix.push(0, 1, 1, "", DIRECTORY); // 0: empty root name
        ix.push(0, 2, 1, "ab", 0); // 1
        ix.push(0, 3, 1, "cd", 0); // 2: "bc" straddles 1|2
        ix.push(0, 4, 1, "xxbcxx", 0); // 3: genuine "bc"
        ix.push(0, 5, 1, "bc", TOMBSTONE); // 4: tombstoned "bc"
        ix.push(0, 6, 1, "", 0); // 5: empty file name
        ix.push(0, 7, 1, "abcabc", 0); // 6: overlapping "bc"
        ix.push(0, 8, 1, "café", 0); // 7: non-ASCII name
        ix.push(0, 9, 1, "aab", 0); // 8: "ab" at 1, decoy 'a' at 0
        ix.push(0, 10, 1, "AAB", 0); // 9: case variant of the same trap
        ix.finalize();
        ix.rebuild_by_name();
        assert_eq!(ix.arena_prefix_len as usize, ix.len());
        let check = |ix: &Index, prefix_note: &str| {
            for qs in ["bc", "case:bc", "ab", "case:AB", "abc", "zz", "café", ""] {
                let q = crate::query::parse(qs);
                let m = Matcher::build(&q.root);
                for cap in [0usize, 1, 3, usize::MAX] {
                    let (ta, ha) = match ArenaPlan::for_matcher(&m) {
                        Some(plan) => {
                            let (t, h, fp) = scan_arena(ix, &plan, &m, cap, true);
                            // Fused keys stay complete while collection does.
                            if t <= cap as u64 {
                                let nk: usize = fp.chunk_keys.iter().map(Vec::len).sum();
                                assert_eq!(
                                    nk as u64, t,
                                    "{prefix_note} fused keys {qs:?} cap {cap}"
                                );
                            }
                            (t, h)
                        }
                        None => scan_entries(ix, &m, cap),
                    };
                    let (tb, hb) = scan_entries_generic(ix, &m, cap);
                    assert_eq!(ta, tb, "{prefix_note} total {qs:?} cap {cap}");
                    if ta <= cap as u64 {
                        assert_eq!(ha, hb, "{prefix_note} hits {qs:?} cap {cap}");
                    } else {
                        assert_eq!(ha.len(), cap, "{prefix_note} trunc {qs:?}");
                        assert_eq!(hb.len(), cap, "{prefix_note} trunc {qs:?}");
                    }
                }
            }
        };
        check(&ix, "full prefix");
        // Truncated prefix: first 3 ids via arena, rest via tail.
        ix.arena_prefix_len = 3;
        check(&ix, "truncated prefix");
        // Degenerate prefix: everything via tail.
        ix.arena_prefix_len = 0;
        check(&ix, "empty prefix");
    }

    /// Single-thread arena scan floor (`#[ignore]`d): best-of-5 timings for
    /// the raw SIMD passes over a 1M-entry arena — `memchr2` first-byte,
    /// `memmem` Finder substring, and the `is_ascii` chunk gate — reported
    /// as GB/s so the parallel scan has a known ceiling.
    #[test]
    #[ignore]
    fn arena_scan_throughput_1m() {
        use std::time::Instant;
        let mut ix = Index::new();
        ix.add_volume(crate::index::Volume {
            letter: 'C',
            guid: [0; 16],
            journal_id: 1,
            next_usn: 0,
            root_frn: 1,
            enabled: true,
            monitor: true,
        });
        ix.push(0, 1, 1, "", DIRECTORY);
        for i in 0..1_000_000u32 {
            ix.push(
                0,
                100 + u64::from(i),
                1,
                &format!("file_{i:07}_report_final_v{}.txt", i % 97),
                0,
            );
        }
        let arena = ix.names.as_bytes();
        let gb = arena.len() as f64 / 1e9;
        fn best5(f: impl Fn() -> usize) -> (f64, usize) {
            let mut best = f64::INFINITY;
            let mut out = 0;
            for _ in 0..5 {
                let t0 = Instant::now();
                out = f();
                best = best.min(t0.elapsed().as_secs_f64());
            }
            (best, out)
        }
        let (t2, n2) = best5(|| {
            let mut n = 0usize;
            for _ in memchr::memchr2_iter(b'm', b'M', arena) {
                n += 1;
            }
            n
        });
        let finder = memmem::Finder::new(b"report");
        let (tf, nf) = best5(|| {
            let mut n = 0usize;
            for _ in finder.find_iter(arena) {
                n += 1;
            }
            n
        });
        // Same, but a needle that never occurs: pure skim rate (no
        // per-occurrence iterator cost).
        let finder_rare = memmem::Finder::new(b"qw7x9z");
        let (tfr, _nfr) = best5(|| {
            let mut n = 0usize;
            for _ in finder_rare.find_iter(arena) {
                n += 1;
            }
            n
        });
        // The gate is sub-millisecond and loop-invariant (the compiler
        // hoists it): re-opaque the slice per repetition.
        let (ta, na) = best5(|| {
            let mut n = 0usize;
            for _ in 0..20 {
                if std::hint::black_box(arena).is_ascii() {
                    n += 1;
                }
            }
            n
        });
        std::hint::black_box((n2, nf, na));
        println!(
            "arena_throughput_1m: MiB={:.1} memchr2={:.1}GB/s finder_dense={:.1}GB/s finder_rare={:.1}GB/s ascii_gate={:.1}GB/s",
            arena.len() as f64 / (1 << 20) as f64,
            gb / t2,
            gb / tf,
            gb / tfr,
            gb * 20.0 / ta,
        );
    }

    /// Live-index probe (Delta 8): structural stats + per-query timings on a
    /// real daemon index copy, replicating the service call pattern
    /// (`search_paged`, NameAsc, page 100, no prev, 8 threads, best of 3).
    /// Path comes from `FLOKI_LIVE_INDEX`, EMPTY in committed code (the test
    /// skips then). PRIVACY: prints counts, timings, and structural stats
    /// ONLY — never names, paths, or hits. Does not copy the file.
    const FLOKI_LIVE_INDEX: &str = "";

    const LIVE_QUERIES: &[&str] = &[
        "a",
        "e",
        "win",
        "dll",
        "ext:exe",
        "ext:rs;toml",
        "readme",
        "path:program",
        "*.log",
        "folder:temp",
        "!ext:dll sys",
        "config|settings",
        "regex:^[a-c].*\\.txt$",
        "case:Windows",
        "wfn:notepad.exe",
        "x",
        "node_modules",
        "ext:png;jpg;gif",
        "\"program files\"",
        "cache",
    ];

    #[test]
    #[ignore]
    fn live_probe() {
        use std::time::Instant;
        if FLOKI_LIVE_INDEX.is_empty() {
            eprintln!("live_probe: skipped (FLOKI_LIVE_INDEX empty)");
            return;
        }
        set_search_threads(8);
        let t0 = Instant::now();
        let ix = Index::load(std::path::Path::new(FLOKI_LIVE_INDEX)).expect("load live index");
        let load_ms = t0.elapsed().as_secs_f64() * 1000.0;
        let b = ix.memory_breakdown();
        eprintln!(
            "live_probe: load_ms={load_ms:.0} entries={} arena_MiB={:.1} prefix={} ({}%) \
             pending={} tombstones={} frn_runs={} by_name_len={} frn_len={} \
             by_name_fresh={} frn_fresh={} rss_MiB={:.1}",
            ix.len(),
            ix.names.len() as f64 / (1 << 20) as f64,
            ix.arena_prefix_len,
            100.0 * ix.arena_prefix_len as f64 / ix.len().max(1) as f64,
            ix.pending_len(),
            ix.tombstone_count(),
            ix.frn_runs.len(),
            ix.by_name.len(),
            ix.frn_index.len(),
            ix.by_name_is_fresh(),
            ix.frn_is_fresh(),
            ix.memory_usage() as f64 / (1 << 20) as f64,
        );
        eprintln!(
            "live_probe: breakdown entries={} arena={} by_name={} frn={} vol={} pending={} aux={} tomb={}",
            b.entries_bytes,
            b.arena_bytes,
            b.by_name_bytes,
            b.frn_index_bytes,
            b.entry_vol_bytes,
            b.pending_bytes,
            b.arena_aux_bytes,
            b.tombstone_bytes,
        );
        for qs in LIVE_QUERIES {
            let q = crate::query::parse(qs);
            let opts = SearchOptions::new(100, 0, Sort::NameAsc);
            // Best of 3, daemon pattern (no prev). Rows replicated as byte
            // lengths only (never printed) to include row-building cost;
            // the split comes from the best run.
            let mut best_ms = f64::INFINITY;
            let mut best_search = 0.0;
            let mut best_rows = 0.0;
            let mut total = 0u64;
            let mut page = 0usize;
            for _ in 0..3 {
                let t = Instant::now();
                let res = search_paged(&ix, &q, &opts, None);
                let search_ms = t.elapsed().as_secs_f64() * 1000.0;
                let mut row_bytes = 0usize;
                for hit in &res.hits {
                    if let Some(entry) = ix.entry(hit.id) {
                        if entry.flags & TOMBSTONE != 0 {
                            continue;
                        }
                        row_bytes += ix.name(hit.id).unwrap_or("").len();
                        let vol = ix.volume_of(hit.id).unwrap_or(hit.vol);
                        if entry.frn != entry.parent_frn
                            && ix.lookup(vol, entry.parent_frn).is_none()
                        {
                            continue;
                        }
                        if let Some(parent) = ix.lookup(vol, entry.parent_frn) {
                            row_bytes += ix.path(parent).len();
                        }
                    }
                }
                std::hint::black_box(row_bytes);
                let combined = t.elapsed().as_secs_f64() * 1000.0;
                if combined < best_ms {
                    best_ms = combined;
                    best_search = search_ms;
                    best_rows = combined - search_ms;
                }
                total = res.total;
                page = res.hits.len();
            }
            assert!(page <= 100);
            eprintln!(
                "live_probe: query={qs:?} total={total} page={page} \
                 best_ms={best_ms:.1} search_ms={best_search:.1} rows_ms={best_rows:.1}"
            );
        }
    }
    /// `None` (unstattable) sorts last in both directions; `desc` reverses
    /// only real timestamps.
    #[test]
    fn time_key_ordering_puts_none_last() {
        use std::cmp::Ordering;
        for desc in [false, true] {
            assert_eq!(cmp_time_key(None, Some(1), desc), Ordering::Greater);
            assert_eq!(cmp_time_key(Some(1), None, desc), Ordering::Less);
            assert_eq!(cmp_time_key(None, None, desc), Ordering::Equal);
        }
        assert_eq!(cmp_time_key(Some(1), Some(2), false), Ordering::Less);
        assert_eq!(cmp_time_key(Some(1), Some(2), true), Ordering::Greater);
    }

    /// `time_key` reads real filesystem times; a missing path yields `None`.
    #[test]
    fn time_key_stats_real_files() {
        let dir = std::env::temp_dir().join(format!("floki-timekey-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.txt");
        std::fs::write(&file, b"x").unwrap();
        let path = file.to_string_lossy().into_owned();
        assert!(time_key(&path, TimeField::Modified).is_some());
        assert!(time_key(&path, TimeField::Created).is_some());
        let missing = dir.join("nope.txt").to_string_lossy().into_owned();
        assert_eq!(time_key(&missing, TimeField::Modified), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Time sorts page like path sorts: the windowed page is always a slice
    /// of the full ordering, whatever the stat results are (a real file at
    /// `X:\shared-name.txt` would only change the key, never the contract).
    #[test]
    fn time_sort_pages_consistently() {
        let ix = two_volume_index();
        let q = crate::query::parse("shared-name");
        for sort in [
            Sort::ModifiedAsc,
            Sort::ModifiedDesc,
            Sort::CreatedAsc,
            Sort::CreatedDesc,
        ] {
            let got = search_paged(&ix, &q, &SearchOptions::new(100, 0, sort), None);
            assert_eq!(got.total, 2);
            assert_eq!(got.hits.len(), 2);
            // Both matches appear exactly once (a permutation, not a dup).
            assert_ne!(got.hits[0], got.hits[1]);
            // The offset page is the tail of the same ordering.
            let page = search_paged(&ix, &q, &SearchOptions::new(1, 1, sort), None);
            assert_eq!(page.hits.len(), 1);
            assert_eq!(page.hits[0], got.hits[1]);
        }
    }
}
