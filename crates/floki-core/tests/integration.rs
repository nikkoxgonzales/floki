//! Integration tests for floki-core: parser operators, search semantics,
//! persistence round trip, prev narrowing, sorting/paging, and the 1M bench.

use std::path::PathBuf;
use std::time::Instant;

use floki_core::{
    count, parse, prev_reusable, search, search_paged, set_search_threads, EntryId, Index,
    IndexEvent, SearchOptions, Sort, Volume, DIRECTORY,
};

fn test_volume(letter: char, root_frn: u64) -> Volume {
    Volume {
        letter,
        guid: [0; 16],
        journal_id: 1,
        next_usn: 0,
        root_frn,
        enabled: true,
        monitor: true,
    }
}

fn sample_index() -> Index {
    let mut ix = Index::new();
    ix.add_volume(test_volume('C', 100));
    ix.push(0, 100, 100, "", DIRECTORY); // 0 root
    ix.push(0, 101, 100, "Docs", DIRECTORY); // 1
    ix.push(0, 102, 100, "src", DIRECTORY); // 2
    ix.push(0, 200, 101, "report.txt", 0);
    ix.push(0, 201, 101, "photo.PNG", 0);
    ix.push(0, 202, 102, "main.rs", 0);
    ix.push(0, 203, 102, "lib.rs", 0);
    ix.push(0, 204, 100, "notes.TXT", 0);
    ix.push(0, 205, 100, "setup.exe", 0);
    ix.push(0, 206, 102, "data.toml", 0);
    ix.push(0, 103, 102, "deep", DIRECTORY); // 10
    ix.push(0, 207, 103, "deep_file.md", 0);
    ix
}

fn names_of(ix: &Index, hits: &[floki_core::Hit]) -> Vec<String> {
    hits.iter()
        .map(|h| ix.name(h.id).unwrap().to_string())
        .collect()
}

fn all_opts() -> SearchOptions {
    SearchOptions::new(0, 0, Sort::NameAsc)
}

#[test]
fn search_and_or_not() {
    let ix = sample_index();
    let o = all_opts();
    assert_eq!(
        names_of(&ix, &search(&ix, &parse("main rs"), &o, None)),
        vec!["main.rs"]
    );
    let mut got = names_of(&ix, &search(&ix, &parse("main | photo"), &o, None));
    got.sort();
    assert_eq!(got, vec!["main.rs", "photo.PNG"]);
    let got = names_of(&ix, &search(&ix, &parse("rs !main"), &o, None));
    assert_eq!(got, vec!["lib.rs"]);
    let got = names_of(&ix, &search(&ix, &parse("\"main.rs\""), &o, None));
    assert_eq!(got, vec!["main.rs"]);
    let got = names_of(&ix, &search(&ix, &parse("(main | lib) rs"), &o, None));
    assert_eq!(got.len(), 2);
}

#[test]
fn dotted_terms_are_case_insensitive_filename_and_substrings() {
    let mut ix = Index::new();
    ix.add_volume(test_volume('C', 100));
    ix.push(0, 100, 100, "", DIRECTORY);
    for (frn, name) in [
        (200, "pikachu.zip"),
        (201, "Pikachu-archive.ZIP"),
        (202, "pikachu.txt"),
        (203, "raichu.zip"),
        (204, "pikachu.zip.bak"),
    ] {
        ix.push(0, frn, 100, name, 0);
    }

    let opts = all_opts();
    let names = names_of(&ix, &search(&ix, &parse("pikachu .zip"), &opts, None));
    assert_eq!(
        names,
        vec![
            "Pikachu-archive.ZIP".to_owned(),
            "pikachu.zip".to_owned(),
            "pikachu.zip.bak".to_owned(),
        ]
    );

    let final_zip_names = names_of(&ix, &search(&ix, &parse("pikachu ext:zip"), &opts, None));
    assert_eq!(
        final_zip_names,
        vec!["Pikachu-archive.ZIP".to_owned(), "pikachu.zip".to_owned()]
    );
}

#[test]
fn search_functions() {
    let ix = sample_index();
    let o = all_opts();
    let mut got = names_of(&ix, &search(&ix, &parse("ext:rs;toml"), &o, None));
    got.sort();
    assert_eq!(got, vec!["data.toml", "lib.rs", "main.rs"]);
    // extension match is case-insensitive
    let got = names_of(&ix, &search(&ix, &parse("ext:TXT"), &o, None));
    assert_eq!(got.len(), 2);

    let got = names_of(&ix, &search(&ix, &parse("folder:"), &o, None));
    assert_eq!(got.len(), 4); // root + Docs + src + deep
    let got = names_of(&ix, &search(&ix, &parse("file:rs"), &o, None));
    assert_eq!(got.len(), 2);

    let got = names_of(&ix, &search(&ix, &parse("regex:^m.*\\.rs$"), &o, None));
    assert_eq!(got, vec!["main.rs"]);
    let got = names_of(&ix, &search(&ix, &parse("wfn:main.rs"), &o, None));
    assert_eq!(got, vec!["main.rs"]);
    let got = names_of(&ix, &search(&ix, &parse("*.rs"), &o, None));
    assert_eq!(got.len(), 2);

    // case-sensitive: uppercase needle misses the lowercase name
    assert!(search(&ix, &parse("case:MAIN"), &o, None).is_empty());
    assert_eq!(search(&ix, &parse("case:main"), &o, None).len(), 1);

    // path: matches on rebuilt full path
    let got = names_of(&ix, &search(&ix, &parse("path:src"), &o, None));
    assert_eq!(got.len(), 6); // src, main, lib, data, deep, deep_file

    // unknown prefix is a harmless literal
    assert!(search(&ix, &parse("zzz:qqq"), &o, None).is_empty());
    // bad regex falls back to literal, no panic
    assert!(search(&ix, &parse("regex:(unclosed"), &o, None).is_empty());
}

#[test]
fn tombstones_never_match() {
    let mut ix = sample_index();
    ix.apply(0, IndexEvent::Delete { frn: 202 });
    let o = all_opts();
    assert!(search(&ix, &parse("main"), &o, None).is_empty());
    assert_eq!(count(&ix, &parse(""), None) as usize, ix.len() - 1);
}

#[test]
fn prev_narrowing_equals_full_scan() {
    let mut ix = Index::new();
    ix.add_volume(test_volume('C', 1));
    ix.push(0, 1, 1, "", DIRECTORY);
    for i in 0..2000 {
        let n = format!("report_{i:04}.txt");
        ix.push(0, 1000 + i, 1, &n, 0);
    }
    ix.push(0, 999_999, 1, "other.dat", 0);
    let o = all_opts();
    let q1 = parse("rep");
    let q2 = parse("report_01");
    assert!(prev_reusable(&q1, &q2));
    let full = search(&ix, &q1, &o, None);
    assert!(!prev_reusable(&parse("a|b"), &parse("a|bc")));
    assert!(!prev_reusable(&parse("!a"), &parse("!ab")));
    assert!(!prev_reusable(&parse("rep"), &parse("x")));
    let narrowed = search(&ix, &q2, &o, Some(&full));
    let expected = search(&ix, &q2, &o, None);
    assert_eq!(narrowed, expected);
    assert_eq!(narrowed.len(), 100);
}

#[test]
fn sorting_and_paging_with_total() {
    let ix = sample_index();
    let q = parse(""); // everything live
    let total = count(&ix, &q, None);
    assert_eq!(total, ix.len() as u64);

    let asc = search_paged(&ix, &q, &SearchOptions::new(0, 0, Sort::NameAsc), None);
    assert_eq!(asc.total, total);
    let folded: Vec<String> = asc
        .hits
        .iter()
        .map(|h| ix.name(h.id).unwrap().to_lowercase())
        .collect();
    let mut s = folded.clone();
    s.sort();
    assert_eq!(folded, s);

    let desc = search_paged(&ix, &q, &SearchOptions::new(0, 0, Sort::NameDesc), None);
    let folded_d: Vec<String> = desc
        .hits
        .iter()
        .map(|h| ix.name(h.id).unwrap().to_lowercase())
        .collect();
    assert_eq!(folded_d, s.iter().rev().cloned().collect::<Vec<_>>());

    // paging: 3 per page
    let p1 = search_paged(&ix, &q, &SearchOptions::new(3, 0, Sort::NameAsc), None);
    let p2 = search_paged(&ix, &q, &SearchOptions::new(3, 3, Sort::NameAsc), None);
    assert_eq!(p1.total, total);
    assert_eq!(p2.total, total);
    assert_eq!(p1.hits.len(), 3);
    assert_eq!(p2.hits.len(), 3);
    assert_eq!(&asc.hits[..3], &p1.hits[..]);
    assert_eq!(&asc.hits[3..6], &p2.hits[..]);
    // offset past the end
    let empty = search_paged(
        &ix,
        &q,
        &SearchOptions::new(10, 10_000, Sort::NameAsc),
        None,
    );
    assert!(empty.hits.is_empty());
    assert_eq!(empty.total, total);

    // path sort: page itself ordered by path
    let ps = search_paged(&ix, &q, &SearchOptions::new(0, 0, Sort::PathAsc), None);
    assert_eq!(ps.total, total);
    let paths: Vec<String> = ps.hits.iter().map(|h| ix.path(h.id)).collect();
    let mut sp = paths.clone();
    sp.sort();
    assert_eq!(paths, sp);

    // Hit.vol is the volume index
    assert!(asc.hits.iter().all(|h| h.vol == 0));
}

