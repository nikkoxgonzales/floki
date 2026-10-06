//! `NameArena`: append-only UTF-8 name storage (SPEC section 3).

/// Concatenated file names in original case, no separators.
///
/// Entries hold `(name_off, name_len)` into this buffer. Old bytes left behind
/// by renames are garbage until [`compact`](crate::Index::compact) rewrites it.
#[derive(Clone, Debug, Default)]
pub struct NameArena {
    bytes: Vec<u8>,
}

impl NameArena {
    /// Empty arena.
    #[must_use]
    pub fn new() -> Self {
        Self { bytes: Vec::new() }
    }

    /// Empty arena with `capacity` bytes preallocated.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(capacity),
        }
    }

    /// Append `name`, returning `(offset, len)`.
    ///
    /// Names longer than `u16::MAX` bytes are truncated at a UTF-8 boundary.
    pub fn append(&mut self, name: &str) -> (u32, u16) {
        let mut end = name.len().min(u16::MAX as usize);
        while !name.is_char_boundary(end) {
            end -= 1;
        }
        let off = self.bytes.len();
        debug_assert!(off <= u32::MAX as usize);
        self.bytes.extend_from_slice(&name.as_bytes()[..end]);
        (off as u32, end as u16)
    }

    /// Read the slice at `(off, len)`. Returns `None` on out-of-bounds or
    /// non-UTF-8 data (the latter only if the file was corrupt).
    #[must_use]
    pub fn get(&self, off: u32, len: u16) -> Option<&str> {
        let end = (off as usize).checked_add(len as usize)?;
        let b = self.bytes.get(off as usize..end)?;
        std::str::from_utf8(b).ok()
    }

    /// Read the raw bytes at `(off, len)`. Returns `None` on out-of-bounds.
    /// Unlike [`get`](Self::get) this skips UTF-8 validation, so the search
    /// hot path can scan bytes directly (names appended from `&str` are
    /// valid UTF-8 unless the backing file was corrupt).
    #[must_use]
    pub fn get_bytes(&self, off: u32, len: u16) -> Option<&[u8]> {
        let end = (off as usize).checked_add(len as usize)?;
        self.bytes.get(off as usize..end)
    }

    /// Raw bytes (persistence).
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Adopt bytes loaded from disk. Caller must have validated UTF-8.
    pub(crate) fn set_bytes(&mut self, bytes: Vec<u8>) {
        self.bytes = bytes;
    }

    /// Reserve room for `additional` more bytes.
    pub fn reserve(&mut self, additional: usize) {
        self.bytes.reserve(additional);
    }

    /// Byte length of the arena.
    #[must_use]
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// True when empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_read_round_trip() {
        let mut a = NameArena::new();
        let (o1, l1) = a.append("hello");
        let (o2, l2) = a.append("wörld.rs");
        assert_eq!(a.get(o1, l1), Some("hello"));
        assert_eq!(a.get(o2, l2), Some("wörld.rs"));
        assert_eq!(o2, 5);
    }

    #[test]
    fn invalid_get_is_none() {
        let mut a = NameArena::new();
        a.append("abc");
        assert!(a.get(0, 4).is_none());
        assert!(a.get(99, 1).is_none());
        assert!(a.get(u32::MAX, 1).is_none());
        assert!(a.get(0, 0) == Some(""));
    }

    #[test]
    fn long_name_truncates_at_boundary() {
        let mut a = NameArena::new();
        let s = "é".repeat(40_000); // 80_000 bytes
        let (o, l) = a.append(&s);
        assert!(u32::from(l) <= u16::MAX as u32);
        let back = a.get(o, l).unwrap();
        assert!(s.starts_with(back));
    }
}
