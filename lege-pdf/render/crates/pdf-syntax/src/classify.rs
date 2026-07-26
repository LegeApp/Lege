//! Byte classification per ISO 32000-1 §7.2 Tables 1–2.
//!
//! Three classes partition the byte space: whitespace separates tokens,
//! delimiters are self-terminating single/paired tokens, and everything
//! else is "regular" (participates in numbers, names, keywords).
//!
//! Table-free `match` form: the optimizer lowers these to the same jump
//! table a 256-entry const array would give, without the opportunity for
//! the table and the spec to drift apart silently.

/// Whitespace: NUL, HT, LF, FF, CR, SP (ISO 32000-1 Table 1).
#[inline]
pub const fn is_whitespace(b: u8) -> bool {
    matches!(b, 0x00 | 0x09 | 0x0A | 0x0C | 0x0D | 0x20)
}

/// Delimiters: `( ) < > [ ] { } / %` (ISO 32000-1 Table 2).
#[inline]
pub const fn is_delimiter(b: u8) -> bool {
    matches!(
        b,
        b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%'
    )
}

/// Regular characters: everything that is neither whitespace nor delimiter.
#[inline]
pub const fn is_regular(b: u8) -> bool {
    !is_whitespace(b) && !is_delimiter(b)
}

/// End-of-line bytes (CR or LF).
#[inline]
pub const fn is_eol(b: u8) -> bool {
    matches!(b, 0x0A | 0x0D)
}

/// Characters that may appear in a numeric literal: digits, signs, dot.
/// Matches PDFium's `PDFCharIsNumeric` — a run consisting entirely of these
/// is treated as a number token.
#[inline]
pub const fn is_numeric_char(b: u8) -> bool {
    b.is_ascii_digit() || matches!(b, b'+' | b'-' | b'.')
}

/// Decode one ASCII hex digit.
#[inline]
pub const fn hex_value(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Decode one octal digit.
#[inline]
pub const fn octal_value(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'7' => Some(b - b'0'),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn classes_partition_byte_space() {
        for b in 0..=255u8 {
            let classes = [is_whitespace(b), is_delimiter(b), is_regular(b)]
                .iter()
                .filter(|&&c| c)
                .count();
            assert_eq!(classes, 1, "byte 0x{b:02x} must be in exactly one class");
        }
    }

    #[test]
    fn spec_examples() {
        for b in [0x00, 0x09, 0x0A, 0x0C, 0x0D, 0x20] {
            assert!(is_whitespace(b));
        }
        for b in b"()<>[]{}/%" {
            assert!(is_delimiter(*b));
        }
        // Regular: letters, digits, and e.g. '#', '\'', '"', '*'.
        for b in b"AZaz09#'\"*!" {
            assert!(is_regular(*b));
        }
    }

    #[test]
    fn hex_and_octal() {
        assert_eq!(hex_value(b'0'), Some(0));
        assert_eq!(hex_value(b'a'), Some(10));
        assert_eq!(hex_value(b'F'), Some(15));
        assert_eq!(hex_value(b'g'), None);
        assert_eq!(octal_value(b'7'), Some(7));
        assert_eq!(octal_value(b'8'), None);
    }
}