#[test]
fn save_load_round_trip_and_bad_magic() {
    let mut ix = sample_index();
    ix.finalize();
    let dir: PathBuf = std::env::temp_dir();
    let path = dir.join(format!("floki-core-rt-{}-{}.bin", std::process::id(), 1));
    ix.save(&path).unwrap();
    // Saved with the v2 magic.
    let raw = std::fs::read(&path).unwrap();
    assert_eq!(&raw[..8], b"FLOKIDX2");
    let loaded = Index::load(&path).unwrap();
    assert_eq!(loaded.len(), ix.len());
    assert_eq!(loaded.volumes, ix.volumes);
    assert!(loaded.frn_is_fresh());
    assert!(loaded.by_name_is_fresh());
    for id in 0..ix.len() as u32 {
        assert_eq!(loaded.name(id), ix.name(id));
        assert_eq!(loaded.path(id), ix.path(id));
    }
    // FRN lookups survive the round trip, tombstones included.
    for frn in [100, 101, 200, 207] {
        assert_eq!(
            loaded.lookup(0, frn).map(|id| loaded.path(id)),
            ix.lookup(0, frn).map(|id| ix.path(id)),
        );
    }
    let o = all_opts();
    for qs in ["main", "ext:rs;toml", "folder:", "path:src", ""] {
        let a = search(&ix, &parse(qs), &o, None);
        let b = search(&loaded, &parse(qs), &o, None);
        assert_eq!(a, b, "query {qs}");
    }
    std::fs::remove_file(&path).ok();

    // corrupt magic -> error
    let bad = dir.join(format!("floki-core-bad-{}-{}.bin", std::process::id(), 2));
    std::fs::write(&bad, b"NOTANINDEX!!!!").unwrap();
    assert!(Index::load(&bad).is_err());
    std::fs::remove_file(&bad).ok();
}

// F1: compact bumps the epoch and returns the id remap; stale prev ids can
// never panic or hit the wrong entry (OOB -> no match).
#[test]
fn compact_epoch_and_remap() {
    let mut ix = Index::new();
    ix.add_volume(test_volume('C', 1));
    ix.push(0, 1, 1, "", DIRECTORY);
    ix.push(0, 2, 1, "aaa.txt", 0);
    ix.push(0, 3, 1, "bbb.txt", 0);
    assert_eq!(ix.epoch(), 0);
    let o = SearchOptions::new(0, 0, Sort::NameAsc);
    let stale = search(&ix, &parse("bbb"), &o, None);
    assert_eq!(stale.len(), 1);
    let stale_id = stale[0].id;
    ix.apply(0, IndexEvent::Delete { frn: 2 }); // delete aaa (id 1)
    let remap = ix.compact(); // bbb slides 2 -> 1
    assert_eq!(ix.epoch(), 1);
    assert_eq!(remap[stale_id as usize], ix.lookup(0, 3).unwrap());
    assert_eq!(remap[1], EntryId::MAX); // deleted aaa
                                        // Translating through the remap recovers the hit.
    let fresh = search(&ix, &parse("bbb"), &o, None);
    assert_eq!(fresh.len(), 1);
    assert_eq!(fresh[0].id, remap[stale_id as usize]);
    // Untranslated stale prev: no panic, and the dangling id simply misses.
    let r = search(&ix, &parse("bbb"), &o, Some(&stale));
    assert!(r.is_empty());
    // Garbage ids never panic either.
    let junk = vec![
        floki_core::Hit { id: 9999, vol: 0 },
        floki_core::Hit {
            id: EntryId::MAX,
            vol: 0,
        },
    ];
    assert!(search(&ix, &parse("bbb"), &o, Some(&junk)).is_empty());
}

// Rename/apply design: compact() with a non-empty pending list must
// translate pending ids through the remap (not silently attach live names
// to wrong ids), and search stays exact afterwards.
#[test]
fn compact_translates_pending() {
    let mut ix = Index::new();
    ix.add_volume(test_volume('C', 1));
    ix.push(0, 1, 1, "", DIRECTORY);
    ix.push(0, 2, 1, "aaa.txt", 0);
    ix.rebuild_by_name();
    ix.apply(
        0,
        IndexEvent::Create {
            frn: 3,
            parent_frn: 1,
            name: "zzz.txt",
            flags: 0,
        },
    );
    ix.apply(
        0,
        IndexEvent::Create {
            frn: 4,
            parent_frn: 1,
            name: "mmm.txt",
            flags: 0,
        },
    );
    assert_eq!(ix.pending_len(), 2);
    // One pending entry dies before compaction (exercises the MAX path).
    ix.apply(0, IndexEvent::Delete { frn: 4 });
    let _ = ix.compact();
    assert_eq!(ix.pending_len(), 0);
    assert!(ix.by_name_is_fresh());
    let all = search_paged(
        &ix,
        &parse(""),
        &SearchOptions::new(0, 0, Sort::NameAsc),
        None,
    );
    assert_eq!(all.total, 3);
    assert_eq!(names_of(&ix, &all.hits), vec!["", "aaa.txt", "zzz.txt"]);
    assert_eq!(ix.lookup(0, 3).and_then(|id| ix.name(id)), Some("zzz.txt"));
    // Tombstoned pending entry stays dead and unfindable.
    assert!(ix
        .lookup(0, 4)
        .is_none_or(|id| ix.entries[id as usize].is_tombstone()));
    assert!(search(&ix, &parse("mmm"), &all_opts(), None).is_empty());
}

// F1b: load bumps the epoch, so service caches from before a load are dropped.
#[test]
fn load_bumps_epoch() {
    let ix = sample_index();
    let dir: PathBuf = std::env::temp_dir();
    let path = dir.join(format!("floki-core-epoch-{}-{}.bin", std::process::id(), 7));
    ix.save(&path).unwrap();
    let loaded = Index::load(&path).unwrap();
    assert_eq!(loaded.epoch(), ix.epoch() + 1);
    std::fs::remove_file(&path).ok();
}

// Failure 1a: an index saved with pending updates (merged, tombstone-free,
// SHORTER by_name) loads without rescan, identical to a fresh rebuild.
fn live_updated_index() -> Index {
    let mut ix = Index::new();
    ix.add_volume(test_volume('C', 1));
    ix.push(0, 1, 1, "", DIRECTORY);
    for i in 0..500u64 {
        ix.push(0, 100 + i, 1, &format!("file_{i:04}.dat"), 0);
    }
    ix.finalize();
    ix.rebuild_by_name();
    for i in 0..100u64 {
        ix.apply(
            0,
            IndexEvent::Create {
                frn: 10_000 + i,
                parent_frn: 1,
                name: &format!("new_{i:03}.tmp"),
                flags: 0,
            },
        );
    }
    for i in (0..500u64).step_by(7) {
        ix.apply(0, IndexEvent::Delete { frn: 100 + i });
    }
    ix
}

#[test]
fn load_short_by_name_after_live_updates() {
    let ix = live_updated_index();
    assert_eq!(ix.pending_len(), 100);
    let path: PathBuf =
        std::env::temp_dir().join(format!("floki-short-{}-{}.bin", std::process::id(), 21));
    ix.save(&path).unwrap();
    // Precondition: the persisted by_name really is shorter than entries.
    let raw = std::fs::read(&path).unwrap();
    let hlen = u32::from_le_bytes(raw[8..12].try_into().unwrap()) as usize;
    let header: serde_json::Value = serde_json::from_slice(&raw[12..12 + hlen]).unwrap();
    let bn = header["by_name_len"].as_u64().unwrap();
    assert!(bn < ix.len() as u64, "merged save must drop tombstones");
    let loaded = Index::load(&path).unwrap();
    std::fs::remove_file(&path).ok();
    assert!(loaded.by_name_is_fresh());
    assert!(loaded.frn_is_fresh());
    // Identical to a fresh rebuild of the same state.
    let mut control = live_updated_index();
    control.rebuild_by_name();
    assert_eq!(loaded.by_name(), control.by_name());
    for qs in ["", "new_", "file_0001", "ext:tmp", "zzz_nope"] {
        let q = parse(qs);
        let o = SearchOptions::new(0, 0, Sort::NameAsc);
        assert_eq!(
            search_paged(&loaded, &q, &o, None).hits,
            search_paged(&control, &q, &o, None).hits,
            "query {qs}"
        );
        assert_eq!(count(&loaded, &q, None), count(&control, &q, None));
    }
    assert_eq!(
        loaded.lookup(0, 10_000).and_then(|id| loaded.name(id)),
        control.lookup(0, 10_000).and_then(|id| control.name(id))
    );
    assert_eq!(
        loaded.path(loaded.lookup(0, 10_000).unwrap()),
        control.path(control.lookup(0, 10_000).unwrap())
    );
}

// Failure 1b: a file with by_name.len() == entries.len() (old writer:
// tombstoned ids included) loads; the stale ids are skipped, never trusted.
#[test]
fn load_full_by_name_old_writer() {
    let mut ix = Index::new();
    ix.add_volume(test_volume('C', 1));
    ix.push(0, 1, 1, "", DIRECTORY);
    for i in 0..200u64 {
        ix.push(0, 100 + i, 1, &format!("old_{i:03}.dat"), 0);
    }
    ix.finalize();
    ix.rebuild_by_name();
    // Tombstones stay IN the snapshot (apply never touches by_name).
    for i in (0..200u64).step_by(5) {
        ix.apply(0, IndexEvent::Delete { frn: 100 + i });
    }
    assert_eq!(ix.pending_len(), 0);
    let path: PathBuf =
        std::env::temp_dir().join(format!("floki-full-{}-{}.bin", std::process::id(), 22));
    ix.save(&path).unwrap();
    let raw = std::fs::read(&path).unwrap();
    let hlen = u32::from_le_bytes(raw[8..12].try_into().unwrap()) as usize;
    let header: serde_json::Value = serde_json::from_slice(&raw[12..12 + hlen]).unwrap();
    assert_eq!(header["by_name_len"].as_u64().unwrap(), ix.len() as u64);
    let loaded = Index::load(&path).unwrap();
    std::fs::remove_file(&path).ok();
    assert!(loaded.by_name_is_fresh());
    assert_eq!(loaded.by_name().len(), loaded.len());
    let mut control = Index::new();
    control.add_volume(test_volume('C', 1));
    control.push(0, 1, 1, "", DIRECTORY);
    for i in 0..200u64 {
        control.push(0, 100 + i, 1, &format!("old_{i:03}.dat"), 0);
    }
    control.finalize();
    control.rebuild_by_name();
    for i in (0..200u64).step_by(5) {
        control.apply(0, IndexEvent::Delete { frn: 100 + i });
    }
    control.rebuild_by_name();
    for qs in ["", "old_", "old_0001"] {
        let q = parse(qs);
        let o = SearchOptions::new(0, 0, Sort::NameAsc);
        assert_eq!(
            search_paged(&loaded, &q, &o, None).hits,
            search_paged(&control, &q, &o, None).hits,
            "query {qs}"
        );
        assert_eq!(count(&loaded, &q, None), count(&control, &q, None));
    }
    // The tombstoned id resolves (lookup covers tombstones) but never matches.
    let tid = loaded.lookup(0, 100).unwrap();
    assert!(loaded.entries[tid as usize].is_tombstone());
    assert!(search(&loaded, &parse("old_000"), &all_opts(), None).is_empty());
}

