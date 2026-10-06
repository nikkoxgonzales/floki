//! `Index`: volumes, entries, arena, lookup maps (SPEC section 3).

use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::arena::NameArena;
use crate::entry::{Entry, EntryId, TOMBSTONE};
use crate::fold::{cmp_folded_bytes, fold};

/// One indexed volume.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Volume {
    /// Drive letter, e.g. `'C'`.
    pub letter: char,
    /// Volume GUID bytes.
    pub guid: [u8; 16],
    /// USN journal id the index was caught up to.
    pub journal_id: u64,
    /// Next USN to read on the next journal poll.
    pub next_usn: i64,
    /// FRN of the volume root directory.
    pub root_frn: u64,
    /// Search visibility: `false` hides the volume's entries from search
    /// results (the entries stay indexed and tailed).
    #[serde(default = "volume_enabled_default")]
    pub enabled: bool,
    /// Journal-tail monitoring: `false` means scan-only (index once at
    /// add/rescan, no live tail thread). Old index files load as `true`.
    #[serde(default = "volume_monitor_default")]
    pub monitor: bool,
}

/// Serde default for [`Volume::enabled`] (legacy index files).
fn volume_enabled_default() -> bool {
    true
}

/// Serde default for [`Volume::monitor`] (legacy index files).
fn volume_monitor_default() -> bool {
    true
}

/// Live-update event for [`Index::apply`].
///
/// The `floki-ntfs` crate has its own `UsnEvent`; the service maps it onto this
/// enum. `Create` allocates a NEW entry (reviving a tombstone with the same
/// FRN tombstones the old id and appends a new one; [`lookup`](Index::lookup)
/// returns the new id), `Delete` tombstones, `Rename` tombstones the old id
/// and appends a NEW entry (same frn/parent/flags, new name) so the sorted
/// `by_name` snapshot is never patched in place, `Update` replaces attribute
/// flags but never clears a tombstone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexEvent<'a> {
    /// New file or directory seen.
    Create {
        /// NTFS file reference number.
        frn: u64,
        /// Parent directory FRN.
        parent_frn: u64,
        /// File name (no path).
        name: &'a str,
        /// Attribute flag bits.
        flags: u16,
    },
    /// FRN deleted.
    Delete {
        /// NTFS file reference number.
        frn: u64,
    },
    /// Same FRN under a new name and/or parent (rename or move).
    Rename {
        /// NTFS file reference number.
        frn: u64,
        /// New parent directory FRN.
        parent_frn: u64,
        /// New file name (no path).
        name: &'a str,
    },
    /// Attribute/flag change for a live entry.
    Update {
        /// NTFS file reference number.
        frn: u64,
        /// New attribute flag bits.
        flags: u16,
    },
}

/// Maximum parent hops [`Index::path`] will walk before giving up (cycle guard).
pub const MAX_PATH_HOPS: u32 = 512;

/// Byte-level RAM breakdown of an [`Index`], for observability.
/// See [`Index::memory_breakdown`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryBreakdown {
    /// `entries.len() * 24` (live and tombstoned).
    pub entries_bytes: u64,
    /// Name arena bytes (live names plus rename garbage until compact).
    pub arena_bytes: u64,
    /// `by_name.len() * 4` (live ids only after a merge).
    pub by_name_bytes: u64,
    /// `frn_index.len() * 4` (every id, live and tombstoned).
    pub frn_index_bytes: u64,
    /// `entry_vol.len()` (one byte per entry).
    pub entry_vol_bytes: u64,
    /// `pending.len() * 4`.
    pub pending_bytes: u64,
    /// Arena-scan auxiliary heap: the block presence bitsets
    /// (`arena_blocks.len() * 32`); the offset→id mapping itself sweeps the
    /// non-decreasing `name_off` prefix (`arena_prefix_len`) using offsets
    /// already stored in `entries`, so no per-entry table exists. Well under
    /// the 4 B/entry budget (~512 B per MiB of arena).
    pub arena_aux_bytes: u64,
    /// Dead entry slots: `tombstone_count() * 24`. Informational subset of
    /// `entries_bytes`, not an additional term.
    pub tombstone_bytes: u64,
}

impl MemoryBreakdown {
    /// Sum of the live terms; equals [`Index::memory_usage`].
    #[must_use]
    pub fn total_bytes(&self) -> u64 {
        self.entries_bytes
            + self.arena_bytes
            + self.by_name_bytes
            + self.frn_index_bytes
            + self.entry_vol_bytes
            + self.pending_bytes
            + self.arena_aux_bytes
    }
}

/// Sort key of an entry id: `(volume, frn, id)` (see
/// [`Index::lookup`](Index::lookup) grouping note on the method). Free
/// function so run sealing can sort an `frn_index` slice while borrowing the
/// entry arrays separately.
fn frn_key_of(entries: &[Entry], vols: &[u8], id: EntryId) -> (u8, u64, EntryId) {
    (
        vols.get(id as usize).copied().unwrap_or(0),
        entries.get(id as usize).map_or(u64::MAX, |e| e.frn),
        id,
    )
}

/// Stable two-way merge of id lists each sorted by `cmp` (ties take `left`).
fn merge_sorted(
    left: &[EntryId],
    right: &[EntryId],
    cmp: impl Fn(EntryId, EntryId) -> std::cmp::Ordering,
) -> Vec<EntryId> {
    let mut merged: Vec<EntryId> = Vec::with_capacity(left.len() + right.len());
    let (mut l, mut r) = (0usize, 0usize);
    while l < left.len() && r < right.len() {
        if cmp(left[l], right[r]) != std::cmp::Ordering::Greater {
            merged.push(left[l]);
            l += 1;
        } else {
            merged.push(right[r]);
            r += 1;
        }
    }
    merged.extend_from_slice(&left[l..]);
    merged.extend_from_slice(&right[r..]);
    merged
}

/// Maximum [`pending`](Index::pending) ids before [`by_name`](Index::by_name)
/// stops counting as fresh. `apply` keeps appending past this (searches fall
/// back to collect+sort); the next [`rebuild_by_name`](Index::rebuild_by_name)
/// / [`compact`](Index::compact) folds them in. Sized so a burst of live
/// updates never forces a resort, while the merge walk stays cheap.
pub const PENDING_MAX: usize = 50_000;

/// Unsealed `frn_index` tail length that triggers an automatic seal inside
/// [`push`](Index::push). Backstop for callers that never call
/// [`seal_batch`](Index::seal_batch): keeps the linear tail scan in
/// [`resolve`](Index::resolve) capped without any interior mutability.
const FRN_AUTO_SEAL: usize = 16_384;

/// Unsealed tail length that triggers a seal on the live-update path
/// ([`apply`](Index::apply)). Smaller than [`FRN_AUTO_SEAL`] because every
/// live event resolves its FRN first, and that lookup scans the unsealed
/// tail linearly; scan pushes never look up, so they seal less often.
const FRN_APPLY_SEAL: usize = 1_024;

/// Maximum sealed `frn_index` runs before the smallest adjacent pair merges.
/// Bounds per-lookup binary searches; merging is linear in the pair size.
const FRN_MAX_RUNS: usize = 64;

/// Largest pair of newest runs [`seal_live`](Index::seal_live) merges.
/// Keeps live creates in one delta run (a few ms of merging per journal
/// poll at most) without ever merging scan-sized runs under the write lock.
const FRN_LIVE_MERGE: usize = 1 << 18;

/// Maximum cached `path:` verdict sets (see [`PathVerdictCache`]).
pub(crate) const PATH_CACHE_ENTRIES: usize = 4;

/// Cache key for one single-segment `path:` verdict set: the segment's byte
/// pattern plus casing plus the exact index content era. `mutation` bumps on
/// every content change (`push`, `apply`, `compact`, rebuilds, loads), so an
/// equal key guarantees equal verdicts; `len` guards the bitset bounds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PathVerdictKey {
    pub(crate) seg: Vec<u8>,
    pub(crate) folded: bool,
    pub(crate) mutation: u64,
    pub(crate) len: usize,
}

/// Cross-query memo for single-segment `path:` verdicts (one bit per entry
/// id): repeat queries skip the ancestor walk and replay the bitset. Bounded
/// insertion-ordered LRU ([`PATH_CACHE_ENTRIES`] sets max, ~1 MiB per
/// 8.5 M-entry set) behind a mutex; contention-free in practice (lock held
/// only for key compare / `Arc` clone / store). Stale sets are impossible —
/// any content change bumps `mutation` and misses the key — and eviction only
/// costs a recompute. Never persisted (rebuilt on demand after load).
#[derive(Debug, Default)]
pub(crate) struct PathVerdictCache {
    slots: Vec<(PathVerdictKey, std::sync::Arc<Vec<u64>>)>,
}

impl PathVerdictCache {
    fn new() -> Self {
        Self { slots: Vec::new() }
    }

    /// Shared verdict bits for `key`, when a set covering `len` ids is cached.
    pub(crate) fn lookup(
        &self,
        key: &PathVerdictKey,
        len: usize,
    ) -> Option<std::sync::Arc<Vec<u64>>> {
        self.slots.iter().find_map(|(k, bits)| {
            (k == key && bits.len() * 64 >= len).then(|| std::sync::Arc::clone(bits))
        })
    }

    /// Store `bits` under `key`, evicting the oldest set past capacity.
    /// Same-key stores replace in place (a recompute under a new era lands on
    /// a new key; a duplicate compute races to the same value).
    pub(crate) fn store(&mut self, key: PathVerdictKey, bits: std::sync::Arc<Vec<u64>>) {
        if let Some(pos) = self.slots.iter().position(|(k, _)| k == &key) {
            self.slots[pos].1 = bits;
            return;
        }
        if self.slots.len() >= PATH_CACHE_ENTRIES {
            self.slots.remove(0);
        }
        self.slots.push((key, bits));
    }
}

/// The file-name index: entries + name arena + lookup maps.
///
/// `volumes`, `entries` and `names` are public per SPEC. `frn_index` and
/// `by_name` are rebuilt on load. `entry_vol` (volume index per entry) is a
/// small v1 addition the SPEC file format does not describe; it is persisted
/// as trailing bytes (see [`save`](Index::save)) so [`Hit`](crate::Hit) can
/// report its volume without relying on entries staying grouped by volume.
///
/// `frn_index` holds every entry id (live and tombstoned), partitioned into
/// sorted runs by `(entry_vol, frn, id)` plus one unsealed tail; lookup
/// binary-searches each run newest-first (4 B/entry). Scan [`push`](Self::push)es
/// append to the tail (and seal it every [`FRN_AUTO_SEAL`] ids or on explicit
/// [`seal_batch`](Self::seal_batch)); [`finalize`](Self::finalize) (also called
/// by [`rebuild_by_name`](Self::rebuild_by_name), [`compact`](Self::compact)
/// and [`load`](Self::load)) merges everything into one run. [`apply`](Self::apply)
/// appends new ids to the same tail as `push` (never a mid-array insert: an
/// O(n) `Vec::insert` per create made build bursts and boot replays hold the
/// write lock for minutes on a multi-million-entry index).
///
/// `by_name` is the sorted snapshot from the last `rebuild_by_name` /
/// `compact`: live ids ordered by `(fold(name), id)`. It is NEVER patched in
/// place by `apply` (an in-place rename would silently break its position).
/// Instead `apply` appends ids created/renamed/revived since that snapshot to
/// `pending`, kept sorted by the same key via binary-search insert (small:
/// bounded by [`PENDING_MAX`] for freshness). Tombstoned ids in either list
/// are skipped by walks and dropped by the next rebuild. `push` (scan phase)
/// still appends an unsorted `by_name` tail and sets the dirty flag, so
/// mid-scan searches fall back to collect+sort exactly as before.
/// Detached sorted arrays ready to install: the merged `by_name` plus a
/// single-run `frn_index` (with its run table), stamped with the source
/// index's mutation counter. Build off the write lock with
/// [`Index::sorted_snapshot`], then commit with [`Index::install_sorted`].
#[derive(Debug, Clone)]
pub struct SortedSnapshot {
    by_name: Vec<EntryId>,
    frn_index: Vec<EntryId>,
    frn_runs: Vec<Range<usize>>,
    mutation: u64,
}

/// A rescanned volume merged into a detached copy of the index arrays,
/// stamped with the source index's mutation counter. Build it under a READ
/// lock with [`Index::volume_swap`], commit with
/// [`Index::install_volume_swap`] (O(1) under the write lock).
#[derive(Debug)]
pub struct VolumeSwap {
    entries: Vec<Entry>,
    entry_vol: Vec<u8>,
    names: NameArena,
    arena_blocks: Vec<[u64; 4]>,
    by_name: Vec<EntryId>,
    frn_index: Vec<EntryId>,
    mutation: u64,
    /// Live entries of the volume that the swap drops.
    pub stale: u64,
    /// Staged entries the swap takes in.
    pub fresh: u64,
}

/// A volume's removal built into a detached copy of the index arrays. Build
/// it under a READ lock with [`Index::volume_removal`], commit with
/// [`Index::install_volume_removal`] (O(1) under the write lock).
#[derive(Debug)]
pub struct VolumeRemoval {
    pos: usize,
    letter: char,
    arrays: VolumeSwap,
}

impl VolumeRemoval {
    /// Entries the removal drops (live and tombstoned).
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.arrays.stale
    }
}

