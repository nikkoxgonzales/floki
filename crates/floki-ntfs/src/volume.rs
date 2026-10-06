//! `VolumeHandle`: open NTFS/ReFS volumes and drive the USN ioctls.
//!
//! Wire pictures (see also `docs/research/ntfs-index.md` sections 1.1-1.3):
//!
//! - `FSCTL_ENUM_USN_DATA` input: [`MftEnumDataV0`] (NTFS) or
//!   [`MftEnumDataV1`] asking for V3 (ReFS); output: `u64` cursor (next
//!   `StartFileReferenceNumber`) followed by packed `USN_RECORD_V2`/`V3`s.
//! - `FSCTL_READ_USN_JOURNAL` input: [`ReadUsnJournalDataV0`] (NTFS) or
//!   [`ReadUsnJournalDataV1`] asking for V3 (ReFS); output: `i64` next-USN
//!   followed by packed records.
//! - ReFS ids are 128-bit; [`crate::pack_file_id`] folds them into the
//!   64-bit FRN everything above this crate uses.
//! - ReFS has no `FSCTL_ENUM_USN_DATA` (`ERROR_INVALID_FUNCTION` for every
//!   input shape, verified on a Dev Drive), so its full enumeration is a
//!   directory walk ([`VolumeHandle::walk_refs`]) feeding the same callback.
//! - Both outputs are walked with `RecordLength`; see
//!   <https://learn.microsoft.com/en-us/windows/win32/fileio/walking-a-buffer-of-change-journal-records>

use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ACCESS_DENIED, ERROR_HANDLE_EOF, ERROR_INVALID_PARAMETER,
    ERROR_JOURNAL_DELETE_IN_PROGRESS, ERROR_JOURNAL_ENTRY_DELETED, ERROR_JOURNAL_NOT_ACTIVE,
    ERROR_NO_MORE_FILES, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FileIdExtdDirectoryInfo, FileIdExtdDirectoryRestartInfo, FileIdInfo,
    GetFileInformationByHandleEx, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_ID_EXTD_DIR_INFO, FILE_ID_INFO, FILE_LIST_DIRECTORY,
    FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::DeviceIoControl;

use crate::ffi::{
    CreateUsnJournalData, MftEnumDataV0, MftEnumDataV1, ParsedRecord, ReadUsnJournalDataV0,
    ReadUsnJournalDataV1, UsnJournalDataV1, FSCTL_CREATE_USN_JOURNAL, FSCTL_ENUM_USN_DATA,
    FSCTL_QUERY_USN_JOURNAL, FSCTL_READ_USN_JOURNAL, USN_JOURNAL_DATA_V0_LEN,
    USN_JOURNAL_DATA_V1_WIRE_LEN, USN_MAJOR_VERSION_V3, USN_REASON_CLOSE, USN_REASON_FILE_CREATE,
    USN_REASON_FILE_DELETE, USN_REASON_HARD_LINK_CHANGE, USN_REASON_RENAME_NEW_NAME,
    USN_REASON_RENAME_OLD_NAME, USN_REASON_REPARSE_POINT_CHANGE,
};
use crate::{frn_index, pack_file_id, FsKind, NtfsError, ROOT_FRN_INDEX};

use std::collections::HashSet;
use std::ptr::{null, null_mut};

/// Output buffer size for enumeration/journal reads: 1 MiB.
///
/// At ~120 B/record a 64 KiB buffer holds ~500 records, so a 4.7 M-record
/// volume needs ~9.5k `DeviceIoControl` round-trips; 1 MiB cuts that ~16x and
/// is still a trivial resident working set. This is the dominant term behind
/// the 120k/s -> >500k/s throughput fix (F9).
const BUFFER_LEN: usize = 1024 * 1024;

/// Directory-listing buffer for the ReFS walk (one
/// `GetFileInformationByHandleEx` call returns this much at most).
const DIR_BUFFER_LEN: usize = 64 * 1024;

// `FILE_ID_EXTD_DIR_INFO` byte offsets the walk parses (asserted against
// the `windows-sys` layout so a binding change fails the build).
const DIR_ATTRS_OFF: usize = 56;
const DIR_NAME_LEN_OFF: usize = 60;
const DIR_ID_OFF: usize = 72;
const DIR_NAME_OFF: usize = 88;
const _: () = assert!(std::mem::offset_of!(FILE_ID_EXTD_DIR_INFO, FileAttributes) == DIR_ATTRS_OFF);
const _: () =
    assert!(std::mem::offset_of!(FILE_ID_EXTD_DIR_INFO, FileNameLength) == DIR_NAME_LEN_OFF);
const _: () = assert!(std::mem::offset_of!(FILE_ID_EXTD_DIR_INFO, FileId) == DIR_ID_OFF);
const _: () = assert!(std::mem::offset_of!(FILE_ID_EXTD_DIR_INFO, FileName) == DIR_NAME_OFF);

/// A raw enumerated file record: FRNs, attribute flags, decoded name.
///
/// `attrs` passes through the NTFS `FileAttributes` `u32`
/// (`FILE_ATTRIBUTE_DIRECTORY`, `HIDDEN`, …) unchanged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawRecord {
    pub frn: u64,
    pub parent_frn: u64,
    pub attrs: u32,
    pub name: String,
}

/// A classified journal delta, ready for `Index::apply`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UsnEvent {
    Create(RawRecord),
    Delete {
        frn: u64,
    },
    RenameOld {
        frn: u64,
    },
    RenameNew(RawRecord),
    /// Attribute/data/other change observed at close.
    Overwrite(RawRecord),
}

/// Journal identity + cursor state from `FSCTL_QUERY_USN_JOURNAL`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JournalInfo {
    pub journal_id: u64,
    pub first_usn: i64,
    pub next_usn: i64,
    pub lowest_valid_usn: i64,
    pub max_usn: i64,
    pub maximum_size: u64,
    pub allocation_delta: u64,
    pub min_supported_major_version: u16,
    pub max_supported_major_version: u16,
}

/// Map a `GetLastError()` code to the SPEC error taxonomy.
#[must_use]
pub fn map_win32_error(code: u32) -> NtfsError {
    match code {
        ERROR_ACCESS_DENIED => NtfsError::AccessDenied,
        ERROR_JOURNAL_NOT_ACTIVE => NtfsError::JournalNotActive,
        ERROR_JOURNAL_DELETE_IN_PROGRESS => NtfsError::JournalDeleteInProgress,
        ERROR_JOURNAL_ENTRY_DELETED => NtfsError::JournalWrapped,
        other => NtfsError::Io(other),
    }
}

