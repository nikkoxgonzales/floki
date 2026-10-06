//! floki-ntfs: Win32 volume handle + `FSCTL_ENUM_USN_DATA` enumeration + USN journal tail.
//!
//! NTFS and ReFS. ReFS reports 128-bit file ids (`USN_RECORD_V3`); they are
//! packed into the same 64-bit FRN slot NTFS uses (see [`pack_file_id`]), so
//! nothing above this crate knows the difference.
//!
//! Windows-only. All journal operations require an elevated (Administrator)
//! process; without elevation every ioctl fails with [`NtfsError::AccessDenied`].
//!
//! Hard-link policy (v1): the index holds one name per FRN. A journal record
//! carrying only `USN_REASON_HARD_LINK_CHANGE` (e.g. a second link added to an
//! existing file) is skipped by the classifier and never surfaces as an
//! `Overwrite`; additional hard-link names become visible on the next full
//! rescan. `USN_REASON_REPARSE_POINT_CHANGE` records stay `Overwrite` (the
//! link name itself is indexed; the target is never followed).

pub mod ffi;
pub mod stat;
pub mod volume;
pub mod volumes;

pub use stat::{file_meta, FileMeta};
pub use volume::{map_win32_error, JournalInfo, RawRecord, UsnEvent, VolumeHandle};
pub use volumes::{is_elevated, list_indexable_volumes, volume_fs};

/// File systems Floki indexes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FsKind {
    Ntfs,
    /// ReFS (incl. Windows Dev Drives): change journal off by default, and
    /// journal records arrive later than on NTFS (ReFS buffers them).
    Refs,
}

impl FsKind {
    /// From a `GetVolumeInformationW` file-system name.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "NTFS" => Some(Self::Ntfs),
            "ReFS" => Some(Self::Refs),
            _ => None,
        }
    }
}

/// Map a 128-bit file id (`lo` = bytes 0..8, `hi` = bytes 8..16) to Floki's
/// 64-bit FRN, or `None` when it can't be represented.
///
/// NTFS ids live entirely in `lo` (`hi` = 0). ReFS ids are (directory table,
/// entry within it) = (`hi`, `lo`): both small (observed < 2^16 on a 160 GB
/// Dev Drive), packed as `hi << 32 | lo`. Ceiling: a table or entry number
/// past 2^32 can't be packed and is skipped (counted, warned); the upgrade
/// path is a per-volume overflow map.
#[must_use]
pub fn pack_file_id(fs: FsKind, lo: u64, hi: u64) -> Option<u64> {
    match fs {
        FsKind::Ntfs => (hi == 0).then_some(lo),
        FsKind::Refs => {
            (hi <= u64::from(u32::MAX) && lo <= u64::from(u32::MAX)).then_some(hi << 32 | lo)
        }
    }
}

/// MFT record index of the volume root directory.
///
/// NTFS reserves the first 16 MFT records; index 5 is the root directory
/// (`\`). An FRN packs a 48-bit record index (low bits) with a 16-bit sequence
/// number (high bits), so the root's FRN is `5 | (seq << 48)`.
pub const ROOT_FRN_INDEX: u64 = 5;

/// Mask an FRN down to its 48-bit MFT record index (drops the 16-bit sequence number).
#[must_use]
pub fn frn_index(frn: u64) -> u64 {
    frn & 0x0000_FFFF_FFFF_FFFF
}

/// Errors from the NTFS boundary. Every variant is produced without panicking.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NtfsError {
    /// `ERROR_ACCESS_DENIED` (5): the process is not elevated, or the handle
    /// lacks rights. Run elevated.
    #[error("access denied: run elevated (Administrator) to read NTFS journals")]
    AccessDenied,
    /// The volume is neither NTFS nor ReFS (checked in
    /// [`VolumeHandle::open`] via `GetVolumeInformationW`).
    #[error("volume is not NTFS or ReFS")]
    NotNtfs,
    /// `ERROR_JOURNAL_NOT_ACTIVE` (1179): no journal on this volume. Create one
    /// with [`VolumeHandle::create_journal`] and do a full enumeration.
    #[error("USN journal is not active on this volume")]
    JournalNotActive,
    /// `ERROR_JOURNAL_DELETE_IN_PROGRESS` (1178): a journal delete is running;
    /// retry later, then rescan the volume.
    #[error("USN journal delete is in progress")]
    JournalDeleteInProgress,
    /// The journal no longer covers the requested USN: journal id mismatch,
    /// `FirstUsn > from`, or `ERROR_JOURNAL_ENTRY_DELETED` (1181). The caller
    /// must do a full re-enumeration of that volume.
    #[error("USN journal wrapped or was recreated; full rescan required")]
    JournalWrapped,
    /// The volume yields only record versions Floki can't read (V4, or V2
    /// on ReFS). Holds the first observed major version.
    ///
    /// *Deviation from SPEC section 5:* the SPEC maps this case to
    /// `NtfsError::Io` with a message, but `Io(u32)` carries only a Win32 code
    /// and cannot name the version, so a dedicated variant is used instead.
    #[error("unsupported USN record major version {0}; only V2 (NTFS) and V3 (ReFS) are read")]
    UnsupportedRecordVersion(u16),
    /// Any other Win32 error; holds the `GetLastError()` code.
    #[error("Win32 I/O error (code {0})")]
    Io(u32),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frn_index_masks_off_sequence_number() {
        assert_eq!(frn_index(0x0005_0000_0000_0005), 5);
        assert_eq!(frn_index(0xABCD_1234_5678_9ABC), 0x0000_1234_5678_9ABC);
        assert_eq!(frn_index(ROOT_FRN_INDEX), ROOT_FRN_INDEX);
        assert_eq!(frn_index(u64::MAX), 0x0000_FFFF_FFFF_FFFF);
    }

    #[test]
    fn ntfs_ids_pass_through_and_refs_ids_pack_losslessly() {
        // NTFS: the FRN (with its sequence number) is the low half.
        assert_eq!(
            pack_file_id(FsKind::Ntfs, 0x9_0000_001d_487c, 0),
            Some(0x9_0000_001d_487c)
        );
        assert_eq!(pack_file_id(FsKind::Ntfs, 5, 1), None);
        // ReFS ids sampled from a Dev Drive: root, a folder, a file in it.
        assert_eq!(pack_file_id(FsKind::Refs, 0, 0x600), Some(0x600_0000_0000));
        assert_eq!(
            pack_file_id(FsKind::Refs, 0, 0x857d),
            Some(0x857d_0000_0000)
        );
        assert_eq!(
            pack_file_id(FsKind::Refs, 7, 0x857d),
            Some(0x857d_0000_0007)
        );
        // Halves past 32 bits can't be packed without collisions.
        assert_eq!(pack_file_id(FsKind::Refs, 1 << 32, 1), None);
        assert_eq!(pack_file_id(FsKind::Refs, 1, 1 << 32), None);
        assert_eq!(FsKind::from_name("ReFS"), Some(FsKind::Refs));
        assert_eq!(FsKind::from_name("FAT32"), None);
    }

    #[test]
    fn io_error_display_shows_code() {
        assert_eq!(NtfsError::Io(87).to_string(), "Win32 I/O error (code 87)");
        assert!(NtfsError::UnsupportedRecordVersion(3)
            .to_string()
            .contains('3'));
    }
}
