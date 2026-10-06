//! `Entry`: the 24-byte index record (SPEC section 3).

/// Index into [`Index`](crate::Index)'s entry array. Never `usize` in stored data.
pub type EntryId = u32;

/// Flag bit: entry is a directory.
pub const DIRECTORY: u16 = 1;
/// Flag bit: hidden attribute.
pub const HIDDEN: u16 = 2;
/// Flag bit: system attribute.
pub const SYSTEM: u16 = 4;
/// Flag bit: reparse point (symlink / junction).
pub const REPARSE: u16 = 8;
/// Flag bit: tombstoned (deleted, awaiting compaction). Never returned by search.
pub const TOMBSTONE: u16 = 0x8000;

/// One indexed file or directory: 24 bytes, `Copy` + `repr(C)`.
///
/// Names live in the [`NameArena`](crate::NameArena); this holds only the offset.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    /// NTFS file reference number (48-bit index + 16-bit sequence).
    pub frn: u64,
    /// Parent directory FRN; the root directory's `parent_frn` equals its own `frn`.
    pub parent_frn: u64,
    /// Byte offset of the UTF-8 name inside the arena.
    pub name_off: u32,
    /// Byte length of the UTF-8 name.
    pub name_len: u16,
    /// [`DIRECTORY`] / [`HIDDEN`] / [`SYSTEM`] / [`REPARSE`] / [`TOMBSTONE`] bits.
    pub flags: u16,
}

const _: () = assert!(size_of::<Entry>() == 24);

impl Entry {
    /// Flag bit: entry is a directory.
    pub const DIRECTORY: u16 = DIRECTORY;
    /// Flag bit: hidden attribute.
    pub const HIDDEN: u16 = HIDDEN;
    /// Flag bit: system attribute.
    pub const SYSTEM: u16 = SYSTEM;
    /// Flag bit: reparse point (symlink / junction).
    pub const REPARSE: u16 = REPARSE;
    /// Flag bit: tombstoned (deleted, awaiting compaction).
    pub const TOMBSTONE: u16 = TOMBSTONE;
    /// Serialized size on disk (little-endian).
    pub const BYTE_LEN: usize = 24;

    /// True when the [`DIRECTORY`] bit is set.
    #[must_use]
    pub fn is_dir(self) -> bool {
        self.flags & DIRECTORY != 0
    }

    /// True when the [`TOMBSTONE`] bit is set.
    #[must_use]
    pub fn is_tombstone(self) -> bool {
        self.flags & TOMBSTONE != 0
    }

    /// Little-endian bytes for persistence.
    #[must_use]
    pub fn to_le_bytes(self) -> [u8; Self::BYTE_LEN] {
        let mut out = [0u8; Self::BYTE_LEN];
        out[0..8].copy_from_slice(&self.frn.to_le_bytes());
        out[8..16].copy_from_slice(&self.parent_frn.to_le_bytes());
        out[16..20].copy_from_slice(&self.name_off.to_le_bytes());
        out[20..22].copy_from_slice(&self.name_len.to_le_bytes());
        out[22..24].copy_from_slice(&self.flags.to_le_bytes());
        out
    }

    /// Inverse of [`to_le_bytes`](Self::to_le_bytes). Returns `None` on short input.
    #[must_use]
    pub fn from_le_bytes(b: &[u8]) -> Option<Self> {
        let b: [u8; Self::BYTE_LEN] = b.get(..Self::BYTE_LEN)?.try_into().ok()?;
        Some(Self {
            frn: u64::from_le_bytes(b[0..8].try_into().ok()?),
            parent_frn: u64::from_le_bytes(b[8..16].try_into().ok()?),
            name_off: u32::from_le_bytes(b[16..20].try_into().ok()?),
            name_len: u16::from_le_bytes(b[20..22].try_into().ok()?),
            flags: u16::from_le_bytes(b[22..24].try_into().ok()?),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flag_values_match_spec() {
        assert_eq!(DIRECTORY, 1);
        assert_eq!(HIDDEN, 2);
        assert_eq!(SYSTEM, 4);
        assert_eq!(REPARSE, 8);
        assert_eq!(TOMBSTONE, 0x8000);
    }

    #[test]
    fn round_trip_le_bytes() {
        let e = Entry {
            frn: 0x1234_5678_9abc_def0,
            parent_frn: 42,
            name_off: 7,
            name_len: 9,
            flags: DIRECTORY | HIDDEN,
        };
        let b = e.to_le_bytes();
        assert_eq!(Entry::from_le_bytes(&b), Some(e));
        assert!(Entry::from_le_bytes(&b[..10]).is_none());
    }

    #[test]
    fn predicates() {
        let d = Entry {
            frn: 1,
            parent_frn: 1,
            name_off: 0,
            name_len: 0,
            flags: DIRECTORY,
        };
        assert!(d.is_dir());
        assert!(!d.is_tombstone());
        let t = Entry {
            flags: TOMBSTONE,
            ..d
        };
        assert!(t.is_tombstone());
    }
}