/// Map USN reason bits to an [`UsnEvent`] per SPEC section 5.
///
/// Priority: `RENAME_OLD_NAME` → `RENAME_NEW_NAME` → `FILE_DELETE` →
/// `FILE_CREATE` → (`HARD_LINK_CHANGE` → skip) → (`CLOSE` → `Overwrite`).
/// Records matching none of these (e.g. a mid-handle `DATA_EXTEND` without
/// `CLOSE`, which the later close record will repeat) yield `None` and are
/// skipped by [`VolumeHandle::read_journal`].
///
/// Tombstone-wins (`DELETE` before `CREATE`): a coalesced
/// `CREATE|DELETE|CLOSE` record (temp file created and deleted within one
/// handle lifetime) must map to `Delete`; a missed create self-heals on next
/// sighting while a missed delete leaves a permanent ghost entry.
///
/// `HARD_LINK_CHANGE` without any `CREATE`/`DELETE`/`RENAME` bit is skipped
/// (`None`), never `Overwrite`: the record carries a *second link's*
/// `(parent, name)` and mapping it to `Overwrite` would rename the primary
/// index entry to the second link's name (v1 indexes one name per FRN; see
/// `lib.rs`). `REPARSE_POINT_CHANGE` intentionally stays `Overwrite`: for a
/// name-only index the link name itself is indexed and the target is never
/// followed, so an attr-style update is the correct classification.
fn classify_usn_event(reason: u32, record: RawRecord) -> Option<UsnEvent> {
    // Reference the reparse-point bit so the mapping choice is explicit and
    // the import cannot silently go unused if priorities change.
    let _ = USN_REASON_REPARSE_POINT_CHANGE;
    let frn = record.frn;
    if reason & USN_REASON_RENAME_OLD_NAME != 0 {
        Some(UsnEvent::RenameOld { frn })
    } else if reason & USN_REASON_RENAME_NEW_NAME != 0 {
        Some(UsnEvent::RenameNew(record))
    } else if reason & USN_REASON_FILE_DELETE != 0 {
        Some(UsnEvent::Delete { frn })
    } else if reason & USN_REASON_FILE_CREATE != 0 {
        Some(UsnEvent::Create(record))
    } else if reason & USN_REASON_HARD_LINK_CHANGE != 0 {
        None
    } else if reason & USN_REASON_CLOSE != 0 {
        Some(UsnEvent::Overwrite(record))
    } else {
        None
    }
}

/// Outcome counts for one output-buffer walk.
#[derive(Clone, Copy, Debug, Default)]
struct WalkCounts {
    total: usize,
    /// Records in a version this file system's walk doesn't read.
    unsupported: usize,
    first_unsupported: u16,
    corrupt: usize,
    /// V3 ids [`pack_file_id`] can't fit into 64 bits.
    unpackable: usize,
}

/// Per-record callback for [`walk_records_ref`]: `(frn, parent_frn, usn,
/// reason, attrs, name)` with `name` borrowing the walk scratch buffer.
type WalkRefFn<'a> = dyn FnMut(u64, u64, i64, u32, u32, &str) + 'a;

/// Walk packed records in `data` (leading 8-byte cursor already skipped by the
/// caller), invoking `f` per readable record with a borrowed name and ids
/// already packed to 64 bits.
///
/// This is the hot path: `scratch_u16`/`scratch_str` are allocated once per
/// `enumerate`/`read_journal` call and reused for every record, so decoding
/// performs zero per-record allocation (the `&str` borrows `scratch_str` and
/// must not outlive the callback). Malformed records end the walk without
/// panicking only for truncated tails; undecodable records are counted in
/// `corrupt` and skipped. NTFS reads V2 (and V3 with 64-bit ids), ReFS reads
/// V3 only; anything else is counted in `unsupported` and skipped.
fn walk_records_ref(
    data: &[u8],
    fs: FsKind,
    scratch_u16: &mut Vec<u16>,
    scratch_str: &mut String,
    f: &mut WalkRefFn<'_>,
) -> WalkCounts {
    let mut counts = WalkCounts::default();
    let mut offset = 0usize;
    // Need at least 8 bytes to even read the next RecordLength.
    while data.len().saturating_sub(offset) >= 8 {
        let rest = &data[offset..];
        let record_len = u32::from_le_bytes([rest[0], rest[1], rest[2], rest[3]]) as usize;
        if record_len == 0 || record_len < crate::ffi::USN_RECORD_V2_HEADER_LEN {
            break;
        }
        let Some(end) = offset.checked_add(record_len) else {
            break;
        };
        if end > data.len() {
            // Truncated tail: stop, do not panic.
            break;
        }
        counts.total += 1;
        let major = u16::from_le_bytes([rest[4], rest[5]]);
        let readable = major == USN_MAJOR_VERSION_V3
            || (major == crate::ffi::SUPPORTED_USN_MAJOR_VERSION && fs == FsKind::Ntfs);
        if !readable {
            counts.unsupported += 1;
            if counts.unsupported == 1 {
                counts.first_unsupported = major;
            }
        } else {
            match crate::ffi::parse_record_at_with_scratch(
                &data[offset..end],
                scratch_u16,
                scratch_str,
            ) {
                Ok((fields, name, _)) => {
                    match (
                        pack_file_id(fs, fields.frn, fields.frn_hi),
                        pack_file_id(fs, fields.parent_frn, fields.parent_hi),
                    ) {
                        (Some(frn), Some(parent_frn)) => {
                            f(
                                frn,
                                parent_frn,
                                fields.usn,
                                fields.reason,
                                fields.attrs,
                                name,
                            );
                        }
                        _ => counts.unpackable += 1,
                    }
                }
                Err(_) => {
                    counts.corrupt += 1;
                }
            }
        }
        offset = end;
    }
    counts
}

/// Walk packed records in `data`, invoking `f` per V2 record with an owned
/// [`ParsedRecord`].
///
/// Thin wrapper over [`walk_records_ref`]: the borrowed name is copied once
/// (`to_owned`, a single allocation) so the `&mut dyn FnMut(RawRecord)`
/// signature is preserved while still avoiding the per-record `Vec<u16>` of
/// the old `parse_record_at` path. Used by unit tests; the production
/// `enumerate`/`read_journal` paths use [`walk_records_ref`] directly.
#[allow(dead_code)]
fn walk_records(
    data: &[u8],
    fs: FsKind,
    scratch_u16: &mut Vec<u16>,
    scratch_str: &mut String,
    f: &mut dyn FnMut(ParsedRecord),
) -> WalkCounts {
    walk_records_ref(
        data,
        fs,
        scratch_u16,
        scratch_str,
        &mut |frn, parent_frn, usn, reason, attrs, name| {
            f(ParsedRecord {
                frn,
                parent_frn,
                usn,
                reason,
                attrs,
                name: name.to_owned(),
            });
        },
    )
}

/// Open handle to one NTFS or ReFS volume (`\\.\X:`).
#[derive(Debug)]
pub struct VolumeHandle {
    handle: HANDLE,
    letter: char,
    fs: FsKind,
    /// Packed id of the root directory, read at open (ReFS only; NTFS
    /// roots are recognised by MFT index 5).
    refs_root: Option<u64>,
}

// SAFETY: the handle is owned by us (closed in `Drop`) and every
// `DeviceIoControl` on it is synchronous (`OVERLAPPED` is null).
//
// `Send` (without `Sync`) matches the SPEC section 7 design: one thread per
// volume owns its `VolumeHandle`. Do not share `&VolumeHandle` across threads;
// concurrent synchronous ioctls on one handle race on the driver cursor.
unsafe impl Send for VolumeHandle {}