/// Reassemble a v2 file with a replaced by_name section (same entries/arena/
/// vols/frn); fixes `by_name_len` in the header JSON.
fn rewrite_by_name_section(raw: &[u8], ids: &[EntryId]) -> Vec<u8> {
    let hlen = u32::from_le_bytes(raw[8..12].try_into().unwrap()) as usize;
    let mut header: serde_json::Value = serde_json::from_slice(&raw[12..12 + hlen]).unwrap();
    let n = header["entry_count"].as_u64().unwrap() as usize;
    let alen = header["arena_len"].as_u64().unwrap() as usize;
    let old_bn = header["by_name_len"].as_u64().unwrap() as usize;
    let eend = 12 + hlen + n * 24;
    let aend = eend + alen;
    let bend = aend + old_bn * 4;
    let vols = &raw[bend..bend + n];
    let frn = &raw[bend + n..];
    header["by_name_len"] = serde_json::json!(ids.len() as u64);
    let hbytes = serde_json::to_vec(&header).unwrap();
    let mut out = Vec::new();
    out.extend_from_slice(&raw[..8]);
    out.extend_from_slice(&(hbytes.len() as u32).to_le_bytes());
    out.extend_from_slice(&hbytes);
    out.extend_from_slice(&raw[12 + hlen..eend]);
    out.extend_from_slice(&raw[eend..aend]);
    for id in ids {
        out.extend_from_slice(&id.to_le_bytes());
    }
    out.extend_from_slice(vols);
    out.extend_from_slice(frn);
    out
}

fn read_by_name_section(raw: &[u8]) -> Vec<EntryId> {
    let hlen = u32::from_le_bytes(raw[8..12].try_into().unwrap()) as usize;
    let header: serde_json::Value = serde_json::from_slice(&raw[12..12 + hlen]).unwrap();
    let n = header["entry_count"].as_u64().unwrap() as usize;
    let alen = header["arena_len"].as_u64().unwrap() as usize;
    let bn = header["by_name_len"].as_u64().unwrap() as usize;
    let start = 12 + hlen + n * 24 + alen;
    let (words, _) = raw[start..start + bn * 4].as_chunks::<4>();
    words.iter().map(|c| EntryId::from_le_bytes(*c)).collect()
}

// Failure 1c: a corrupt by_name section (duplicated id, then a shortened
// array) still loads — via rebuild — identical to a fresh rebuild.
#[test]
fn load_corrupt_by_name_rebuilds() {
    let mut base = Index::new();
    base.add_volume(test_volume('C', 1));
    base.push(0, 1, 1, "", DIRECTORY);
    for i in 0..300u64 {
        base.push(0, 100 + i, 1, &format!("wfile_{i:03}.dat"), 0);
    }
    base.finalize();
    base.rebuild_by_name();
    let path: PathBuf =
        std::env::temp_dir().join(format!("floki-corrupt-{}-{}.bin", std::process::id(), 23));
    base.save(&path).unwrap();
    let raw = std::fs::read(&path).unwrap();
    let ids = read_by_name_section(&raw);
    assert_eq!(ids.len(), base.len());

    // Case A: duplicated id, same length (offsets intact).
    let mut duped = ids.clone();
    duped[5] = duped[0];
    let hlen = u32::from_le_bytes(raw[8..12].try_into().unwrap()) as usize;
    let n = {
        let h: serde_json::Value = serde_json::from_slice(&raw[12..12 + hlen]).unwrap();
        h["entry_count"].as_u64().unwrap() as usize
    };
    let alen = {
        let h: serde_json::Value = serde_json::from_slice(&raw[12..12 + hlen]).unwrap();
        h["arena_len"].as_u64().unwrap() as usize
    };
    let start = 12 + hlen + n * 24 + alen;
    let mut raw_dup = raw.clone();
    raw_dup[start + 5 * 4..start + 6 * 4].copy_from_slice(&duped[5].to_le_bytes());
    std::fs::write(&path, &raw_dup).unwrap();
    let loaded = Index::load(&path).unwrap();
    assert!(loaded.by_name_is_fresh());
    assert_eq!(loaded.by_name(), base.by_name());
    for qs in ["", "wfile_"] {
        let q = parse(qs);
        let o = SearchOptions::new(0, 0, Sort::NameAsc);
        assert_eq!(
            search_paged(&loaded, &q, &o, None).hits,
            search_paged(&base, &q, &o, None).hits,
            "dup query {qs}"
        );
    }

    // Case B: shortened array (drops live ids) with a fixed header.
    let short = rewrite_by_name_section(&raw, &ids[..ids.len() - 10]);
    std::fs::write(&path, &short).unwrap();
    let loaded = Index::load(&path).unwrap();
    assert!(loaded.by_name_is_fresh());
    assert_eq!(loaded.by_name(), base.by_name());
    for qs in ["", "wfile_"] {
        let q = parse(qs);
        let o = SearchOptions::new(0, 0, Sort::NameAsc);
        assert_eq!(
            search_paged(&loaded, &q, &o, None).hits,
            search_paged(&base, &q, &o, None).hits,
            "short query {qs}"
        );
    }
    std::fs::remove_file(&path).ok();
}

// Failure 2 (correctness, gate-covered): mid-scan lookups, searches and
// paths match the post-finalize results exactly (small scale; the full
// 500k timing characterization below is #[ignore]d like bench_1m).
#[test]
fn scan_mid_scan_matches_post_finalize() {
    let mut ix = Index::new();
    ix.add_volume(test_volume('C', 5));
    ix.push(0, 5, 5, "", DIRECTORY);
    ix.push(0, 50, 5, "subdir", DIRECTORY);
    for i in 0..5000u64 {
        let tag = if i % 500 == 0 { "needle" } else { "file" };
        let parent = if i % 100 == 0 { 50 } else { 5 };
        ix.push(0, 100_000 + i, parent, &format!("{tag}_{i:06}.dat"), 0);
        if (i + 1) % 1000 == 0 {
            ix.seal_batch();
        }
    }
    assert_eq!(ix.len(), 5002);
    assert!(!ix.frn_is_fresh());
    let q = parse("needle");
    let opts = SearchOptions::new(100, 0, Sort::NameAsc);
    let mut lookups = Vec::new();
    for k in 0..100u64 {
        lookups.push(ix.lookup(0, 100_000 + k * 49));
    }
    let r = search_paged(&ix, &q, &opts, None);
    assert_eq!(r.total, 10);
    assert_eq!(r.hits.len(), 10);
    let p0 = ix.path(ix.lookup(0, 100_000).unwrap());
    assert_eq!(p0, "C:\\subdir\\needle_000000.dat");
    ix.finalize();
    ix.rebuild_by_name();
    assert!(ix.frn_is_fresh());
    for (n, k) in (0..100u64).enumerate() {
        assert_eq!(ix.lookup(0, 100_000 + k * 49), lookups[n]);
    }
    let r2 = search_paged(&ix, &q, &opts, None);
    assert_eq!(r2.total, r.total);
    assert_eq!(r2.hits, r.hits);
    assert_eq!(ix.path(ix.lookup(0, 100_000).unwrap()), p0);
}

// Failure 2 (timing characterization, #[ignore]d like bench_1m): simulated
// scan (500k pushes, seal every 10k, no finalize); 1,000 lookups + 100
// searches x 100 hits each, with results equal to post-finalize. NOTE: the
// <300 ms bound holds in release but NOT in debug — 100 full debug scans
// cost ~40 ms each on this box (unoptimized matcher chain), so debug runs
// measure ~4 s. Run explicitly:
// `cargo test -p floki-core --release -- --ignored scan_sim_lookups_and_searches --nocapture`
#[test]
#[ignore]
fn scan_sim_lookups_and_searches() {
    let mut ix = Index::new();
    ix.add_volume(test_volume('C', 5));
    ix.push(0, 5, 5, "", DIRECTORY);
    ix.push(0, 50, 5, "subdir", DIRECTORY);
    for i in 0..500_000u64 {
        let tag = if i % 5000 == 0 { "needle" } else { "file" };
        let parent = if i % 1000 == 0 { 50 } else { 5 };
        ix.push(0, 100_000 + i, parent, &format!("{tag}_{i:06}.dat"), 0);
        if (i + 1) % 10_000 == 0 {
            ix.seal_batch();
        }
    }
    assert_eq!(ix.len(), 500_002);
    assert!(!ix.frn_is_fresh());
    let q = parse("needle");
    let opts = SearchOptions::new(100, 0, Sort::NameAsc);
    // Warm up once (pool creation, page faults) outside the timer.
    let _ = search_paged(&ix, &q, &opts, None);
    let t0 = Instant::now();
    let mut lookups = Vec::with_capacity(1000);
    for k in 0..1000u64 {
        lookups.push(ix.lookup(0, 100_000 + k * 499));
    }
    let mut pages = Vec::with_capacity(100);
    for _ in 0..100 {
        pages.push(search_paged(&ix, &q, &opts, None));
    }
    let ms = t0.elapsed().as_secs_f64() * 1000.0;
    println!("scan_sim: lookups=1000 searches=100 total_ms={ms:.1}");
    assert!(ms < 300.0, "mid-scan ops took {ms:.1} ms");
    for r in &pages {
        assert_eq!(r.total, 100);
        assert_eq!(r.hits.len(), 100);
    }
    // Multi-hop paths resolve mid-scan too.
    let p0 = ix.path(ix.lookup(0, 100_000).unwrap());
    assert_eq!(p0, "C:\\subdir\\needle_000000.dat");
    // Finalize + rebuild: every result must agree exactly.
    ix.finalize();
    ix.rebuild_by_name();
    assert!(ix.frn_is_fresh());
    for (n, k) in (0..1000u64).enumerate() {
        assert_eq!(ix.lookup(0, 100_000 + k * 499), lookups[n]);
    }
    for r in &pages {
        let r2 = search_paged(&ix, &q, &opts, None);
        assert_eq!(r2.total, r.total);
        assert_eq!(r2.hits, r.hits);
    }
    assert_eq!(ix.path(ix.lookup(0, 100_000).unwrap()), p0);
}

