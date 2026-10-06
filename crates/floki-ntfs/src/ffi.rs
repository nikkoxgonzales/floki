//! Hand-written Win32 USN/MFT FFI types and pure record-parsing helpers.
//!
//! These layouts mirror the Microsoft Learn documentation for the change-journal
//! ioctls; `windows-sys` only supplies the `FSCTL_*`/`USN_REASON_*` constants (re-exported
//! below), so the structs are declared here instead of depending on a journal crate.
//!
//! Struct references:
//! - `USN_RECORD_V2`: <https://learn.microsoft.com/en-us/windows/win32/api/winioctl/ns-winioctl-usn_record_v2>
//! - `USN_RECORD_V3` (ReFS, 128-bit ids): <https://learn.microsoft.com/en-us/windows/win32/api/winioctl/ns-winioctl-usn_record_v3>
//! - `MFT_ENUM_DATA_V1` / `READ_USN_JOURNAL_DATA_V1`: V0 plus the accepted record versions
//! - `MFT_ENUM_DATA_V0`: <https://learn.microsoft.com/en-us/windows/win32/api/winioctl/ns-winioctl-mft_enum_data_v0>
//! - `READ_USN_JOURNAL_DATA_V0`: <https://learn.microsoft.com/en-us/windows/win32/api/winioctl/ns-winioctl-read_usn_journal_data_v0>
//! - `USN_JOURNAL_DATA_V1`: <https://learn.microsoft.com/en-us/windows/win32/api/winioctl/ns-winioctl-usn_journal_data_v1>
//! - `CREATE_USN_JOURNAL_DATA`: <https://learn.microsoft.com/en-us/windows/win32/api/winioctl/ni-winioctl-fsctl_create_usn_journal>

pub use windows_sys::Win32::System::Ioctl::{
    FSCTL_CREATE_USN_JOURNAL, FSCTL_ENUM_USN_DATA, FSCTL_QUERY_USN_JOURNAL, FSCTL_READ_USN_JOURNAL,
    USN_REASON_CLOSE, USN_REASON_FILE_CREATE, USN_REASON_FILE_DELETE, USN_REASON_HARD_LINK_CHANGE,
    USN_REASON_RENAME_NEW_NAME, USN_REASON_RENAME_OLD_NAME, USN_REASON_REPARSE_POINT_CHANGE,
};

/// Fixed-size header of a `USN_RECORD_V2` (offsets 0..60), followed by a
/// variable-length UTF-16 file name. The trailing `FileName[1]` flexible array
/// member is intentionally *not* part of this struct; name bytes are decoded
/// separately from `FileNameOffset`/`FileNameLength` so malformed records can
/// be bounds-checked without ever forming an unaligned reference.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct UsnRecordV2Header {
    pub record_length: u32,
    pub major_version: u16,
    pub minor_version: u16,
    pub file_reference_number: u64,
    pub parent_file_reference_number: u64,
    pub usn: i64,
    pub timestamp: i64,
    pub reason: u32,
    pub source_info: u32,
    pub security_id: u32,
    pub file_attributes: u32,
    pub file_name_length: u16,
    pub file_name_offset: u16,
}

/// `MFT_ENUM_DATA_V0` input for `FSCTL_ENUM_USN_DATA` (NTFS variant).
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct MftEnumDataV0 {
    pub start_file_reference_number: u64,
    pub low_usn: i64,
    pub high_usn: i64,
}

/// `MFT_ENUM_DATA_V1`: [`MftEnumDataV0`] plus the record versions accepted.
/// ReFS needs it (with 3..=3) to get `USN_RECORD_V3` and its 128-bit ids.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct MftEnumDataV1 {
    pub start_file_reference_number: u64,
    pub low_usn: i64,
    pub high_usn: i64,
    pub min_major_version: u16,
    pub max_major_version: u16,
}

/// `READ_USN_JOURNAL_DATA_V0` input for `FSCTL_READ_USN_JOURNAL`.
///
/// NTFS uses this V0 shape (a zero-filled V1-sized buffer is rejected with
/// `ERROR_INVALID_PARAMETER` on Win10+; see `docs/research/ntfs-index.md`
/// section 1.3). ReFS uses [`ReadUsnJournalDataV1`] with real versions set.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct ReadUsnJournalDataV0 {
    pub start_usn: i64,
    pub reason_mask: u32,
    pub return_only_on_close: u32,
    pub timeout: u64,
    pub bytes_to_wait_for: u64,
    pub usn_journal_id: u64,
}