impl Drop for VolumeHandle {
    fn drop(&mut self) {
        // SAFETY: `handle` came from a successful `CreateFileW` and is closed
        // exactly once here; `VolumeHandle` is not `Clone`, so no alias exists.
        unsafe {
            CloseHandle(self.handle);
        }
    }
}

impl VolumeHandle {
    /// Drive letter this handle was opened for (uppercase).
    #[must_use]
    pub fn letter(&self) -> char {
        self.letter
    }

    /// File system of this volume.
    #[must_use]
    pub fn fs(&self) -> FsKind {
        self.fs
    }

    /// Root directory FRN when known before enumeration (ReFS); NTFS
    /// learns it from the stream (see [`VolumeHandle::is_root`]).
    #[must_use]
    pub fn root_frn(&self) -> Option<u64> {
        self.refs_root
    }

    /// `true` for the volume root directory's FRN.
    #[must_use]
    pub fn is_root(&self, frn: u64) -> bool {
        match self.fs {
            FsKind::Ntfs => frn_index(frn) == ROOT_FRN_INDEX,
            FsKind::Refs => self.refs_root == Some(frn),
        }
    }

    /// The parent to index for a record: the ReFS root becomes its own
    /// parent, the NTFS convention `Index::path` and the metafile filter
    /// rely on. Identity everywhere else.
    fn parent_for(&self, frn: u64, parent_frn: u64) -> u64 {
        if self.fs == FsKind::Refs && self.refs_root == Some(frn) {
            frn
        } else {
            parent_frn
        }
    }

    fn wide_nul(s: &str) -> Vec<u16> {
        s.encode_utf16().chain([0]).collect()
    }