pub struct Index {
    /// Volumes; the position in this vec is the volume index used everywhere.
    pub volumes: Vec<Volume>,
    /// Global NTFS-targets policy (auto-include / auto-remove). Persisted in
    /// `index.bin` (see [`save`](Self::save)); never affects search ranking.
    pub targets: crate::persist::TargetsConfig,
    /// All entries across volumes.
    pub entries: Vec<Entry>,
    /// Original-case names.
    pub names: NameArena,
    pub(crate) frn_index: Vec<EntryId>,
    /// Sealed sorted runs partitioning `frn_index[..sealed]` oldest-first;
    /// the remainder is the unsealed tail (push order, at most
    /// [`FRN_AUTO_SEAL`] ids). Run contents are contiguous push-order id
    /// intervals, so every id in a newer run exceeds every id in an older
    /// one and newest-first lookup is exact. Empty unless a scan is (or was
    /// recently) pushing; [`finalize`](Self::finalize) collapses to one run.
    pub(crate) frn_runs: Vec<Range<usize>>,
    pub(crate) by_name: Vec<EntryId>,
    pub(crate) entry_vol: Vec<u8>,
    /// Ids appended by [`apply`](Self::apply) since the last
    /// [`rebuild_by_name`](Self::rebuild_by_name), sorted by
    /// `(fold(name), id)` so name-ordered search can merge-walk
    /// `by_name + pending` without resorting.
    pub(crate) pending: Vec<EntryId>,
    /// Bumped on every [`compact`](Self::compact) and on [`load`](Self::load).
    /// `compact` renumbers every [`EntryId`](crate::EntryId), so callers
    /// holding [`Hit`](crate::Hit)s (the `prev` cache) must drop them when
    /// this changes. Paths are FRN-based and stay valid across compaction.
    pub(crate) epoch: u64,
    /// True when `by_name` may be unsorted or hold tombstones / miss fresh
    /// pushes (set by [`push`](Self::push) only, cleared by
    /// [`rebuild_by_name`](Self::rebuild_by_name) / [`compact`](Self::compact)).
    /// [`apply`](Self::apply) never sets it: live updates go to `pending`.
    pub(crate) by_name_dirty: AtomicBool,
    /// True when `frn_index` is not exactly one sorted run covering all ids
    /// (unsealed tail and/or several runs waiting; set by [`push`](Self::push)
    /// and [`apply`](Self::apply) appends, cleared by [`finalize`](Self::finalize),
    /// snapshot installs, and whenever sealing restores single-run state).
    /// [`resolve`](Self::resolve) does not consult this flag: runs plus the
    /// capped tail are always queryable.
    pub(crate) frn_dirty: AtomicBool,
    /// Mutation counter for [`SortedSnapshot`] installs: bumped by every
    /// operation that changes the sorted arrays or entry ids (`push`,
    /// `apply`, `compact`, `rebuild_by_name`, `finalize`, FRN sealing, and
    /// loads). [`install_sorted`](Self::install_sorted) swaps in a detached
    /// snapshot only when its stamp still matches, so a mutation between
    /// [`sorted_snapshot`](Self::sorted_snapshot) and install fails instead
    /// of installing stale arrays.
    pub(crate) mutation: u64,
    /// Length of the leading id range whose arena offsets are contiguous and
    /// non-decreasing (`entries[id].name_off == end of id - 1` for
    /// `id < arena_prefix_len`): exactly the range the contiguous arena scan
    /// may sweep, mapping hit byte offsets back to ids by forward walk (no
    /// per-entry table — the offsets already live in `entries`, counted in
    /// `entries_bytes`). Contiguity matters: names tile the prefix byte range
    /// exactly, so every in-name occurrence maps to its true owner. Pushes
    /// and live appends extend it in O(1) when the new name continues the
    /// tiling; `compact` restores it (arena rewritten in order); load
    /// verifies it (a corrupt file may break the order — ids past the break
    /// use the per-entry scan). Sweep cost: 4 bytes total, ~0 B/entry.
    pub(crate) arena_prefix_len: u32,
    /// Byte-presence bitsets, one 256-bit mask per 64 KiB arena block: which
    /// byte values occur in the block. Lets the arena scan skip blocks
    /// lacking a query's first byte and know block ASCII-ness (top-half
    /// mask zero) without re-scanning bytes. Maintained on every append
    /// (bits ORed on a hot block), rebuilt by `compact`, recovered on load
    /// in the validation pass. ~512 B per MiB of arena; rebuilt, never
    /// persisted (see [`memory_breakdown`](Self::memory_breakdown)).
    pub(crate) arena_blocks: Vec<[u64; 4]>,
    /// Cross-query `path:` verdict sets (see [`PathVerdictCache`]): transient
    /// per-index memo, never persisted, excluded from [`memory_breakdown`](Self::memory_breakdown)
    /// (bounded: [`PATH_CACHE_ENTRIES`] bitsets). Guarded by a mutex so
    /// concurrent searches share it; poison clears on next use.
    pub(crate) path_verdicts: Mutex<PathVerdictCache>,
}

/// Arena block size for [`Index::arena_blocks`](Index::arena_blocks):
/// 2^16 bytes per bitset (256-bit presence mask).
pub(crate) const ARENA_BLOCK_BITS: u32 = 16;

impl Default for Index {
    fn default() -> Self {
        Self::new()
    }
}

impl Index {
    /// Empty index.
    #[must_use]
    pub fn new() -> Self {
        Self {
            volumes: Vec::new(),
            targets: crate::persist::TargetsConfig::default(),
            entries: Vec::new(),
            names: NameArena::new(),
            frn_index: Vec::new(),
            frn_runs: Vec::new(),
            by_name: Vec::new(),
            entry_vol: Vec::new(),
            pending: Vec::new(),
            epoch: 0,
            by_name_dirty: AtomicBool::new(false),
            frn_dirty: AtomicBool::new(false),
            mutation: 0,
            arena_prefix_len: 0,
            arena_blocks: Vec::new(),
            path_verdicts: Mutex::new(PathVerdictCache::new()),
        }
    }

    /// Register a volume; returns its volume index.
    ///
    /// Both policy flags are forced on: callers constructing a [`Volume`]
    /// with `enabled: false` / `monitor: false` (e.g. a test staging a
    /// scan-only fixture through [`add_volume`](Self::add_volume)) still get
    /// a fully live volume; policy changes go through explicit flag writes,
    /// never the registration path.
    pub fn add_volume(&mut self, mut volume: Volume) -> u8 {
        volume.enabled = true;
        volume.monitor = true;
        self.volumes.push(volume);
        debug_assert!(self.volumes.len() <= u8::MAX as usize);
        (self.volumes.len() - 1) as u8
    }

    /// Drop the volume `letter` and ALL its entries, compacting every array
    /// (`entries`, `entry_vol`, `names` arena, `by_name`, `frn_index`,
    /// `pending`) so no id, FRN group, name slice, or volume slot of the
    /// removed volume survives. Volume indexes shift (every surviving entry
    /// is re-tagged to its new position in [`volumes`](Self::volumes)), so
    /// callers holding raw `u8` volume indexes must re-resolve them — the
    /// same contract as [`compact`](Self::compact) for entry ids. Search
    /// stays exact afterwards because both sorted arrays are carried over
    /// in order (same O(n) path as [`compact`](Self::compact), no re-sort).
    ///
    /// Returns `false` (index untouched) when `letter` is not indexed.
    pub fn remove_volume(&mut self, letter: char) -> bool {
        let Some(removal) = self.volume_removal(letter) else {
            return false;
        };
        let installed = self.install_volume_removal(removal);
        debug_assert!(installed, "no mutation can interleave under &mut self");
        true
    }

    /// Read-only half of [`remove_volume`](Self::remove_volume): build the
    /// arrays without `letter` under a READ lock (searches proceed), then
    /// commit with [`install_volume_removal`](Self::install_volume_removal).
    /// `None` when `letter` is not indexed.
    #[must_use]
    pub fn volume_removal(&self, letter: char) -> Option<VolumeRemoval> {
        let pos = self.volumes.iter().position(|v| v.letter == letter)?;
        let removed = pos as u8;
        // Volume indexes above the removed slot shift down by one.
        let (arrays, _) = self.retained(|_, vol| match vol.cmp(&removed) {
            std::cmp::Ordering::Less => Some(vol),
            std::cmp::Ordering::Equal => None,
            std::cmp::Ordering::Greater => Some(vol - 1),
        });
        Some(VolumeRemoval {
            pos,
            letter,
            arrays,
        })
    }

    /// Commit a [`volume_removal`](Self::volume_removal): O(1) swap when
    /// nothing mutated the index since it was built and the volume still
    /// sits at the same slot. Returns `false` (index unchanged) otherwise;
    /// rebuild, or fall back to [`remove_volume`](Self::remove_volume).
    /// Renumbers ids and shifts volume indexes like `remove_volume`.
    pub fn install_volume_removal(&mut self, r: VolumeRemoval) -> bool {
        let same_slot = self
            .volumes
            .get(r.pos)
            .is_some_and(|v| v.letter == r.letter);
        if r.arrays.mutation != self.mutation || !same_slot {
            return false;
        }
        self.volumes.remove(r.pos);
        self.install_arrays(r.arrays);
        true
    }

    /// Reserve room for `additional` more entries (scan fast path: no
    /// per-entry allocation beyond the arena append).
    pub fn reserve(&mut self, additional: usize) {
        self.entries.reserve(additional);
        self.by_name.reserve(additional);
        self.entry_vol.reserve(additional);
        self.frn_index.reserve(additional);
        self.names.reserve(additional.saturating_mul(32));
    }

    /// Record a mutation of the sorted arrays or entry ids (invalidates
    /// outstanding [`SortedSnapshot`]s — see [`mutation`](Self::mutation)).
    pub(crate) fn bump_mutation(&mut self) {
        self.mutation = self.mutation.wrapping_add(1);
    }

    /// Extend [`arena_prefix_len`](Self::arena_prefix_len) past a freshly
    /// appended entry with arena offset `off`: O(1) — the prefix covers ids
    /// `[0, prefix)` and the new id is `entries.len() - 1`, so it extends the
    /// prefix exactly when everything before tiled contiguously and `off`
    /// continues the tiling (`off == previous end`; appends pack exactly).
    /// Record one arena append at `off` holding `name`: extends
    /// [`arena_prefix_len`](Self::arena_prefix_len) in O(1) when the new
    /// name continues the tiling, and ORs the name's bytes into the block
    /// presence bitset. Called by [`push`](Self::push) and
    /// [`append_live`](Self::append_live) (both append arena + entry in
    /// lockstep, so scan-built prefixes stay whole).
    fn note_arena_append(&mut self, off: u32, name: &[u8]) {
        let n = self.entries.len() as u64;
        if u64::from(self.arena_prefix_len) + 1 == n {
            let prev_end = if n >= 2 {
                let p = &self.entries[n as usize - 2];
                p.name_off as u64 + p.name_len as u64
            } else {
                0
            };
            if off as u64 == prev_end {
                self.arena_prefix_len = n as u32;
            }
        }
        Self::block_add_bytes(&mut self.arena_blocks, off, name);
    }

    /// OR `name`'s bytes (at arena `off`) into the block presence bitset,
    /// growing it when the append reaches a new block.
    pub(crate) fn block_add_bytes(blocks: &mut Vec<[u64; 4]>, off: u32, name: &[u8]) {
        let idx = (off as usize) >> ARENA_BLOCK_BITS;
        if blocks.len() <= idx {
            blocks.resize(idx + 1, [0; 4]);
        }
        let mask = &mut blocks[idx];
        for &b in name {
            mask[(b >> 6) as usize] |= 1 << (b & 63);
        }
    }

    /// Push one scan entry. Returns its [`EntryId`].
    ///
    /// Appends to `entries` (plus arena / `entry_vol` / `by_name` tail and the
    /// unsealed `frn_index` tail) only; the tail stays unsorted until
    /// [`seal_batch`](Self::seal_batch) (or an automatic seal every
    /// [`FRN_AUTO_SEAL`] pushes), and the `by_name` tail stays unsorted until
    /// [`rebuild_by_name`](Self::rebuild_by_name). Scan batches must call
    /// `finalize` + `rebuild_by_name` once at batch end (which also clears
    /// [`pending`](Self::pending_len)). Never routes through `pending`:
    /// one-by-one sorted inserts would be O(n²) over a full scan.
    pub fn push(&mut self, vol: u8, frn: u64, parent_frn: u64, name: &str, flags: u16) -> EntryId {
        let (off, len) = self.names.append(name);
        debug_assert!(self.entries.len() < u32::MAX as usize);
        let id = self.entries.len() as EntryId;
        self.entries.push(Entry {
            frn,
            parent_frn,
            name_off: off,
            name_len: len,
            flags,
        });
        self.entry_vol.push(vol);
        self.by_name.push(id);
        self.by_name_dirty.store(true, Ordering::Relaxed);
        self.bump_mutation();
        self.note_arena_append(off, name.as_bytes());
        self.frn_append(id, FRN_AUTO_SEAL);
        id
    }

    /// Append a fresh (highest) id to the unsealed `frn_index` tail, sealing
    /// it into a sorted run once it reaches `seal_at` ids. Shared by scan
    /// pushes and live creates/renames: O(1) amortized, never a mid-array
    /// insert, so a burst of creates can't turn into O(n) memmoves.
    fn frn_append(&mut self, id: EntryId, seal_at: usize) {
        self.frn_index.push(id);
        self.frn_dirty.store(true, Ordering::Relaxed);
        if self.frn_index.len() - self.frn_sealed_len() >= seal_at {
            self.seal_frn_tail();
        }
    }