/// `READ_USN_JOURNAL_DATA_V1`: [`ReadUsnJournalDataV0`] plus the record
/// versions accepted (ReFS: 3..=3).
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct ReadUsnJournalDataV1 {
    pub start_usn: i64,
    pub reason_mask: u32,
    pub return_only_on_close: u32,
    pub timeout: u64,
    pub bytes_to_wait_for: u64,
    pub usn_journal_id: u64,
    pub min_major_version: u16,
    pub max_major_version: u16,
}

/// `USN_JOURNAL_DATA_V1` output of `FSCTL_QUERY_USN_JOURNAL`.
///
/// This struct carries the 9-field V1 wire layout (the 7-field V0 prefix plus
/// `MinSupportedMajorVersion` / `MaxSupportedMajorVersion`); see
/// <https://learn.microsoft.com/en-us/windows/win32/api/winioctl/ns-winioctl-usn_journal_data_v1>.
/// The driver may return only the 56-byte V0 prefix on older layouts; callers
/// must branch on the `returned` byte count (see `VolumeHandle::query_journal`).
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct UsnJournalDataV1 {
    pub usn_journal_id: u64,
    pub first_usn: i64,
    pub next_usn: i64,
    pub lowest_valid_usn: i64,
    pub max_usn: i64,
    pub maximum_size: u64,
    pub allocation_delta: u64,
    pub min_supported_major_version: u16,
    pub max_supported_major_version: u16,
}

/// Backwards-compatible alias for the previous (misnamed) `UsnJournalDataV0`.
/// New code should use [`UsnJournalDataV1`].
pub type UsnJournalDataV0 = UsnJournalDataV1;

/// Minimum valid `returned` byte count from `FSCTL_QUERY_USN_JOURNAL`: the
/// 7-field V0 prefix (`UsnJournalID` .. `AllocationDelta`).
pub const USN_JOURNAL_DATA_V0_LEN: usize = 56;
/// Wire length of the 9-field V1 layout (`V0` prefix + 2x `u16` versions).
pub const USN_JOURNAL_DATA_V1_WIRE_LEN: usize = 60;

/// `CREATE_USN_JOURNAL_DATA` input for `FSCTL_CREATE_USN_JOURNAL`.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct CreateUsnJournalData {
    pub maximum_size: u64,
    pub allocation_delta: u64,
}

const _: () = assert!(size_of::<MftEnumDataV0>() == 24);
const _: () = assert!(size_of::<ReadUsnJournalDataV0>() == 40);
// C pads both V1 inputs to 8-byte alignment exactly like Rust does.
const _: () = assert!(size_of::<MftEnumDataV1>() == 32);
const _: () = assert!(std::mem::offset_of!(MftEnumDataV1, min_major_version) == 24);
const _: () = assert!(size_of::<ReadUsnJournalDataV1>() == 48);
const _: () = assert!(std::mem::offset_of!(ReadUsnJournalDataV1, min_major_version) == 40);
const _: () = assert!(size_of::<CreateUsnJournalData>() == 16);
// Wire sizes are 60 bytes, but 8-byte field alignment appends 4 bytes of tail
// padding to the Rust structs, hence 64. The `offset_of` asserts below prove
// every field still sits at its documented wire offset.
const _: () = assert!(size_of::<UsnRecordV2Header>() == 64);
const _: () = assert!(size_of::<UsnJournalDataV1>() == 64);
const _: () = assert!(std::mem::offset_of!(UsnRecordV2Header, record_length) == 0);
const _: () = assert!(std::mem::offset_of!(UsnRecordV2Header, major_version) == 4);
const _: () = assert!(std::mem::offset_of!(UsnRecordV2Header, minor_version) == 6);
const _: () = assert!(std::mem::offset_of!(UsnRecordV2Header, file_reference_number) == 8);
const _: () = assert!(std::mem::offset_of!(UsnRecordV2Header, parent_file_reference_number) == 16);
const _: () = assert!(std::mem::offset_of!(UsnRecordV2Header, usn) == 24);
const _: () = assert!(std::mem::offset_of!(UsnRecordV2Header, timestamp) == 32);
const _: () = assert!(std::mem::offset_of!(UsnRecordV2Header, reason) == 40);
const _: () = assert!(std::mem::offset_of!(UsnRecordV2Header, source_info) == 44);
const _: () = assert!(std::mem::offset_of!(UsnRecordV2Header, security_id) == 48);
const _: () = assert!(std::mem::offset_of!(UsnRecordV2Header, file_attributes) == 52);
const _: () = assert!(std::mem::offset_of!(UsnRecordV2Header, file_name_length) == 56);
const _: () = assert!(std::mem::offset_of!(UsnRecordV2Header, file_name_offset) == 58);
const _: () = assert!(std::mem::offset_of!(UsnJournalDataV1, usn_journal_id) == 0);
const _: () = assert!(std::mem::offset_of!(UsnJournalDataV1, first_usn) == 8);
const _: () = assert!(std::mem::offset_of!(UsnJournalDataV1, next_usn) == 16);
const _: () = assert!(std::mem::offset_of!(UsnJournalDataV1, lowest_valid_usn) == 24);
const _: () = assert!(std::mem::offset_of!(UsnJournalDataV1, max_usn) == 32);
const _: () = assert!(std::mem::offset_of!(UsnJournalDataV1, maximum_size) == 40);
const _: () = assert!(std::mem::offset_of!(UsnJournalDataV1, allocation_delta) == 48);
const _: () = assert!(std::mem::offset_of!(UsnJournalDataV1, min_supported_major_version) == 56);
const _: () = assert!(std::mem::offset_of!(UsnJournalDataV1, max_supported_major_version) == 58);

