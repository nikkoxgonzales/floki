//! Exercise the real enumerate + journal path on one volume and report what
//! the index would see: file system, root id, record count, unresolved
//! parents, root-level `$` names, and how long a create/rename/delete takes
//! to reach the journal (ReFS buffers journal writes).
//!
//! Usage (elevated): `volume_probe <LETTER> [--create-journal] [--touch-dir <DIR>]`
//! `--create-journal` enables a missing journal with flokid's sizes;
//! `--touch-dir` must be a folder on that volume (a probe file is created,
//! renamed and deleted there).

use std::collections::HashSet;
use std::time::{Duration, Instant};

use floki_ntfs::{is_elevated, NtfsError, UsnEvent, VolumeHandle};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let letter = args
        .first()
        .and_then(|a| a.chars().next())
        .unwrap_or('C')
        .to_ascii_uppercase();
    let create = args.iter().any(|a| a == "--create-journal");
    let touch_dir = args
        .iter()
        .position(|a| a == "--touch-dir")
        .and_then(|i| args.get(i + 1));
    println!("elevated: {}", is_elevated());
    let vol = match VolumeHandle::open(letter) {
        Ok(v) => v,
        Err(e) => return println!("open {letter}: {e}"),
    };
    println!("fs: {:?}  root_frn: {:x?}", vol.fs(), vol.root_frn());
    let info = match vol.query_journal() {
        Ok(i) => i,
        Err(NtfsError::JournalNotActive) if create => {
            println!("journal: not active; creating 256 MB / 16 MB delta");
            if let Err(e) = vol.create_journal(256 << 20, 16 << 20) {
                return println!("create_journal: {e}");
            }
            vol.query_journal().expect("journal after create")
        }
        Err(e) => return println!("query_journal: {e}"),
    };
    println!(
        "journal: id={:x} next_usn={} max={} MB versions={}..={}",
        info.journal_id,
        info.next_usn,
        info.maximum_size >> 20,
        info.min_supported_major_version,
        info.max_supported_major_version
    );

    let started = Instant::now();
    let mut frns = HashSet::new();
    let mut parents = Vec::new();
    let mut roots = Vec::new();
    let mut dollar = Vec::new();
    let mut first = Vec::new();
    let result = vol.enumerate_with(|frn, parent, attrs, name| {
        frns.insert(frn);
        parents.push(parent);
        if vol.is_root(frn) {
            roots.push(format!(
                "{frn:x} parent={parent:x} name={name:?} attrs={attrs:x}"
            ));
        }
        if vol.is_root(parent) && name.starts_with('$') && dollar.len() < 20 {
            dollar.push(name.to_owned());
        }
        if first.len() < 8 {
            first.push(format!("{frn:x} <- {parent:x} {name}"));
        }
    });
    let elapsed = started.elapsed();
    match result {
        Ok(next) => println!(
            "enumerate: {} records in {:.2}s ({:.0}/s), next_usn={next}",
            frns.len(),
            elapsed.as_secs_f64(),
            frns.len() as f64 / elapsed.as_secs_f64().max(1e-9)
        ),
        Err(e) => return println!("enumerate: {e}"),
    }
    let unresolved = parents.iter().filter(|p| !frns.contains(p)).count();
    let distinct_unresolved: HashSet<_> = parents.iter().filter(|p| !frns.contains(p)).collect();
    println!(
        "unresolved parents: {unresolved} records, {} distinct: {:x?}",
        distinct_unresolved.len(),
        distinct_unresolved.iter().take(5).collect::<Vec<_>>()
    );
    println!("root records: {roots:?}");
    println!("root-level $ names: {dollar:?}");
    println!("first: {first:#?}");

    let Some(dir) = touch_dir else { return };
    let from = vol.query_journal().expect("query").next_usn;
    let a = std::path::Path::new(dir).join("floki-probe-a.txt");
    let b = std::path::Path::new(dir).join("floki-probe-b.txt");
    let t0 = Instant::now();
    std::fs::write(&a, b"probe").expect("create probe file");
    std::fs::rename(&a, &b).expect("rename probe file");
    std::fs::remove_file(&b).expect("delete probe file");
    // Poll like the tail does until the delete shows up (or 120 s).
    let mut seen: Vec<String> = Vec::new();
    let mut cursor = from;
    while t0.elapsed() < Duration::from_secs(120) {
        let next = vol
            .read_journal(cursor, info.journal_id, &mut |ev| {
                let line = match &ev {
                    UsnEvent::Create(r) => {
                        format!("create {} ({:x} <- {:x})", r.name, r.frn, r.parent_frn)
                    }
                    UsnEvent::RenameOld { frn } => format!("rename-old {frn:x}"),
                    UsnEvent::RenameNew(r) => format!("rename-new {} ({:x})", r.name, r.frn),
                    UsnEvent::Delete { frn } => format!("delete {frn:x}"),
                    UsnEvent::Overwrite(r) => format!("overwrite {}", r.name),
                };
                seen.push(format!("{:>6} ms  {line}", t0.elapsed().as_millis()));
            })
            .expect("read_journal");
        cursor = next;
        if seen.iter().any(|l| l.contains("delete")) {
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    println!(
        "journal events after touch ({} ms total):",
        t0.elapsed().as_millis()
    );
    for l in seen {
        println!("  {l}");
    }
}