    /// Sort the unsealed `frn_index` tail into a sealed run (no-op when
    /// empty), then merge runs past the bound. Called automatically every
    /// [`FRN_AUTO_SEAL`] pushes; call explicitly after each scan flush batch
    /// (before releasing the lock) via [`seal_batch`](Self::seal_batch) so
    /// concurrent lookups stay logarithmic.
    fn seal_frn_tail(&mut self) {
        let start = self.frn_sealed_len();
        let end = self.frn_index.len();
        if start >= end {
            return;
        }
        self.bump_mutation();
        let entries = &self.entries;
        let vols = &self.entry_vol;
        self.frn_index[start..end].sort_by_key(|&id| frn_key_of(entries, vols, id));
        self.frn_runs.push(start..end);
        while self.frn_runs.len() > FRN_MAX_RUNS {
            self.merge_smallest_frn_runs();
        }
        // A lone run covering everything is globally sorted again.
        if self.frn_runs.len() <= 1 && self.frn_sealed_len() == self.frn_index.len() {
            self.frn_dirty.store(false, Ordering::Relaxed);
        }
    }

    /// Seal the current `frn_index` tail into a sorted run; no-op when empty.
    /// The daemon calls this after each scan flush batch (before releasing
    /// the lock) so concurrent `lookup`/`path` calls binary-search runs
    /// instead of linear-scanning a growing tail.
    pub fn seal_batch(&mut self) {
        self.seal_frn_tail();
    }

    /// Seal the tail and fold it into the newest runs while the pair stays
    /// under [`FRN_LIVE_MERGE`] ids. The journal path calls this after every
    /// poll: sealing alone left one run per poll (up to [`FRN_MAX_RUNS`]),
    /// and every parent lookup binary-searched all of them.
    pub fn seal_live(&mut self) {
        self.seal_frn_tail();
        while let [.., a, b] = self.frn_runs.as_slice() {
            if a.len() + b.len() > FRN_LIVE_MERGE {
                break;
            }
            self.merge_frn_pair(self.frn_runs.len() - 2);
        }
        if self.frn_runs.len() <= 1 && self.frn_sealed_len() == self.frn_index.len() {
            self.frn_dirty.store(false, Ordering::Relaxed);
        }
    }

    /// Merge the smallest adjacent pair of sealed runs (linear two-way merge
    /// by `(vol, frn, id)`). Keeps the run count bounded with minimal work;
    /// merging adjacent runs preserves push-order contiguity, so newest-first
    /// lookup stays exact.
    fn merge_smallest_frn_runs(&mut self) {
        debug_assert!(self.frn_runs.len() > FRN_MAX_RUNS);
        let mut best = 0usize;
        let mut best_len = usize::MAX;
        for i in 0..self.frn_runs.len() - 1 {
            let len = (self.frn_runs[i].end - self.frn_runs[i].start)
                + (self.frn_runs[i + 1].end - self.frn_runs[i + 1].start);
            if len < best_len {
                best_len = len;
                best = i;
            }
        }
        self.merge_frn_pair(best);
    }

    /// Merge sealed runs `i` and `i + 1` into one sorted run.
    fn merge_frn_pair(&mut self, best: usize) {
        let lo = self.frn_runs[best].start;
        let mid = self.frn_runs[best].end;
        let hi = self.frn_runs[best + 1].end;
        debug_assert_eq!(mid, self.frn_runs[best + 1].start);
        let merged = self.merge_frn_sorted(&self.frn_index[lo..mid], &self.frn_index[mid..hi]);
        self.frn_index[lo..hi].copy_from_slice(&merged);
        self.frn_runs[best].end = hi;
        self.frn_runs.remove(best + 1);
    }

    /// End of sealed `frn_index` coverage (start of the unsealed tail).
    fn frn_sealed_len(&self) -> usize {
        self.frn_runs.last().map_or(0, |r| r.end)
    }

    /// Number of entries including tombstones.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when there are no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Entry by id, or `None` when out of range.
    #[must_use]
    pub fn entry(&self, id: EntryId) -> Option<&Entry> {
        self.entries.get(id as usize)
    }

    /// Name by id, or `None` when out of range / corrupt.
    #[must_use]
    pub fn name(&self, id: EntryId) -> Option<&str> {
        let e = self.entries.get(id as usize)?;
        self.names.get(e.name_off, e.name_len)
    }

    /// Name slice for an entry value.
    #[must_use]
    pub fn name_of(&self, entry: &Entry) -> &str {
        self.names.get(entry.name_off, entry.name_len).unwrap_or("")
    }

    /// Name bytes for an entry value, without UTF-8 validation (search hot
    /// path; falls back to empty on out-of-bounds, mirroring [`name_of`](Self::name_of)).
    #[must_use]
    pub(crate) fn name_bytes_of(&self, entry: &Entry) -> &[u8] {
        self.names
            .get_bytes(entry.name_off, entry.name_len)
            .unwrap_or(b"")
    }

    /// Volume index for an entry id.
    #[must_use]
    pub fn volume_of(&self, id: EntryId) -> Option<u8> {
        self.entry_vol.get(id as usize).copied()
    }

    /// Entry ids sorted case-insensitively by name: the snapshot from the
    /// last [`rebuild_by_name`](Self::rebuild_by_name), PLUS ids appended by
    /// [`push`](Self::push) since (unsorted tail — only while
    /// [`by_name_is_fresh`](Self::by_name_is_fresh) is false). Ids appended
    /// by [`apply`](Self::apply) live in [`pending`](Self::pending_len), not
    /// here; name-ordered search merge-walks both.
    #[must_use]
    pub fn by_name(&self) -> &[EntryId] {
        &self.by_name
    }

    /// Ids appended by [`apply`](Self::apply) since the last
    /// [`rebuild_by_name`](Self::rebuild_by_name), sorted by
    /// `(fold(name), id)`. Empty after a rebuild / compact / load.
    pub(crate) fn pending(&self) -> &[EntryId] {
        &self.pending
    }

    /// Number of ids waiting in [`pending`](Self::pending_len) (live-update
    /// inserts since the last rebuild). Useful for observability; the daemon
    /// rebuilds/compacts on its own schedule.
    #[must_use]
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// Compaction / load generation. `compact()` renumbers every `EntryId`,
    /// so a cached `prev` hit list is only valid while this is unchanged.
    #[must_use]
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Content-change counter: bumps on every entry/sorted-array mutation
    /// (`push`, `apply`, `compact`, rebuilds, loads). Equal values mean the
    /// entries are unchanged (volume records and policy are not covered).
    #[must_use]
    pub fn mutation(&self) -> u64 {
        self.mutation
    }

    /// True when name-ordered search may use the `by_name + pending` merge
    /// walk: no scan [`push`](Self::push)es since the last rebuild (those
    /// leave an unsorted `by_name` tail) and at most [`PENDING_MAX`] live
    /// updates waiting in `pending`. False during scans (collect+sort
    /// fallback, as before) and once the pending burst exceeds the bound
    /// (the next rebuild folds it in). Note this can be true with a non-empty
    /// `pending`: [`save`](Self::save) still persists the merged view.
    #[must_use]
    pub fn by_name_is_fresh(&self) -> bool {
        !self.by_name_dirty.load(Ordering::Relaxed) && self.pending.len() <= PENDING_MAX
    }

    /// Number of tombstoned entries.
    #[must_use]
    pub fn tombstone_count(&self) -> usize {
        self.entries.iter().filter(|e| e.is_tombstone()).count()
    }

    /// Fraction of entries that are tombstones (compaction trigger).
    #[must_use]
    pub fn garbage_ratio(&self) -> f64 {
        if self.entries.is_empty() {
            0.0
        } else {
            self.tombstone_count() as f64 / self.entries.len() as f64
        }
    }

    /// Approximate RAM footprint: `entries * 24 + arena + by_name * 4 +
    /// frn_index * 4 + entry_vol * 1 + pending * 4 + arena block bitsets`
    /// (honest, no map).
    #[must_use]
    pub fn memory_usage(&self) -> usize {
        self.entries.len() * size_of::<Entry>()
            + self.names.len()
            + self.by_name.len() * size_of::<EntryId>()
            + self.frn_index.len() * size_of::<EntryId>()
            + self.entry_vol.len()
            + self.pending.len() * size_of::<EntryId>()
            + self.arena_blocks.len() * size_of::<[u64; 4]>()
    }

    /// Byte-level memory breakdown for observability (`flk status`).
    /// `total_bytes()` equals [`memory_usage`](Self::memory_usage);
    /// `tombstone_bytes` is an informational subset of `entries_bytes`
    /// (dead 24-byte slots awaiting [`compact`](Self::compact)), not an
    /// additional term.
    #[must_use]
    pub fn memory_breakdown(&self) -> MemoryBreakdown {
        MemoryBreakdown {
            entries_bytes: self.entries.len() as u64 * size_of::<Entry>() as u64,
            arena_bytes: self.names.len() as u64,
            by_name_bytes: self.by_name.len() as u64 * size_of::<EntryId>() as u64,
            frn_index_bytes: self.frn_index.len() as u64 * size_of::<EntryId>() as u64,
            entry_vol_bytes: self.entry_vol.len() as u64,
            pending_bytes: self.pending.len() as u64 * size_of::<EntryId>() as u64,
            arena_aux_bytes: self.arena_blocks.len() as u64 * size_of::<[u64; 4]>() as u64,
            tombstone_bytes: self.tombstone_count() as u64 * size_of::<Entry>() as u64,
        }
    }

    /// Sort key of an entry id: `(volume, frn, id)`.
    ///
    /// The trailing id keeps the key unique when a rename/revive leaves two
    /// ids sharing one FRN (tombstoned old + live new): the group stays
    /// sorted newest-last, and lookup takes the last id of the group.
    /// Single-FRN indexes sort exactly as before.
    fn frn_key(&self, id: EntryId) -> (u8, u64, EntryId) {
        frn_key_of(&self.entries, &self.entry_vol, id)
    }

    /// Fresh sorted `frn_index` without installing it (used by `save` when
    /// dirty, so the file always carries a sorted, complete index).
    ///
    /// The sealed runs are already sorted, so only the (capped) tail is
    /// sorted and everything is merged smallest-pair-first: the big base
    /// run is copied through once instead of re-sorting millions of keys.
    /// Falls back to a full sort if `frn_index` does not cover every entry.
    pub(crate) fn sorted_frn_index(&self) -> Vec<EntryId> {
        let n = self.entries.len();
        let sealed = self.frn_sealed_len();
        if self.frn_index.len() != n || sealed > n {
            let mut ids: Vec<EntryId> = (0..n as EntryId).collect();
            ids.sort_by_key(|&a| self.frn_key(a));
            return ids;
        }
        let mut parts: Vec<Vec<EntryId>> = self
            .frn_runs
            .iter()
            .map(|r| self.frn_index[r.clone()].to_vec())
            .collect();
        let mut tail = self.frn_index[sealed..].to_vec();
        if !tail.is_empty() {
            tail.sort_by_key(|&id| self.frn_key(id));
            parts.push(tail);
        }
        while parts.len() > 1 {
            parts.sort_by_key(|p| std::cmp::Reverse(p.len()));
            let a = parts.pop().unwrap_or_default();
            let b = parts.pop().unwrap_or_default();
            parts.push(self.merge_frn_sorted(&a, &b));
        }
        parts.pop().unwrap_or_default()
    }

    /// Two-way merge of sorted `frn_index` slices by `(vol, frn, id)`.
    fn merge_frn_sorted(&self, left: &[EntryId], right: &[EntryId]) -> Vec<EntryId> {
        merge_sorted(left, right, |a, b| self.frn_key(a).cmp(&self.frn_key(b)))
    }

    /// Rebuild `frn_index` in one sort; clears `frn_dirty` and collapses any
    /// runs into a single run covering everything.
    pub fn finalize(&mut self) {
        self.frn_index = self.sorted_frn_index();
        self.note_single_frn_run();
        self.frn_dirty.store(false, Ordering::Relaxed);
        self.bump_mutation();
    }

    /// Record that `frn_index` is exactly one sorted run covering everything
    /// (used after whole-array sorts and sorted inserts).
    pub(crate) fn note_single_frn_run(&mut self) {
        let n = self.frn_index.len();
        self.frn_runs.clear();
        if n > 0 {
            self.frn_runs.push(0..n);
        }
    }

    /// False when `push` ran since the last [`finalize`](Self::finalize) /
    /// [`seal_batch`](Self::seal_batch) single-run restore, i.e. `frn_index`
    /// holds an unsealed tail and/or several runs. [`apply`] and
    /// [`lookup`](Self::lookup) tolerate it ([`resolve`](Self::resolve) queries
    /// runs plus the capped tail directly); call [`finalize`](Self::finalize)
    /// once per scan batch end.
    #[must_use]
    pub fn frn_is_fresh(&self) -> bool {
        !self.frn_dirty.load(Ordering::Relaxed)
    }

    /// Resolve `(vol, frn)` to an entry id: newest id of the group wins
    /// (rename/revive groups stay newest-last). The unsealed tail holds ids
    /// newer than every sealed run, so a tail hit is the global newest and
    /// short-circuits; otherwise sealed runs are binary-searched newest-first
    /// with last-in-group per run. The tail scan is linear but capped at
    /// [`FRN_AUTO_SEAL`] ids by push-time sealing, so no O(n) lookup can
    /// grow without bound — including mid-scan, where the old code fell back
    /// to a full linear scan per lookup.
    ///
    /// `pub(crate)` for the search chain-memo walk, which resolves each
    /// distinct parent once per query (plus a thread-local sibling cache).
    pub(crate) fn resolve(&self, vol: u8, frn: u64) -> Option<EntryId> {
        let sealed = self.frn_sealed_len().min(self.frn_index.len());
        for &id in self.frn_index[sealed..].iter().rev() {
            if self.entry_vol.get(id as usize).copied().unwrap_or(0) != vol {
                continue;
            }
            if self.entries.get(id as usize).is_some_and(|e| e.frn == frn) {
                return Some(id);
            }
        }
        for range in self.frn_runs.iter().rev() {
            let Some(run) = self.frn_index.get(range.clone()) else {
                continue;
            };
            if let Some(id) = self.frn_group_latest(run, vol, frn) {
                return Some(id);
            }
        }
        None
    }