/// Byte size of the fixed `USN_RECORD_V2` header (everything before `FileName`).
pub const USN_RECORD_V2_HEADER_LEN: usize = 60;

/// Byte size of the fixed `USN_RECORD_V3` header: 128-bit ids push every
/// field after them 16 bytes further than in V2.
pub const USN_RECORD_V3_HEADER_LEN: usize = 76;

/// Record version NTFS returns for V0 requests.
pub const SUPPORTED_USN_MAJOR_VERSION: u16 = 2;
/// Record version ReFS returns (128-bit ids). V4 (range tracking) is counted
/// and skipped; see `NtfsError::UnsupportedRecordVersion`.
pub const USN_MAJOR_VERSION_V3: u16 = 3;

/// A successfully decoded V2 record with the file name as a Rust `String`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedRecord {
    pub frn: u64,
    pub parent_frn: u64,
    pub usn: i64,
    pub reason: u32,
    pub attrs: u32,
    pub name: String,
}

/// Failure modes of [`parse_record_at`]; none of them panic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParseError {
    /// Fewer than 8 bytes remain (cannot read `RecordLength`), or the declared
    /// `RecordLength` runs past the end of the buffer.
    Truncated,
    /// `RecordLength` is smaller than the 60-byte fixed header, or zero (which
    /// would otherwise loop forever).
    InvalidLength,
    /// The name slice (`FileNameOffset`/`FileNameLength`) runs past the end of
    /// the record.
    InvalidNameRange,
}

/// Decoded header fields of one record, without the allocated name.
///
/// `frn`/`parent_frn` are the low 64 bits of the id (the whole id for V2);
/// `frn_hi`/`parent_hi` are the high 64 bits of a V3 id, 0 for V2.
#[derive(Clone, Copy, Debug)]
pub struct RecordFields {
    pub frn: u64,
    pub parent_frn: u64,
    pub frn_hi: u64,
    pub parent_hi: u64,
    pub usn: i64,
    pub reason: u32,
    pub attrs: u32,
    pub name_off: usize,
    pub name_len: usize,
    pub record_len: usize,
}

