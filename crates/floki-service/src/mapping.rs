//! Mapping layer between `floki-ntfs` records/events and `floki-core`.
//!
//! Two pure functions, both unit-tested without admin rights:
//! [`attrs_to_flags`] (NTFS `FILE_ATTRIBUTE_*` bits to core flag bits) and
//! [`map_usn_event`] (`UsnEvent` to `IndexEvent`; `RenameOld` is dropped).

use floki_core::{Index, IndexEvent, DIRECTORY, HIDDEN, REPARSE, SYSTEM};
use floki_ntfs::{RawRecord, UsnEvent};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_ATTRIBUTE_SYSTEM,
};

/// Map NTFS `FileAttributes` (`u32`) to floki-core entry flag bits.
///
/// Only the four v1 attributes are kept (`DIRECTORY`, `HIDDEN`, `SYSTEM`,
/// `REPARSE`); everything else (`ARCHIVE`, `READONLY`, `COMPRESSED`, …) is
/// ignored. A tombstone bit in the input can never occur (attributes come
/// from the filesystem), but mask it out defensively anyway.
#[must_use]
pub fn attrs_to_flags(attrs: u32) -> u16 {
    let mut flags: u16 = 0;
    if attrs & FILE_ATTRIBUTE_DIRECTORY != 0 {
        flags |= DIRECTORY;
    }
    if attrs & FILE_ATTRIBUTE_HIDDEN != 0 {
        flags |= HIDDEN;
    }
    if attrs & FILE_ATTRIBUTE_SYSTEM != 0 {
        flags |= SYSTEM;
    }
    if attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        flags |= REPARSE;
    }
    flags
}

/// Build the [`IndexEvent::Create`] payload for a scanned [`RawRecord`].
#[must_use]
pub fn create_event(record: &RawRecord) -> IndexEvent<'_> {
    IndexEvent::Create {
        frn: record.frn,
        parent_frn: record.parent_frn,
        name: record.name.as_str(),
        flags: attrs_to_flags(record.attrs),
    }
}

/// True for NTFS metafiles (`$MFT`, `$Bitmap`, `$Extend`, …): a record whose
/// parent is the volume root and whose name starts with `$` (F14).
///
/// The daemon filters these out of scans and journal tails so system files
/// never become searchable.
#[must_use]
pub fn is_ntfs_metafile(parent_frn: u64, name: &str, root_frn: u64) -> bool {
    // The root entry itself parents to itself and never starts with `$`.
    parent_frn == root_frn && name.starts_with('$')
}

/// Map one [`UsnEvent`] onto an [`IndexEvent`] for [`floki_core::Index::apply`].
///
/// Returns `None` for [`UsnEvent::RenameOld`]: the old-name half carries no
/// update; the paired [`UsnEvent::RenameNew`] performs the rename/move.
///
/// Note: the `Rename` mapping carries no attribute flags (core has no field
/// for them); journal application should go through [`apply_usn_event`],
/// which follows the rename with an `Update` so flags survive (F7).
#[must_use]
pub fn map_usn_event(event: &UsnEvent) -> Option<IndexEvent<'_>> {
    match event {
        UsnEvent::Create(record) => Some(create_event(record)),
        UsnEvent::Delete { frn } => Some(IndexEvent::Delete { frn: *frn }),
        UsnEvent::RenameOld { .. } => None,
        UsnEvent::RenameNew(record) => Some(IndexEvent::Rename {
            frn: record.frn,
            parent_frn: record.parent_frn,
            name: record.name.as_str(),
        }),
        UsnEvent::Overwrite(record) => Some(IndexEvent::Update {
            frn: record.frn,
            flags: attrs_to_flags(record.attrs),
        }),
    }
}