    /// Newest id of the `(vol, frn)` group inside one sorted run slice, if
    /// the group is present.
    fn frn_group_latest(&self, run: &[EntryId], vol: u8, frn: u64) -> Option<EntryId> {
        let pos = run.partition_point(|&id| self.frn_key(id) < (vol, frn, EntryId::MAX));
        if pos == 0 {
            return None;
        }
        let id = run[pos - 1];
        let (v, f, _) = self.frn_key(id);
        ((v, f) == (vol, frn)).then_some(id)
    }

    /// Rebuild `L:\dir\sub\name` by walking `parent_frn` links.
    ///
    /// The root entry's own name is never included (`path(root)` is `L:\`).
    /// Stops at the root (self-parent FRN), at a missing parent, or after
    /// [`MAX_PATH_HOPS`] hops (cycle guard). Returns an empty string for an
    /// unknown id.
    #[must_use]
    pub fn path(&self, id: EntryId) -> String {
        if self.entries.get(id as usize).is_none() {
            return String::new();
        }
        let vol = self.entry_vol.get(id as usize).copied().unwrap_or(0);
        let letter = self.volumes.get(vol as usize).map_or('?', |v| v.letter);
        let mut rev: Vec<&str> = Vec::new();
        let mut cur_id = id;
        let mut hops = 0u32;
        while let Some(cur) = self.entries.get(cur_id as usize) {
            if hops >= MAX_PATH_HOPS {
                break;
            }
            hops += 1;
            let is_root = cur.frn == cur.parent_frn;
            if !is_root {
                if let Some(n) = self.names.get(cur.name_off, cur.name_len) {
                    if !n.is_empty() {
                        rev.push(n);
                    }
                }
            } else {
                break;
            }
            let cur_vol = self.entry_vol.get(cur_id as usize).copied().unwrap_or(vol);
            match self.resolve(cur_vol, cur.parent_frn) {
                Some(pid) if pid != cur_id => cur_id = pid,
                _ => break,
            }
        }
        rev.reverse();
        if rev.is_empty() {
            format!("{letter}:\\")
        } else {
            format!("{letter}:\\{}", rev.join("\\"))
        }
    }

    /// Apply one live-update event for volume `vol`.
    ///
    /// Lookups go through [`resolve`](Self::resolve) (sorted runs plus the
    /// capped unsealed tail); new ids append to the `frn_index` tail like
    /// scan [`push`](Self::push)es, sealing into sorted runs every
    /// [`FRN_APPLY_SEAL`] ids. Callers applying a batch should call
    /// [`seal_batch`](Self::seal_batch) at the batch end so the next batch's
    /// lookups stay logarithmic. No per-event O(n) work.
    ///
    /// `by_name` is never touched here, so it stays a valid sorted snapshot:
    /// genuinely new ids go to [`pending`](Self::pending_len) (binary-search
    /// insert by `(fold(name), id)`), and any event that would change an
    /// existing entry's name (`Create` hitting a live or tombstoned FRN,
    /// `Rename` hitting any FRN) tombstones the old id and appends a NEW
    /// entry with the same frn. The FRN group keeps every id newest-last, so
    /// [`lookup`](Self::lookup) resolves to the new id. In-place name
    /// rewrites would silently break the snapshot's position, hence the
    /// tombstone+new rule (uniform even when the name is unchanged: the new
    /// id alone changes the `(fold, id)` key). `Delete` just tombstones;
    /// `Update` only touches flags, so order is unaffected and neither list
    /// changes. Tombstoned ids stay in the index until
    /// [`compact`](Self::compact).
    pub fn apply(&mut self, vol: u8, ev: IndexEvent<'_>) {
        self.bump_mutation();
        match ev {
            IndexEvent::Create {
                frn,
                parent_frn,
                name,
                flags,
            } => {
                let flags = flags & !TOMBSTONE;
                let old = self.resolve(vol, frn);
                // Upsert: retire the previous id for this FRN (live or
                // tombstoned) and append a fresh entry; the FRN group keeps
                // every id newest-last for lookup.
                if let Some(old) = old {
                    if let Some(e) = self.entries.get_mut(old as usize) {
                        e.flags |= TOMBSTONE;
                    }
                }
                let new = self.append_live(vol, frn, parent_frn, name, flags);
                self.frn_append(new, FRN_APPLY_SEAL);
                self.pending_insert(new);
            }
            IndexEvent::Delete { frn } => {
                if let Some(id) = self.resolve(vol, frn) {
                    if let Some(e) = self.entries.get_mut(id as usize) {
                        e.flags |= TOMBSTONE;
                    }
                }
            }
            IndexEvent::Rename {
                frn,
                parent_frn,
                name,
            } => {
                let old = self.resolve(vol, frn);
                // New name, same FRN (0 for a journal-gap miss): retire the
                // previous id — keeping its flags, so renaming a tombstone
                // does NOT revive it — and append the new name as a fresh
                // entry in the same FRN group.
                let flags =
                    old.map_or(0, |id| self.entries.get(id as usize).map_or(0, |e| e.flags));
                if let Some(old) = old {
                    if let Some(e) = self.entries.get_mut(old as usize) {
                        e.flags |= TOMBSTONE;
                    }
                }
                let new = self.append_live(vol, frn, parent_frn, name, flags);
                self.frn_append(new, FRN_APPLY_SEAL);
                self.pending_insert(new);
            }
            IndexEvent::Update { frn, flags } => {
                if let Some(id) = self.resolve(vol, frn) {
                    if let Some(e) = self.entries.get_mut(id as usize) {
                        let tomb = e.flags & TOMBSTONE;
                        e.flags = (flags & !TOMBSTONE) | tomb;
                    }
                }
            }
        }
    }

    /// Append one live entry (arena + `entries` + `entry_vol`); returns its
    /// id. Touches neither lookup array: the caller re-points or inserts into
    /// `frn_index` and registers `pending`.
    fn append_live(
        &mut self,
        vol: u8,
        frn: u64,
        parent_frn: u64,
        name: &str,
        flags: u16,
    ) -> EntryId {
        let (off, len) = self.names.append(name);
        debug_assert!(self.entries.len() < u32::MAX as usize);
        let id = self.entries.len() as EntryId;
        self.entries.push(Entry {
            frn,
            parent_frn,
            name_off: off,
            name_len: len,
            flags,
        });
        self.entry_vol.push(vol);
        self.note_arena_append(off, name.as_bytes());
        id
    }

    /// Insert `id` into `pending`, keeping it sorted by `(fold(name), id)`.
    fn pending_insert(&mut self, id: EntryId) {
        let nb = self
            .entries
            .get(id as usize)
            .and_then(|e| self.names.get_bytes(e.name_off, e.name_len))
            .unwrap_or(b"");
        let pos = self
            .pending
            .binary_search_by(|&other| {
                let ob = self
                    .entries
                    .get(other as usize)
                    .and_then(|e| self.names.get_bytes(e.name_off, e.name_len))
                    .unwrap_or(b"");
                // Probe-vs-target order (like `other.cmp(target)`).
                cmp_folded_bytes(ob, nb).then(other.cmp(&id))
            })
            .unwrap_or_else(|pos| pos);
        self.pending.insert(pos, id);
    }

    /// Sort [`by_name`](Self::by_name) case-insensitively and fold in
    /// [`pending`](Self::pending_len), clearing it. When no scan
    /// [`push`](Self::push)es are waiting (both lists already sorted) this is
    /// a linear merge with no per-entry allocation; with an unsorted push
    /// tail it falls back to the full [`sorted_by_name`](Self::sorted_by_name)
    /// sort. Also rebuilds `frn_index` when dirty (same as
    /// [`finalize`](Self::finalize); skipped when already fresh) so one call
    /// freshens both.
    pub fn rebuild_by_name(&mut self) {
        if self.by_name_dirty.load(Ordering::Relaxed) {
            self.by_name = self.sorted_by_name();
        } else {
            self.by_name = self.merged_by_name();
        }
        self.pending.clear();
        self.by_name_dirty.store(false, Ordering::Relaxed);
        self.bump_mutation();
        if self.frn_dirty.load(Ordering::Relaxed) {
            self.finalize();
        }
    }

    /// Detached copy of the sorted arrays: the merged `by_name` (full sort
    /// when scan pushes are waiting, linear merge of the snapshot with
    /// `pending` otherwise — exactly what [`rebuild_by_name`](Self::rebuild_by_name)
    /// would install) plus a single-run `frn_index` when the live one holds
    /// several runs or an unsealed tail (cloned as-is when already single-run).
    /// Computing this is the expensive part of a rebuild; run it off the
    /// write lock, then commit with [`install_sorted`](Self::install_sorted).
    /// Read-only: the index is untouched and stays queryable throughout.
    #[must_use]
    pub fn sorted_snapshot(&self) -> SortedSnapshot {
        let by_name = if self.by_name_dirty.load(Ordering::Relaxed) {
            self.sorted_by_name()
        } else {
            self.merged_by_name()
        };
        let single_run = !self.frn_dirty.load(Ordering::Relaxed)
            && self.frn_runs.len() <= 1
            && self.frn_sealed_len() == self.frn_index.len();
        let (frn_index, frn_runs) = if single_run {
            (self.frn_index.clone(), self.frn_runs.clone())
        } else {
            let frn_index = self.sorted_frn_index();
            let mut frn_runs = Vec::new();
            if !frn_index.is_empty() {
                frn_runs.push(0..frn_index.len());
            }
            (frn_index, frn_runs)
        };
        SortedSnapshot {
            by_name,
            frn_index,
            frn_runs,
            mutation: self.mutation,
        }
    }

    /// Commit a [`sorted_snapshot`](Self::sorted_snapshot): O(1) swap of the
    /// sorted arrays (plus absorbing `pending`, which the snapshot already
    /// merged) when no [`push`](Self::push) / [`apply`](Self::apply) /
    /// [`compact`](Self::compact) / rebuild / finalize / seal / load happened
    /// since the snapshot — detected via the mutation stamp. Returns `true`
    /// and installs on a match; returns `false` and leaves the index
    /// unchanged on a mismatch (caller falls back to [`rebuild_by_name`](Self::rebuild_by_name)).
    pub fn install_sorted(&mut self, s: SortedSnapshot) -> bool {
        if s.mutation != self.mutation {
            return false;
        }
        self.by_name = s.by_name;
        self.frn_index = s.frn_index;
        self.frn_runs = s.frn_runs;
        self.pending.clear();
        self.by_name_dirty.store(false, Ordering::Relaxed);
        self.frn_dirty.store(false, Ordering::Relaxed);
        true
    }

    /// True for ids that must survive a rebuild: in range and not tombstoned.
    fn is_live_id(&self, id: EntryId) -> bool {
        self.entries
            .get(id as usize)
            .is_some_and(|e| !e.is_tombstone())
    }

    /// Name-order key comparison of two ids, `(fold(name), id)`, without
    /// allocating (see [`cmp_folded_bytes`](crate::fold::cmp_folded_bytes)).
    /// Exactly the full-sort key order.
    fn cmp_ids_folded(&self, a: EntryId, b: EntryId) -> std::cmp::Ordering {
        let na = self
            .entries
            .get(a as usize)
            .and_then(|e| self.names.get_bytes(e.name_off, e.name_len))
            .unwrap_or(b"");
        let nb = self
            .entries
            .get(b as usize)
            .and_then(|e| self.names.get_bytes(e.name_off, e.name_len))
            .unwrap_or(b"");
        cmp_folded_bytes(na, nb).then(a.cmp(&b))
    }

    /// Linear merge of the sorted `by_name` snapshot with the sorted
    /// `pending` list into a fresh `Vec<u32>`, dropping tombstoned ids from
    /// either list (free garbage removal). Only valid when `by_name` is
    /// sorted, i.e. no [`push`](Self::push)es since the last rebuild —
    /// callers must take the [`sorted_by_name`](Self::sorted_by_name) full
    /// sort otherwise. Every live pending id sorts after every snapshot id
    /// on fold ties (pending ids are always newer), so the merge reproduces
    /// the full-sort `(fold(name), id)` order exactly.
    pub(crate) fn merged_by_name(&self) -> Vec<EntryId> {
        let mut out = Vec::with_capacity(self.by_name.len() + self.pending.len());
        let mut i = 0usize;
        let mut j = 0usize;
        loop {
            while i < self.by_name.len() && !self.is_live_id(self.by_name[i]) {
                i += 1;
            }
            while j < self.pending.len() && !self.is_live_id(self.pending[j]) {
                j += 1;
            }
            let (Some(&aid), Some(&pid)) = (self.by_name.get(i), self.pending.get(j)) else {
                break;
            };
            if self.cmp_ids_folded(aid, pid) != std::cmp::Ordering::Greater {
                out.push(aid);
                i += 1;
            } else {
                out.push(pid);
                j += 1;
            }
        }
        while i < self.by_name.len() {
            let id = self.by_name[i];
            i += 1;
            if self.is_live_id(id) {
                out.push(id);
            }
        }
        while j < self.pending.len() {
            let id = self.pending[j];
            j += 1;
            if self.is_live_id(id) {
                out.push(id);
            }
        }
        out
    }