/// Validate one record at the start of `buf` and split out its header fields.
///
/// Shared by [`parse_record_at`] and the scratch-based walker so both enforce
/// the same bounds, including `FileNameOffset >= header length` (a corrupt
/// offset inside the fixed header must not decode id bytes as a name).
/// Reads the V3 (ReFS) layout when `MajorVersion` is 3, V2 otherwise;
/// callers filter versions first.
pub(crate) fn record_fields(buf: &[u8]) -> Result<RecordFields, ParseError> {
    if buf.len() < 8 {
        return Err(ParseError::Truncated);
    }
    let record_len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    let v3 = u16::from_le_bytes([buf[4], buf[5]]) == USN_MAJOR_VERSION_V3;
    let header_len = if v3 {
        USN_RECORD_V3_HEADER_LEN
    } else {
        USN_RECORD_V2_HEADER_LEN
    };
    if record_len == 0 || record_len < header_len {
        return Err(ParseError::InvalidLength);
    }
    if buf.len() < record_len {
        return Err(ParseError::Truncated);
    }
    let rec = &buf[..record_len];
    let u64_at = |o: usize| u64::from_le_bytes(rec[o..o + 8].try_into().expect("len checked"));
    let u32_at = |o: usize| u32::from_le_bytes(rec[o..o + 4].try_into().expect("len checked"));
    let u16_at = |o: usize| usize::from(u16::from_le_bytes([rec[o], rec[o + 1]]));
    // V3 widens both ids to 16 bytes; every later field shifts by 16.
    let (frn, frn_hi, parent_frn, parent_hi, rest) = if v3 {
        (u64_at(8), u64_at(16), u64_at(24), u64_at(32), 40)
    } else {
        (u64_at(8), 0, u64_at(16), 0, 24)
    };
    let usn = u64_at(rest) as i64;
    let reason = u32_at(rest + 16);
    let attrs = u32_at(rest + 28);
    let name_len = u16_at(rest + 32);
    let name_off = u16_at(rest + 34);
    if !name_len.is_multiple_of(2)
        || name_off < header_len
        || name_off
            .checked_add(name_len)
            .is_none_or(|end| end > record_len)
    {
        return Err(ParseError::InvalidNameRange);
    }
    Ok(RecordFields {
        frn,
        parent_frn,
        frn_hi,
        parent_hi,
        usn,
        reason,
        attrs,
        name_off,
        name_len,
        record_len,
    })
}

/// Decode UTF-16LE `name_bytes` into `scratch`, reusing both scratch buffers.
///
/// Returns the decoded name borrowing `scratch` (lossy: lone surrogates become
/// U+FFFD and never abort a pass). No per-call allocation occurs once the
/// scratch capacities have grown to the longest name seen.
pub fn decode_name_into<'a>(
    name_bytes: &[u8],
    scratch_u16: &mut Vec<u16>,
    scratch: &'a mut String,
) -> &'a str {
    debug_assert!(name_bytes.len().is_multiple_of(2));
    let n = name_bytes.len() / 2;
    scratch_u16.clear();
    scratch_u16.reserve(n);
    let (chunks, _) = name_bytes.as_chunks::<2>();
    scratch_u16.extend(chunks.iter().map(|c| u16::from_le_bytes(*c)));
    scratch.clear();
    scratch.reserve(n);
    for c in char::decode_utf16(scratch_u16.iter().copied()) {
        scratch.push(c.unwrap_or(char::REPLACEMENT_CHARACTER));
    }
    scratch.as_str()
}

/// Decode one record at the start of `buf`.
///
/// Returns the parsed record plus its `RecordLength` so the caller can advance
/// to the next record. Every offset/length is bounds-checked; malformed input
/// yields `Err` instead of panicking (in particular no unaligned struct casts
/// are performed).
pub fn parse_record_at(buf: &[u8]) -> Result<(ParsedRecord, usize), ParseError> {
    let fields = record_fields(buf)?;
    let rec = &buf[..fields.record_len];
    let name_bytes = &rec[fields.name_off..fields.name_off + fields.name_len];
    let (chunks, _) = name_bytes.as_chunks::<2>();
    let mut units = Vec::with_capacity(chunks.len());
    for chunk in chunks {
        units.push(u16::from_le_bytes(*chunk));
    }
    // Lossy (never fails): NTFS names are UTF-16 and lone surrogates must not
    // abort an entire enumeration pass.
    let name = String::from_utf16_lossy(&units);
    Ok((
        ParsedRecord {
            frn: fields.frn,
            parent_frn: fields.parent_frn,
            usn: fields.usn,
            reason: fields.reason,
            attrs: fields.attrs,
            name,
        },
        fields.record_len,
    ))
}

/// Decode one record at the start of `buf` into reusable scratch buffers.
///
/// Same validation as [`parse_record_at`]; on success returns the header
/// fields, the decoded name borrowing `scratch`, and `RecordLength`.
/// Hot path for enumeration: zero per-record allocation once scratch has
/// grown to the longest name.
pub fn parse_record_at_with_scratch<'a>(
    buf: &[u8],
    scratch_u16: &mut Vec<u16>,
    scratch: &'a mut String,
) -> Result<(RecordFields, &'a str, usize), ParseError> {
    let fields = record_fields(buf)?;
    let rec = &buf[..fields.record_len];
    let name_bytes = &rec[fields.name_off..fields.name_off + fields.name_len];
    let name = decode_name_into(name_bytes, scratch_u16, scratch);
    Ok((fields, name, fields.record_len))
}

