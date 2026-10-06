//! Persistence: `index.bin` per SPEC section 3.
//!
//! ```text
//! magic b"FLOKIDX2" | u32 header_len LE | header serde_json | entries[] LE
//! | arena bytes | by_name u32[] LE | entry_vol u8[] | frn_index u32[] LE
//! ```
//!
//! `by_name` holds `header.by_name_len` ids (`== entry_count` when saved
//! fresh; fewer when saved while dirty, tombstones excluded). Files written
//! before `by_name_len` existed always hold `entry_count` ids and still load.
//!
//! The trailing `entry_vol` bytes are a v1 addition to the SPEC layout (one
//! byte per entry, same order as `entries[]`). Files without them (exactly
//! zero trailing bytes) still load, with every entry assigned to volume 0.
//! `frn_index` (every entry id sorted by `(vol, frn)`, 4 B/entry) follows
//! `entry_vol` in v2 files; v1 files (`b"FLOKIDX1"`, no `frn_index` bytes)
//! still load by rebuilding it in one sort.
//! Written atomically (`.tmp` + rename). Loaded with a plain read, which is
//! fast enough for 1 M entries (< 300 ms); mmap via `memmap2` is left for a
//! later pass.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::entry::{Entry, EntryId};
use crate::error::CoreError;
use crate::index::{Index, Volume};

pub(crate) const MAGIC_V1: &[u8; 8] = b"FLOKIDX1";
/// Magic written by [`save`](Index::save).
pub(crate) const MAGIC_V2: &[u8; 8] = b"FLOKIDX2";

#[derive(Serialize, Deserialize)]
struct FileHeader {
    volumes: Vec<Volume>,
    entry_count: u64,
    arena_len: u64,
    /// Length of the persisted `by_name` array. Equals `entry_count` for a
    /// fresh index; shorter when `save` persisted a rebuilt copy while dirty
    /// (tombstones excluded). Absent (`None`) in files written before this
    /// field existed, where it always equalled `entry_count`.
    #[serde(default)]
    by_name_len: Option<u64>,
    /// Length of the persisted `frn_index` array (`== entry_count`, live and
    /// tombstoned). Absent (`None`) in v1 files, which carry no `frn_index`
    /// bytes and rebuild it on load.
    #[serde(default)]
    frn_index_len: Option<u64>,
    /// Global NTFS-targets policy (see [`TargetsConfig`]). Absent in files
    /// written before it existed; those load with [`TargetsConfig::default`].
    #[serde(default = "targets_config_default")]
    targets: TargetsConfig,
}

/// Re-exported policy type: the canonical shape lives in `floki-proto` (the
/// pipe contract), but `floki-core` must stay dependency-free, so the fields
/// are mirrored here and converted at the service boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct TargetsConfig {
    /// Auto-index newly arrived `DRIVE_FIXED` NTFS volumes.
    #[serde(default = "targets_fixed_default")]
    pub auto_include_fixed: bool,
    /// Auto-index newly arrived `DRIVE_REMOVABLE` NTFS volumes.
    #[serde(default = "targets_removable_default")]
    pub auto_include_removable: bool,
    /// Drop indexed volumes that stay unopenable (offline media).
    #[serde(default = "targets_remove_offline_default")]
    pub auto_remove_offline: bool,
    /// Drive letters the user removed (bit `n` = letter `'A' + n`): never
    /// auto-included again until explicitly re-added (a `Rescan` of that
    /// letter). Without it a removed fixed drive came straight back on the
    /// next arrival poll. Daemon-side state; not part of the pipe policy.
    #[serde(default)]
    pub excluded: u32,
}

impl Default for TargetsConfig {
    fn default() -> Self {
        Self {
            auto_include_fixed: true,
            auto_include_removable: false,
            auto_remove_offline: true,
            excluded: 0,
        }
    }
}

impl TargetsConfig {
    /// Bit for `letter` in [`excluded`](Self::excluded); `None` for
    /// anything but `A`-`Z` (case-insensitive).
    fn letter_bit(letter: char) -> Option<u32> {
        let up = letter.to_ascii_uppercase();
        up.is_ascii_uppercase()
            .then(|| 1u32 << (up as u32 - 'A' as u32))
    }

    /// True when the user removed `letter` and has not re-added it.
    #[must_use]
    pub fn is_excluded(&self, letter: char) -> bool {
        Self::letter_bit(letter).is_some_and(|bit| self.excluded & bit != 0)
    }

    /// Mark (`true`) or clear (`false`) `letter` as user-removed.
    pub fn set_excluded(&mut self, letter: char, excluded: bool) {
        if let Some(bit) = Self::letter_bit(letter) {
            if excluded {
                self.excluded |= bit;
            } else {
                self.excluded &= !bit;
            }
        }
    }
}