    /// Open `\\.\<letter>:` for synchronous journal I/O.
    ///
    /// The handle is opened with `GENERIC_READ | GENERIC_WRITE` (rather than
    /// the SPEC's `GENERIC_READ` alone) so [`VolumeHandle::create_journal`]
    /// works on the same handle; enumeration itself only needs read access.
    /// Returns [`NtfsError::NotNtfs`] when the volume is neither NTFS nor ReFS.
    pub fn open(letter: char) -> Result<Self, NtfsError> {
        if !letter.is_ascii_alphabetic() {
            return Err(NtfsError::Io(ERROR_INVALID_PARAMETER));
        }
        let letter = letter.to_ascii_uppercase();
        let path = Self::wide_nul(&format!(r"\\.\{letter}:"));
        // SAFETY: `path` is NUL-terminated and lives through the call; all
        // flag/size arguments are plain values; handle is checked below.
        let handle = unsafe {
            CreateFileW(
                path.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                null(),
                OPEN_EXISTING,
                0,
                null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            // SAFETY: `CreateFileW` just failed; `GetLastError` is valid here.
            let code = unsafe { GetLastError() };
            return Err(map_win32_error(code));
        }
        let mut this = Self {
            handle,
            letter,
            fs: FsKind::Ntfs,
            refs_root: None,
        };
        // `Drop` closes the handle on the early returns below.
        this.fs = crate::volume_fs(letter).ok_or(NtfsError::NotNtfs)?;
        if this.fs == FsKind::Refs {
            this.refs_root = Some(refs_root_frn(letter)?);
        }
        Ok(this)
    }

    /// `FSCTL_QUERY_USN_JOURNAL`: journal identity + cursor state.
    pub fn query_journal(&self) -> Result<JournalInfo, NtfsError> {
        let out = UsnJournalDataV1 {
            usn_journal_id: 0,
            first_usn: 0,
            next_usn: 0,
            lowest_valid_usn: 0,
            max_usn: 0,
            maximum_size: 0,
            allocation_delta: 0,
            min_supported_major_version: 0,
            max_supported_major_version: 0,
        };
        let mut out_box = Box::new(out);
        let mut returned = 0u32;
        // SAFETY: `out_box` is a valid 64-byte `repr(C)` buffer (60 wire bytes
        // + 4 tail padding) matching the documented `USN_JOURNAL_DATA_V1`
        // layout (compile-time size/offset asserts in `ffi.rs`); no input
        // buffer; synchronous call (null overlapped). The buffer is
        // zero-initialized, so a short (V0-sized) driver write leaves the
        // version fields as 0 instead of garbage.
        let ok = unsafe {
            DeviceIoControl(
                self.handle,
                FSCTL_QUERY_USN_JOURNAL,
                null(),
                0,
                out_box.as_mut() as *mut UsnJournalDataV1 as *mut std::ffi::c_void,
                size_of::<UsnJournalDataV1>() as u32,
                &mut returned,
                null_mut(),
            )
        };
        if ok == 0 {
            // SAFETY: `DeviceIoControl` just failed.
            let code = unsafe { GetLastError() };
            return Err(map_win32_error(code));
        }
        let returned = returned as usize;
        if returned < USN_JOURNAL_DATA_V0_LEN {
            return Err(NtfsError::Io(
                windows_sys::Win32::Foundation::ERROR_INSUFFICIENT_BUFFER,
            ));
        }
        // The driver may return only the 56-byte V0 prefix; version fields are
        // valid only when at least the 60-byte V1 wire length was written.
        let (min_supported_major_version, max_supported_major_version) =
            if returned >= USN_JOURNAL_DATA_V1_WIRE_LEN {
                (
                    out_box.min_supported_major_version,
                    out_box.max_supported_major_version,
                )
            } else {
                (0, 0)
            };
        Ok(JournalInfo {
            journal_id: out_box.usn_journal_id,
            first_usn: out_box.first_usn,
            next_usn: out_box.next_usn,
            lowest_valid_usn: out_box.lowest_valid_usn,
            max_usn: out_box.max_usn,
            maximum_size: out_box.maximum_size,
            allocation_delta: out_box.allocation_delta,
            min_supported_major_version,
            max_supported_major_version,
        })
    }

    /// `FSCTL_CREATE_USN_JOURNAL` (also resizes an existing journal). Never
    /// deletes anything; deletion is deliberately not exposed.
    pub fn create_journal(&self, max: u64, delta: u64) -> Result<(), NtfsError> {
        let input = CreateUsnJournalData {
            maximum_size: max,
            allocation_delta: delta,
        };
        let mut returned = 0u32;
        // SAFETY: `input` is a valid 16-byte `repr(C)` buffer (size asserted in
        // `ffi.rs`); no output buffer; synchronous call.
        let ok = unsafe {
            DeviceIoControl(
                self.handle,
                FSCTL_CREATE_USN_JOURNAL,
                &input as *const CreateUsnJournalData as *const std::ffi::c_void,
                size_of::<CreateUsnJournalData>() as u32,
                null_mut(),
                0,
                &mut returned,
                null_mut(),
            )
        };
        if ok == 0 {
            // SAFETY: `DeviceIoControl` just failed.
            let code = unsafe { GetLastError() };
            return Err(map_win32_error(code));
        }
        Ok(())
    }

    /// Full-volume enumeration via `FSCTL_ENUM_USN_DATA` with a 1 MiB buffer,
    /// looping until `ERROR_HANDLE_EOF`. Returns the journal's `NextUsn` (fresh
    /// `QUERY` after the walk) so the caller can tail from there.
    ///
    /// Snapshot semantics: `HighUsn` is bound to the journal's `NextUsn` from
    /// a pre-walk `QUERY`, so files created *during* the walk (USN newer than
    /// the snapshot) are excluded from the enumeration and picked up by the
    /// first tail poll instead of being double-applied. The returned cursor is
    /// a post-walk `QUERY.next_usn`, covering the `(snapshot, ∞)` tail window.
    ///
    /// The volume root directory (MFT index 5) is reported exactly as NTFS
    /// returns it — its `parent_frn` refers to MFT index 5 itself — and is
    /// passed to `sink` like any other record.
    pub fn enumerate(&self, sink: &mut dyn FnMut(RawRecord)) -> Result<i64, NtfsError> {
        self.enumerate_with(|frn, parent_frn, attrs, name| {
            sink(RawRecord {
                frn,
                parent_frn,
                attrs,
                name: name.to_owned(),
            });
        })
    }

    /// Full-volume enumeration with a borrowed name (zero per-record allocation).
    ///
    /// Same snapshot semantics as [`VolumeHandle::enumerate`], but the callback
    /// receives `(frn, parent_frn, attrs, name)` with `name: &str` borrowing an
    /// internal scratch buffer reused across records. The borrow is valid only
    /// for the duration of the callback; copy it if it must outlive the call.
    /// Names are decoded lossy (lone surrogates become U+FFFD) and never abort
    /// the pass. Corrupt records are counted and reported once via
    /// `tracing::warn!`; see [`VolumeHandle::enumerate`].
    pub fn enumerate_with<F: FnMut(u64, u64, u32, &str)>(
        &self,
        mut f: F,
    ) -> Result<i64, NtfsError> {
        if self.fs == FsKind::Refs {
            return self.walk_refs(&mut f);
        }
        let snapshot_usn = self.query_journal()?.next_usn;
        let mut input = MftEnumDataV1 {
            start_file_reference_number: 0,
            low_usn: 0,
            high_usn: snapshot_usn,
            min_major_version: USN_MAJOR_VERSION_V3,
            max_major_version: USN_MAJOR_VERSION_V3,
        };
        // NTFS keeps the proven V0 request (V2 records); ReFS must ask for
        // V3 to get its 128-bit ids.
        let input_len = match self.fs {
            FsKind::Ntfs => size_of::<MftEnumDataV0>(),
            FsKind::Refs => size_of::<MftEnumDataV1>(),
        } as u32;
        let mut buffer = vec![0u8; BUFFER_LEN];
        let mut scratch_u16: Vec<u16> = Vec::with_capacity(256);
        let mut scratch_str = String::with_capacity(256);
        let mut aggregate = WalkCounts::default();
        loop {
            let mut returned = 0u32;
            // SAFETY: `input` is a valid 32-byte `repr(C)` buffer whose
            // first 24 bytes are exactly `MftEnumDataV0` (layout asserted in
            // `ffi.rs`), so passing either length is sound; `buffer` is a
            // live 1 MiB output buffer with its length passed correctly;
            // synchronous call.
            let ok = unsafe {
                DeviceIoControl(
                    self.handle,
                    FSCTL_ENUM_USN_DATA,
                    &input as *const MftEnumDataV1 as *const std::ffi::c_void,
                    input_len,
                    buffer.as_mut_ptr() as *mut std::ffi::c_void,
                    buffer.len() as u32,
                    &mut returned,
                    null_mut(),
                )
            };
            if ok == 0 {
                // SAFETY: `DeviceIoControl` just failed.
                let code = unsafe { GetLastError() };
                if code == ERROR_HANDLE_EOF {
                    break;
                }
                return Err(map_win32_error(code));
            }
            let returned = returned as usize;
            if returned < 8 {
                break;
            }
            input.start_file_reference_number =
                u64::from_le_bytes(buffer[0..8].try_into().expect("len checked"));
            let first_batch = aggregate.total == 0;
            let counts = walk_records_ref(
                &buffer[8..returned],
                self.fs,
                &mut scratch_u16,
                &mut scratch_str,
                &mut |frn, parent_frn, _usn, _reason, attrs, name| {
                    f(frn, self.parent_for(frn, parent_frn), attrs, name);
                },
            );
            aggregate.total += counts.total;
            aggregate.unsupported += counts.unsupported;
            aggregate.corrupt += counts.corrupt;
            aggregate.unpackable += counts.unpackable;
            if first_batch && counts.total > 0 {
                aggregate.first_unsupported = counts.first_unsupported;
            }
        }
        if aggregate.corrupt > 0 || aggregate.unpackable > 0 {
            tracing::warn!(
                volume = %self.letter,
                corrupt = aggregate.corrupt,
                unpackable = aggregate.unpackable,
                total = aggregate.total,
                "enumerate: skipped corrupt or unpackable USN records"
            );
        }
        if aggregate.total > 0 && aggregate.unsupported == aggregate.total {
            return Err(NtfsError::UnsupportedRecordVersion(
                aggregate.first_unsupported,
            ));
        }
        Ok(self.query_journal()?.next_usn)
    }

    /// ReFS full enumeration: walk every directory from the root with
    /// `GetFileInformationByHandleEx(FileIdExtdDirectoryInfo)`, reporting
    /// `(frn, parent_frn, attrs, name)` like the NTFS path, root first
    /// (named `.`, its own parent, as NTFS reports it).
    ///
    /// Returns the journal's `NextUsn` from *before* the walk: the tail then
    /// replays everything that changed while the tree was being read (a
    /// replayed create of an already-listed file is an index upsert).
    /// Reparse-point directories are listed but not entered (their target
    /// is indexed where it lives). A file reached through several hard links
    /// is reported once, under the first name seen — NTFS enumeration also
    /// yields one name per file. Directories that can't be opened (access
    /// denied) are skipped and counted.
    ///
    /// Ceiling: the hard-link dedupe holds a `HashSet<u64>` of file ids for
    /// the walk (~18 MB per million files, freed when it returns); the
    /// upgrade path is deduping only files the directory info marks as
    /// multi-link once Windows exposes that there.
    fn walk_refs(&self, f: &mut dyn FnMut(u64, u64, u32, &str)) -> Result<i64, NtfsError> {
        let snapshot_usn = self.query_journal()?.next_usn;
        let root = self.refs_root.ok_or(NtfsError::NotNtfs)?;
        f(root, root, FILE_ATTRIBUTE_DIRECTORY, ".");
        let mut stack: Vec<(String, u64)> = vec![(format!(r"\\?\{}:\", self.letter), root)];
        let mut files_seen: HashSet<u64> = HashSet::new();
        // u64 elements keep the buffer 8-byte aligned for the entry walk.
        let mut buffer = vec![0u64; DIR_BUFFER_LEN / 8];
        let mut scratch_u16: Vec<u16> = Vec::with_capacity(256);
        let mut name = String::with_capacity(256);
        let (mut unopened, mut unpackable) = (0usize, 0usize);
        while let Some((path, dir_frn)) = stack.pop() {
            let wide = Self::wide_nul(&path);
            // SAFETY: `wide` is NUL-terminated and outlives the call; backup
            // semantics is required to open a directory; checked below.
            let dir = unsafe {
                CreateFileW(
                    wide.as_ptr(),
                    FILE_LIST_DIRECTORY,
                    FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                    null(),
                    OPEN_EXISTING,
                    FILE_FLAG_BACKUP_SEMANTICS,
                    null_mut(),
                )
            };
            if dir == INVALID_HANDLE_VALUE {
                unopened += 1;
                continue;
            }
            let mut class = FileIdExtdDirectoryRestartInfo;
            loop {
                // SAFETY: `dir` is an open directory handle; `buffer` is a
                // live, 8-byte aligned buffer of `DIR_BUFFER_LEN` bytes.
                let ok = unsafe {
                    GetFileInformationByHandleEx(
                        dir,
                        class,
                        buffer.as_mut_ptr() as *mut std::ffi::c_void,
                        DIR_BUFFER_LEN as u32,
                    )
                };
                if ok == 0 {
                    // ERROR_NO_MORE_FILES ends the listing; any other error
                    // also ends this directory (the rest of the walk goes on).
                    // SAFETY: the call just failed.
                    let code = unsafe { GetLastError() };
                    if code != ERROR_NO_MORE_FILES {
                        unopened += 1;
                    }
                    break;
                }
                class = FileIdExtdDirectoryInfo;
                // SAFETY: plain bytes view of the u64 buffer, same length.
                let bytes = unsafe {
                    std::slice::from_raw_parts(buffer.as_ptr() as *const u8, DIR_BUFFER_LEN)
                };
                for entry in dir_entries(bytes) {
                    let DirEntry {
                        attrs,
                        id_lo,
                        id_hi,
                        name_bytes,
                    } = entry;
                    let name =
                        crate::ffi::decode_name_into(name_bytes, &mut scratch_u16, &mut name);
                    if name == "." || name == ".." {
                        continue;
                    }
                    let Some(frn) = pack_file_id(FsKind::Refs, id_lo, id_hi) else {
                        unpackable += 1;
                        continue;
                    };
                    if attrs & FILE_ATTRIBUTE_DIRECTORY != 0 {
                        f(frn, dir_frn, attrs, name);
                        if attrs & FILE_ATTRIBUTE_REPARSE_POINT == 0 {
                            let sep = if path.ends_with('\\') { "" } else { "\\" };
                            stack.push((format!("{path}{sep}{name}"), frn));
                        }
                    } else if files_seen.insert(frn) {
                        f(frn, dir_frn, attrs, name);
                    }
                }
            }
            // SAFETY: `dir` came from a successful `CreateFileW`; closed once.
            unsafe {
                CloseHandle(dir);
            }
        }
        if unopened > 0 || unpackable > 0 {
            tracing::warn!(
                volume = %self.letter,
                unopened,
                unpackable,
                "ReFS walk: skipped unreadable directories or unpackable ids"
            );
        }
        Ok(snapshot_usn)
    }

    /// Tail the journal from `from` for the journal `journal_id`.
    ///
    /// `FSCTL_READ_USN_JOURNAL` with `ReasonMask = all`, `ReturnOnlyOnClose =
    /// 0` (summary mode would lose the `RENAME_OLD_NAME` half of renames).
    /// Returns the next USN to poll from. Journal wrap (`FirstUsn > from`) or
    /// a journal id mismatch yields [`NtfsError::JournalWrapped`].
    pub fn read_journal(
        &self,
        from: i64,
        journal_id: u64,
        sink: &mut dyn FnMut(UsnEvent),
    ) -> Result<i64, NtfsError> {
        let info = self.query_journal()?;
        if info.journal_id != journal_id {
            return Err(NtfsError::JournalWrapped);
        }
        if info.first_usn > from {
            return Err(NtfsError::JournalWrapped);
        }
        let mut input = ReadUsnJournalDataV1 {
            start_usn: from,
            reason_mask: u32::MAX,
            return_only_on_close: 0,
            timeout: 0,
            bytes_to_wait_for: 0,
            usn_journal_id: journal_id,
            min_major_version: USN_MAJOR_VERSION_V3,
            max_major_version: USN_MAJOR_VERSION_V3,
        };
        // NTFS: proven V0 request. ReFS: V1 asking for V3 records.
        let input_len = match self.fs {
            FsKind::Ntfs => size_of::<ReadUsnJournalDataV0>(),
            FsKind::Refs => size_of::<ReadUsnJournalDataV1>(),
        } as u32;
        let mut buffer = vec![0u8; BUFFER_LEN];
        let mut scratch_u16: Vec<u16> = Vec::with_capacity(256);
        let mut scratch_str = String::with_capacity(256);
        let mut next_usn = from;
        let mut corrupt_total = 0usize;
        loop {
            let mut returned = 0u32;
            // SAFETY: `input` is a valid 48-byte `repr(C)` buffer whose
            // first 40 bytes are exactly `ReadUsnJournalDataV0` (layout
            // asserted in `ffi.rs`), so passing either length is sound;
            // `buffer` is a live 1 MiB output buffer with its length passed
            // correctly; synchronous call.
            let ok = unsafe {
                DeviceIoControl(
                    self.handle,
                    FSCTL_READ_USN_JOURNAL,
                    &input as *const ReadUsnJournalDataV1 as *const std::ffi::c_void,
                    input_len,
                    buffer.as_mut_ptr() as *mut std::ffi::c_void,
                    buffer.len() as u32,
                    &mut returned,
                    null_mut(),
                )
            };
            if ok == 0 {
                // SAFETY: `DeviceIoControl` just failed.
                let code = unsafe { GetLastError() };
                if code == ERROR_HANDLE_EOF {
                    break;
                }
                return Err(map_win32_error(code));
            }
            let returned = returned as usize;
            if returned < 8 {
                break;
            }
            let cursor = i64::from_le_bytes(buffer[0..8].try_into().expect("len checked"));
            next_usn = cursor;
            if returned == 8 {
                break;
            }
            let counts = walk_records_ref(
                &buffer[8..returned],
                self.fs,
                &mut scratch_u16,
                &mut scratch_str,
                &mut |frn, parent_frn, _usn, reason, attrs, name| {
                    // Single owned allocation per delivered event; skipped
                    // records (hard-link changes, untracked bits) cost none.
                    let record = RawRecord {
                        frn,
                        parent_frn: self.parent_for(frn, parent_frn),
                        attrs,
                        name: name.to_owned(),
                    };
                    if let Some(event) = classify_usn_event(reason, record) {
                        sink(event);
                    }
                },
            );
            corrupt_total += counts.corrupt + counts.unpackable;
            if cursor <= input.start_usn {
                // Cursor did not move; stop rather than spin on the same window.
                break;
            }
            input.start_usn = cursor;
        }
        if corrupt_total > 0 {
            tracing::warn!(
                corrupt = corrupt_total,
                volume = %self.letter,
                "read_journal: skipped corrupt or unpackable USN records"
            );
        }
        Ok(next_usn)
    }
}

/// One parsed `FILE_ID_EXTD_DIR_INFO` entry, name still UTF-16LE bytes.
#[derive(Debug, PartialEq, Eq)]
struct DirEntry<'a> {
    attrs: u32,
    id_lo: u64,
    id_hi: u64,
    name_bytes: &'a [u8],
}

/// Walk the `NextEntryOffset` chain of a `FileIdExtdDirectoryInfo` buffer.
/// Bounds-checked: a malformed entry ends the walk instead of panicking.
fn dir_entries(buf: &[u8]) -> impl Iterator<Item = DirEntry<'_>> {
    let mut offset = Some(0usize);
    std::iter::from_fn(move || {
        let at = offset?;
        let e = buf.get(at..)?;
        if e.len() < DIR_NAME_OFF {
            offset = None;
            return None;
        }
        let u32_at = |o: usize| u32::from_le_bytes(e[o..o + 4].try_into().expect("len checked"));
        let u64_at = |o: usize| u64::from_le_bytes(e[o..o + 8].try_into().expect("len checked"));
        let next = u32_at(0) as usize;
        let name_len = u32_at(DIR_NAME_LEN_OFF) as usize;
        let name_bytes = e.get(DIR_NAME_OFF..DIR_NAME_OFF.checked_add(name_len)?)?;
        if !name_len.is_multiple_of(2) {
            offset = None;
            return None;
        }
        offset = (next != 0).then(|| at + next);
        Some(DirEntry {
            attrs: u32_at(DIR_ATTRS_OFF),
            id_lo: u64_at(DIR_ID_OFF),
            id_hi: u64_at(DIR_ID_OFF + 8),
            name_bytes,
        })
    })
}

/// Packed id of `letter`'s root directory on ReFS, from
/// `GetFileInformationByHandleEx(FileIdInfo)` (no elevation needed).
fn refs_root_frn(letter: char) -> Result<u64, NtfsError> {
    let path = VolumeHandle::wide_nul(&format!("{letter}:\\"));
    // SAFETY: `path` is NUL-terminated and lives through the call; backup
    // semantics is required to open a directory; handle checked below.
    let dir = unsafe {
        CreateFileW(
            path.as_ptr(),
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            null_mut(),
        )
    };
    if dir == INVALID_HANDLE_VALUE {
        // SAFETY: `CreateFileW` just failed.
        return Err(map_win32_error(unsafe { GetLastError() }));
    }
    // SAFETY: all-zero is a valid `FILE_ID_INFO` (plain integers/bytes).
    let mut info: FILE_ID_INFO = unsafe { std::mem::zeroed() };
    // SAFETY: `dir` is the open handle from above; `info` is a live
    // `FILE_ID_INFO` with its size passed correctly.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            dir,
            FileIdInfo,
            &mut info as *mut FILE_ID_INFO as *mut std::ffi::c_void,
            size_of::<FILE_ID_INFO>() as u32,
        )
    };
    // SAFETY: GetLastError is read before CloseHandle can overwrite it.
    let code = if ok == 0 {
        unsafe { GetLastError() }
    } else {
        0
    };
    // SAFETY: `dir` came from a successful `CreateFileW`; closed once.
    unsafe {
        CloseHandle(dir);
    }
    if ok == 0 {
        return Err(map_win32_error(code));
    }
    let id = info.FileId.Identifier;
    let lo = u64::from_le_bytes(id[..8].try_into().expect("16-byte id"));
    let hi = u64::from_le_bytes(id[8..].try_into().expect("16-byte id"));
    pack_file_id(FsKind::Refs, lo, hi).ok_or(NtfsError::Io(ERROR_INVALID_PARAMETER))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ffi::SUPPORTED_USN_MAJOR_VERSION;
    use windows_sys::Win32::Foundation::{
        ERROR_ACCESS_DENIED, ERROR_HANDLE_EOF, ERROR_JOURNAL_DELETE_IN_PROGRESS,
        ERROR_JOURNAL_ENTRY_DELETED, ERROR_JOURNAL_NOT_ACTIVE,
    };