/// Apply one [`UsnEvent`] to `index` for volume `vol`.
///
/// Returns `false` (and applies nothing) for [`UsnEvent::RenameOld`] and for
/// NTFS metafiles (see [`is_ntfs_metafile`]); `true` otherwise.
///
/// `RenameNew` applies as `Rename` plus an `Update` carrying
/// `record.attrs`, so an unknown-FRN rename miss synthesizes the entry with
/// the correct flags (`DIRECTORY` when the attributes say so) instead of
/// `flags: 0` (F7). `IndexEvent::Rename` alone never touches flags.
pub fn apply_usn_event(index: &mut Index, vol: u8, event: &UsnEvent, root_frn: u64) -> bool {
    match event {
        UsnEvent::Create(record) => {
            if is_ntfs_metafile(record.parent_frn, &record.name, root_frn) {
                return false;
            }
            if let Some(mapped) = map_usn_event(event) {
                index.apply(vol, mapped);
            }
            true
        }
        UsnEvent::Delete { frn } => {
            index.apply(vol, IndexEvent::Delete { frn: *frn });
            true
        }
        UsnEvent::RenameOld { .. } => false,
        UsnEvent::RenameNew(record) => {
            if is_ntfs_metafile(record.parent_frn, &record.name, root_frn) {
                return false;
            }
            index.apply(
                vol,
                IndexEvent::Rename {
                    frn: record.frn,
                    parent_frn: record.parent_frn,
                    name: record.name.as_str(),
                },
            );
            index.apply(
                vol,
                IndexEvent::Update {
                    frn: record.frn,
                    flags: attrs_to_flags(record.attrs),
                },
            );
            true
        }
        UsnEvent::Overwrite(_) => {
            if let Some(mapped) = map_usn_event(event) {
                index.apply(vol, mapped);
            }
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(name: &str, attrs: u32) -> RawRecord {
        RawRecord {
            frn: 42,
            parent_frn: 5,
            attrs,
            name: name.to_owned(),
        }
    }

    #[test]
    fn attrs_map_to_core_flags() {
        assert_eq!(attrs_to_flags(0), 0);
        assert_eq!(attrs_to_flags(FILE_ATTRIBUTE_DIRECTORY), DIRECTORY);
        assert_eq!(attrs_to_flags(FILE_ATTRIBUTE_HIDDEN), HIDDEN);
        assert_eq!(attrs_to_flags(FILE_ATTRIBUTE_SYSTEM), SYSTEM);
        assert_eq!(attrs_to_flags(FILE_ATTRIBUTE_REPARSE_POINT), REPARSE);
        assert_eq!(
            attrs_to_flags(FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_HIDDEN),
            DIRECTORY | HIDDEN
        );
        assert_eq!(
            attrs_to_flags(
                FILE_ATTRIBUTE_DIRECTORY
                    | FILE_ATTRIBUTE_HIDDEN
                    | FILE_ATTRIBUTE_SYSTEM
                    | FILE_ATTRIBUTE_REPARSE_POINT
            ),
            DIRECTORY | HIDDEN | SYSTEM | REPARSE
        );
    }

    #[test]
    fn non_v1_attribute_bits_are_ignored() {
        // READONLY (0x1), ARCHIVE (0x20), COMPRESSED (0x800), ENCRYPTED (0x4000)
        // have no core flag and must not leak into the index.
        const READONLY: u32 = 0x1;
        const ARCHIVE: u32 = 0x20;
        const COMPRESSED: u32 = 0x800;
        const ENCRYPTED: u32 = 0x4000;
        assert_eq!(
            attrs_to_flags(READONLY | ARCHIVE | COMPRESSED | ENCRYPTED),
            0
        );
        assert_eq!(attrs_to_flags(ARCHIVE | FILE_ATTRIBUTE_HIDDEN), HIDDEN);
    }

    #[test]
    fn create_event_carries_flags() {
        let rec = record("note.txt", FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM);
        assert_eq!(
            create_event(&rec),
            IndexEvent::Create {
                frn: 42,
                parent_frn: 5,
                name: "note.txt",
                flags: HIDDEN | SYSTEM,
            }
        );
    }

    #[test]
    fn usn_create_delete_overwrite_map() {
        let rec = record("a.txt", FILE_ATTRIBUTE_DIRECTORY);
        assert_eq!(
            map_usn_event(&UsnEvent::Create(rec.clone())),
            Some(IndexEvent::Create {
                frn: 42,
                parent_frn: 5,
                name: "a.txt",
                flags: DIRECTORY,
            })
        );
        assert_eq!(
            map_usn_event(&UsnEvent::Delete { frn: 7 }),
            Some(IndexEvent::Delete { frn: 7 })
        );
        assert_eq!(
            map_usn_event(&UsnEvent::Overwrite(rec.clone())),
            Some(IndexEvent::Update {
                frn: 42,
                flags: DIRECTORY,
            })
        );
    }

    #[test]
    fn rename_old_is_dropped_rename_new_carries_update() {
        assert_eq!(map_usn_event(&UsnEvent::RenameOld { frn: 42 }), None);
        let rec = record("new.txt", 0);
        assert_eq!(
            map_usn_event(&UsnEvent::RenameNew(rec)),
            Some(IndexEvent::Rename {
                frn: 42,
                parent_frn: 5,
                name: "new.txt",
            })
        );
    }

    #[test]
    fn metafile_is_root_child_starting_with_dollar() {
        // NTFS metafiles live directly under the volume root.
        assert!(is_ntfs_metafile(100, "$MFT", 100));
        assert!(is_ntfs_metafile(100, "$Bitmap", 100));
        assert!(is_ntfs_metafile(100, "$Extend", 100));
        // Ordinary files are never metafiles, even at the root …
        assert!(!is_ntfs_metafile(100, "pagefile.sys", 100));
        assert!(!is_ntfs_metafile(100, "docs", 100));
        // … nor are `$`-named files in subdirectories (they are user files).
        assert!(!is_ntfs_metafile(101, "$notes.txt", 100));
        assert!(!is_ntfs_metafile(101, "$MFT", 100));
        // The root entry parents to itself and has no `$` name.
        assert!(!is_ntfs_metafile(100, "", 100));
    }

    #[test]
    fn apply_rename_new_carries_attrs_on_miss() {
        use floki_core::Volume;
        let mut index = Index::new();
        index.add_volume(Volume {
            letter: 'C',
            guid: [0; 16],
            journal_id: 1,
            next_usn: 0,
            root_frn: 100,
            enabled: true,
            monitor: true,
        });
        // Unknown FRN (journal gap): the miss synthesizes the entry with the
        // DIRECTORY bit from `attrs`, not `flags: 0`.
        let rec = RawRecord {
            frn: 200,
            parent_frn: 100,
            attrs: FILE_ATTRIBUTE_DIRECTORY,
            name: "newdir".to_owned(),
        };
        assert!(apply_usn_event(
            &mut index,
            0,
            &UsnEvent::RenameNew(rec),
            100
        ));
        let id = index.lookup(0, 200).expect("synthesized entry");
        assert!(index.entries[id as usize].is_dir());
        assert_eq!(index.name(id), Some("newdir"));
    }

    #[test]
    fn apply_skips_metafiles_and_rename_old() {
        use floki_core::Volume;
        let mut index = Index::new();
        index.add_volume(Volume {
            letter: 'C',
            guid: [0; 16],
            journal_id: 1,
            next_usn: 0,
            root_frn: 100,
            enabled: true,
            monitor: true,
        });
        let mft = RawRecord {
            frn: 3,
            parent_frn: 100,
            attrs: FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM,
            name: "$MFT".to_owned(),
        };
        assert!(!apply_usn_event(&mut index, 0, &UsnEvent::Create(mft), 100));
        assert!(index.lookup(0, 3).is_none());
        assert!(!apply_usn_event(
            &mut index,
            0,
            &UsnEvent::RenameOld { frn: 3 },
            100
        ));
        // A `$` file below the root is a user file and is kept.
        let user = RawRecord {
            frn: 201,
            parent_frn: 101,
            attrs: 0,
            name: "$notes.txt".to_owned(),
        };
        assert!(apply_usn_event(&mut index, 0, &UsnEvent::Create(user), 100));
        assert!(index.lookup(0, 201).is_some());
    }
}
