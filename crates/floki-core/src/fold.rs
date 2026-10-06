//! Case folding: ASCII fast path + Unicode lowercase fallback (SPEC section 4).

/// Lowercase `s` for case-insensitive matching.
///
/// Pure-ASCII input takes a byte-wise fast path; anything else falls back to
/// Unicode [`str::to_lowercase`].
#[must_use]
pub fn fold(s: &str) -> String {
    if s.is_ascii() {
        s.to_ascii_lowercase()
    } else {
        s.to_lowercase()
    }
}

/// Lowercase raw name bytes (see [`fold`]): single pass, no UTF-8 validation
/// (arena bytes are always valid UTF-8 — built from `&str`, load-validated).
/// Non-ASCII input falls back to [`fold`]; pure-ASCII lowercases inline.
#[must_use]
pub fn fold_bytes(nb: &[u8]) -> String {
    let mut out = String::with_capacity(nb.len());
    for &b in nb {
        if b < 0x80 {
            out.push(b.to_ascii_lowercase() as char);
        } else {
            return fold(std::str::from_utf8(nb).unwrap_or(""));
        }
    }
    out
}

/// [`fold_bytes`] appended into `out` without allocating: byte-identical
/// output (ASCII lowercases inline; a non-ASCII byte discards the prefix and
/// extends with the whole-input Unicode lowercase form, exactly like
/// [`fold_bytes`]). The ASCII fast path goes through `extend` (one capacity
/// check for the whole name, not one per byte).
pub fn fold_bytes_into(nb: &[u8], out: &mut Vec<u8>) {
    if nb.is_ascii() {
        out.extend(nb.iter().map(|b| b.to_ascii_lowercase()));
        return;
    }
    let base = out.len();
    out.reserve(nb.len());
    for &b in nb {
        if b < 0x80 {
            out.push(b.to_ascii_lowercase());
        } else {
            out.truncate(base);
            out.extend_from_slice(fold(std::str::from_utf8(nb).unwrap_or("")).as_bytes());
            return;
        }
    }
}

/// Case-insensitive substring test where the needle is already folded.
///
/// ASCII/ASCII pairs scan without allocating (SIMD-prefiltered first-byte
/// search via `memchr`, byte-wise compare after); other pairs fold the
/// haystack once and use `contains`.
#[must_use]
pub fn contains_insensitive(haystack: &str, needle_folded: &str) -> bool {
    if needle_folded.is_empty() {
        return true;
    }
    if haystack.is_ascii() && needle_folded.is_ascii() {
        let h = haystack.as_bytes();
        let n = needle_folded.as_bytes();
        if n.len() > h.len() {
            return false;
        }
        let b0 = n[0];
        let b0u = b0.to_ascii_uppercase();
        let mut start = 0;
        while start + n.len() <= h.len() {
            let rel = if b0 == b0u {
                memchr::memchr(b0, &h[start..])
            } else {
                memchr::memchr2(b0, b0u, &h[start..])
            };
            let Some(off) = rel else {
                return false;
            };
            let pos = start + off;
            if pos + n.len() > h.len() {
                return false;
            }
            if h[pos + 1..pos + n.len()]
                .iter()
                .zip(&n[1..])
                .all(|(a, b)| a.to_ascii_lowercase() == *b)
            {
                return true;
            }
            start = pos + 1;
        }
        false
    } else {
        fold(haystack).contains(needle_folded)
    }
}

/// Case-insensitive substring test over raw name bytes; the needle is already
/// folded (`&str` so callers keep the precomputed `String`). See
/// [`contains_insensitive_prefolded`] for the fast path.
#[must_use]
pub fn contains_insensitive_bytes(haystack: &[u8], needle_folded: &str) -> bool {
    contains_insensitive_prefolded(haystack, needle_folded.as_bytes(), needle_folded.is_ascii())
}

/// [`contains_insensitive_bytes`] with the needle pre-split into bytes plus
/// a precomputed ASCII flag. The flag is hoisted so the per-entry hot path
/// never rescans the needle. ASCII/ASCII pairs run a single SIMD first-byte
/// prefilter (`memchr`/`memchr2`) with lowercase-compare verify — one pass
/// for the overwhelmingly common miss, no folding, no allocation. Anything
/// else folds the haystack once and searches the folded bytes via
/// `memchr::memmem::find` (a byte match of a valid-UTF-8 needle in a
/// valid-UTF-8 haystack is always char-boundary aligned).
#[must_use]
#[inline(always)]
pub fn contains_insensitive_prefolded(haystack: &[u8], needle: &[u8], needle_ascii: bool) -> bool {
    if needle.is_empty() {
        return true;
    }
    if needle.len() > haystack.len() {
        return false;
    }
    if needle_ascii && haystack.is_ascii() {
        let b0 = needle[0];
        let b0u = b0.to_ascii_uppercase();
        let mut start = 0;
        while start + needle.len() <= haystack.len() {
            let rel = if b0 == b0u {
                memchr::memchr(b0, &haystack[start..])
            } else {
                memchr::memchr2(b0, b0u, &haystack[start..])
            };
            let Some(off) = rel else {
                return false;
            };
            let pos = start + off;
            if pos + needle.len() > haystack.len() {
                return false;
            }
            if haystack[pos + 1..pos + needle.len()]
                .iter()
                .zip(&needle[1..])
                .all(|(a, b)| a.to_ascii_lowercase() == *b)
            {
                return true;
            }
            start = pos + 1;
        }
        return false;
    }
    match std::str::from_utf8(haystack) {
        Ok(h) => memchr::memmem::find(fold(h).as_bytes(), needle).is_some(),
        Err(_) => false,
    }
}
/// Case-insensitive equality. `b_folded` must equal `fold(b_raw)`.
#[must_use]
pub fn eq_insensitive(a: &str, b_raw: &str, b_folded: &str) -> bool {
    if a.is_ascii() && b_raw.is_ascii() {
        a.as_bytes().eq_ignore_ascii_case(b_raw.as_bytes())
    } else {
        fold(a) == b_folded
    }
}

