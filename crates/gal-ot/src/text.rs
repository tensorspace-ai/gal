//! UTF-16 aware string helpers.
//!
//! Every offset and length in this crate is measured in UTF-16 code units, not
//! bytes and not Unicode scalar values. That is deliberate: the browser client
//! addresses text through DOM selection APIs, which are defined over UTF-16 code
//! units. Using `chars().count()` here would silently desynchronise the server
//! and the client the moment anyone typed an emoji.

/// Length of `s` in UTF-16 code units.
#[inline]
pub fn utf16_len(s: &str) -> usize {
    s.chars().map(char::len_utf16).sum()
}

/// Advance `units` UTF-16 code units from byte position `from`, returning the
/// resulting byte position.
///
/// If `units` would land in the middle of a surrogate pair (only reachable from
/// a malformed peer op) the position is rounded *up* to the next character
/// boundary, so the result is always a valid slice index.
pub fn advance_utf16(s: &str, from: usize, units: usize) -> usize {
    let mut consumed = 0usize;
    for (byte_idx, ch) in s[from..].char_indices() {
        if consumed >= units {
            return from + byte_idx;
        }
        consumed += ch.len_utf16();
    }
    s.len()
}

/// Byte index of UTF-16 offset `target` within `s`.
#[inline]
pub fn utf16_to_byte(s: &str, target: usize) -> usize {
    advance_utf16(s, 0, target)
}

/// Slice `s` by UTF-16 offsets, clamped to the bounds of the string.
pub fn utf16_slice(s: &str, start: usize, end: usize) -> &str {
    let start_byte = utf16_to_byte(s, start);
    let end_byte = advance_utf16(s, start_byte, end.saturating_sub(start));
    &s[start_byte..end_byte]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_lengths() {
        assert_eq!(utf16_len("hello"), 5);
        assert_eq!(utf16_len(""), 0);
    }

    #[test]
    fn astral_chars_count_as_two_units() {
        // U+1F30A WATER WAVE is a surrogate pair in UTF-16.
        assert_eq!(utf16_len("🌊"), 2);
        assert_eq!(
            "🌊".chars().count(),
            1,
            "differs from scalar count, as intended"
        );
        assert_eq!(utf16_len("a🌊b"), 4);
    }

    #[test]
    fn bmp_multibyte_counts_as_one_unit() {
        assert_eq!(utf16_len("é"), 1);
        assert_eq!("é".len(), 2, "two bytes, one code unit");
        assert_eq!(utf16_len("日本語"), 3);
    }

    #[test]
    fn slicing_by_utf16_offsets() {
        assert_eq!(utf16_slice("hello", 1, 3), "el");
        assert_eq!(utf16_slice("a🌊b", 0, 1), "a");
        assert_eq!(utf16_slice("a🌊b", 1, 3), "🌊");
        assert_eq!(utf16_slice("a🌊b", 3, 4), "b");
        assert_eq!(utf16_slice("日本語", 1, 2), "本");
    }

    #[test]
    fn slicing_clamps_out_of_range() {
        assert_eq!(utf16_slice("hi", 0, 99), "hi");
        assert_eq!(utf16_slice("hi", 5, 9), "");
    }

    #[test]
    fn split_inside_surrogate_pair_rounds_up() {
        // Offset 1 is between the high and low surrogate of the wave emoji.
        assert_eq!(utf16_slice("🌊b", 0, 1), "🌊");
    }

    #[test]
    fn advance_is_incremental() {
        let s = "a🌊b";
        let p0 = 0;
        let p1 = advance_utf16(s, p0, 1);
        let p2 = advance_utf16(s, p1, 2);
        let p3 = advance_utf16(s, p2, 1);
        assert_eq!(&s[p0..p1], "a");
        assert_eq!(&s[p1..p2], "🌊");
        assert_eq!(&s[p2..p3], "b");
    }
}