// Dedicated search pool: opt-in only (ignored) — the pool is process-global
// first-wins, and must not perturb the timing-sensitive scan test above.
// Uses a HIGH count deliberately: whichever of the two runs first, the scan
// test keeps enough threads (it never lowers the count).
#[test]
#[ignore]
fn search_pool_threads_smoke() {
    floki_core::set_search_threads(16);
    let ix = sample_index();
    let got = names_of(&ix, &search(&ix, &parse("main rs"), &all_opts(), None));
    assert_eq!(got, vec!["main.rs"]);
}

// F2: prev_reusable rejects broadening OR and quotes/groups; plain extension
// stays reusable and narrows exactly.
#[test]
fn prev_reusable_rejects_or_broadening() {
    assert!(!prev_reusable(&parse("aaa"), &parse("aaa|bbb")));
    assert!(!prev_reusable(&parse("aaa"), &parse("aaa bbb\"")));
    assert!(!prev_reusable(&parse("aaa"), &parse("aaa (bbb)")));
    assert!(!prev_reusable(&parse("aaa"), &parse("aaa <bbb>")));
    assert!(!prev_reusable(&parse("a|b"), &parse("a|bc")));
    assert!(!prev_reusable(&parse("!a"), &parse("!ab")));
    assert!(prev_reusable(&parse("foo"), &parse("foobar")));
    // End to end: full scan finds both; the gate forbids narrowing here.
    let mut ix = Index::new();
    ix.add_volume(test_volume('C', 1));
    ix.push(0, 1, 1, "", DIRECTORY);
    ix.push(0, 2, 1, "aaa", 0);
    ix.push(0, 3, 1, "bbb", 0);
    let o = SearchOptions::new(0, 0, Sort::NameAsc);
    let full = search(&ix, &parse("aaa|bbb"), &o, None);
    assert_eq!(full.len(), 2);
    // Safe narrowing still matches the full scan exactly.
    let mut jx = Index::new();
    jx.add_volume(test_volume('C', 1));
    jx.push(0, 1, 1, "", DIRECTORY);
    jx.push(0, 2, 1, "report_x", 0);
    jx.push(0, 3, 1, "other", 0);
    let q1 = parse("report");
    let q2 = parse("report_x");
    assert!(prev_reusable(&q1, &q2));
    let p1 = search(&jx, &q1, &o, None);
    assert_eq!(search(&jx, &q2, &o, Some(&p1)), search(&jx, &q2, &o, None));
}

// F3: Path sort pages are globally ordered, not page-local.
#[test]
fn path_sort_pages_are_global() {
    let mut ix = Index::new();
    ix.add_volume(test_volume('C', 1));
    ix.push(0, 1, 1, "", DIRECTORY);
    ix.push(0, 10, 1, "bdir", DIRECTORY);
    ix.push(0, 11, 1, "adir", DIRECTORY);
    ix.push(0, 20, 10, "f.txt", 0);
    ix.push(0, 21, 11, "f.txt", 0);
    let q = parse("");
    let full = search_paged(&ix, &q, &SearchOptions::new(0, 0, Sort::PathAsc), None);
    let p0 = search_paged(&ix, &q, &SearchOptions::new(1, 0, Sort::PathAsc), None);
    let p1 = search_paged(&ix, &q, &SearchOptions::new(1, 1, Sort::PathAsc), None);
    assert_eq!(
        vec![p0.hits[0], p1.hits[0]],
        vec![full.hits[0], full.hits[1]]
    );
    let desc = search_paged(&ix, &q, &SearchOptions::new(0, 0, Sort::PathDesc), None);
    let paths: Vec<String> = desc.hits.iter().map(|h| ix.path(h.id)).collect();
    let mut sp = paths.clone();
    sp.sort();
    sp.reverse();
    assert_eq!(paths, sp);
}

// F4: memory_usage covers entries + arena + by_name + frn_index + entry_vol
// + pending.
#[test]
fn memory_usage_counts_all() {
    let mut ix = Index::new();
    ix.add_volume(test_volume('C', 1));
    for i in 0..1000u64 {
        ix.push(0, 100 + i, 1, "some_name_for_sizing.txt", 0);
    }
    ix.finalize();
    let expect = ix.entries.len() * 24
        + ix.names.len()
        + ix.by_name().len() * 4
        + ix.len()
        + ix.len() * 4
        + ix.pending_len() * 4
        + ix.memory_breakdown().arena_aux_bytes as usize;
    assert_eq!(ix.memory_usage(), expect, "honest formula, no map");
    // frn_index holds every entry (4 B each) once finalized.
    assert!(ix.frn_is_fresh());
}

