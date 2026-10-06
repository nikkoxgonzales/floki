//! Enumerate a drive and report record count, elapsed time, first names.
//!
//! Usage: `enum_count [DRIVE_LETTER]` (default `C`). Must run elevated.
//! Example: `cargo run -p floki-ntfs --example enum_count -- C`

use std::time::Instant;

use floki_ntfs::{is_elevated, list_indexable_volumes, VolumeHandle};

fn main() {
    let letter = std::env::args()
        .nth(1)
        .and_then(|a| a.chars().next())
        .unwrap_or('C')
        .to_ascii_uppercase();
    println!("elevated: {}", is_elevated());
    println!("indexable volumes: {:?}", list_indexable_volumes());
    let vol = match VolumeHandle::open(letter) {
        Ok(vol) => vol,
        Err(err) => {
            eprintln!("open {letter}: failed: {err}");
            std::process::exit(1);
        }
    };
    let started = Instant::now();
    let mut count = 0u64;
    let mut first: Vec<String> = Vec::new();
    match vol.enumerate_with(|_frn, _parent, _attrs, name| {
        count += 1;
        if first.len() < 5 {
            first.push(name.to_owned());
        }
    }) {
        Ok(next_usn) => {
            let elapsed = started.elapsed();
            let per_sec = if elapsed.as_secs_f64() > 0.0 {
                count as f64 / elapsed.as_secs_f64()
            } else {
                count as f64
            };
            println!("drive: {letter}:");
            println!("records: {count}");
            println!("elapsed_ms: {}", elapsed.as_millis());
            println!("records_per_sec: {per_sec:.0}");
            println!("next_usn: {next_usn}");
            for (i, name) in first.iter().enumerate() {
                println!("name[{i}]: {name}");
            }
        }
        Err(err) => {
            eprintln!("enumerate {letter}: failed: {err}");
            std::process::exit(1);
        }
    }
}