    fn record_named(name: &str) -> RawRecord {
        RawRecord {
            frn: 42,
            parent_frn: 5,
            attrs: 0x10,
            name: name.to_owned(),
        }
    }

    #[test]
    fn reason_bits_map_to_events_in_spec_priority() {
        // RenameOld wins over everything.
        assert_eq!(
            classify_usn_event(
                USN_REASON_RENAME_OLD_NAME | USN_REASON_CLOSE,
                record_named("a")
            ),
            Some(UsnEvent::RenameOld { frn: 42 })
        );
        // RenameNew beats Create.
        assert_eq!(
            classify_usn_event(
                USN_REASON_RENAME_NEW_NAME | USN_REASON_FILE_CREATE,
                record_named("b")
            ),
            Some(UsnEvent::RenameNew(record_named("b")))
        );
        assert_eq!(
            classify_usn_event(USN_REASON_FILE_CREATE | USN_REASON_CLOSE, record_named("c")),
            Some(UsnEvent::Create(record_named("c")))
        );
        assert_eq!(
            classify_usn_event(USN_REASON_FILE_DELETE, record_named("d")),
            Some(UsnEvent::Delete { frn: 42 })
        );
        // Plain close (attr/data change) -> Overwrite.
        assert_eq!(
            classify_usn_event(USN_REASON_CLOSE, record_named("e")),
            Some(UsnEvent::Overwrite(record_named("e")))
        );
        // Mid-handle data extend with no CLOSE and no other tracked bit: skipped.
        assert_eq!(classify_usn_event(0x2, record_named("f")), None);
        assert_eq!(classify_usn_event(0x0, record_named("g")), None);
    }

