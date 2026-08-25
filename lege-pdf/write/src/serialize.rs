//! Low-level COS serialization: write_pdf_real, name/string escaping,
//! dictionaries, arrays, indirect object framing. PdfValue borrow enum for
//! the few cold-path generic dictionaries (catalog, outlines, metadata).
//!
//! Every writer appends to a caller-owned `Vec<u8>`; none allocate a temporary
//! String. Numbers are formatted directly into the output buffer.

use crate::types::ObjectId;

/// Largest magnitude for which the fixed-point real formatter is exact enough;
/// beyond this we fall back to an integer round. PDF page coordinates never
/// approach it, so the fallback is a safety net, not a real path.
const REAL_FIXED_LIMIT: f64 = 1.0e12;

/// Append the decimal digits of `n`.
pub fn write_u64(out: &mut Vec<u8>, n: u64) {
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    let mut v = n;
    loop {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    out.extend_from_slice(&buf[i..]);
}

/// Append a signed integer.
pub fn write_i64(out: &mut Vec<u8>, n: i64) {
    if n < 0 {
        out.push(b'-');
        write_u64(out, n.unsigned_abs());
    } else {
        write_u64(out, n as u64);
    }
}

/// Append a PDF real number. PDF reals forbid exponent notation, so this emits
/// plain fixed-point, rounded to 1e-6, with trailing zeros and a bare trailing
/// dot trimmed. Integer-valued reals print as integers (e.g. `612`, not
/// `612.0`). Non-finite input degrades to `0` (guarded, should not occur).
pub fn write_real(out: &mut Vec<u8>, x: f64) {
    if !x.is_finite() {
        out.push(b'0');
        return;
    }

    if x.abs() >= REAL_FIXED_LIMIT {
        // Out of the fixed-point range; emit a rounded integer.
        write_i64(out, x.round() as i64);
        return;
    }

    // Round to six decimal places in integer arithmetic.
    let neg = x.is_sign_negative();
    let scaled = (x.abs() * 1_000_000.0).round() as u64;
    if scaled == 0 {
        out.push(b'0');
        return;
    }
    if neg {
        out.push(b'-');
    }

    let int_part = scaled / 1_000_000;
    let mut frac = scaled % 1_000_000;
    write_u64(out, int_part);

    if frac != 0 {
        let mut digits = [0u8; 6];
        for slot in digits.iter_mut().rev() {
            *slot = b'0' + (frac % 10) as u8;
            frac /= 10;
        }
        let mut end = digits.len();
        while end > 0 && digits[end - 1] == b'0' {
            end -= 1;
        }
        out.push(b'.');
        out.extend_from_slice(&digits[..end]);
    }
}

/// Append a boolean literal.
pub fn write_bool(out: &mut Vec<u8>, b: bool) {
    out.extend_from_slice(if b { b"true" } else { b"false" });
}

/// Append a PDF name object, e.g. `/Type`. Bytes outside the regular range
/// (0x21..=0x7E) and the delimiter/whitespace characters are `#XX`-escaped per
/// PDF 32000-1 §7.3.5.
pub fn write_name(out: &mut Vec<u8>, name: &[u8]) {
    out.push(b'/');
    for &c in name {
        let regular = (0x21..=0x7e).contains(&c)
            && !matches!(
                c,
                b'#' | b'/' | b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'%'
            );
        if regular {
            out.push(c);
        } else {
            out.push(b'#');
            out.push(hex_digit(c >> 4));
            out.push(hex_digit(c & 0x0f));
        }
    }
}

/// Append a literal string `(...)`. Backslash, both parentheses, and the
/// non-printable control bytes are escaped; everything else is emitted raw
/// (PDF literal strings are byte strings). Escaping every parenthesis avoids
/// having to balance them.
pub fn write_literal_string(out: &mut Vec<u8>, s: &[u8]) {
    out.push(b'(');
    for &c in s {
        match c {
            b'\\' => out.extend_from_slice(b"\\\\"),
            b'(' => out.extend_from_slice(b"\\("),
            b')' => out.extend_from_slice(b"\\)"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            b'\t' => out.extend_from_slice(b"\\t"),
            0x08 => out.extend_from_slice(b"\\b"),
            0x0c => out.extend_from_slice(b"\\f"),
            c if c < 0x20 || c == 0x7f => {
                // Octal escape \ddd (always three digits to be unambiguous).
                out.push(b'\\');
                out.push(b'0' + ((c >> 6) & 0x07));
                out.push(b'0' + ((c >> 3) & 0x07));
                out.push(b'0' + (c & 0x07));
            }
            c => out.push(c),
        }
    }
    out.push(b')');
}

/// Append a hexadecimal string `<...>`. Used for the OCR text runs
/// (UTF-16BE code units) exactly as the current writer emits them.
pub fn write_hex_string(out: &mut Vec<u8>, bytes: &[u8]) {
    out.push(b'<');
    for &c in bytes {
        out.push(hex_digit(c >> 4));
        out.push(hex_digit(c & 0x0f));
    }
    out.push(b'>');
}

/// Append a PDF text string: ASCII as a compact literal, otherwise UTF-16BE
/// with a byte-order mark in a hexadecimal string.
pub fn write_text_string(out: &mut Vec<u8>, value: &str) {
    if value.is_ascii() {
        write_literal_string(out, value.as_bytes());
    } else {
        let mut bytes = Vec::with_capacity(2 + value.len() * 2);
        bytes.extend_from_slice(&[0xFE, 0xFF]);
        for unit in value.encode_utf16() {
            bytes.extend_from_slice(&unit.to_be_bytes());
        }
        write_hex_string(out, &bytes);
    }
}

/// Append an indirect reference, e.g. `12 0 R`.
pub fn write_ref(out: &mut Vec<u8>, id: ObjectId) {
    write_u64(out, id.num as u64);
    out.push(b' ');
    write_u64(out, id.generation as u64);
    out.extend_from_slice(b" R");
}

fn hex_digit(nibble: u8) -> u8 {
    match nibble {
        0..=9 => b'0' + nibble,
        _ => b'A' + (nibble - 10),
    }
}

/// A generic COS value for cold paths (catalog, outline items, metadata) where
/// a typed writer would be overkill. Streams and indirect objects are NOT
/// members — they are framed by the sink, not nested inside a value. Borrows so
/// callers can build one on the stack without owning heap copies.
#[derive(Clone, Debug)]
pub enum PdfValue<'a> {
    Null,
    Bool(bool),
    Integer(i64),
    Real(f64),
    Name(&'a [u8]),
    LiteralString(&'a [u8]),
    HexString(&'a [u8]),
    Reference(ObjectId),
    Array(&'a [PdfValue<'a>]),
    /// Inline dictionary as ordered key/value pairs.
    Dict(&'a [(&'a [u8], PdfValue<'a>)]),
}

/// Serialize a `PdfValue`.
pub fn write_value(out: &mut Vec<u8>, v: &PdfValue<'_>) {
    match v {
        PdfValue::Null => out.extend_from_slice(b"null"),
        PdfValue::Bool(b) => write_bool(out, *b),
        PdfValue::Integer(n) => write_i64(out, *n),
        PdfValue::Real(x) => write_real(out, *x),
        PdfValue::Name(n) => write_name(out, n),
        PdfValue::LiteralString(s) => write_literal_string(out, s),
        PdfValue::HexString(s) => write_hex_string(out, s),
        PdfValue::Reference(id) => write_ref(out, *id),
        PdfValue::Array(items) => write_array(out, items),
        PdfValue::Dict(entries) => write_dict(out, entries),
    }
}

/// Serialize an array `[ ... ]` (space-separated).
pub fn write_array(out: &mut Vec<u8>, items: &[PdfValue<'_>]) {
    out.push(b'[');
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push(b' ');
        }
        write_value(out, item);
    }
    out.push(b']');
}

/// Serialize a dictionary `<< /Key value ... >>`.
pub fn write_dict(out: &mut Vec<u8>, entries: &[(&[u8], PdfValue<'_>)]) {
    out.extend_from_slice(b"<<");
    for (key, val) in entries {
        write_name(out, key);
        out.push(b' ');
        write_value(out, val);
    }
    out.extend_from_slice(b">>");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(f: impl Fn(&mut Vec<u8>)) -> Vec<u8> {
        let mut v = Vec::new();
        f(&mut v);
        v
    }

    #[test]
    fn reals_are_trimmed_and_never_exponential() {
        assert_eq!(s(|o| write_real(o, 612.0)), b"612");
        assert_eq!(s(|o| write_real(o, 0.5)), b"0.5");
        assert_eq!(s(|o| write_real(o, -0.25)), b"-0.25");
        assert_eq!(s(|o| write_real(o, 0.0)), b"0");
        assert_eq!(s(|o| write_real(o, -0.0)), b"0");
        assert_eq!(s(|o| write_real(o, 1.234567)), b"1.234567");
        // rounds to 6 dp
        assert_eq!(s(|o| write_real(o, 1.2345674)), b"1.234567");
        // very small magnitude rounds to zero
        assert_eq!(s(|o| write_real(o, 0.0000004)), b"0");
        // no scientific notation for a large value
        let big = s(|o| write_real(o, 1_000_000.0));
        assert!(big.iter().all(|&c| c != b'e' && c != b'E'), "{:?}", big);
        // non-finite guarded
        assert_eq!(s(|o| write_real(o, f64::NAN)), b"0");
    }

    #[test]
    fn integers() {
        assert_eq!(s(|o| write_i64(o, 0)), b"0");
        assert_eq!(s(|o| write_i64(o, -42)), b"-42");
        assert_eq!(s(|o| write_i64(o, i64::MIN)), b"-9223372036854775808");
    }

    #[test]
    fn name_escaping() {
        assert_eq!(s(|o| write_name(o, b"Type")), b"/Type");
        // '#' and space and delimiters get #XX escaped
        assert_eq!(s(|o| write_name(o, b"A B")), b"/A#20B");
        assert_eq!(s(|o| write_name(o, b"a#b")), b"/a#23b");
        assert_eq!(s(|o| write_name(o, b"Im/1")), b"/Im#2F1");
    }

    #[test]
    fn literal_string_escaping() {
        assert_eq!(s(|o| write_literal_string(o, b"hi")), b"(hi)");
        assert_eq!(
            s(|o| write_literal_string(o, b"a(b)c\\d")),
            &b"(a\\(b\\)c\\\\d)"[..]
        );
        assert_eq!(s(|o| write_literal_string(o, b"\n")), b"(\\n)");
        assert_eq!(s(|o| write_literal_string(o, &[0x01])), b"(\\001)");
    }

    #[test]
    fn hex_string_of_utf16() {
        // "AB" as UTF-16BE => 00 41 00 42
        assert_eq!(
            s(|o| write_hex_string(o, &[0x00, 0x41, 0x00, 0x42])),
            b"<00410042>"
        );
    }

    #[test]
    fn reference_syntax() {
        assert_eq!(s(|o| write_ref(o, ObjectId::new(12))), b"12 0 R");
    }

    #[test]
    fn dict_and_array_nesting() {
        let arr = [PdfValue::Integer(0), PdfValue::Integer(1)];
        let out = s(|o| {
            write_dict(
                o,
                &[
                    (b"Type", PdfValue::Name(b"Page")),
                    (b"Decode", PdfValue::Array(&arr)),
                    (b"Parent", PdfValue::Reference(ObjectId::new(3))),
                ],
            )
        });
        assert_eq!(out, &b"<</Type /Page/Decode [0 1]/Parent 3 0 R>>"[..]);
    }
}