    /// Validate a persisted `by_name` for use as-is: every id in range, no
    /// duplicates, sorted by `(fold, id)`, and every live id present exactly
    /// once (tombstoned ids may be present or absent — old writers persist
    /// them, merged writers drop them; both load). Anything else → the caller
    /// regenerates all ids and rebuilds. O(n) with one bitset, no per-entry
    /// allocation.
    pub(crate) fn validate_loaded_by_name(&self, by_name: &[EntryId]) -> bool {
        let n = self.entries.len();
        let mut seen = vec![false; n];
        let mut prev: Option<EntryId> = None;
        for &id in by_name {
            let i = id as usize;
            if i >= n || seen[i] {
                return false;
            }
            seen[i] = true;
            if let Some(p) = prev {
                if self.cmp_ids_folded(p, id) == std::cmp::Ordering::Greater {
                    return false;
                }
            }
            prev = Some(id);
        }
        for (i, e) in self.entries.iter().enumerate() {
            if !e.is_tombstone() && !seen[i] {
                return false;
            }
        }
        true
    }

    /// Install a persisted `by_name`: use as-is when it validates (see
    /// [`validate_loaded_by_name`](Self::validate_loaded_by_name)), else warn
    /// and regenerate every id with the dirty flag set so the caller rebuilds
    /// (full sort) instead of trusting it. Never rejects the file for this.
    pub(crate) fn take_loaded_by_name(&mut self, by_name: Vec<EntryId>) {
        self.bump_mutation();
        if self.validate_loaded_by_name(&by_name) {
            self.by_name = by_name;
        } else {
            tracing::warn!(
                target: "floki-core",
                by_name_len = by_name.len(),
                entries = self.entries.len(),
                "by_name missing/short/inconsistent; rebuilding from entries"
            );
            self.by_name = (0..self.entries.len() as EntryId).collect();
            self.by_name_dirty.store(true, Ordering::Relaxed);
        }
    }

    /// Fresh sorted `by_name` without installing it: live ids from the
    /// `by_name` snapshot (push tails included) plus live ids from `pending`,
    /// tombstones dropped. Used by [`rebuild_by_name`](Self::rebuild_by_name)
    /// and by [`save`](Self::save) when the persisted copy must include
    /// un-rebuilt updates.
    pub(crate) fn sorted_by_name(&self) -> Vec<EntryId> {
        let mut keyed: Vec<(String, EntryId)> = self
            .by_name
            .iter()
            .chain(self.pending.iter())
            .filter_map(|&id| {
                let e = *self.entries.get(id as usize)?;
                if e.is_tombstone() {
                    return None;
                }
                let n = self.names.get(e.name_off, e.name_len).unwrap_or("");
                Some((fold(n), id))
            })
            .collect();
        keyed.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
        keyed.into_iter().map(|(_, id)| id).collect()
    }

    /// Drop tombstones and rewrite the arena without garbage. Paths are
    /// FRN-based and stay valid, but every [`EntryId`](crate::EntryId) is
    /// renumbered: returns the remap (`remap[old_id] -> new_id`,
    /// [`EntryId::MAX`] for removed entries) and bumps
    /// [`epoch`](Self::epoch). Callers holding [`Hit`](crate::Hit)s must
    /// either translate them through the remap or drop them when the epoch
    /// changes. Also carries `frn_index` and the sorted `by_name` over to
    /// the new ids (O(n), no re-sort), and clears `pending` (folded into the
    /// fresh snapshot).
    ///
    /// Remap note for the tombstone+new [`apply`](Self::apply) rule: a
    /// renamed/revived FRN occupies TWO slots until compaction (tombstoned
    /// old id -> `MAX`, new id -> its compacted position, like any live
    /// entry). Translating a pre-compact hit that pointed at the OLD id of a
    /// renamed entry therefore yields `MAX`: re-resolve via
    /// [`lookup`](Self::lookup) (FRN-based, like [`path`](Self::path)).
    pub fn compact(&mut self) -> Vec<EntryId> {
        self.retain_entries(|e, vol| (!e.is_tombstone()).then_some(vol))
    }

    /// Keep the entries for which `keep(entry, vol)` returns their (possibly
    /// shifted) volume index, rewriting the arena without garbage and
    /// renumbering ids in order. Returns the remap (`remap[old] -> new`,
    /// [`EntryId::MAX`] for dropped entries) and bumps the epoch.
    ///
    /// Both sorted arrays are carried over instead of re-sorted: the remap
    /// is monotonic and `keep` may only shift volume indexes monotonically,
    /// so `(fold(name), id)` and `(vol, frn, id)` orders survive the
    /// renumbering — filter + translate is O(n). (A full String-keyed
    /// re-sort here took 7-14 s under the write lock at 9.3M entries.)
    fn retain_entries(&mut self, keep: impl Fn(&Entry, u8) -> Option<u8>) -> Vec<EntryId> {
        let (arrays, remap) = self.retained(keep);
        self.install_arrays(arrays);
        remap
    }

    /// Read-only half of [`retain_entries`](Self::retain_entries): the kept
    /// arrays (stamped with the current mutation; `stale` counts dropped
    /// entries) plus the remap.
    fn retained(&self, keep: impl Fn(&Entry, u8) -> Option<u8>) -> (VolumeSwap, Vec<EntryId>) {
        let by_name_sorted = if self.by_name_dirty.load(Ordering::Relaxed) {
            self.sorted_by_name()
        } else {
            self.merged_by_name()
        };
        let frn_sorted = self.sorted_frn_index();
        let mut remap: Vec<EntryId> = vec![EntryId::MAX; self.entries.len()];
        let mut entries = Vec::with_capacity(self.entries.len());
        let mut vols: Vec<u8> = Vec::with_capacity(self.entries.len());
        let mut arena = NameArena::new();
        let mut blocks: Vec<[u64; 4]> = Vec::new();
        for (i, e) in self.entries.iter().enumerate() {
            let Some(vol) = keep(e, self.entry_vol.get(i).copied().unwrap_or(0)) else {
                continue;
            };
            let name = self.names.get(e.name_off, e.name_len).unwrap_or("");
            let (off, len) = arena.append(name);
            Self::block_add_bytes(&mut blocks, off, name.as_bytes());
            remap[i] = entries.len() as EntryId;
            entries.push(Entry {
                frn: e.frn,
                parent_frn: e.parent_frn,
                name_off: off,
                name_len: len,
                flags: e.flags,
            });
            vols.push(vol);
        }
        let translate = |ids: Vec<EntryId>| -> Vec<EntryId> {
            ids.into_iter()
                .filter_map(|id| {
                    let new = remap.get(id as usize).copied()?;
                    (new != EntryId::MAX).then_some(new)
                })
                .collect()
        };
        // `by_name_sorted` already merged `pending` in and dropped
        // tombstones, so install simply clears pending.
        let arrays = VolumeSwap {
            by_name: translate(by_name_sorted),
            frn_index: translate(frn_sorted),
            stale: (self.entries.len() - entries.len()) as u64,
            fresh: 0,
            entries,
            entry_vol: vols,
            names: arena,
            arena_blocks: blocks,
            mutation: self.mutation,
        };
        (arrays, remap)
    }

    /// Swap in rebuilt arrays (no stamp check; callers do that).
    fn install_arrays(&mut self, s: VolumeSwap) {
        self.entries = s.entries;
        self.entry_vol = s.entry_vol;
        self.names = s.names;
        self.arena_blocks = s.arena_blocks;
        self.by_name = s.by_name;
        self.frn_index = s.frn_index;
        // Arena rebuilt by appending in new-id order: a monotonic prefix.
        self.arena_prefix_len = self.entries.len().min(u32::MAX as usize) as u32;
        self.pending.clear();
        self.note_single_frn_run();
        self.by_name_dirty.store(false, Ordering::Relaxed);
        self.frn_dirty.store(false, Ordering::Relaxed);
        self.bump_mutation();
        self.epoch += 1;
    }

    /// Build the index arrays with every entry of volume `vol` replaced by
    /// the entries of `staged` (a private rescan index; each staged entry
    /// becomes volume `vol`, and `skip(entry, name)` drops ones like NTFS
    /// metafiles). Tombstones are dropped too. Read-only: run it under a
    /// READ lock, then commit with [`install_volume_swap`](Self::install_volume_swap).
    ///
    /// Linear: both sides' sorted arrays are merged, never re-sorted,
    /// provided `staged` was sorted beforehand (`rebuild_by_name` +
    /// `finalize`; otherwise it is sorted here). The old tombstone + push +
    /// compact commit re-sorted every name under the write lock (8 s at 9M
    /// entries; searches stalled), and even this linear merge took 3 s there
    /// because the name compares hit the arena at random.
    #[must_use]
    pub fn volume_swap(
        &self,
        vol: u8,
        staged: &Index,
        skip: impl Fn(&Entry, &str) -> bool,
    ) -> VolumeSwap {
        let sorted = |ix: &Index| {
            let by_name = if ix.by_name_dirty.load(Ordering::Relaxed) {
                ix.sorted_by_name()
            } else {
                ix.merged_by_name()
            };
            (by_name, ix.sorted_frn_index())
        };
        let (old_names, old_frn) = sorted(self);
        let (new_names, new_frn) = sorted(staged);

        let total = self.entries.len() + staged.entries.len();
        let mut entries: Vec<Entry> = Vec::with_capacity(total);
        let mut entry_vol: Vec<u8> = Vec::with_capacity(total);
        let mut names = NameArena::new();
        let mut arena_blocks: Vec<[u64; 4]> = Vec::new();
        let mut take = |name: &str, e: &Entry, v: u8| -> EntryId {
            let (off, len) = names.append(name);
            Self::block_add_bytes(&mut arena_blocks, off, name.as_bytes());
            entries.push(Entry {
                frn: e.frn,
                parent_frn: e.parent_frn,
                name_off: off,
                name_len: len,
                flags: e.flags,
            });
            entry_vol.push(v);
            (entries.len() - 1) as EntryId
        };
        let mut old_remap: Vec<EntryId> = vec![EntryId::MAX; self.entries.len()];
        let mut stale = 0u64;
        for (i, e) in self.entries.iter().enumerate() {
            let v = self.entry_vol.get(i).copied().unwrap_or(0);
            if e.is_tombstone() {
                continue;
            }
            if v == vol {
                stale += 1;
                continue;
            }
            old_remap[i] = take(self.names.get(e.name_off, e.name_len).unwrap_or(""), e, v);
        }
        let mut new_remap: Vec<EntryId> = vec![EntryId::MAX; staged.entries.len()];
        let mut fresh = 0u64;
        for (i, e) in staged.entries.iter().enumerate() {
            let name = staged.names.get(e.name_off, e.name_len).unwrap_or("");
            if e.is_tombstone() || skip(e, name) {
                continue;
            }
            new_remap[i] = take(name, e, vol);
            fresh += 1;
        }
        // Each remap is monotonic and every staged id lands above every
        // kept one, so both sides stay sorted after translation and one
        // merge restores the `(fold(name), id)` and `(vol, frn, id)` orders.
        let translate = |ids: Vec<EntryId>, remap: &[EntryId]| -> Vec<EntryId> {
            ids.into_iter()
                .filter_map(|id| {
                    let new = remap.get(id as usize).copied()?;
                    (new != EntryId::MAX).then_some(new)
                })
                .collect()
        };
        let old_names = translate(old_names, &old_remap);
        let new_names = translate(new_names, &new_remap);
        let old_frn = translate(old_frn, &old_remap);
        let new_frn = translate(new_frn, &new_remap);
        let name_key = |id: EntryId| {
            let e = &entries[id as usize];
            names.get_bytes(e.name_off, e.name_len).unwrap_or(b"")
        };
        let by_name = merge_sorted(&old_names, &new_names, |a, b| {
            cmp_folded_bytes(name_key(a), name_key(b)).then(a.cmp(&b))
        });
        let frn_index = merge_sorted(&old_frn, &new_frn, |a, b| {
            frn_key_of(&entries, &entry_vol, a).cmp(&frn_key_of(&entries, &entry_vol, b))
        });
        VolumeSwap {
            entries,
            entry_vol,
            names,
            arena_blocks,
            by_name,
            frn_index,
            mutation: self.mutation,
            stale,
            fresh,
        }
    }

    /// Commit a [`volume_swap`](Self::volume_swap): O(1) swap of every array
    /// when nothing mutated the index since the swap was built (mutation
    /// stamp). Returns `false` and leaves the index unchanged otherwise;
    /// rebuild, or fall back to [`replace_volume`](Self::replace_volume).
    /// Renumbers ids like [`compact`](Self::compact) (bumps the epoch).
    pub fn install_volume_swap(&mut self, s: VolumeSwap) -> bool {
        if s.mutation != self.mutation {
            return false;
        }
        self.install_arrays(s);
        true
    }

    /// [`volume_swap`](Self::volume_swap) + install in one call, for callers
    /// already holding the write lock. Returns `(stale, fresh)`.
    pub fn replace_volume(
        &mut self,
        vol: u8,
        staged: &Index,
        skip: impl Fn(&Entry, &str) -> bool,
    ) -> (u64, u64) {
        let swap = self.volume_swap(vol, staged, skip);
        let counts = (swap.stale, swap.fresh);
        let installed = self.install_volume_swap(swap);
        debug_assert!(installed, "no mutation can interleave under &mut self");
        counts
    }