    #[test]
    fn delete_wins_over_create_for_coalesced_record() {
        // F1: temp file created and deleted within one handle lifetime yields a
        // single close record with CREATE|DELETE|CLOSE; the delete must win so
        // no ghost entry stays in the index.
        assert_eq!(
            classify_usn_event(
                USN_REASON_FILE_CREATE | USN_REASON_FILE_DELETE | USN_REASON_CLOSE,
                record_named("tmp")
            ),
            Some(UsnEvent::Delete { frn: 42 })
        );
        assert_eq!(
            classify_usn_event(
                USN_REASON_FILE_CREATE | USN_REASON_FILE_DELETE,
                record_named("tmp")
            ),
            Some(UsnEvent::Delete { frn: 42 })
        );
        // Rename halves still outrank the tombstone.
        assert_eq!(
            classify_usn_event(
                USN_REASON_RENAME_OLD_NAME | USN_REASON_FILE_DELETE,
                record_named("tmp")
            ),
            Some(UsnEvent::RenameOld { frn: 42 })
        );
        assert_eq!(
            classify_usn_event(
                USN_REASON_RENAME_NEW_NAME | USN_REASON_FILE_DELETE | USN_REASON_CLOSE,
                record_named("tmp")
            ),
            Some(UsnEvent::RenameNew(record_named("tmp")))
        );
        // Normal rename-close record still maps to RenameNew (not Close).
        assert_eq!(
            classify_usn_event(
                USN_REASON_RENAME_NEW_NAME | USN_REASON_CLOSE,
                record_named("new")
            ),
            Some(UsnEvent::RenameNew(record_named("new")))
        );
    }