/// Case-insensitive byte-slice ordering, exactly `fold(a).cmp(&fold(b))`
/// without allocating: pure-ASCII pairs compare lowercased bytes directly
/// (one pass); any non-ASCII byte falls back to folding both sides.
#[must_use]
pub fn cmp_folded_bytes(a: &[u8], b: &[u8]) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let mut i = 0;
    while i < a.len() && i < b.len() {
        let (x, y) = (a[i], b[i]);
        if x >= 0x80 || y >= 0x80 {
            let sa = std::str::from_utf8(a).unwrap_or("");
            let sb = std::str::from_utf8(b).unwrap_or("");
            return fold(sa).cmp(&fold(sb));
        }
        match x.to_ascii_lowercase().cmp(&y.to_ascii_lowercase()) {
            Ordering::Equal => i += 1,
            ord => return ord,
        }
    }
    a.len().cmp(&b.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_fold() {
        assert_eq!(fold("HeLLo.TXT"), "hello.txt");
    }

    #[test]
    fn unicode_fallback() {
        assert_eq!(fold("ÄPFEL"), "äpfel");
        assert!(contains_insensitive("Äpfelbaum", "äpfel"));
    }

    #[test]
    fn bytes_version_matches_str_version() {
        let cases = [
            ("HelloWorld", "low", true),
            ("HelloWorld", "loww", false),
            ("hi", "hello", false),
            ("abc", "", true),
            ("a+b", "+", true),
            ("MiXeD", "mixed", true),
            ("not here", "zzz", false),
            ("Äpfelbaum", "äpfel", true),
            ("plain", "ä", false),
        ];
        for (h, n, want) in cases {
            assert_eq!(contains_insensitive(h, n), want, "{h:?} {n:?}");
            assert_eq!(
                contains_insensitive_bytes(h.as_bytes(), n),
                want,
                "{h:?} {n:?}"
            );
        }
        // Over-long ASCII name takes the fallback path with the same result.
        let long = "a".repeat(600) + "Needle" + &"b".repeat(600);
        assert!(contains_insensitive_bytes(long.as_bytes(), "needle"));
        assert!(!contains_insensitive_bytes(long.as_bytes(), "missing"));
    }

    #[test]
    fn fold_bytes_matches_fold() {
        for s in [
            "",
            "Hello.TXT",
            "MiXeD",
            "plain",
            "Äpfel",
            "naïve.rs",
            "Zürich.md",
        ] {
            assert_eq!(fold_bytes(s.as_bytes()), fold(s), "{s:?}");
        }
    }

    #[test]
    fn cmp_folded_bytes_matches_fold_cmp() {
        let names = [
            "",
            "a",
            "A",
            "apple",
            "Apple",
            "APPLE",
            "apples",
            "Banana",
            "banana",
            "Zebra",
            "zebra",
            "Äpfel",
            "äpfel",
            "ÄPFEL",
            "äBC",
            "Zürich",
            "zürich",
            "naïve.rs",
            "NAÏVE.RS",
            "résumé.pdf",
            ".hidden",
            "aA",
            "aa",
            "aB",
        ];
        for a in names {
            for b in names {
                assert_eq!(
                    cmp_folded_bytes(a.as_bytes(), b.as_bytes()),
                    fold(a).cmp(&fold(b)),
                    "{a:?} vs {b:?}"
                );
            }
        }
    }

    #[test]
    fn ascii_contains_cases() {
        assert!(contains_insensitive("HelloWorld", "low"));
        assert!(contains_insensitive("HelloWorld", "low")); // needle must be pre-folded
        assert!(!contains_insensitive("HelloWorld", "loww"));
        assert!(!contains_insensitive("hi", "hello"));
        assert!(contains_insensitive("abc", ""));
        assert!(contains_insensitive("a+b", "+"));
        assert!(contains_insensitive("MiXeD", "mixed"));
    }
}