    /// Look up a live or tombstoned id by `(vol, frn)`.
    ///
    /// Newest id of the FRN group wins (see [`resolve`](Self::resolve));
    /// exact at every index state, including mid-scan.
    #[must_use]
    pub fn lookup(&self, vol: u8, frn: u64) -> Option<EntryId> {
        self.resolve(vol, frn)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::DIRECTORY;
    fn vol() -> Volume {
        Volume {
            letter: 'C',
            guid: [0; 16],
            journal_id: 1,
            next_usn: 0,
            root_frn: 100,
            enabled: true,
            monitor: true,
        }
    }

    fn three_level() -> Index {
        let mut ix = Index::new();
        ix.add_volume(vol());
        ix.push(0, 100, 100, "", DIRECTORY); // root
        ix.push(0, 101, 100, "a", DIRECTORY);
        ix.push(0, 102, 101, "b", DIRECTORY);
        ix.push(0, 103, 102, "file.txt", 0);
        ix
    }

    #[test]
    fn path_rebuild_three_levels() {
        let ix = three_level();
        assert_eq!(ix.path(3), "C:\\a\\b\\file.txt");
        assert_eq!(ix.path(1), "C:\\a");
        assert_eq!(ix.path(0), "C:\\");
        assert_eq!(ix.path(999), "");
    }

    #[test]
    fn cycle_guard_terminates() {
        let mut ix = Index::new();
        ix.add_volume(vol());
        ix.push(0, 1, 2, "x", 0);
        ix.push(0, 2, 1, "y", 0);
        let p = ix.path(0); // must terminate via hop bound / parent walk
        assert!(p.starts_with("C:\\"));
        assert!(p.len() < 20_000);
    }

    #[test]
    fn apply_create_delete_rename_move() {
        let mut ix = three_level();
        ix.apply(
            0,
            IndexEvent::Create {
                frn: 200,
                parent_frn: 101,
                name: "new.md",
                flags: 0,
            },
        );
        let id = ix.lookup(0, 200).unwrap();
        assert_eq!(ix.path(id), "C:\\a\\new.md");

        ix.apply(0, IndexEvent::Delete { frn: 200 });
        assert!(ix.entries[id as usize].is_tombstone());

        // revive via create with same frn: tombstone+new, lookup moves on.
        ix.apply(
            0,
            IndexEvent::Create {
                frn: 200,
                parent_frn: 102,
                name: "back.md",
                flags: 0,
            },
        );
        let id2 = ix.lookup(0, 200).unwrap();
        assert_ne!(id2, id);
        assert!(ix.entries[id as usize].is_tombstone());
        assert!(!ix.entries[id2 as usize].is_tombstone());
        assert_eq!(ix.path(id2), "C:\\a\\b\\back.md");

        // rename + parent move: another new id, old one retired.
        ix.apply(
            0,
            IndexEvent::Rename {
                frn: 200,
                parent_frn: 100,
                name: "moved.md",
            },
        );
        let id3 = ix.lookup(0, 200).unwrap();
        assert_ne!(id3, id2);
        assert!(ix.entries[id2 as usize].is_tombstone());
        assert_eq!(ix.path(id3), "C:\\moved.md");

        // update flags, tombstone preserved
        ix.apply(0, IndexEvent::Delete { frn: 200 });
        ix.apply(
            0,
            IndexEvent::Update {
                frn: 200,
                flags: DIRECTORY,
            },
        );
        let e = ix.entries[id3 as usize];
        assert!(e.is_tombstone());
        assert!(e.is_dir());
    }

    #[test]
    fn compact_removes_tombstones_keeps_paths() {
        let mut ix = three_level();
        ix.apply(0, IndexEvent::Delete { frn: 101 });
        assert_eq!(ix.tombstone_count(), 1);
        ix.compact();
        assert_eq!(ix.tombstone_count(), 0);
        assert_eq!(ix.len(), 3);
        // orphaned children keep best-effort paths
        assert!(ix.path(ix.lookup(0, 103).unwrap()).contains("file.txt"));
        // by_name sorted
        let names: Vec<String> = ix
            .by_name()
            .iter()
            .map(|&id| ix.name(id).unwrap().to_string())
            .collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted);
    }

    #[test]
    fn pending_sorted_insert_keeps_fresh() {
        let mut ix = three_level();
        ix.rebuild_by_name();
        assert!(ix.by_name_is_fresh());
        assert_eq!(ix.pending_len(), 0);
        for (i, n) in ["zebra", "apple", "Mango", "apple"].iter().enumerate() {
            ix.apply(
                0,
                IndexEvent::Create {
                    frn: 500 + i as u64,
                    parent_frn: 100,
                    name: n,
                    flags: 0,
                },
            );
        }
        // Live updates never dirty the snapshot: still fresh, pending sorted.
        assert!(ix.by_name_is_fresh());
        assert_eq!(ix.pending_len(), 4);
        let mut sorted = ix.pending().to_vec();
        sorted.sort_by(|&a, &b| ix.cmp_ids_folded(a, b));
        assert_eq!(sorted, ix.pending());
        // Deleting a pending id keeps freshness (tombstone filtered later).
        ix.apply(0, IndexEvent::Delete { frn: 501 });
        assert!(ix.by_name_is_fresh());
        assert_eq!(ix.pending_len(), 4);
    }

    #[test]
    fn merge_matches_full_resort() {
        let mut ix = Index::new();
        ix.add_volume(vol());
        ix.push(0, 1, 1, "", DIRECTORY);
        // Mixed case, unicode, duplicate folds, dots, spaces.
        let names = [
            "Zebra.txt",
            "apple",
            "Äpfel",
            "äBC",
            "Banana.DLL",
            "Zürich.md",
            "a",
            "A",
            "aA",
            "aa",
            ".hidden",
            "noext",
            "UPPER.DLL",
            "MiXeD.TxT",
            "résumé.pdf",
            "naïve.rs",
            "file with spaces.log",
            "archive.tar.gz",
        ];
        for (i, n) in names.iter().enumerate() {
            ix.push(0, 100 + i as u64, 1, n, 0);
        }
        ix.rebuild_by_name();
        // Mixed live updates, no pushes (merge path): creates incl. unicode
        // and fold-duplicates, renames (incl. of a pending id), deletes of
        // snapshot ids and of a pending id.
        for (i, n) in ["live_1.tmp", "Äpfelchen", "APPLE", "zebra.txt"]
            .iter()
            .enumerate()
        {
            ix.apply(
                0,
                IndexEvent::Create {
                    frn: 1000 + i as u64,
                    parent_frn: 1,
                    name: n,
                    flags: 0,
                },
            );
        }
        ix.apply(
            0,
            IndexEvent::Rename {
                frn: 100,
                parent_frn: 1,
                name: "renamed_aaa.txt",
            },
        );
        ix.apply(
            0,
            IndexEvent::Rename {
                frn: 1000, // rename of a pending id
                parent_frn: 1,
                name: "renamed_zzz.tmp",
            },
        );
        ix.apply(0, IndexEvent::Delete { frn: 101 }); // snapshot id
        ix.apply(0, IndexEvent::Delete { frn: 1001 }); // pending id
        assert!(!ix.by_name_dirty.load(std::sync::atomic::Ordering::Relaxed));
        // Same input, two algorithms: full Schwartzian re-sort vs merge.
        let full = ix.sorted_by_name();
        ix.rebuild_by_name();
        assert_eq!(ix.by_name, full);
        assert!(ix.pending.is_empty());
        assert!(ix.by_name_is_fresh());
        // And the merged snapshot is exactly the live set in (fold, id) order.
        let mut expect: Vec<EntryId> = (0..ix.len() as EntryId)
            .filter(|&id| ix.is_live_id(id))
            .collect();
        expect.sort_by(|&a, &b| ix.cmp_ids_folded(a, b));
        assert_eq!(ix.by_name, expect);
        // Push-dirty plus pending takes the full-sort path and agrees too.
        ix.push(0, 2000, 1, "aaa_scan_pushed.txt", 0);
        ix.apply(
            0,
            IndexEvent::Create {
                frn: 2001,
                parent_frn: 1,
                name: "zzz_live_applied.tmp",
                flags: 0,
            },
        );
        assert!(ix.by_name_dirty.load(std::sync::atomic::Ordering::Relaxed));
        let full = ix.sorted_by_name();
        ix.rebuild_by_name();
        assert_eq!(ix.by_name, full);
        assert!(ix.by_name_is_fresh());
    }

    /// Reference `frn_index`: every id, full sort by `(vol, frn, id)`.
    fn full_sort_frn(ix: &Index) -> Vec<EntryId> {
        let mut ids: Vec<EntryId> = (0..ix.entries.len() as EntryId).collect();
        ids.sort_by_key(|&a| ix.frn_key(a));
        ids
    }

    /// Two volumes, scan pushes, then live creates/renames/deletes across
    /// both with sealed runs, a waiting tail, and a non-empty `pending`.
    fn churned_two_volume_index() -> Index {
        let mut ix = Index::new();
        ix.add_volume(vol());
        ix.add_volume(Volume {
            letter: 'D',
            ..vol()
        });
        ix.push(0, 100, 100, "", DIRECTORY);
        ix.push(1, 100, 100, "", DIRECTORY);
        for i in 0..3_000u64 {
            ix.push(
                (i % 2) as u8,
                1_000 + i * 3,
                100,
                &format!("Name_{:05}", (i * 7919) % 3_000),
                0,
            );
        }
        ix.finalize();
        ix.rebuild_by_name();
        for i in 0..2_000u64 {
            let v = (i % 2) as u8;
            ix.apply(
                v,
                IndexEvent::Create {
                    frn: 1_001 + i * 3,
                    parent_frn: 100,
                    name: &format!("live_{:05}", (i * 31) % 2_000),
                    flags: 0,
                },
            );
            if i % 5 == 0 {
                ix.apply(
                    v,
                    IndexEvent::Rename {
                        frn: 1_000 + i * 3,
                        parent_frn: 100,
                        name: &format!("ren_{i}"),
                    },
                );
            }
            if i % 7 == 0 {
                ix.apply(v, IndexEvent::Delete { frn: 1_003 + i * 3 });
            }
            if i % 300 == 299 {
                ix.seal_batch();
            }
        }
        assert!(ix.pending_len() > 0);
        assert!(!ix.frn_is_fresh(), "fixture must leave runs + a tail");
        ix
    }

    /// The merged-runs `frn_index` rebuild equals a from-scratch full sort.
    #[test]
    fn sorted_frn_index_merge_equals_full_sort() {
        let ix = churned_two_volume_index();
        assert!(ix.frn_runs.len() > 1);
        assert_eq!(ix.sorted_frn_index(), full_sort_frn(&ix));
    }

    /// `compact` and `remove_volume` carry both sorted arrays over to the
    /// new ids without re-sorting; the result must equal a full rebuild,
    /// and every surviving FRN must still resolve to its newest name.
    #[test]
    fn retain_entries_keeps_sorted_arrays_exact() {
        let mut ix = churned_two_volume_index();
        let expect_names: Vec<(u8, u64, String)> = (0..ix.entries.len() as EntryId)
            .filter(|&id| !ix.entries[id as usize].is_tombstone())
            .map(|id| {
                let e = ix.entries[id as usize];
                (ix.entry_vol[id as usize], e.frn, ix.name_of(&e).to_owned())
            })
            .collect();
        ix.compact();
        assert_eq!(ix.tombstone_count(), 0);
        assert_eq!(ix.by_name, ix.sorted_by_name());
        assert_eq!(ix.frn_index, full_sort_frn(&ix));
        assert!(ix.frn_is_fresh() && ix.by_name_is_fresh());
        for (v, frn, name) in &expect_names {
            let id = ix.lookup(*v, *frn).expect("survivor resolves");
            assert_eq!(ix.name(id), Some(name.as_str()));
        }

        assert!(ix.remove_volume('C'));
        assert_eq!(ix.volumes.len(), 1);
        assert!(ix.entry_vol.iter().all(|&v| v == 0));
        assert_eq!(ix.by_name, ix.sorted_by_name());
        assert_eq!(ix.frn_index, full_sort_frn(&ix));
        for (v, frn, name) in expect_names.iter().filter(|(v, _, _)| *v == 1) {
            let id = ix.lookup(v - 1, *frn).expect("D survivor resolves");
            assert_eq!(ix.name(id), Some(name.as_str()));
        }
    }

