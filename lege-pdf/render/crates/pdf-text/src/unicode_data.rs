//! PDFium-compatible BMP bidirectional and normalization data.
//!
//! The large arrays are generated at compile time from byte-exact copied
//! PDFium inputs. The compact algorithms here are the only hand-written part.

mod generated {
    include!(concat!(env!("OUT_DIR"), "/pdfium_unicode_tables.rs"));
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Direction {
    Neutral,
    Left,
    Right,
    LeftWeak,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Segment {
    pub start: usize,
    pub count: usize,
    pub direction: Direction,
}

pub(crate) fn direction(unit: u16) -> Direction {
    match generated::BIDI_CLASS[usize::from(unit)] {
        1 => Direction::Left,
        2 | 5 => Direction::Right,
        3 | 4 | 6..=10 => Direction::LeftWeak,
        _ => Direction::Neutral,
    }
}

pub(crate) fn mirror(unit: u16) -> u16 {
    let index = generated::MIRROR_INDEX[usize::from(unit)];
    if index == 0x1ff {
        unit
    } else {
        generated::BIDI_MIRRORS[usize::from(index)]
    }
}

/// The deliberately small segmenter used by PDFium, not the full UAX #9
/// algorithm. `force_rtl` models `CPDF_TextPage`'s RTL constructor argument.
pub(crate) fn segments(units: &[u16], force_rtl: bool) -> (Direction, Vec<Segment>) {
    let mut result = Vec::new();
    let mut current = Direction::Neutral;
    let mut start = 0usize;
    for (index, &unit) in units.iter().enumerate() {
        let next = direction(unit);
        if next != current {
            if index > start {
                result.push(Segment {
                    start,
                    count: index - start,
                    direction: current,
                });
            }
            start = index;
            current = next;
        }
    }
    if start < units.len() {
        result.push(Segment {
            start,
            count: units.len() - start,
            direction: current,
        });
    }

    let right = result
        .iter()
        .filter(|segment| segment.direction == Direction::Right)
        .count();
    let left = result
        .iter()
        .filter(|segment| segment.direction == Direction::Left)
        .count();
    let overall = if force_rtl || (right > 0 && right >= left) {
        result.reverse();
        Direction::Right
    } else {
        Direction::Left
    };
    (overall, result)
}

/// Return PDFium's compatibility mapping for one UTF-16 code unit.
pub(crate) fn normalize(unit: u16) -> Vec<u16> {
    let found = generated::UNICODE_NORMALIZATION[usize::from(unit)];
    if found == 0 {
        return vec![unit];
    }
    if found >= 0x8000 {
        return vec![generated::UNICODE_NORMALIZATION_MAP1[usize::from(found - 0x8000)]];
    }

    let offset = usize::from(found & 0x0fff);
    match found >> 12 {
        2 => generated::UNICODE_NORMALIZATION_MAP2[offset..offset + 2].to_vec(),
        3 => generated::UNICODE_NORMALIZATION_MAP3[offset..offset + 3].to_vec(),
        4 => {
            let count = usize::from(generated::UNICODE_NORMALIZATION_MAP4[offset]);
            generated::UNICODE_NORMALIZATION_MAP4[offset + 1..offset + 1 + count].to_vec()
        }
        _ => vec![unit],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copied_tables_match_known_pdfium_entries() {
        assert_eq!(direction(b'A' as u16), Direction::Left);
        assert_eq!(direction(0x05d0), Direction::Right);
        assert_eq!(mirror(b'(' as u16), b')' as u16);
        assert_eq!(mirror(b'A' as u16), b'A' as u16);
    }

    #[test]
    fn normalization_decomposes_ligatures() {
        assert_eq!(normalize(0xfb00), [b'f' as u16, b'f' as u16]);
        assert_eq!(normalize(0xfb03), [b'f' as u16, b'f' as u16, b'i' as u16]);
        assert_eq!(normalize(b'A' as u16), [b'A' as u16]);
    }

    #[test]
    fn pdfium_segment_order_is_preserved() {
        let (overall, line) = segments(&[b'A' as u16, 0x05d0], false);
        assert_eq!(overall, Direction::Right);
        assert_eq!(line[0].start, 1);
        assert_eq!(line[1].start, 0);
    }
}