/// Serde default for the whole [`TargetsConfig`] block (pre-targets files).
fn targets_config_default() -> TargetsConfig {
    TargetsConfig::default()
}

/// Field-level serde defaults so a partially written `targets` block still
/// loads with the Everything-matching policy (not `false` everywhere).
fn targets_fixed_default() -> bool {
    true
}

/// See [`targets_fixed_default`].
fn targets_removable_default() -> bool {
    false
}

/// See [`targets_fixed_default`].
fn targets_remove_offline_default() -> bool {
    true
}

fn tmp_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".tmp");
    PathBuf::from(s)
}

impl Index {
    /// Save atomically: write `<path>.tmp`, `sync_all`, then rename over `path`.
    ///
    /// When the persisted `by_name` would be incomplete (scan pushes since
    /// the last rebuild, or ids waiting in `pending`), a merged copy is
    /// persisted instead, so the file's `by_name` is always sorted and
    /// tombstone-free: a linear merge when the snapshot is sorted, a full
    /// sort after scan pushes. Likewise a dirty `frn_index` is rebuilt in one sort before writing, so
    /// the file always carries a sorted, complete index. That pays the sort
    /// costs on save; callers applying batched live updates should call
    /// [`rebuild_by_name`](Index::rebuild_by_name) once per batch instead. A
    /// failed save never leaves `<path>.tmp` behind.
    pub fn save(&self, path: &Path) -> Result<(), CoreError> {
        let tmp = tmp_path(path);
        let owned_by_name: Vec<EntryId>;
        let by_name: &[EntryId] = if self.by_name_is_fresh() && self.pending.is_empty() {
            self.by_name()
        } else if self
            .by_name_dirty
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            owned_by_name = self.sorted_by_name();
            &owned_by_name
        } else {
            owned_by_name = self.merged_by_name();
            &owned_by_name
        };
        let owned_frn: Vec<EntryId>;
        let frn_index: &[EntryId] = if self.frn_is_fresh() {
            &self.frn_index
        } else {
            owned_frn = self.sorted_frn_index();
            &owned_frn
        };
        if let Err(e) = self.save_to(&tmp, by_name, frn_index) {
            let _ = fs::remove_file(&tmp);
            return Err(e);
        }
        if let Err(e) = fs::rename(&tmp, path) {
            let _ = fs::remove_file(&tmp);
            return Err(CoreError::Io(e));
        }
        Ok(())
    }

    /// Write the full v2 image to `tmp` (caller renames / cleans up).
    fn save_to(
        &self,
        tmp: &Path,
        by_name: &[EntryId],
        frn_index: &[EntryId],
    ) -> Result<(), CoreError> {
        let entry_count = self.entries.len();
        let header = FileHeader {
            volumes: self.volumes.clone(),
            entry_count: entry_count as u64,
            arena_len: self.names.len() as u64,
            by_name_len: Some(by_name.len() as u64),
            frn_index_len: Some(frn_index.len() as u64),
            targets: self.targets,
        };
        let hbytes = serde_json::to_vec(&header)?;
        let hlen: u32 = u32::try_from(hbytes.len())
            .map_err(|_| CoreError::Corrupt("header too large".to_string()))?;
        // Buffered: a bare `File` turns every `write_all` into a syscall,
        // so the per-entry loop below was ~one WriteFile per 24-byte entry
        // — millions of syscalls on a large index, which made the shutdown
        // save churn the disk for minutes. The wire format is unchanged.
        let mut f = io::BufWriter::with_capacity(8 << 20, File::create(tmp)?);
        f.write_all(MAGIC_V2)?;
        f.write_all(&hlen.to_le_bytes())?;
        f.write_all(&hbytes)?;
        for e in &self.entries {
            f.write_all(&e.to_le_bytes())?;
        }
        f.write_all(self.names.as_bytes())?;
        for id in by_name {
            f.write_all(&id.to_le_bytes())?;
        }
        let mut vols = vec![0u8; entry_count];
        for (i, v) in vols.iter_mut().enumerate() {
            *v = self.volume_of(i as EntryId).unwrap_or(0);
        }
        f.write_all(&vols)?;
        for id in frn_index {
            f.write_all(&id.to_le_bytes())?;
        }
        f.flush()?;
        f.get_ref().sync_all()?;
        drop(f);
        Ok(())
    }

    /// Load an index written by [`save`](Self::save); v1 files
    /// (`b"FLOKIDX1"`, no `frn_index` bytes) still load by rebuilding the
    /// FRN index in one sort.
    ///
    /// Tolerant by design: a short `by_name` (merged saves drop tombstones)
    /// loads as-is once validated (in range, no duplicates, sorted, every
    /// live id present); a missing/short/inconsistent `by_name` or `frn_index`
    /// is warned about (via `tracing`) and rebuilt from entries instead of
    /// rejecting the file. Only a bad magic, truncated entries/arena, or an
    /// out-of-range id is a hard error.
    pub fn load(path: &Path) -> Result<Self, CoreError> {
        let data = fs::read(path)?;
        let bad = |msg: &str| CoreError::Corrupt(msg.to_string());
        if data.len() < MAGIC_V2.len() + 4 {
            return Err(CoreError::BadMagic);
        }
        let is_v1 = &data[..8] == MAGIC_V1;
        let is_v2 = &data[..8] == MAGIC_V2;
        if !is_v1 && !is_v2 {
            return Err(CoreError::BadMagic);
        }
        let hlen = u32::from_le_bytes(
            data[8..12]
                .try_into()
                .map_err(|_| bad("truncated header len"))?,
        ) as usize;
        let hstart = MAGIC_V2.len() + 4;
        let hend = hstart
            .checked_add(hlen)
            .ok_or_else(|| bad("bad header len"))?;
        if data.len() < hend {
            return Err(bad("truncated header"));
        }
        let header: FileHeader = serde_json::from_slice(&data[hstart..hend])?;
        let n: usize = usize::try_from(header.entry_count).map_err(|_| bad("bad entry count"))?;
        let alen: usize = usize::try_from(header.arena_len).map_err(|_| bad("bad arena len"))?;
        let ebytes = n
            .checked_mul(Entry::BYTE_LEN)
            .ok_or_else(|| bad("bad entry count"))?;
        let mut cur = hend;

        let need = |cur: usize, want: usize| {
            cur.checked_add(want)
                .filter(|&end| end <= data.len())
                .ok_or_else(|| bad("truncated file"))
        };
        let eend = need(cur, ebytes)?;
        let mut entries = Vec::with_capacity(n);
        let (chunks, _) = data[cur..eend].as_chunks::<{ Entry::BYTE_LEN }>();
        for chunk in chunks {
            entries.push(Entry::from_le_bytes(chunk).ok_or_else(|| bad("bad entry"))?);
        }
        cur = eend;
        let aend = need(cur, alen)?;
        std::str::from_utf8(&data[cur..aend]).map_err(|_| bad("arena is not UTF-8"))?;
        let mut index = Index::new();
        index.volumes = header.volumes;
        index.targets = header.targets;
        index.entries = entries;
        index.names.set_bytes(data[cur..aend].to_vec());
        // Validate name ranges and recover the monotonic arena prefix in the
        // same pass: a corrupt file may break offset order or tiling, and
        // ids past the break must use the per-entry scan (see
        // `arena_prefix_len`). The first name must start at 0, each next one
        // exactly where the previous ended (append packing). Block presence
        // bitsets rebuild alongside (same bytes, no extra pass).
        let mut prefix = 0u32;
        let mut prev_end = 0u64;
        let mut unbroken = true;
        let mut blocks: Vec<[u64; 4]> = Vec::new();
        for (i, e) in index.entries.iter().enumerate() {
            let name = match index.names.get(e.name_off, e.name_len) {
                Some(n) => n,
                None => return Err(bad("bad name range")),
            };
            Index::block_add_bytes(&mut blocks, e.name_off, name.as_bytes());
            if unbroken {
                if e.name_off as u64 == prev_end {
                    prefix = (i + 1).min(u32::MAX as usize) as u32;
                } else {
                    unbroken = false;
                }
            }
            prev_end = e.name_off as u64 + e.name_len as u64;
        }
        index.arena_prefix_len = prefix;
        index.arena_blocks = blocks;
        cur = aend;
        let bn: usize = match header.by_name_len {
            Some(m) => usize::try_from(m).map_err(|_| bad("bad by_name len"))?,
            None => n, // pre-field files always stored entry_count ids
        };
        // No `bn > n` rejection here: a short array is legitimate (merged
        // saves drop tombstones) and any other inconsistency (over-long,
        // duplicates, unsorted, missing live ids) fails content validation
        // below and is rebuilt, never trusted blindly. Only out-of-range ids
        // (a different entry set) and structural truncation stay hard errors.
        let bend = need(
            cur,
            bn.checked_mul(size_of::<EntryId>())
                .ok_or_else(|| bad("bad count"))?,
        )?;
        let mut by_name = Vec::with_capacity(n);
        let (words, _) = data[cur..bend].as_chunks::<{ size_of::<EntryId>() }>();
        for chunk in words {
            let id = EntryId::from_le_bytes(*chunk);
            if (id as usize) >= n {
                return Err(bad("by_name id out of range"));
            }
            by_name.push(id);
        }
        cur = bend;
        if is_v1 {
            let rest = data.len() - cur;
            let entry_vol: Vec<u8> = if rest == 0 {
                vec![0u8; n] // legacy file without per-entry volumes
            } else if rest == n {
                data[cur..].to_vec()
            } else {
                return Err(bad("trailing bytes mismatch"));
            };
            index.entry_vol = entry_vol;
            index.take_loaded_by_name(by_name);
            // No persisted frn_index: always rebuild it in one sort.
            index.finalize();
            if index
                .by_name_dirty
                .load(std::sync::atomic::Ordering::Relaxed)
            {
                index.rebuild_by_name();
            }
            index.epoch += 1;
            index
                .by_name_dirty
                .store(false, std::sync::atomic::Ordering::Relaxed);
            return Ok(index);
        }
        // v2: entry_vol (n bytes) then frn_index (fi * 4 bytes).
        let vend = need(cur, n)?;
        let entry_vol: Vec<u8> = data[cur..vend].to_vec();
        cur = vend;
        let fi: usize = match header.frn_index_len {
            Some(m) => usize::try_from(m).map_err(|_| bad("bad frn_index len"))?,
            None => {
                // Header predates the field: rebuild from entries.
                index.entry_vol = entry_vol;
                index.take_loaded_by_name(by_name);
                index.finalize();
                if index
                    .by_name_dirty
                    .load(std::sync::atomic::Ordering::Relaxed)
                {
                    index.rebuild_by_name();
                }
                index.epoch += 1;
                index
                    .by_name_dirty
                    .store(false, std::sync::atomic::Ordering::Relaxed);
                return Ok(index);
            }
        };
        if fi != n {
            return Err(bad("bad frn_index len"));
        }
        let fend = need(
            cur,
            fi.checked_mul(size_of::<EntryId>())
                .ok_or_else(|| bad("bad count"))?,
        )?;
        if data.len() != fend {
            return Err(bad("trailing bytes mismatch"));
        }
        let mut frn_index = Vec::with_capacity(fi);
        let (fwords, _) = data[cur..fend].as_chunks::<{ size_of::<EntryId>() }>();
        for chunk in fwords {
            let id = EntryId::from_le_bytes(*chunk);
            if (id as usize) >= n {
                return Err(bad("frn_index id out of range"));
            }
            frn_index.push(id);
        }
        index.entry_vol = entry_vol;
        index.take_loaded_by_name(by_name);
        index.set_loaded_frn(frn_index);
        if index
            .by_name_dirty
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            index.rebuild_by_name();
        }
        index.epoch += 1;
        index
            .by_name_dirty
            .store(false, std::sync::atomic::Ordering::Relaxed);
        Ok(index)
    }

    /// Install a persisted `frn_index`: use as-is when it holds exactly every
    /// id once, sorted by `(vol, frn, id)`; else warn and rebuild in one sort.
    /// Out-of-range ids are rejected at parse time (hard error); a wrong
    /// length likewise (`fi != entry_count` cannot anchor the trailing
    /// sections, so unlike short-but-located `by_name` it is unrecoverable).
    fn set_loaded_frn(&mut self, frn_index: Vec<EntryId>) {
        self.bump_mutation();
        let n = self.entries.len();
        let mut ok = frn_index.len() == n;
        if ok {
            let mut seen = vec![false; n];
            let mut prev: Option<(u8, u64, EntryId)> = None;
            for &id in &frn_index {
                let i = id as usize;
                if i >= n || seen[i] {
                    ok = false;
                    break;
                }
                seen[i] = true;
                // Triple key, strictly increasing: same-FRN groups (a
                // rename/revive tombstone pair) are legal when newest-last;
                // exact duplicates are impossible (one id, one key).
                let key = (
                    self.entry_vol.get(i).copied().unwrap_or(0),
                    self.entries[i].frn,
                    id,
                );
                if let Some(p) = prev {
                    if key <= p {
                        ok = false;
                        break;
                    }
                }
                prev = Some(key);
            }
        }
        if ok {
            self.frn_index = frn_index;
            self.note_single_frn_run();
            self.frn_dirty
                .store(false, std::sync::atomic::Ordering::Relaxed);
        } else {
            tracing::warn!(
                target: "floki-core",
                "persisted frn_index inconsistent; rebuilding"
            );
            self.finalize();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::DIRECTORY;

    fn small_index() -> Index {
        let mut ix = Index::new();
        ix.add_volume(Volume {
            letter: 'C',
            guid: [0; 16],
            journal_id: 1,
            next_usn: 0,
            root_frn: 1,
            enabled: true,
            monitor: true,
        });
        ix.push(0, 1, 1, "", DIRECTORY);
        ix.push(0, 2, 1, "alpha.txt", 0);
        ix.push(0, 3, 1, "beta.txt", 0);
        ix.push(0, 4, 1, "gamma.txt", 0);
        ix.push(0, 5, 1, "delta.txt", 0);
        ix.finalize();
        ix.rebuild_by_name();
        ix
    }

    fn tmp(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "floki_core_prefix_{}_{}.idx",
            std::process::id(),
            tag
        ))
    }

    #[test]
    fn load_recovers_arena_prefix() {
        let ix = small_index();
        assert_eq!(ix.arena_prefix_len as usize, ix.len());
        let p = tmp("ok");
        ix.save(&p).unwrap();
        let loaded = Index::load(&p).unwrap();
        assert_eq!(loaded.arena_prefix_len as usize, loaded.len());
        std::fs::remove_file(&p).ok();
    }

    /// A corrupt file that breaks offset tiling (entry 3 repointed at entry
    /// 2's offset — range still valid, so it loads) truncates the prefix at
    /// the break; the tail keeps search exact over the corrupted bytes
    /// (entry 3 now effectively reads "beta.txt").
    #[test]
    fn load_broken_tiling_truncates_prefix() {
        let ix = small_index();
        let p = tmp("corrupt");
        ix.save(&p).unwrap();
        let mut data = fs::read(&p).unwrap();
        // Layout: magic(8) + hlen(4) + JSON header, then n×24B entries with
        // name_off at +16 in each record.
        let hlen = u32::from_le_bytes(data[8..12].try_into().unwrap()) as usize;
        let base = 8 + 4 + hlen;
        let off2 = u32::from_le_bytes(
            data[base + 2 * 24 + 16..base + 2 * 24 + 20]
                .try_into()
                .unwrap(),
        );
        data[base + 3 * 24 + 16..base + 3 * 24 + 20].copy_from_slice(&off2.to_le_bytes());
        fs::write(&p, &data).unwrap();
        let loaded = Index::load(&p).unwrap();
        assert_eq!(loaded.arena_prefix_len, 3);
        // Effective names: "", alpha, beta, beta, delta — tail ids (3, 4)
        // exact through the per-entry path.
        for (qs, expect) in [
            ("alpha", 1),
            ("beta", 2),
            ("gamma", 0),
            ("delta", 1),
            ("txt", 4),
        ] {
            let q = crate::query::parse(qs);
            assert_eq!(
                crate::search::count(&loaded, &q, None),
                expect,
                "query {qs:?}"
            );
        }
        std::fs::remove_file(&p).ok();
    }
    #[test]
    fn roundtrip_preserves_volume_flags_and_targets() {
        let mut ix = small_index();
        ix.volumes[0].enabled = false;
        ix.volumes[0].monitor = false;
        ix.targets = TargetsConfig {
            auto_include_fixed: false,
            auto_include_removable: true,
            auto_remove_offline: false,
            excluded: 0,
        };
        ix.targets.set_excluded('e', true);
        let p = tmp("targets");
        ix.save(&p).unwrap();
        let loaded = Index::load(&p).unwrap();
        assert!(!loaded.volumes[0].enabled);
        assert!(!loaded.volumes[0].monitor);
        assert_eq!(loaded.targets, ix.targets);
        assert!(loaded.targets.is_excluded('E'), "removed letter persists");
        assert!(!loaded.targets.is_excluded('C'));
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn legacy_file_without_targets_loads_with_defaults() {
        // Pre-targets header: no `targets` block and v1 volume records
        // without the flags. Serde defaults must supply the Everything
        // policy (fixed on, removable off, remove-offline on) and live
        // volumes — never a zeroed `false` policy.
        let raw = br#"{"volumes":[{"letter":"C","guid":[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],"journal_id":1,"next_usn":0,"root_frn":1}],"entry_count":0,"arena_len":0}"#;
        let header: FileHeader = serde_json::from_slice(raw).unwrap();
        assert_eq!(header.targets, TargetsConfig::default());
        assert!(header.volumes[0].enabled);
        assert!(header.volumes[0].monitor);
        // A partial `targets` block still defaults field-by-field.
        let partial: TargetsConfig = serde_json::from_str(r#"{}"#).unwrap();
        assert_eq!(partial, TargetsConfig::default());
    }
}