    /// A rescan commit swaps one volume for a staged index by merging, not
    /// re-sorting: the result must equal a full rebuild, the other volume
    /// must be untouched, skipped entries must be gone, and every FRN must
    /// resolve on both volumes.
    #[test]
    fn replace_volume_merges_staged_exactly() {
        let mut ix = churned_two_volume_index();
        let keep_d: Vec<(u64, String)> = (0..ix.entries.len())
            .filter(|&i| ix.entry_vol[i] == 1 && !ix.entries[i].is_tombstone())
            .map(|i| (ix.entries[i].frn, ix.name_of(&ix.entries[i]).to_owned()))
            .collect();
        let stale_c = (0..ix.entries.len())
            .filter(|&i| ix.entry_vol[i] == 0 && !ix.entries[i].is_tombstone())
            .count() as u64;

        let mut staged = Index::new();
        staged.add_volume(vol());
        staged.push(0, 100, 100, "", DIRECTORY);
        for i in 0..1_500u64 {
            // Overlapping names with the old generation exercise fold ties.
            staged.push(
                0,
                5_000 + i,
                100,
                &format!("Name_{:05}", (i * 13) % 2_000),
                0,
            );
        }
        staged.push(0, 9_999, 100, "$Skip", 0);
        staged.rebuild_by_name();
        staged.finalize();

        // A swap built before another volume's tail applied must be refused.
        let raced = ix.volume_swap(0, &staged, |_, _| false);
        ix.apply(
            1,
            IndexEvent::Create {
                frn: 77_777,
                parent_frn: 100,
                name: "raced",
                flags: 0,
            },
        );
        assert!(!ix.install_volume_swap(raced), "stale swap refused");

        let (stale, fresh) = ix.replace_volume(0, &staged, |_, name| name.starts_with('$'));
        assert_eq!(stale, stale_c);
        assert_eq!(fresh, 1_501);
        assert_eq!(ix.tombstone_count(), 0);
        assert_eq!(ix.by_name, ix.sorted_by_name());
        assert_eq!(ix.frn_index, full_sort_frn(&ix));
        assert!(ix.frn_is_fresh() && ix.by_name_is_fresh());
        assert!(ix.lookup(0, 9_999).is_none(), "skipped entry absent");
        for i in 0..1_500u64 {
            let id = ix.lookup(0, 5_000 + i).expect("staged entry resolves");
            assert_eq!(
                ix.name(id),
                Some(format!("Name_{:05}", (i * 13) % 2_000).as_str())
            );
        }
        for (frn, name) in &keep_d {
            let id = ix.lookup(1, *frn).expect("other volume untouched");
            assert_eq!(ix.name(id), Some(name.as_str()));
        }
    }

    #[test]
    fn memory_breakdown_adds_up() {
        let mut ix = Index::new();
        ix.add_volume(vol());
        ix.push(0, 1, 1, "", DIRECTORY);
        ix.push(0, 2, 1, "b.txt", 0);
        ix.push(0, 3, 1, "a.txt", 0);
        ix.rebuild_by_name();
        ix.apply(
            0,
            IndexEvent::Create {
                frn: 4,
                parent_frn: 1,
                name: "c.txt",
                flags: 0,
            },
        );
        ix.apply(0, IndexEvent::Delete { frn: 3 });
        let b = ix.memory_breakdown();
        assert_eq!(b.entries_bytes, ix.entries.len() as u64 * 24);
        assert_eq!(b.arena_bytes, ix.names.len() as u64);
        assert_eq!(b.by_name_bytes, ix.by_name.len() as u64 * 4);
        assert_eq!(b.frn_index_bytes, ix.frn_index.len() as u64 * 4);
        assert_eq!(b.entry_vol_bytes, ix.entry_vol.len() as u64);
        assert_eq!(b.pending_bytes, ix.pending.len() as u64 * 4);
        assert_eq!(
            b.arena_aux_bytes,
            ix.arena_blocks.len() as u64 * size_of::<[u64; 4]>() as u64
        );
        // One 64 KiB block covers this tiny arena; presence bits match the
        // names byte-for-byte.
        assert_eq!(ix.arena_blocks.len(), 1);
        let mut expect = [0u64; 4];
        for &byte in ix.names.as_bytes() {
            expect[(byte >> 6) as usize] |= 1 << (byte & 63);
        }
        assert_eq!(ix.arena_blocks[0], expect);
        assert_eq!(b.tombstone_bytes, ix.tombstone_count() as u64 * 24);
        assert_eq!(b.total_bytes(), ix.memory_usage() as u64);
        assert_eq!(ix.tombstone_count(), 1);
    }

    #[test]
    fn helpers() {
        let mut ix = Index::new();
        assert!(ix.is_empty());
        ix.reserve(10);
        ix.add_volume(vol());
        ix.push(0, 100, 100, "", DIRECTORY);
        assert_eq!(ix.len(), 1);
        assert_eq!(ix.volume_of(0), Some(0));
        assert!(ix.volume_of(9).is_none());
        assert!(ix.garbage_ratio() < 0.01);
    }

    fn scan_fixture(n: u64) -> Index {
        // Flat scan-like index: root plus `n` files, FRNs pushed in order.
        let mut ix = Index::new();
        ix.add_volume(vol());
        ix.push(0, 1, 1, "", DIRECTORY);
        for i in 0..n {
            ix.push(0, 1000 + i, 1, &format!("f{i:05}.dat"), 0);
        }
        ix
    }

    #[test]
    fn frn_runs_seal_and_resolve() {
        let mut ix = scan_fixture(300);
        // No seal yet: everything is unsealed tail, still exactly right.
        assert!(ix.frn_runs.is_empty());
        for i in [0, 1, 150, 299u64] {
            assert_eq!(ix.lookup(0, 1000 + i), Some((i + 1) as EntryId));
        }
        assert_eq!(ix.lookup(0, 999_999), None);
        // Seal: one run, lookups unchanged.
        ix.seal_batch();
        assert_eq!(ix.frn_runs.len(), 1);
        assert_eq!(ix.frn_runs[0], 0..301);
        for i in [0, 1, 150, 299u64] {
            assert_eq!(ix.lookup(0, 1000 + i), Some((i + 1) as EntryId));
        }
        // More pushes + second seal: two runs, still exact.
        for i in 300..400u64 {
            ix.push(0, 2000 + i, 1, &format!("g{i:05}.dat"), 0);
        }
        ix.seal_batch();
        assert_eq!(ix.frn_runs.len(), 2);
        for i in [0, 150, 299u64] {
            assert_eq!(ix.lookup(0, 1000 + i), Some((i + 1) as EntryId));
        }
        for i in 300..400u64 {
            assert_eq!(ix.lookup(0, 2000 + i), Some((i + 1) as EntryId));
        }
        // finalize collapses to one run; results identical.
        ix.finalize();
        assert_eq!(ix.frn_runs, vec![0..ix.len()]);
        assert!(ix.frn_is_fresh());
        for i in [0, 150, 299u64] {
            assert_eq!(ix.lookup(0, 1000 + i), Some((i + 1) as EntryId));
        }
    }

    #[test]
    fn frn_runs_last_id_wins_across_runs() {
        // Same FRN pushed twice with a seal between (scan-dup shape): lookup
        // must return the newest id, like the rename/revive groups.
        let mut ix = Index::new();
        ix.add_volume(vol());
        ix.push(0, 1, 1, "", DIRECTORY);
        ix.push(0, 42, 1, "old.txt", 0);
        ix.seal_batch();
        ix.push(0, 42, 1, "new.txt", 0);
        // Newest wins while split across tail and sealed run, too.
        assert_eq!(ix.lookup(0, 42), Some(2));
        ix.seal_batch();
        assert_eq!(ix.frn_runs.len(), 2);
        assert_eq!(ix.lookup(0, 42), Some(2));
        // And after finalize.
        ix.finalize();
        assert_eq!(ix.lookup(0, 42), Some(2));
    }

    #[test]
    fn frn_auto_seal_caps_tail() {
        let mut ix = Index::new();
        ix.add_volume(vol());
        ix.push(0, 1, 1, "", DIRECTORY);
        for i in 0..20_000u64 {
            ix.push(0, 1000 + i, 1, &format!("f{i:05}.dat"), 0);
            // The unsealed tail never grows without bound, even though
            // seal_batch is never called.
            assert!(ix.frn_index.len() - ix.frn_sealed_len() < 16_384 + 1);
        }
        // Spot-check lookups across the whole range (tail + sealed runs).
        for i in [0, 777, 16_383, 16_384, 19_999u64] {
            assert_eq!(ix.lookup(0, 1000 + i), Some((i + 1) as EntryId));
        }
        assert_eq!(ix.lookup(0, 1), Some(0));
    }

    #[test]
    fn frn_merge_bound_and_correctness() {
        let mut ix = Index::new();
        ix.add_volume(vol());
        ix.push(0, 1, 1, "", DIRECTORY);
        // 70 seals of 100 pushes each: forces merges past the 64-run bound.
        for b in 0..70u64 {
            for i in 0..100u64 {
                let k = b * 100 + i;
                ix.push(0, 1000 + k, 1, &format!("f{k:05}.dat"), 0);
            }
            ix.seal_batch();
            assert!(
                ix.frn_runs.len() <= 64,
                "runs bounded after seal {}",
                ix.frn_runs.len()
            );
        }
        assert_eq!(ix.len(), 7001);
        // Every lookup still exact (compare against post-finalize truth).
        let mut expect = Vec::new();
        for k in (0..7000u64).step_by(333) {
            expect.push(ix.lookup(0, 1000 + k));
        }
        ix.finalize();
        for (n, k) in (0..7000u64).step_by(333).enumerate() {
            assert_eq!(ix.lookup(0, 1000 + k), expect[n]);
            assert_eq!(ix.lookup(0, 1000 + k), Some((k + 1) as EntryId));
        }
        assert!(ix.frn_is_fresh());
    }

    #[test]
    fn frn_seal_live_keeps_one_delta_run() {
        // A day of journal polls (each a few creates, then a seal) must not
        // pile up one run per poll: every lookup binary-searches each run, so
        // 64 runs made `path:` searches 50x slower than a fresh index.
        let base = FRN_LIVE_MERGE as u64;
        let mut ix = Index::new();
        ix.add_volume(vol());
        for i in 0..base {
            ix.push(0, 10 + i, 10, "f", 0);
        }
        ix.seal_batch();
        for b in 0..200u64 {
            for i in 0..5u64 {
                let k = b * 5 + i;
                ix.push(0, 10_000_000 + k, 10, "live", 0);
            }
            ix.seal_live();
            assert_eq!(ix.frn_runs.len(), 2, "base + one delta after poll {b}");
        }
        for k in (0..1000u64).step_by(77) {
            assert_eq!(ix.lookup(0, 10_000_000 + k), Some((base + k) as EntryId));
        }
        for i in [0, 4_321, base - 1] {
            assert_eq!(ix.lookup(0, 10 + i), Some(i as EntryId));
        }
    }

    #[test]
    fn frn_seal_live_leaves_big_runs_alone() {
        // Merging under the journal's write lock stays cheap: two runs that
        // together pass FRN_LIVE_MERGE are not merged.
        let half = FRN_LIVE_MERGE as u64 / 2 + 1;
        let mut ix = Index::new();
        ix.add_volume(vol());
        for i in 0..half {
            ix.push(0, 10 + i, 10, "a", 0);
        }
        ix.seal_batch();
        for i in 0..half {
            ix.push(0, 10 + half + i, 10, "b", 0);
        }
        ix.seal_live();
        let [.., a, b] = ix.frn_runs.as_slice() else {
            panic!("expected at least two runs, got {:?}", ix.frn_runs);
        };
        assert!(a.len() + b.len() > FRN_LIVE_MERGE);
        assert_eq!(ix.lookup(0, 10), Some(0));
        assert_eq!(ix.lookup(0, 10 + half), Some(half as EntryId));
    }

    #[test]
    fn frn_apply_after_sealed_runs() {
        // Journal event landing mid-scan: apply merges first (as before),
        // then keeps the single run sorted with an O(n) insert.
        let mut ix = scan_fixture(300);
        ix.seal_batch();
        ix.apply(
            0,
            IndexEvent::Create {
                frn: 50_000,
                parent_frn: 1,
                name: "live.txt",
                flags: 0,
            },
        );
        assert_eq!(ix.frn_runs.len(), 1);
        assert_eq!(ix.lookup(0, 50_000), Some(301));
        assert_eq!(ix.lookup(0, 1000), Some(1));
        // Rename of a sealed id: newest id wins, run stays single + sorted.
        ix.apply(
            0,
            IndexEvent::Rename {
                frn: 1000,
                parent_frn: 1,
                name: "renamed.txt",
            },
        );
        let latest = ix.lookup(0, 1000).unwrap();
        assert_ne!(latest, 1);
        assert_eq!(ix.frn_runs.len(), 1);
        assert!(ix.lookup(0, 50_000).is_some());
    }

    /// Split commit at scale (`#[ignore]`d like `bench_1m`): 1M scan pushes
    /// plus 20k live applies. A snapshot installed with no mutation between
    /// matches `rebuild_by_name` exactly (sorted arrays, freshness, search
    /// behavior); a push between snapshot and install is refused and changes
    /// nothing.
    #[test]
    #[ignore]
    fn sorted_snapshot_install_matches_rebuild() {
        use crate::query::parse;
        use crate::search::{search_paged, SearchOptions, Sort};

        fn build() -> Index {
            let mut ix = Index::new();
            ix.add_volume(vol());
            ix.push(0, 100, 100, "", DIRECTORY);
            let exts = ["txt", "rs", "log", "dll", "md"];
            let mut i = 0u32;
            while ix.len() < 1_000_000 {
                let tag = if i.is_multiple_of(1000) {
                    "qw7x"
                } else if i.is_multiple_of(50) {
                    "k20"
                } else {
                    "doc"
                };
                ix.push(
                    0,
                    100_000 + u64::from(i),
                    100,
                    &format!("{tag}_{i:06}.{}", exts[i as usize % 5]),
                    0,
                );
                i += 1;
            }
            ix
        }
        fn pages(ix: &Index, other: &Index) {
            for qs in ["qw7x", "k20", "doc", "*.log", "ext:rs"] {
                let q = parse(qs);
                let opts = SearchOptions::new(100, 0, Sort::NameAsc);
                let a = search_paged(ix, &q, &opts, None);
                let b = search_paged(other, &q, &opts, None);
                assert_eq!(a.total, b.total, "query {qs:?}");
                assert_eq!(a.hits, b.hits, "query {qs:?}");
            }
        }
        let mut ix = build();
        let mut control = build();
        // Dirty-tail regime: snapshot off-lock, install with no mutation.
        assert!(ix.install_sorted(ix.sorted_snapshot()));
        control.rebuild_by_name();
        assert_eq!(ix.by_name, control.by_name);
        assert_eq!(ix.frn_index, control.frn_index);
        assert_eq!(ix.frn_runs, control.frn_runs);
        assert!(ix.by_name_is_fresh());
        assert!(ix.frn_is_fresh());
        assert_eq!(ix.pending_len(), 0);
        pages(&ix, &control);
        // Pending regime: 20k live applies (fresh FRNs append at the end),
        // snapshot over the merge path, install.
        for i in 0..20_000u64 {
            let name = format!("live_{i:05}.liv");
            ix.apply(
                0,
                IndexEvent::Create {
                    frn: 2_000_000 + i,
                    parent_frn: 100,
                    name: &name,
                    flags: 0,
                },
            );
            let name = format!("live_{i:05}.liv");
            control.apply(
                0,
                IndexEvent::Create {
                    frn: 2_000_000 + i,
                    parent_frn: 100,
                    name: &name,
                    flags: 0,
                },
            );
        }
        assert_eq!(ix.pending_len(), 20_000);
        assert!(ix.install_sorted(ix.sorted_snapshot()));
        control.rebuild_by_name();
        assert_eq!(ix.by_name, control.by_name);
        assert!(ix.by_name_is_fresh());
        assert_eq!(ix.pending_len(), 0);
        pages(&ix, &control);
        // Push between snapshot and install: refused, index untouched.
        let snap = ix.sorted_snapshot();
        let before = ix.by_name.clone();
        ix.push(0, 9_000_000, 100, "late_arrival.txt", 0);
        assert!(!ix.install_sorted(snap));
        assert_eq!(ix.by_name.len(), before.len() + 1);
        assert_eq!(&ix.by_name[..before.len()], &before[..]);
        assert_eq!(ix.pending_len(), 0);
    }

