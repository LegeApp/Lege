//! Object streams (`/Type /ObjStm`, ISO 32000-1 §7.5.7).
//!
//! The structure layer parses the *layout* — the `N` offset pairs before
//! `/First` — from already-decompressed stream data. Parsing individual
//! members happens at resolution time in `pdf-document`, which caches the
//! decompressed container bytes per worker so members share one inflate.

use pdf_syntax::{Lexer, SyntaxLimits, Token};

use crate::StructureError;

/// One member of an object stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObjStmMember {
    /// Object number the member claims (generation is always 0 in-stream).
    pub number: u32,
    /// Byte offset of the member, relative to the decompressed data start
    /// (i.e. `/First` already added).
    pub offset: u64,
}

/// The offset-pair table of an object stream.
#[derive(Debug, Clone)]
pub struct ObjectStreamLayout {
    pub members: Vec<ObjStmMember>,
}

impl ObjectStreamLayout {
    /// Member at `index`, per xref type-2 entries.
    pub fn member(&self, index: u32) -> Option<ObjStmMember> {
        self.members.get(index as usize).copied()
    }
}

/// Parse the pair table of a decompressed object stream.
///
/// Tolerances (ported from PDFium `CPDF_ObjectStream::Init`): a table
/// shorter than `/N` yields the members that exist; members with object
/// number 0 are dropped; offsets are taken as-is (bounds are the member
/// parser's problem — a lying offset fails that one member, not the
/// container).
pub fn parse_object_stream_layout(
    decoded: &[u8],
    n: i64,
    first: i64,
    limits: &SyntaxLimits,
) -> Result<ObjectStreamLayout, StructureError> {
    let (Ok(n), Ok(first)) = (u32::try_from(n), u64::try_from(first)) else {
        return Err(StructureError::LimitExceeded(
            "object stream /N or /First is negative",
        ));
    };
    // The pair table lives before /First; lex only that region so member
    // data can never be misread as table entries.
    let table_end = usize::try_from(first)
        .unwrap_or(decoded.len())
        .min(decoded.len());
    let mut lexer = Lexer::new(&decoded[..table_end], 0, limits);
    let mut members = Vec::with_capacity(n.min(65536) as usize);
    for _ in 0..n {
        let Ok(num_tok) = lexer.next_token() else {
            break;
        };
        let Token::Integer(number) = num_tok.value else {
            break;
        };
        let Ok(off_tok) = lexer.next_token() else {
            break;
        };
        let Token::Integer(offset) = off_tok.value else {
            break;
        };
        let (Ok(number), Ok(offset)) = (u32::try_from(number), u64::try_from(offset)) else {
            continue;
        };
        if number == 0 {
            continue;
        }
        members.push(ObjStmMember {
            number,
            offset: first + offset,
        });
    }
    Ok(ObjectStreamLayout { members })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn parses_pair_table() {
        // N=2, First=12: "11 0 12 5 " then member bodies.
        let data = b"11 0 12 5   AAAAABBBB";
        let limits = SyntaxLimits::default();
        let layout = parse_object_stream_layout(data, 2, 12, &limits).unwrap();
        assert_eq!(
            layout.members,
            vec![
                ObjStmMember {
                    number: 11,
                    offset: 12
                },
                ObjStmMember {
                    number: 12,
                    offset: 17
                },
            ]
        );
        assert_eq!(
            layout.member(1),
            Some(ObjStmMember {
                number: 12,
                offset: 17
            })
        );
        assert_eq!(layout.member(2), None);
    }

    #[test]
    fn short_table_yields_partial_members() {
        let limits = SyntaxLimits::default();
        let layout = parse_object_stream_layout(b"7 0", 3, 4, &limits).unwrap();
        assert_eq!(layout.members.len(), 1);
        assert_eq!(layout.members[0].number, 7);
    }

    #[test]
    fn zero_object_numbers_dropped() {
        let limits = SyntaxLimits::default();
        let layout = parse_object_stream_layout(b"0 0 9 3 ", 2, 8, &limits).unwrap();
        assert_eq!(layout.members.len(), 1);
        assert_eq!(layout.members[0].number, 9);
    }

    #[test]
    fn negative_n_rejected() {
        let limits = SyntaxLimits::default();
        assert!(parse_object_stream_layout(b"", -1, 0, &limits).is_err());
    }
}