/// Build a minimal well-formed V3 record; ids are `(lo, hi)` pairs (test
/// helper shared by this crate's unit tests).
#[cfg(test)]
pub(crate) fn build_v3_record(
    frn: (u64, u64),
    parent: (u64, u64),
    reason: u32,
    attrs: u32,
    name: &str,
) -> Vec<u8> {
    let units: Vec<u16> = name.encode_utf16().collect();
    let name_len = units.len() * 2;
    let record_len = USN_RECORD_V3_HEADER_LEN + name_len;
    let mut buf = vec![0u8; record_len];
    buf[0..4].copy_from_slice(&(record_len as u32).to_le_bytes());
    buf[4..6].copy_from_slice(&3u16.to_le_bytes());
    buf[8..16].copy_from_slice(&frn.0.to_le_bytes());
    buf[16..24].copy_from_slice(&frn.1.to_le_bytes());
    buf[24..32].copy_from_slice(&parent.0.to_le_bytes());
    buf[32..40].copy_from_slice(&parent.1.to_le_bytes());
    buf[40..48].copy_from_slice(&77i64.to_le_bytes());
    buf[56..60].copy_from_slice(&reason.to_le_bytes());
    buf[68..72].copy_from_slice(&attrs.to_le_bytes());
    buf[72..74].copy_from_slice(&(name_len as u16).to_le_bytes());
    buf[74..76].copy_from_slice(&(USN_RECORD_V3_HEADER_LEN as u16).to_le_bytes());
    for (i, unit) in units.iter().enumerate() {
        let o = USN_RECORD_V3_HEADER_LEN + i * 2;
        buf[o..o + 2].copy_from_slice(&unit.to_le_bytes());
    }
    buf
}

/// Read the `MajorVersion` of the record at the start of `buf`, if present.
pub fn major_version_at(buf: &[u8]) -> Option<u16> {
    if buf.len() < USN_RECORD_V2_HEADER_LEN {
        return None;
    }
    let record_len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    if record_len == 0 || buf.len() < record_len {
        return None;
    }
    Some(u16::from_le_bytes([buf[4], buf[5]]))
}