    /// The monotonic arena prefix tracks every append path and compaction.
    #[test]
    fn arena_prefix_maintenance() {
        // Block bitsets track the same transitions: every name's bytes are
        // present in exactly the blocks they span.
        fn blocks_cover(ix: &Index) {
            for e in &ix.entries {
                let end = e.name_off as usize + e.name_len as usize;
                assert!(end <= ix.names.len());
                for i in e.name_off as usize..end {
                    let b = ix.names.as_bytes()[i];
                    let blk = &ix.arena_blocks[i >> ARENA_BLOCK_BITS as usize];
                    assert_ne!(blk[(b >> 6) as usize] & (1 << (b & 63)), 0);
                }
            }
        }
        let mut ix = Index::new();
        ix.add_volume(vol());
        assert_eq!(ix.arena_prefix_len, 0);
        ix.push(0, 100, 100, "", DIRECTORY);
        ix.push(0, 101, 100, "b.txt", 0);
        ix.push(0, 102, 100, "a.txt", 0);
        assert_eq!(ix.arena_prefix_len as usize, ix.len());
        blocks_cover(&ix);
        ix.apply(
            0,
            IndexEvent::Create {
                frn: 200,
                parent_frn: 100,
                name: "c.txt",
                flags: 0,
            },
        );
        assert_eq!(ix.arena_prefix_len as usize, ix.len());
        blocks_cover(&ix);
        ix.apply(0, IndexEvent::Delete { frn: 101 });
        assert_eq!(ix.arena_prefix_len as usize, ix.len());
        blocks_cover(&ix);
        ix.apply(
            0,
            IndexEvent::Rename {
                frn: 102,
                parent_frn: 100,
                name: "z.txt",
            },
        );
        assert_eq!(ix.arena_prefix_len as usize, ix.len());
        blocks_cover(&ix);
        ix.compact();
        assert_eq!(ix.arena_prefix_len as usize, ix.len());
        blocks_cover(&ix);
    }

    /// Fold cycles retain no spare generations: at ~100k entries the fold
    /// buffers (`by_name` + `frn_index`, ~400 KB each) are large enough that
    /// a retained generation would show up as capacity growth or as
    /// structural bytes beyond the new entries. Folding is a pure
    /// reorganization: with no tombstones, the breakdown total moves by
    /// exactly the new entries' structural bytes, the live `by_name` keeps
    /// exact capacity (merge allocates exactly `len`), and the `frn_index`
    /// capacity is stable across cycles. (Allocator-level retention at 8M
    /// scale is covered by the ignored `soak_8m_fold_rss_flat` release test
    /// in `tests/integration.rs`, which reads process RSS per cycle.)
    #[test]
    fn fold_cycles_keep_no_spare_generations() {
        const BASE: usize = 100_000;
        const CYCLES: usize = 6;
        const PER_CYCLE: usize = 2_000;
        // Fixed-width names ("foldprobe_CC_IIII.liv" = 21 bytes) so each new
        // entry's structural cost is exact.
        const NAME_LEN: usize = 21;

        fn name_for(c: usize, i: usize) -> String {
            format!("foldprobe_{c:02}_{i:04}.liv")
        }
        let mut ix = Index::new();
        ix.add_volume(vol());
        ix.reserve(BASE + CYCLES * PER_CYCLE + 8);
        ix.push(0, 1, 1, "", DIRECTORY);
        for i in 0..BASE as u64 {
            ix.push(0, 100_000 + i, 1, &format!("base_{i:06}.dat"), 0);
        }
        ix.finalize();
        ix.rebuild_by_name();
        assert!(ix.by_name_is_fresh());
        assert!(ix.frn_is_fresh());

        let dir = std::env::temp_dir().join(format!(
            "floki_foldprobe_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.subsec_nanos())
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let mut frn = 100_000 + BASE as u64 + 1_000_000;
        for c in 0..CYCLES {
            let pre_apply = ix.memory_breakdown().total_bytes();
            let aux_before = ix.arena_blocks.len();
            let mut new_name_bytes = 0u64;
            for i in 0..PER_CYCLE {
                let name = name_for(c, i);
                debug_assert_eq!(name.len(), NAME_LEN);
                new_name_bytes += name.len() as u64;
                ix.apply(
                    0,
                    IndexEvent::Create {
                        frn,
                        parent_frn: 1,
                        name: &name,
                        flags: 0,
                    },
                );
                frn += 1;
            }
            assert_eq!(ix.pending_len(), PER_CYCLE);
            // Each create adds one 24 B entry, one 4 B `frn_index` id, one
            // `entry_vol` byte, its arena bytes, one 4 B `pending` id, plus
            // 32 B per 64 KiB arena block crossed on the way.
            let post_apply = ix.memory_breakdown().total_bytes();
            let aux_delta = (ix.arena_blocks.len() - aux_before) as u64 * 32;
            assert_eq!(
                post_apply,
                pre_apply + PER_CYCLE as u64 * (24 + 4 + 4 + 1) + new_name_bytes + aux_delta,
                "cycle {c}: applies must add exactly the new entries' bytes"
            );
            let snap = ix.sorted_snapshot();
            assert!(ix.install_sorted(snap));
            assert_eq!(ix.pending_len(), 0);
            assert!(ix.by_name_is_fresh());
            assert!(ix.frn_is_fresh());
            // The fold is a pure reorganization: `pending` bytes move into
            // `by_name` one-for-one (no tombstones here), so the total does
            // not move at all. A retained spare generation — or any fold
            // path that duplicates instead of moves — would break this.
            let post_fold = ix.memory_breakdown().total_bytes();
            assert_eq!(
                post_fold, post_apply,
                "cycle {c}: fold must not move the structural total"
            );
            // No spare capacity accumulates in the live arrays: both fold
            // targets land with exact capacity (the merge builds exactly
            // `len`, the snapshot clone is exact), so a fold that retained
            // the displaced generation alongside the new one would show up
            // as capacity above `len` here.
            assert_eq!(
                ix.by_name.capacity(),
                ix.by_name.len(),
                "cycle {c}: by_name must keep exact capacity"
            );
            assert_eq!(
                ix.frn_index.capacity(),
                ix.frn_index.len(),
                "cycle {c}: frn_index must keep exact capacity"
            );
            if c % 2 == 1 {
                let p = dir.join(format!("save_{c}.bin"));
                ix.save(&p).unwrap();
                assert_eq!(
                    ix.memory_breakdown().total_bytes(),
                    post_fold,
                    "cycle {c}: save takes &self and must not move the total"
                );
            }
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Rescan pattern at small scale (live: tombstone half + push half +
    /// compact): the prefix and the block bitsets cover all entries after
    /// every transition, and search stays exact throughout.
    #[test]
    fn arena_prefix_rescan_pattern() {
        let mut ix = Index::new();
        ix.add_volume(vol());
        ix.push(0, 1, 1, "", DIRECTORY);
        for i in 0..2000u32 {
            ix.push(0, 1000 + u64::from(i), 1, &format!("file_{i:05}.txt"), 0);
        }
        ix.finalize();
        ix.rebuild_by_name();
        assert_eq!(ix.arena_prefix_len as usize, ix.len());
        // Tombstone the first half (rescan deads), push a fresh half.
        for i in 0..1000u64 {
            ix.apply(0, IndexEvent::Delete { frn: 1000 + i });
        }
        for i in 0..1000u32 {
            ix.apply(
                0,
                IndexEvent::Create {
                    frn: 2_000_000 + u64::from(i),
                    parent_frn: 1,
                    name: &format!("new_{i:05}.log"),
                    flags: 0,
                },
            );
        }
        assert_eq!(ix.arena_prefix_len as usize, ix.len());
        let q = crate::query::parse("file_");
        assert_eq!(crate::search::count(&ix, &q, None), 1000);
        let q = crate::query::parse("new_");
        assert_eq!(crate::search::count(&ix, &q, None), 1000);
        ix.compact();
        assert_eq!(ix.arena_prefix_len as usize, ix.len());
        assert_eq!(ix.pending_len(), 0);
        let q = crate::query::parse("file_");
        assert_eq!(crate::search::count(&ix, &q, None), 1000);
        let q = crate::query::parse("new_");
        assert_eq!(crate::search::count(&ix, &q, None), 1000);
        // Block bitsets agree with the arena byte-for-byte.
        let mut expect = vec![[0u64; 4]; ix.arena_blocks.len()];
        for (i, &b) in ix.names.as_bytes().iter().enumerate() {
            expect[i >> ARENA_BLOCK_BITS as usize][(b >> 6) as usize] |= 1 << (b & 63);
        }
        assert_eq!(ix.arena_blocks, expect);
    }
    #[test]
    fn add_volume_forces_both_flags_on() {
        let mut ix = Index::new();
        let vol = Volume {
            letter: 'C',
            guid: [0; 16],
            journal_id: 1,
            next_usn: 0,
            root_frn: 100,
            enabled: false,
            monitor: false,
        };
        ix.add_volume(vol);
        assert!(ix.volumes[0].enabled);
        assert!(ix.volumes[0].monitor);
    }

    #[test]
    fn remove_volume_drops_entries_and_shifts_indexes() {
        let mut ix = Index::new();
        for letter in ['C', 'D'] {
            ix.add_volume(Volume {
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
        ix.push(0, 101, 100, "c-file.txt", 0);
        ix.push(1, 200, 200, "", DIRECTORY);
        ix.push(1, 201, 200, "d-file.txt", 0);
        ix.finalize();
        ix.rebuild_by_name();
        assert_eq!(ix.len(), 4);
        assert!(ix.remove_volume('C'));
        assert_eq!(ix.volumes.len(), 1);
        assert_eq!(ix.volumes[0].letter, 'D');
        assert_eq!(ix.len(), 2);
        // Surviving entries re-tagged to volume index 0.
        assert!(ix.entry_vol.iter().all(|&v| v == 0));
        // No FRN group of the removed volume resolves.
        assert!(ix.lookup(1, 200).is_none());
        assert!(ix.lookup(0, 200).is_some());
        // Name search sees only the surviving volume.
        let q = crate::query::parse("file.txt");
        assert_eq!(crate::search::count(&ix, &q, None), 1);
        // Unknown letter: no-op.
        assert!(!ix.remove_volume('Z'));
        assert_eq!(ix.volumes.len(), 1);
    }

    /// A removal built off-lock is refused after any mutation (a tail
    /// applying events in between) and leaves the index whole; rebuilt, it
    /// installs and the index matches what `remove_volume` would produce.
    #[test]
    fn volume_removal_refuses_a_stale_build() {
        let mut ix = Index::new();
        for letter in ['C', 'D'] {
            ix.add_volume(Volume {
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
        ix.push(0, 101, 100, "c-file.txt", 0);
        ix.push(1, 200, 200, "", DIRECTORY);
        ix.push(1, 201, 200, "d-file.txt", 0);
        ix.finalize();
        ix.rebuild_by_name();
        let stale = ix.volume_removal('C').expect("C indexed");
        assert_eq!(stale.dropped(), 2);
        ix.apply(
            1,
            IndexEvent::Create {
                frn: 202,
                parent_frn: 200,
                name: "late.txt",
                flags: 0,
            },
        );
        assert!(!ix.install_volume_removal(stale));
        assert_eq!(ix.volumes.len(), 2);
        assert_eq!(ix.len(), 5);

        let fresh = ix.volume_removal('C').expect("C indexed");
        assert!(ix.install_volume_removal(fresh));
        assert_eq!(ix.volumes.len(), 1);
        assert_eq!(ix.volumes[0].letter, 'D');
        assert!(ix.lookup(0, 202).is_some(), "the late create survives");
        let q = crate::query::parse("txt");
        assert_eq!(crate::search::count(&ix, &q, None), 2);
        assert!(ix.volume_removal('Z').is_none());
    }
}