    #[test]
    fn hard_link_change_without_create_delete_rename_is_skipped() {
        // F2: a hard-link add carries HARD_LINK_CHANGE|CLOSE with the second
        // link's (parent, name); it must not become Overwrite.
        assert_eq!(
            classify_usn_event(
                USN_REASON_HARD_LINK_CHANGE | USN_REASON_CLOSE,
                record_named("second-link")
            ),
            None
        );
        assert_eq!(
            classify_usn_event(USN_REASON_HARD_LINK_CHANGE, record_named("second-link")),
            None
        );
        // CREATE/DELETE/RENAME bits still win when combined with the link bit.
        assert_eq!(
            classify_usn_event(
                USN_REASON_HARD_LINK_CHANGE | USN_REASON_FILE_CREATE,
                record_named("h")
            ),
            Some(UsnEvent::Create(record_named("h")))
        );
        assert_eq!(
            classify_usn_event(
                USN_REASON_HARD_LINK_CHANGE | USN_REASON_FILE_DELETE,
                record_named("h")
            ),
            Some(UsnEvent::Delete { frn: 42 })
        );
    }

    #[test]
    fn reparse_point_change_stays_overwrite() {
        // F2: for a name-only index the link name itself is indexed and the
        // target is never followed, so a reparse-point change is an attr-style
        // Overwrite, not a skip.
        assert_eq!(
            classify_usn_event(
                USN_REASON_REPARSE_POINT_CHANGE | USN_REASON_CLOSE,
                record_named("link")
            ),
            Some(UsnEvent::Overwrite(record_named("link")))
        );
    }

    #[test]
    fn win32_errors_map_per_spec() {
        assert_eq!(
            map_win32_error(ERROR_ACCESS_DENIED),
            NtfsError::AccessDenied
        );
        assert_eq!(
            map_win32_error(ERROR_JOURNAL_NOT_ACTIVE),
            NtfsError::JournalNotActive
        );
        assert_eq!(
            map_win32_error(ERROR_JOURNAL_DELETE_IN_PROGRESS),
            NtfsError::JournalDeleteInProgress
        );
        assert_eq!(
            map_win32_error(ERROR_JOURNAL_ENTRY_DELETED),
            NtfsError::JournalWrapped
        );
        // Everything else keeps its code.
        assert_eq!(
            map_win32_error(ERROR_HANDLE_EOF),
            NtfsError::Io(ERROR_HANDLE_EOF)
        );
        assert_eq!(map_win32_error(87), NtfsError::Io(87));
    }

    fn scratch() -> (Vec<u16>, String) {
        (Vec::with_capacity(64), String::with_capacity(64))
    }

    #[test]
    fn walk_skips_truncated_tail_without_panic() {
        let good = crate::ffi::build_v2_record(7, 5, USN_REASON_CLOSE, 0x80, "note.txt");
        let full_len = good.len();
        // Append a truncated record (header claims more than present).
        let mut cut = crate::ffi::build_v2_record(8, 5, USN_REASON_CLOSE, 0x80, "cut.txt");
        cut.truncate(cut.len() - 4);
        let mut data = vec![0u8; 8];
        data.extend_from_slice(&good);
        data.extend_from_slice(&cut);
        // Corrupt the second record's declared length to exceed the buffer.
        let off = 8 + full_len;
        let big = (cut.len() + 64) as u32;
        data[off..off + 4].copy_from_slice(&big.to_le_bytes());

        let mut names = Vec::new();
        let (mut u16buf, mut sbuf) = scratch();
        let counts = walk_records(
            &data[8..],
            FsKind::Ntfs,
            &mut u16buf,
            &mut sbuf,
            &mut |rec| names.push(rec.name),
        );
        assert_eq!(names, ["note.txt"]);
        assert_eq!(counts.total, 1);
        assert_eq!(counts.corrupt, 0);
        let _ = (good, full_len);
    }

    #[test]
    fn walk_counts_unsupported_versions() {
        // V4 is never read; V2 is not read on ReFS (its ids are 128-bit).
        let mut v4 = crate::ffi::build_v2_record(9, 5, USN_REASON_CLOSE, 0x10, "dir");
        v4[4..6].copy_from_slice(&4u16.to_le_bytes());
        let mut seen = 0;
        let (mut u16buf, mut sbuf) = scratch();
        let counts = walk_records(&v4, FsKind::Ntfs, &mut u16buf, &mut sbuf, &mut |_| {
            seen += 1
        });
        assert_eq!((seen, counts.total, counts.unsupported), (0, 1, 1));
        assert_eq!(counts.first_unsupported, 4);
        let v2 = crate::ffi::build_v2_record(9, 5, USN_REASON_CLOSE, 0x10, "dir");
        let counts = walk_records(&v2, FsKind::Refs, &mut u16buf, &mut sbuf, &mut |_| {
            seen += 1
        });
        assert_eq!((seen, counts.unsupported), (0, 1));
        assert_eq!(SUPPORTED_USN_MAJOR_VERSION, 2);
    }

    /// One `FILE_ID_EXTD_DIR_INFO` entry with `next` as its NextEntryOffset.
    fn dir_entry(next: u32, attrs: u32, id: (u64, u64), name: &str) -> Vec<u8> {
        let units: Vec<u16> = name.encode_utf16().collect();
        let mut e = vec![0u8; DIR_NAME_OFF + units.len() * 2];
        e[0..4].copy_from_slice(&next.to_le_bytes());
        e[DIR_ATTRS_OFF..DIR_ATTRS_OFF + 4].copy_from_slice(&attrs.to_le_bytes());
        e[DIR_NAME_LEN_OFF..DIR_NAME_LEN_OFF + 4]
            .copy_from_slice(&((units.len() * 2) as u32).to_le_bytes());
        e[DIR_ID_OFF..DIR_ID_OFF + 8].copy_from_slice(&id.0.to_le_bytes());
        e[DIR_ID_OFF + 8..DIR_ID_OFF + 16].copy_from_slice(&id.1.to_le_bytes());
        for (i, u) in units.iter().enumerate() {
            e[DIR_NAME_OFF + i * 2..DIR_NAME_OFF + i * 2 + 2].copy_from_slice(&u.to_le_bytes());
        }
        e
    }

    #[test]
    fn dir_entries_follow_the_offset_chain() {
        let mut first = dir_entry(0, 0x10, (0, 0x704), "codes");
        first.resize(104, 0); // 8-byte aligned slot, like the OS pads entries
        first[0..4].copy_from_slice(&104u32.to_le_bytes());
        let mut buf = first;
        buf.extend(dir_entry(0, 0x20, (7, 0x857d), "Cargo.toml"));
        let entries: Vec<_> = dir_entries(&buf).collect();
        assert_eq!(entries.len(), 2);
        assert_eq!((entries[0].attrs, entries[0].id_hi), (0x10, 0x704));
        assert_eq!((entries[1].id_lo, entries[1].id_hi), (7, 0x857d));
        assert_eq!(entries[1].name_bytes.len(), "Cargo.toml".len() * 2);
    }