// F5: load rejects a corrupt name offset.
#[test]
fn load_rejects_bad_name_off() {
    let mut ix = Index::new();
    ix.add_volume(test_volume('C', 1));
    ix.push(0, 1, 1, "", DIRECTORY);
    ix.push(0, 2, 1, "hello.txt", 0);
    let path: PathBuf =
        std::env::temp_dir().join(format!("floki-badoff-{}.bin", std::process::id()));
    ix.save(&path).unwrap();
    let mut raw = std::fs::read(&path).unwrap();
    let hlen = u32::from_le_bytes(raw[8..12].try_into().unwrap()) as usize;
    let e0 = 12 + hlen;
    let name_off_pos = e0 + 24 + 16; // second entry's name_off field
    raw[name_off_pos..name_off_pos + 4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
    std::fs::write(&path, &raw).unwrap();
    assert!(Index::load(&path).is_err());
    std::fs::remove_file(&path).ok();
}

// F6: live updates keep by_name fresh via pending; save persists the merged
// view without an explicit rebuild; scan pushes still dirty the snapshot.
#[test]
fn by_name_freshness_and_save() {
    let mut ix = Index::new();
    ix.add_volume(test_volume('C', 1));
    ix.push(0, 1, 1, "", DIRECTORY);
    ix.push(0, 2, 1, "b.txt", 0);
    ix.push(0, 3, 1, "a.txt", 0);
    ix.rebuild_by_name();
    assert!(ix.by_name_is_fresh());
    assert_eq!(ix.pending_len(), 0);
    // Deletes tombstone in place: no pending entry, still fresh.
    ix.apply(0, IndexEvent::Delete { frn: 3 });
    assert!(ix.by_name_is_fresh());
    assert_eq!(ix.pending_len(), 0);
    // Creates/renames go to pending, still fresh (merge walk covers them).
    ix.apply(
        0,
        IndexEvent::Create {
            frn: 4,
            parent_frn: 1,
            name: "0.txt",
            flags: 0,
        },
    );
    assert!(ix.by_name_is_fresh());
    assert_eq!(ix.pending_len(), 1);
    ix.apply(
        0,
        IndexEvent::Rename {
            frn: 2,
            parent_frn: 1,
            name: "c.txt",
        },
    );
    assert!(ix.by_name_is_fresh());
    assert_eq!(ix.pending_len(), 2);
    // Save without an explicit rebuild: the file must still be sorted and
    // tombstone-free, WITH the pending entries merged in.
    let path: PathBuf =
        std::env::temp_dir().join(format!("floki-dirty-save-{}.bin", std::process::id()));
    ix.save(&path).unwrap();
    let loaded = Index::load(&path).unwrap();
    std::fs::remove_file(&path).ok();
    let names: Vec<&str> = loaded
        .by_name()
        .iter()
        .map(|&id| loaded.name(id).unwrap())
        .collect();
    assert!(!names.contains(&"a.txt"));
    assert!(!names.contains(&"b.txt")); // renamed away
    assert!(names.contains(&"0.txt"));
    assert!(names.contains(&"c.txt"));
    let mut s = names.to_vec();
    s.sort();
    assert_eq!(names, s);
    assert!(loaded.by_name_is_fresh());
    // Scan-style pushes DO dirty the snapshot (unsorted tail).
    ix.push(0, 5, 1, "z.txt", 0);
    assert!(!ix.by_name_is_fresh());
    // Explicit rebuild folds pending in and clears the flag.
    ix.rebuild_by_name();
    assert!(ix.by_name_is_fresh());
    assert_eq!(ix.pending_len(), 0);
}

// F7: path: matches via the component fast path, and needles spanning a `\`
// boundary fall back to the full rebuilt path.
#[test]
fn path_component_fast_path() {
    let ix = sample_index();
    let o = SearchOptions::new(0, 0, Sort::NameAsc);
    let mut got = names_of(&ix, &search(&ix, &parse("path:src"), &o, None));
    got.sort();
    assert_eq!(
        got,
        vec![
            "data.toml",
            "deep",
            "deep_file.md",
            "lib.rs",
            "main.rs",
            "src"
        ]
    );
    // Boundary-spanning needle still matches via the full-path fallback.
    let got = names_of(&ix, &search(&ix, &parse(r"path:src\main"), &o, None));
    assert_eq!(got, vec!["main.rs"]);
    // Ancestor-only needle matches the child (component walk, no full path).
    let got = names_of(&ix, &search(&ix, &parse("path:deep"), &o, None));
    assert_eq!(got.len(), 2);
}

// Single-segment path: verdict cache replays exactly (cold walk, then hot
// replay at every cap), mutations invalidate it, and case variants key
// separately. Also pins root / deep-chain / orphan shapes for the cached arm.
#[test]
fn path_verdict_cache_replay_and_invalidate() {
    let mut ix = Index::new();
    ix.add_volume(test_volume('C', 1));
    ix.push(0, 1, 1, "", DIRECTORY); // 0 root (self-parent)
    ix.push(0, 10, 1, "prog", DIRECTORY); // 1
    ix.push(0, 11, 10, "sub", DIRECTORY); // 2
    ix.push(0, 12, 11, "leaf", DIRECTORY); // 3 deep chain
    ix.push(0, 20, 12, "tool.exe", 0); // 4 ancestor-only match
    ix.push(0, 21, 1, "prog_readme.txt", 0); // 5 own-name match at root
    ix.push(0, 22, 999_999, "prog_orphan.txt", 0); // 6 orphan, own name matches
    ix.push(0, 23, 999_999, "plain.txt", 0); // 7 orphan, no match
    ix.push(0, 24, 1, "other.txt", 0); // 8 no match
    ix.push(0, 13, 10, "PROG", DIRECTORY); // 9 case-variant dir
    ix.rebuild_by_name();
    // Deep chain, root-level own-name, dir-name, and orphan shapes.
    let o = SearchOptions::new(0, 0, Sort::NameAsc);
    let mut got = names_of(&ix, &search(&ix, &parse("path:prog"), &o, None));
    got.sort();
    assert_eq!(
        got,
        vec![
            "PROG",
            "leaf",
            "prog",
            "prog_orphan.txt",
            "prog_readme.txt",
            "sub",
            "tool.exe"
        ]
    );
    // Cold vs replay: identical totals and pages at every cap (the replay
    // path serves the second and later identical queries).
    for qs in ["path:prog", "case:path:PROG", "path:zzz_nothing"] {
        let q = parse(qs);
        for (max, off) in [(0u32, 0u32), (1, 0), (3, 1), (100, 0)] {
            let opts = SearchOptions::new(max, off, Sort::NameAsc);
            let first = search_paged(&ix, &q, &opts, None);
            let second = search_paged(&ix, &q, &opts, None);
            assert_eq!(
                (second.total, second.hits),
                (first.total, first.hits),
                "{qs} {max} {off}"
            );
            assert_eq!(count(&ix, &q, None), first.total, "count {qs}");
        }
    }
    // Case variants key separately: different verdict sets, both cached.
    assert_eq!(search(&ix, &parse("case:path:PROG"), &o, None).len(), 1);
    assert_eq!(search(&ix, &parse("path:PROG"), &o, None).len(), 7);
    // A mutation invalidates: renaming the top dir drops the subtree out.
    ix.apply(
        0,
        IndexEvent::Rename {
            frn: 10,
            parent_frn: 1,
            name: "renamed",
        },
    );
    let mut got = names_of(&ix, &search(&ix, &parse("path:prog"), &o, None));
    got.sort();
    assert_eq!(got, vec!["PROG", "prog_orphan.txt", "prog_readme.txt"]);
    // A create under the renamed dir does not resurrect the old verdicts.
    ix.apply(
        0,
        IndexEvent::Create {
            frn: 300,
            parent_frn: 10,
            name: "newfile.txt",
            flags: 0,
        },
    );
    assert_eq!(count(&ix, &parse("path:prog"), None), 3);
}
// Path terms under a stream of live updates, no rebuild: dir renames move
// subtrees (tombstone+new), and all three needle shapes (plain, separator,
// case-sensitive) stay exact against the collect+sort reference.
#[test]
fn path_terms_under_live_updates() {
    let mut ix = Index::new();
    ix.add_volume(test_volume('C', 1));
    ix.push(0, 1, 1, "", DIRECTORY);
    ix.push(0, 10, 1, "srcdir", DIRECTORY);
    ix.push(0, 11, 1, "other", DIRECTORY);
    ix.push(0, 20, 10, "sub", DIRECTORY);
    for (i, n) in ["main.rs", "lib.rs", "notes.txt", "MiXeD.TxT"]
        .iter()
        .enumerate()
    {
        ix.push(0, 100 + i as u64, 20, n, 0);
    }
    for (i, n) in ["readme.md", "top.txt"].iter().enumerate() {
        ix.push(0, 200 + i as u64, 11, n, 0);
    }
    ix.rebuild_by_name();
    // Rename the middle dir: the whole subtree moves with it.
    ix.apply(
        0,
        IndexEvent::Rename {
            frn: 20,
            parent_frn: 10,
            name: "src2",
        },
    );
    // Fresh file under the renamed dir, plus an unrelated create.
    ix.apply(
        0,
        IndexEvent::Create {
            frn: 300,
            parent_frn: 20,
            name: "fresh.log",
            flags: 0,
        },
    );
    ix.apply(
        0,
        IndexEvent::Create {
            frn: 301,
            parent_frn: 11,
            name: "plain.dat",
            flags: 0,
        },
    );
    assert!(ix.by_name_is_fresh());
    // Reference: unlimited (collect+sort) order for each sort.
    for qs in [
        "path:src",
        "path:src2",
        "path:sub",
        r"path:srcdir\src2",
        "path:other",
        "case:path:SRC",
        "path:MiXeD",
        "case:path:mixed",
        "path:zzz_nonexistent",
    ] {
        let q = parse(qs);
        for s in [Sort::NameAsc, Sort::NameDesc] {
            let reference = search_paged(&ix, &q, &SearchOptions::new(0, 0, s), None);
            assert_eq!(count(&ix, &q, None), reference.total, "count {qs} {s:?}");
            for (max, off) in [(3u32, 0u32), (2, 1), (10, 0)] {
                let r = search_paged(&ix, &q, &SearchOptions::new(max, off, s), None);
                assert_eq!(r.total, reference.total, "total {qs} {s:?}");
                let expect: Vec<floki_core::Hit> = reference
                    .hits
                    .iter()
                    .skip(off as usize)
                    .take(max as usize)
                    .copied()
                    .collect();
                assert_eq!(r.hits, expect, "hits {qs} {s:?} {max} {off}");
            }
        }
    }
    // Spot checks: old dir name gone, new one resolves the moved subtree.
    let o = SearchOptions::new(0, 0, Sort::NameAsc);
    assert!(search(&ix, &parse("path:sub"), &o, None).is_empty());
    let got = names_of(&ix, &search(&ix, &parse("path:src2"), &o, None));
    assert!(got.contains(&"fresh.log".to_string()));
    assert!(got.contains(&"main.rs".to_string()));
    // Separator needle spanning the renamed boundary.
    let got = names_of(
        &ix,
        &search(&ix, &parse(r"path:srcdir\src2\main"), &o, None),
    );
    assert_eq!(got, vec!["main.rs"]);
    // Case-sensitive path on mixed-case names.
    assert_eq!(search(&ix, &parse("case:path:MiXeD"), &o, None).len(), 1);
    assert!(search(&ix, &parse("case:path:MIXED"), &o, None).is_empty());
    assert!(search(&ix, &parse("case:path:mixed"), &o, None).is_empty());
}

// F8: name sort orders mixed ASCII / non-ASCII names exactly like fold.
#[test]
fn name_sort_unicode_matches_fold() {
    let mut ix = Index::new();
    ix.add_volume(test_volume('C', 1));
    ix.push(0, 1, 1, "", DIRECTORY);
    for (i, n) in ["Zebra", "apple", "Äpfel", "äBC", "Banana", "Zürich"]
        .iter()
        .enumerate()
    {
        ix.push(0, 10 + i as u64, 1, n, 0);
    }
    let q = parse("");
    let asc = search_paged(&ix, &q, &SearchOptions::new(0, 0, Sort::NameAsc), None);
    let got: Vec<String> = asc
        .hits
        .iter()
        .map(|h| ix.name(h.id).unwrap().to_string())
        .collect();
    // Expected: ordered by Unicode-lowercased name (== fold), exactly what
    // the ASCII fast path must reproduce for ASCII pairs.
    let mut expect = got.clone();
    expect.sort_by_key(|a: &String| a.to_lowercase());
    assert_eq!(got, expect);
    // Non-ASCII names sort among (not after) ASCII ones.
    let pos_apple = got.iter().position(|n| n == "apple").unwrap();
    let pos_aepfel = got.iter().position(|n| n == "Äpfel").unwrap();
    assert!(pos_apple < pos_aepfel);
}

// F9: save leaves no .tmp behind (success path) and cleans up on failure.
#[test]
fn save_leaves_no_tmp() {
    let ix = sample_index();
    let dir: PathBuf = std::env::temp_dir();
    let path = dir.join(format!("floki-core-tmp-{}-{}.bin", std::process::id(), 9));
    let tmp = {
        let mut s = path.as_os_str().to_owned();
        s.push(".tmp");
        PathBuf::from(s)
    };
    std::fs::remove_file(&tmp).ok();
    std::fs::remove_file(&path).ok();
    ix.save(&path).unwrap();
    assert!(path.exists());
    assert!(!tmp.exists());
    std::fs::remove_file(&path).ok();
    // Failure (missing parent dir): no .tmp litter either.
    let bad = dir.join(format!("floki-no-such-dir-{}/x.bin", std::process::id()));
    assert!(ix.save(&bad).is_err());
}

// F13: the cycle guard stops at exactly MAX_PATH_HOPS components.
#[test]
fn hop_bound_is_exact() {
    let mut ix = Index::new();
    ix.add_volume(test_volume('C', 1));
    ix.push(0, 1, 2, "x", 0);
    ix.push(0, 2, 1, "y", 0);
    let p = ix.path(0);
    assert!(p.starts_with("C:\\"));
    assert!(p.matches('\\').count() <= 512);
}

// F14: tiny indexes return exact results (sequential path, no rayon).
#[test]
fn tiny_index_search_exact() {
    let mut ix = Index::new();
    ix.add_volume(test_volume('C', 1));
    ix.push(0, 1, 1, "", DIRECTORY);
    ix.push(0, 2, 1, "alpha.txt", 0);
    ix.push(0, 3, 1, "beta.txt", 0);
    assert!(ix.len() < 4096);
    let o = SearchOptions::new(0, 0, Sort::NameAsc);
    assert_eq!(search(&ix, &parse("alpha"), &o, None).len(), 1);
    assert_eq!(count(&ix, &parse("txt"), None), 2);
    let prev = search(&ix, &parse("a"), &o, None);
    assert_eq!(search(&ix, &parse("alpha"), &o, Some(&prev)).len(), 1);
}

// Edge-case contracts (F10/F11/F12/F15): behavior locked, not changed.
#[test]
fn query_edge_contracts() {
    assert!(parse("|").is_match_all());
    assert!(parse("ext:").is_match_all());
    assert!(parse("case:").is_match_all());
    assert!(parse("path:").is_match_all());
    // Unclosed quote accepted as a substring.
    let q = parse("\"foo");
    assert!(matches!(&q.root, floki_core::Node::Term(t)
        if matches!(&t.kind, floki_core::TermKind::Substring(s) if s == "foo")));
    // regex: is case-insensitive unless case: is stacked.
    let ix = sample_index();
    let o = SearchOptions::new(0, 0, Sort::NameAsc);
    assert_eq!(search(&ix, &parse("regex:^MAIN\\.rs$"), &o, None).len(), 1);
    assert!(search(&ix, &parse("case:regex:^MAIN\\.rs$"), &o, None).is_empty());
}

// Latency work, two-phase design:
// (a) push-dirty reference (collect+sort) agrees with the fresh two-phase
//     search for every sort/paging/prev/count combination;
// (b) after 1,000 mixed applies with NO rebuild (pending merge-walk path),
//     every bounded page equals the unlimited (collect+sort) reference;
// (c) prev narrowing keeps working throughout.
#[test]
fn by_name_walk_equals_fallback() {
    use floki_core::Sort::*;
    let mut ix = Index::new();
    ix.add_volume(test_volume('C', 1));
    ix.push(0, 1, 1, "", DIRECTORY);
    ix.push(0, 10, 1, "adir", DIRECTORY);
    ix.push(0, 11, 1, "bdir", DIRECTORY);
    let raw_names = [
        "Zebra.txt",
        "apple",
        "Äpfel",
        "äBC",
        "Banana.DLL",
        "Zürich.md",
        "main.rs",
        "lib.rs",
        "data.toml",
        "photo.PNG",
        "notes.TXT",
        "setup.exe",
        "deep_file.md",
        "report_0001.txt",
        "report_0002.txt",
        "a",
        "A",
        "aA",
        "aa",
        ".hidden",
        "noext",
        "archive.tar.gz",
        "UPPER.DLL",
        "MiXeD.TxT",
        "résumé.pdf",
        "naïve.rs",
        "file with spaces.log",
        "bdir_file.txt",
        "adir_file.txt",
    ];
    for (i, n) in raw_names.iter().enumerate() {
        let parent = if n.contains("adir") {
            10
        } else if n.contains("bdir") {
            11
        } else {
            1
        };
        ix.push(0, 100 + i as u64, parent, n, 0);
    }
    // Pad past the rayon threshold so the parallel phase-1 scan runs.
    for i in 0..5000u64 {
        ix.push(0, 100_000 + i, 1, &format!("pad_{i:05}.tmp"), 0);
    }
    let queries = [
        "",
        "a",
        "report",
        "ext:rs;toml",
        "ext:TXT",
        "main rs",
        "rs !main",
        "*.rs",
        "wfn:main.rs",
        "regex:^m.*\\.rs$",
        "case:MAIN",
        "case:main",
        "path:adir",
        "folder:",
        "file:txt",
        "zzz:qqq",
        "ä",
        "e s",
    ];
    let sorts = [NameAsc, NameDesc, PathAsc, PathDesc];
    let pages = [
        (3u32, 0u32),
        (5, 2),
        (10, 7),
        (100, 0),
        (50, 3),
        (10, 10_000),
    ];
    let check_prev = |ix: &Index, qs: &str| {
        let q = parse(qs);
        let q1 = parse("rep");
        let full = search(ix, &q1, &all_opts(), None);
        if prev_reusable(&q1, &q) {
            let narrowed = search(ix, &q, &all_opts(), Some(&full));
            let expected = search(ix, &q, &all_opts(), None);
            assert_eq!(narrowed, expected, "prev {qs}");
        }
    };
    // (a) Dirty index (push tail): every Name query takes collect+sort.
    assert!(!ix.by_name_is_fresh());
    let mut reference = Vec::new();
    for qs in queries {
        let q = parse(qs);
        reference.push((qs, count(&ix, &q, None), {
            let mut v = Vec::new();
            for s in sorts {
                for (max, off) in pages {
                    let r = search_paged(&ix, &q, &SearchOptions::new(max, off, s), None);
                    v.push(r);
                }
            }
            v
        }));
        check_prev(&ix, qs);
    }
    // (b) Fresh index: two-phase must agree with the dirty reference.
    ix.rebuild_by_name();
    assert!(ix.by_name_is_fresh());
    for (qs, total, pages_ref) in &reference {
        let q = parse(qs);
        assert_eq!(count(&ix, &q, None), *total, "count {qs}");
        let mut i = 0;
        for s in sorts {
            for (max, off) in pages {
                let r = search_paged(&ix, &q, &SearchOptions::new(max, off, s), None);
                assert_eq!(r.total, pages_ref[i].total, "total {qs} {s:?} {max} {off}");
                assert_eq!(r.hits, pages_ref[i].hits, "hits {qs} {s:?} {max} {off}");
                i += 1;
            }
        }
        check_prev(&ix, qs);
        let q1 = parse("rep");
        let full = search(&ix, &q1, &all_opts(), None);
        if prev_reusable(&q1, &q) {
            assert_eq!(count(&ix, &q, Some(&full)), *total, "prev count {qs}");
        }
    }
    // (c) 1,000 mixed applies, NO rebuild: pending merge-walk path. Every
    // bounded page must equal the unlimited (collect+sort) reference.
    let words = ["Äpfel", "Zebra", "mango", "naïve", "UPPER", "résumé"];
    for i in 0..600u64 {
        let name = format!("live_{i:04}_{}.tmp", words[(i as usize) % words.len()]);
        ix.apply(
            0,
            IndexEvent::Create {
                frn: 200_000 + i,
                parent_frn: 1,
                name: &name,
                flags: 0,
            },
        );
    }
    for i in 0..raw_names.len() {
        let name = format!("renamed_{i:02}_{}.log", words[i % words.len()]);
        ix.apply(
            0,
            IndexEvent::Rename {
                frn: 100 + i as u64,
                parent_frn: 1,
                name: &name,
            },
        );
    }
    for i in 0..220u64 {
        let name = format!("renamed_pad_{i:04}.log");
        ix.apply(
            0,
            IndexEvent::Rename {
                frn: 100_000 + i,
                parent_frn: 1,
                name: &name,
            },
        );
    }
    for i in 0..100u64 {
        ix.apply(0, IndexEvent::Delete { frn: 200_000 + i });
    }
    for i in 0..50u64 {
        ix.apply(0, IndexEvent::Delete { frn: 104_000 + i });
    }
    assert!(ix.by_name_is_fresh(), "1000 applies stay under PENDING_MAX");
    for qs in queries {
        let q = parse(qs);
        for s in sorts {
            // Unlimited always goes collect+sort: the reference order.
            let reference = search_paged(&ix, &q, &SearchOptions::new(0, 0, s), None);
            assert_eq!(
                count(&ix, &q, None),
                reference.total,
                "live count {qs} {s:?}"
            );
            for (max, off) in pages {
                let r = search_paged(&ix, &q, &SearchOptions::new(max, off, s), None);
                assert_eq!(
                    r.total, reference.total,
                    "live total {qs} {s:?} {max} {off}"
                );
                let expect: Vec<floki_core::Hit> = reference
                    .hits
                    .iter()
                    .skip(off as usize)
                    .take(max as usize)
                    .copied()
                    .collect();
                assert_eq!(r.hits, expect, "live hits {qs} {s:?} {max} {off}");
            }
        }
        check_prev(&ix, qs);
    }
}

// Rename allocates a new id and the entry takes its correct name position:
// full NameAsc order, lookup, old-id tombstone, and search visibility.
#[test]
fn rename_takes_name_position() {
    let mut ix = Index::new();
    ix.add_volume(test_volume('C', 1));
    ix.push(0, 1, 1, "", DIRECTORY);
    for (i, n) in ["mango", "apple", "cherry", "Banana"].iter().enumerate() {
        ix.push(0, 10 + i as u64, 1, n, 0);
    }
    ix.rebuild_by_name();
    let before = ix.lookup(0, 10).unwrap(); // mango
    ix.apply(
        0,
        IndexEvent::Rename {
            frn: 10,
            parent_frn: 1,
            name: "apricot",
        },
    );
    assert!(ix.by_name_is_fresh());
    assert_eq!(ix.pending_len(), 1);
    let after = ix.lookup(0, 10).unwrap();
    assert_ne!(after, before);
    assert!(ix.entries[before as usize].is_tombstone());
    assert_eq!(ix.name(after), Some("apricot"));
    assert_eq!(ix.path(after), "C:\\apricot");
    // Full name order reflects the new name (merge walk, no rebuild).
    let all = search_paged(
        &ix,
        &parse(""),
        &SearchOptions::new(0, 0, Sort::NameAsc),
        None,
    );
    assert_eq!(
        names_of(&ix, &all.hits),
        vec!["", "apple", "apricot", "Banana", "cherry"]
    );
    let desc = search_paged(
        &ix,
        &parse(""),
        &SearchOptions::new(0, 0, Sort::NameDesc),
        None,
    );
    assert_eq!(
        names_of(&ix, &desc.hits),
        vec!["cherry", "Banana", "apricot", "apple", ""]
    );
    // Search visibility follows the new name.
    let o = SearchOptions::new(0, 0, Sort::NameAsc);
    assert!(search(&ix, &parse("mango"), &o, None).is_empty());
    assert_eq!(search(&ix, &parse("apricot"), &o, None).len(), 1);
}

// M5: 1000 live events after finalize keep lookup + path exact.
#[test]
fn live_updates_after_finalize_1000() {
    let mut ix = Index::new();
    ix.add_volume(test_volume('C', 100));
    ix.push(0, 100, 100, "", DIRECTORY); // root
    ix.finalize();
    assert!(ix.frn_is_fresh());
    // Scan end, like the daemon: rebuild the name snapshot before serving
    // live updates (push alone leaves an unsorted tail behind).
    ix.rebuild_by_name();
    assert!(ix.by_name_is_fresh());

    // 1000 creates post-finalize (frn-index tail appends + pending inserts;
    // the tail seals into sorted runs, so lookups never go linear).
    for i in 0..1000u64 {
        let name = format!("live_{i:04}.txt");
        ix.apply(
            0,
            IndexEvent::Create {
                frn: 50_000 + i,
                parent_frn: 100,
                name: &name,
                flags: 0,
            },
        );
    }
    assert!(ix.by_name_is_fresh());
    assert_eq!(ix.pending_len(), 1000);
    assert_eq!(ix.len(), 1001);
    for i in 0..1000u64 {
        let id = ix.lookup(0, 50_000 + i).expect("created frn resolves");
        assert_eq!(ix.name(id), Some(format!("live_{i:04}.txt")).as_deref());
        assert_eq!(ix.path(id), format!("C:\\live_{i:04}.txt"));
    }

    // Rename the even half (tombstone + new id each; lookup moves on).
    for i in (0..1000u64).step_by(2) {
        let before = ix.lookup(0, 50_000 + i).unwrap();
        let name = format!("renamed_{i:04}.txt");
        ix.apply(
            0,
            IndexEvent::Rename {
                frn: 50_000 + i,
                parent_frn: 100,
                name: &name,
            },
        );
        let after = ix.lookup(0, 50_000 + i).unwrap();
        assert_ne!(after, before);
        assert!(ix.entries[before as usize].is_tombstone());
        assert_eq!(ix.name(after), Some(name.as_str()));
    }
    assert!(ix.by_name_is_fresh());
    assert_eq!(ix.pending_len(), 1500);
    // Delete every third entry (tombstones stay resolvable until compact).
    for i in (0..1000u64).step_by(3) {
        ix.apply(0, IndexEvent::Delete { frn: 50_000 + i });
    }
    for i in 0..1000u64 {
        let id = ix
            .lookup(0, 50_000 + i)
            .expect("tombstones stay in frn_index");
        let deleted = i.is_multiple_of(3);
        assert_eq!(ix.entries[id as usize].is_tombstone(), deleted);
        let stem = if i.is_multiple_of(2) {
            format!("renamed_{i:04}.txt")
        } else {
            format!("live_{i:04}.txt")
        };
        assert_eq!(ix.path(id), format!("C:\\{stem}"));
    }
}

/// Apply `creates` live creates (odd FRNs, so they land between existing
/// groups) to a finalized index of `base` entries, sealing once per 1000
/// events like the daemon does per batch. Returns the apply wall time.
fn burst_apply_cost(base: u64, creates: u64) -> std::time::Duration {
    let mut ix = Index::new();
    ix.add_volume(test_volume('C', 100));
    ix.push(0, 100, 100, "", DIRECTORY);
    for i in 0..base {
        ix.push(0, 1_000 + i * 2, 100, &format!("base_{i}.dat"), 0);
    }
    ix.finalize();
    ix.rebuild_by_name();
    let stride = (base * 2 / creates).max(2) | 1;
    let started = Instant::now();
    for i in 0..creates {
        ix.apply(
            0,
            IndexEvent::Create {
                frn: 1_001 + i * stride,
                parent_frn: 100,
                name: &format!("burst_{i}.o"),
                flags: 0,
            },
        );
        if i % 1_000 == 999 {
            ix.seal_batch();
        }
    }
    let elapsed = started.elapsed();
    for i in (0..creates).step_by(997) {
        let id = ix
            .lookup(0, 1_001 + i * stride)
            .expect("burst frn resolves");
        assert_eq!(ix.name(id), Some(format!("burst_{i}.o")).as_deref());
        assert_eq!(ix.path(id), format!(r"C:\burst_{i}.o"));
    }
    elapsed
}

/// A journal burst (a build creating tens of thousands of files, or a
/// boot-time replay of a long backlog) must cost O(log n) per event. The old
/// `apply` kept `frn_index` one sorted run with a `Vec::insert` per create —
/// an O(n) memmove each — so a 1.5M-event replay on a 9.3M-entry index held
/// the write lock for ~27 minutes and every search hung behind it. Compares
/// the same burst on a 20x larger index: linear-per-event cost scales ~20x,
/// logarithmic cost stays roughly flat.
#[test]
fn live_create_burst_is_not_linear_per_event() {
    let small = burst_apply_cost(100_000, 20_000);
    let large = burst_apply_cost(2_000_000, 20_000);
    eprintln!("burst apply: 100k base {small:?}, 2M base {large:?}");
    assert!(
        large.as_secs_f64() < small.as_secs_f64() * 5.0 + 0.25,
        "20k creates: {small:?} at 100k entries vs {large:?} at 2M (O(n) per event?)"
    );
}

// M5: v1 files (FLOKIDX1, no frn_index bytes) still load by rebuilding.
#[test]
fn load_v1_rebuilds_frn_index() {
    let mut ix = sample_index();
    ix.finalize();
    // Craft a v1 image with the public API: same layout minus frn_index.
    let header = serde_json::json!({
        "volumes": ix.volumes,
        "entry_count": ix.entries.len() as u64,
        "arena_len": ix.names.len() as u64,
        "by_name_len": ix.by_name().len() as u64,
    });
    let hbytes = serde_json::to_vec(&header).unwrap();
    let mut raw = Vec::new();
    raw.extend_from_slice(b"FLOKIDX1");
    raw.extend_from_slice(&(hbytes.len() as u32).to_le_bytes());
    raw.extend_from_slice(&hbytes);
    for e in &ix.entries {
        raw.extend_from_slice(&e.to_le_bytes());
    }
    raw.extend_from_slice(ix.names.as_bytes());
    for id in ix.by_name() {
        raw.extend_from_slice(&id.to_le_bytes());
    }
    for id in 0..ix.len() as u32 {
        raw.push(ix.volume_of(id).unwrap_or(0));
    }
    let dir: PathBuf = std::env::temp_dir();
    let path = dir.join(format!("floki-core-v1-{}-{}.bin", std::process::id(), 3));
    std::fs::write(&path, &raw).unwrap();
    let loaded = Index::load(&path).unwrap();
    std::fs::remove_file(&path).ok();
    assert_eq!(loaded.len(), ix.len());
    assert!(loaded.frn_is_fresh());
    for id in 0..ix.len() as u32 {
        assert_eq!(loaded.name(id), ix.name(id));
        assert_eq!(loaded.path(id), ix.path(id));
    }
    for frn in [100, 101, 200, 207] {
        assert_eq!(loaded.lookup(0, frn), ix.lookup(0, frn));
    }
    // Saving the loaded v1 index upgrades it to v2 on disk.
    let path2 = dir.join(format!("floki-core-v1up-{}-{}.bin", std::process::id(), 4));
    loaded.save(&path2).unwrap();
    let raw2 = std::fs::read(&path2).unwrap();
    assert_eq!(&raw2[..8], b"FLOKIDX2");
    std::fs::remove_file(&path2).ok();
}

/// Synthetic 1M-entry benchmark. Run with:
/// `cargo test -p floki-core --release -- --ignored bench_1m --nocapture`
#[test]
#[ignore]
fn bench_1m() {
    // Production gate runs the service pool at 4 threads: pin the pool here
    // so before/after numbers are comparable (first call wins process-wide).
    set_search_threads(4);
    let mut ix = Index::new();
    ix.add_volume(test_volume('C', 5));
    ix.reserve(1_020_000);
    ix.push(0, 5, 5, "", DIRECTORY);
    // Directory tree (depth 3, production-like): 200 level-1 dirs under the
    // root, 200 level-2 dirs under NEUTRAL level-1 dirs only (2..199), so the
    // `program_files` (frn 1000) and `Windows_sys` (frn 1001) subtrees hold
    // exactly their direct files plus themselves — trivially countable.
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
    let exts = [
        "txt", "rs", "dll", "exe", "log", "md", "json", "png", "tmp", "dat",
    ];
    let mut i = 0u32;
    while ix.len() < 1_000_401 {
        // realistic ~30-char names; every 1000th carries a rare needle.
        // The first seven of those carry an ultra-rare tag instead (7-hit
        // query), every other 50th entry carries a mid-frequency tag
        // (~19k-hit query), every 10th a ~100k-hit tag (mid-band probe), and
        // every 17th (doc only) a mixed-case tag (~52k-hit case: probe).
        // A 6-extension query covers 60 % of entries.
        let tag = if i < 7000 && i.is_multiple_of(1000) {
            "qw7x"
        } else if i.is_multiple_of(1000) {
            "zxq"
        } else if i.is_multiple_of(50) {
            "k20"
        } else if i % 10 == 7 {
            "mid100"
        } else if i.is_multiple_of(17) {
            "Windows"
        } else {
            "doc"
        };
        // One file in twelve lives directly under program_files (path: probe
        // ~8 %, like Program Files at scale); one in thirteen (not under
        // program) under Windows_sys (case: probe ~7 %); the rest round-robin
        // under level-2 dirs (depth 3).
        let parent = if i.is_multiple_of(12) {
            1000
        } else if i.is_multiple_of(13) {
            1001
        } else {
            2000 + (i % 200) as u64
        };
        let name = format!(
            "{tag}_report_final_v{i:06}_{}.{}",
            i % 97,
            exts[(i as usize) % exts.len()]
        );
        ix.push(0, 100_000 + u64::from(i), parent, &name, 0);
        i += 1;
    }
    assert_eq!(ix.len(), 1_000_401);
    // Steady state: a real scan sorts the FRN index once before serving
    // queries; measure honest RAM (entries + arena + by_name + frn_index +
    // entry_vol + pending) and time search on the finalized index.
    ix.finalize();
    ix.rebuild_by_name();
    // Steady-state service index: both lookup maps fresh.
    //
    // Then replay 30,000 neutral live events with NO rebuild (the production
    // regime: journal trickle between compacts). All three query totals are
    // unaffected by construction: creates use an unqueried `.liv` extension,
    // renames keep their extension, deletes only touch `doc` entries whose
    // extension is outside the queried list.
    for i in 0..18_000u64 {
        let name = format!("liveevt_{i:05}.liv");
        ix.apply(
            0,
            IndexEvent::Create {
                frn: 2_000_000 + i,
                parent_frn: 5,
                name: &name,
                flags: 0,
            },
        );
    }
    let mut renamed = 0;
    for i in 0..1_000_000u64 {
        if renamed >= 8000 {
            break;
        }
        let idx = i as usize;
        // Totals must not move: qw7x/zxq/k20 carriers, the mid100/Windows
        // tags, and the program/Windows subtrees (name AND parent changes
        // would leak into the path:/case: probes).
        if idx.is_multiple_of(50)
            || idx % 10 == 7
            || idx.is_multiple_of(17)
            || idx.is_multiple_of(12)
            || idx.is_multiple_of(13)
        {
            continue;
        }
        let name = format!(
            "doc_renamed_v{i:06}_{}.{ext}",
            idx % 97,
            ext = exts[idx % 10]
        );
        ix.apply(
            0,
            IndexEvent::Rename {
                frn: 100_000 + i,
                parent_frn: 5,
                name: &name,
            },
        );
        renamed += 1;
    }
    assert_eq!(renamed, 8000);
    let mut deleted = 0;
    for i in 0..1_000_000u64 {
        if deleted >= 4000 {
            break;
        }
        let idx = i as usize;
        // Keep qw7x/k20, the mid100/Windows tags, the 6 queried extensions,
        // and the program/Windows subtrees intact.
        if idx.is_multiple_of(50)
            || idx % 10 < 6
            || idx % 10 == 7
            || idx.is_multiple_of(17)
            || idx.is_multiple_of(12)
            || idx.is_multiple_of(13)
        {
            continue;
        }
        ix.apply(0, IndexEvent::Delete { frn: 100_000 + i });
        deleted += 1;
    }
    assert_eq!(deleted, 4000);
    // The fast path must still be live: pending well under PENDING_MAX.
    assert_eq!(ix.pending_len(), 26_000);
    assert!(ix.by_name_is_fresh());
    let bench = |qs: &str, expect_total: u64| {
        let q = parse(qs);
        let opts = SearchOptions::new(100, 0, Sort::NameAsc);
        // warm up, then time the count (phase 1 alone) and the full page.
        let _ = search(&ix, &q, &opts, None);
        let c0 = Instant::now();
        let ctotal = count(&ix, &q, None);
        let cms = c0.elapsed().as_secs_f64() * 1000.0;
        let t0 = Instant::now();
        let res = search_paged(&ix, &q, &opts, None);
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        println!(
            "bench_1m: query={qs:?} matches_total={} page={} count_ms={cms:.1} search_ms={ms:.1}",
            res.total,
            res.hits.len(),
        );
        assert_eq!(ctotal, expect_total, "count {qs}");
        assert_eq!(res.total, expect_total, "query {qs}");
        assert!(res.hits.len() <= 100);
        // Page is globally name-ordered.
        let folded: Vec<String> = res
            .hits
            .iter()
            .map(|h| ix.name(h.id).unwrap().to_lowercase())
            .collect();
        let mut s = folded.clone();
        s.sort();
        assert_eq!(folded, s, "query {qs}");
        ms
    };
    let ms_rare = bench("qw7x", 7);
    let ms_mid = bench("k20", 19_000);
    let ms_big = bench("ext:dll,exe,log,md,rs,txt", 600_000);
    // Slow-class probes (one per class from the live p95 list).
    let ms_path = bench("path:program", 83_335);
    let ms_mid100 = bench("mid100", 100_000);
    let ms_glob = bench("*.log", 100_000);
    let ms_regex = bench("regex:^[a-c].*\\.txt$", 0);
    let ms_case = bench("case:Windows", 51_765);
    let ms_ext1 = bench("ext:rs", 100_000);
    let bytes = ix.entries.len() * 24 + ix.names.len() + ix.by_name().len() * 4;
    println!(
        "bench_1m: entries={} pending={} rare_7_ms={ms_rare:.1} mid_19k_ms={ms_mid:.1} big_600k_ms={ms_big:.1} path_87k_ms={ms_path:.1} mid100k_ms={ms_mid100:.1} glob_100k_ms={ms_glob:.1} regex_0_ms={ms_regex:.1} case_71k_ms={ms_case:.1} ext_100k_ms={ms_ext1:.1} bytes={} ({:.1}/entry) memory_usage={} ({:.1}/entry)",
        ix.len(),
        ix.pending_len(),
        bytes,
        bytes as f64 / ix.len() as f64,
        ix.memory_usage(),
        ix.memory_usage() as f64 / ix.len() as f64,
    );
    // Fold timing last (rebuild clears pending): linear merge of the waiting
    // updates into the snapshot.
    let pending_before = ix.pending_len();
    let f0 = Instant::now();
    ix.rebuild_by_name();
    let fold_ms = f0.elapsed().as_secs_f64() * 1000.0;
    println!(
        "bench_1m: fold pending={pending_before} fold_ms={fold_ms:.1} fresh={}",
        ix.by_name_is_fresh()
    );
    assert_eq!(ix.pending_len(), 0);
    assert!(ix.by_name_is_fresh());
}

/// 8M-entry fold/save soak: RSS must track the structural total within a few
/// MB across 60+ apply-30k/fold cycles (plus periodic saves), proving fold
/// generations are returned rather than retained.
///
/// Run with:
/// `cargo test -p floki-core --release -- --ignored soak_8m_fold_rss_flat --nocapture`
///
/// Background: at 8M entries each fold swaps a ~32 MB `by_name` plus a
/// ~32 MB `frn_index` generation. If the install retained the displaced
/// generation, the RSS-vs-structural gap would grow ~64 MB per cycle; in a
/// passing run the gap stays within a few MB end to end (the allocator
/// returns the freed generations synchronously). No searches run here on
/// purpose: broad-query collect-all fallbacks allocate large search
/// transients whose working-set settle timing is OS-heap behavior, measured
/// separately, not fold retention. Synthetic fixture only (no real names).
#[test]
#[ignore]
#[cfg(windows)]
fn soak_8m_fold_rss_flat() {
    use windows_sys::Win32::System::ProcessStatus::{
        K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    fn rss_mb() -> f64 {
        unsafe {
            let h = GetCurrentProcess();
            let mut pmc = std::mem::zeroed::<PROCESS_MEMORY_COUNTERS>();
            pmc.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
            if K32GetProcessMemoryInfo(h, &mut pmc, pmc.cb) == 0 {
                return -1.0;
            }
            pmc.WorkingSetSize as f64 / 1_048_576.0
        }
    }

    fn structural_mb(ix: &Index) -> f64 {
        ix.memory_breakdown().total_bytes() as f64 / 1_048_576.0
    }

    const BASE: usize = 8_000_000;
    const CYCLES: usize = 65;
    const PER_CYCLE: usize = 30_000;

    let mut ix = Index::new();
    ix.add_volume(test_volume('C', 5));
    ix.reserve(BASE + CYCLES * PER_CYCLE + 1024);
    ix.push(0, 5, 5, "", DIRECTORY);
    for i in 0..BASE as u64 {
        ix.push(0, 100_000 + i, 5, &format!("soak_{i:07}.dat"), 0);
    }
    ix.finalize();
    ix.rebuild_by_name();

    let dir = std::env::temp_dir().join(format!("floki_soak8m_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let save_path = dir.join("soak.bin");

    let mut frn = 100_000 + BASE as u64 + 1_000_000;
    let mut min_gap = f64::INFINITY;
    let mut max_gap = f64::NEG_INFINITY;
    for c in 0..CYCLES {
        for i in 0..PER_CYCLE as u64 {
            // Increasing FRNs append at the frn_index end (journal shape).
            let name = format!("churn_{c:03}_{i:05}.liv");
            ix.apply(
                0,
                IndexEvent::Create {
                    frn,
                    parent_frn: 5,
                    name: &name,
                    flags: 0,
                },
            );
            frn += 1;
        }
        // Daemon pending-fold path: snapshot off-lock, then install.
        let snap = ix.sorted_snapshot();
        assert!(ix.install_sorted(snap));
        assert_eq!(ix.pending_len(), 0);
        if c % 16 == 15 || c + 1 == CYCLES {
            ix.save(&save_path).unwrap();
        }
        let rss = rss_mb();
        let structural = structural_mb(&ix);
        let gap = rss - structural;
        min_gap = min_gap.min(gap);
        max_gap = max_gap.max(gap);
        println!(
            "soak_8m: cycle={c:02} entries={} rss={rss:.1}MB structural={structural:.1}MB gap={gap:.1}MB",
            ix.len(),
        );
    }
    std::fs::remove_dir_all(&dir).ok();
    println!("soak_8m: gap_min={min_gap:.1}MB gap_max={max_gap:.1}MB");
    // One retained fold generation would add ~64 MB to the gap; the whole
    // 65-cycle run must stay within a few MB end to end.
    assert!(
        max_gap - min_gap <= 10.0,
        "RSS gap wandered {min_gap:.1}..{max_gap:.1} MB across {CYCLES} fold cycles"
    );
}