/// Build a minimal well-formed V2 record byte buffer (test helper shared by
/// this crate's unit tests).
#[cfg(test)]
pub(crate) fn build_v2_record(
    frn: u64,
    parent_frn: u64,
    reason: u32,
    attrs: u32,
    name: &str,
) -> Vec<u8> {
    let units: Vec<u16> = name.encode_utf16().collect();
    let name_len = units.len() * 2;
    let record_len = USN_RECORD_V2_HEADER_LEN + name_len;
    let mut buf = vec![0u8; record_len];
    buf[0..4].copy_from_slice(&(record_len as u32).to_le_bytes());
    buf[4..6].copy_from_slice(&2u16.to_le_bytes());
    buf[8..16].copy_from_slice(&frn.to_le_bytes());
    buf[16..24].copy_from_slice(&parent_frn.to_le_bytes());
    buf[40..44].copy_from_slice(&reason.to_le_bytes());
    buf[52..56].copy_from_slice(&attrs.to_le_bytes());
    buf[56..58].copy_from_slice(&(name_len as u16).to_le_bytes());
    buf[58..60].copy_from_slice(&(USN_RECORD_V2_HEADER_LEN as u16).to_le_bytes());
    for (i, unit) in units.iter().enumerate() {
        buf[USN_RECORD_V2_HEADER_LEN + i * 2..USN_RECORD_V2_HEADER_LEN + i * 2 + 2]
            .copy_from_slice(&unit.to_le_bytes());
    }
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hand_built_v2_record() {
        let buf = build_v2_record(0x0005_0000_0005, 0x0005_0000_0005, 0x100, 0x10, "Windows");
        let (rec, len) = parse_record_at(&buf).expect("valid record parses");
        assert_eq!(len, buf.len());
        assert_eq!(rec.frn, 0x0005_0000_0005);
        assert_eq!(rec.parent_frn, 0x0005_0000_0005);
        assert_eq!(rec.reason, 0x100);
        assert_eq!(rec.attrs, 0x10);
        assert_eq!(rec.name, "Windows");
    }

    #[test]
    fn truncated_buffer_does_not_panic() {
        let buf = build_v2_record(5, 5, 1, 0x10, "Windows");
        for cut in [0, 1, 7, 8, 59, 60, 61] {
            let end = cut.min(buf.len());
            assert_eq!(parse_record_at(&buf[..end]), Err(ParseError::Truncated));
        }
        // Declared length longer than the buffer: truncated, not a panic.
        let mut long = buf.clone();
        long[0..4].copy_from_slice(&((buf.len() + 64) as u32).to_le_bytes());
        assert_eq!(parse_record_at(&long), Err(ParseError::Truncated));
        // Zero / tiny declared length: invalid, and must not loop forever.
        let mut zero = buf.clone();
        zero[0..4].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(parse_record_at(&zero), Err(ParseError::InvalidLength));
        // Name range past the record end: invalid, not a panic.
        let mut bad_name = buf;
        bad_name[56..58].copy_from_slice(&0xFFF0u16.to_le_bytes());
        assert_eq!(
            parse_record_at(&bad_name),
            Err(ParseError::InvalidNameRange)
        );
    }

    #[test]
    fn name_offset_inside_header_is_rejected() {
        // F5: FileNameOffset pointing into the fixed header (e.g. 8) would
        // otherwise decode FRN bytes as a name; it must be InvalidNameRange.
        let mut buf = build_v2_record(5, 5, 1, 0x10, "Windows");
        buf[58..60].copy_from_slice(&8u16.to_le_bytes());
        buf[56..58].copy_from_slice(&16u16.to_le_bytes());
        assert_eq!(parse_record_at(&buf), Err(ParseError::InvalidNameRange));
        // Odd FileNameLength is still rejected.
        let mut odd = build_v2_record(5, 5, 1, 0x10, "ab");
        odd[56..58].copy_from_slice(&3u16.to_le_bytes());
        assert_eq!(parse_record_at(&odd), Err(ParseError::InvalidNameRange));
    }

    #[test]
    fn scratch_decode_matches_owned_parse_and_reuses_buffers() {
        let buf = build_v2_record(7, 5, 1, 0x10, "scratch-me.txt");
        let (owned, len) = parse_record_at(&buf).expect("valid record parses");
        let mut u16buf = Vec::new();
        let mut sbuf = String::new();
        let (fields, name, len2) =
            parse_record_at_with_scratch(&buf, &mut u16buf, &mut sbuf).expect("scratch parses");
        assert_eq!(len, len2);
        assert_eq!(fields.frn, owned.frn);
        assert_eq!(fields.parent_frn, owned.parent_frn);
        assert_eq!(name, owned.name);
        let cap_u16 = u16buf.capacity();
        let cap_s = sbuf.capacity();
        // Second decode reuses the same allocations (no growth).
        let buf2 = build_v2_record(8, 5, 1, 0x10, "a.txt");
        let (_, name2, _) = parse_record_at_with_scratch(&buf2, &mut u16buf, &mut sbuf)
            .expect("second scratch parses");
        assert_eq!(name2, "a.txt");
        assert!(u16buf.capacity() >= cap_u16);
        assert!(sbuf.capacity() >= cap_s);
    }

    #[test]
    fn parses_hand_built_v3_record_with_128_bit_ids() {
        let buf = build_v3_record((7, 0x857d), (0, 0x857d), 0x100, 0x20, "Cargo.toml");
        let mut u16buf = Vec::new();
        let mut sbuf = String::new();
        let (f, name, len) =
            parse_record_at_with_scratch(&buf, &mut u16buf, &mut sbuf).expect("v3 parses");
        assert_eq!(len, buf.len());
        assert_eq!((f.frn, f.frn_hi), (7, 0x857d));
        assert_eq!((f.parent_frn, f.parent_hi), (0, 0x857d));
        assert_eq!((f.usn, f.reason, f.attrs), (77, 0x100, 0x20));
        assert_eq!(name, "Cargo.toml");
        // A V3 name offset inside the 76-byte header is rejected.
        let mut bad = buf;
        bad[74..76].copy_from_slice(&60u16.to_le_bytes());
        assert_eq!(parse_record_at(&bad), Err(ParseError::InvalidNameRange));
    }

    #[test]
    fn journal_data_v1_layout_is_64_with_v0_prefix_constants() {
        // F3: V1 wire layout is 60 bytes (64 with Rust tail padding); the V0
        // prefix usable after a short driver write is 56 bytes.
        assert_eq!(size_of::<UsnJournalDataV1>(), 64);
        assert_eq!(USN_JOURNAL_DATA_V0_LEN, 56);
        assert_eq!(USN_JOURNAL_DATA_V1_WIRE_LEN, 60);
        // Backwards-compat alias still resolves to the same layout.
        assert_eq!(size_of::<UsnJournalDataV0>(), 64);
    }
}