    #[test]
    fn dir_entries_stop_at_malformed_entries() {
        // Offset past the end, a name running off the buffer, and an empty
        // buffer all end the walk without panicking.
        let mut buf = dir_entry(4096, 0x20, (1, 1), "a");
        assert_eq!(dir_entries(&buf).count(), 1);
        buf[DIR_NAME_LEN_OFF..DIR_NAME_LEN_OFF + 4].copy_from_slice(&999u32.to_le_bytes());
        assert_eq!(dir_entries(&buf).count(), 0);
        assert_eq!(dir_entries(&[]).count(), 0);
    }

    #[test]
    fn refs_walk_packs_128_bit_ids_and_skips_unpackable_ones() {
        // Ids sampled from a real Dev Drive: a project folder (table 0x857d)
        // and Cargo.toml inside it (entry 7 of that table).
        let mut data = crate::ffi::build_v3_record(
            (7, 0x857d),
            (0, 0x857d),
            USN_REASON_CLOSE,
            0x20,
            "Cargo.toml",
        );
        data.extend(crate::ffi::build_v3_record(
            (1 << 33, 0x857d),
            (0, 0x857d),
            USN_REASON_CLOSE,
            0x20,
            "huge",
        ));
        let mut seen = Vec::new();
        let (mut u16buf, mut sbuf) = scratch();
        let counts = walk_records(&data, FsKind::Refs, &mut u16buf, &mut sbuf, &mut |rec| {
            seen.push((rec.frn, rec.parent_frn, rec.name));
        });
        assert_eq!(
            seen,
            [(0x857d_0000_0007, 0x857d_0000_0000, "Cargo.toml".to_owned())]
        );
        assert_eq!(
            (counts.total, counts.unpackable, counts.unsupported),
            (2, 1, 0)
        );
    }

    #[test]
    fn walk_counts_corrupt_names_instead_of_dropping_silently() {
        // F6: an undecodable V2 record (odd FileNameLength) advances past the
        // record and is counted in `corrupt`, not silently merged into `total`.
        let good = crate::ffi::build_v2_record(7, 5, USN_REASON_CLOSE, 0x80, "ok.txt");
        let mut bad = crate::ffi::build_v2_record(8, 5, USN_REASON_CLOSE, 0x80, "bad");
        bad[56..58].copy_from_slice(&3u16.to_le_bytes()); // odd length
        let mut data = Vec::new();
        data.extend_from_slice(&good);
        data.extend_from_slice(&bad);
        let mut names = Vec::new();
        let (mut u16buf, mut sbuf) = scratch();
        let counts = walk_records(&data, FsKind::Ntfs, &mut u16buf, &mut sbuf, &mut |rec| {
            names.push(rec.name)
        });
        assert_eq!(names, ["ok.txt"]);
        assert_eq!(counts.total, 2);
        assert_eq!(counts.corrupt, 1);
        assert_eq!(counts.unsupported, 0);
    }

    #[test]
    fn walk_ref_borrows_names_with_zero_alloc_path() {
        let rec = crate::ffi::build_v2_record(11, 5, USN_REASON_CLOSE, 0x10, "borrowed.txt");
        let (mut u16buf, mut sbuf) = scratch();
        let mut seen: Vec<(u64, u64, u32, String)> = Vec::new();
        let counts = walk_records_ref(
            &rec,
            FsKind::Ntfs,
            &mut u16buf,
            &mut sbuf,
            &mut |frn, parent, _usn, _reason, attrs, name| {
                seen.push((frn, parent, attrs, name.to_owned()));
            },
        );
        assert_eq!(counts.total, 1);
        assert_eq!(counts.corrupt, 0);
        assert_eq!(seen, [(11, 5, 0x10, "borrowed.txt".to_owned())]);
    }

    #[test]
    fn open_rejects_non_drive_letters_without_touching_win32() {
        assert_eq!(
            VolumeHandle::open('1').unwrap_err(),
            NtfsError::Io(ERROR_INVALID_PARAMETER)
        );
        assert_eq!(
            VolumeHandle::open(':').unwrap_err(),
            NtfsError::Io(ERROR_INVALID_PARAMETER)
        );
    }

    // --- Elevated integration tests: need Administrator + a real NTFS volume. ---
    // Never create or delete a journal here.

    #[test]
    #[ignore]
    fn admin_open_c_query_journal() {
        let vol = VolumeHandle::open('C').expect("open C: (elevated?)");
        let info = vol.query_journal().expect("query journal on C:");
        println!(
            "journal_id={} first_usn={} next_usn={}",
            info.journal_id, info.first_usn, info.next_usn
        );
        assert_ne!(info.journal_id, 0);
        assert!(info.next_usn >= info.first_usn);
    }

    #[test]
    #[ignore]
    fn admin_enumerate_c_counts_records() {
        let vol = VolumeHandle::open('C').expect("open C: (elevated?)");
        let mut count = 0u64;
        let mut saw_windows = false;
        let mut first = Vec::new();
        let next = vol
            .enumerate(&mut |rec: RawRecord| {
                count += 1;
                if rec.name == "Windows" {
                    saw_windows = true;
                }
                if first.len() < 5 {
                    first.push(rec.name.clone());
                }
            })
            .expect("enumerate C:");
        println!("records={count} next_usn={next} first={first:?}");
        assert!(count > 1000, "expected > 1000 records, got {count}");
        assert!(saw_windows, "expected a record named \"Windows\"");
        assert!(next > 0);
    }

    /// Needs Administrator and a ReFS volume at `FLOKI_REFS_TEST` (e.g. `G`).
    #[test]
    #[ignore]
    fn admin_refs_walk_finds_this_repo() {
        let Some(letter) = std::env::var("FLOKI_REFS_TEST")
            .ok()
            .and_then(|v| v.chars().next())
        else {
            return;
        };
        let vol = VolumeHandle::open(letter).expect("open ReFS volume (elevated?)");
        assert_eq!(vol.fs(), FsKind::Refs);
        let (mut count, mut root_seen) = (0u64, false);
        vol.enumerate_with(|frn, parent, _, name| {
            count += 1;
            root_seen |= vol.is_root(frn) && parent == frn && name == ".";
        })
        .expect("walk");
        assert!(root_seen && count > 1);
    }

    #[test]
    #[ignore]
    fn admin_read_journal_from_next_usn_is_empty_ok() {
        let vol = VolumeHandle::open('C').expect("open C: (elevated?)");
        let info = vol.query_journal().expect("query journal on C:");
        let mut events = 0u32;
        let next = vol
            .read_journal(info.next_usn, info.journal_id, &mut |_| events += 1)
            .expect("read journal from NextUsn");
        println!("events={events} next={next}");
        assert_eq!(events, 0);
        assert!(next >= info.next_usn);
    }
}
