#![cfg(feature = "refine")]

use super::dictionary::{encode_symbol_dictionary_segments, plan_symbol_dictionary_layout};
use crate::jbig2structs::Jbig2Config;
use crate::jbig2sym::BitImage;

fn symbol_from_rows(rows: &[&str]) -> BitImage {
    let height = rows.len() as u32;
    let width = rows.first().map_or(0, |row| row.len()) as u32;
    let mut image = BitImage::new(width, height).expect("test bitmap");
    for (y, row) in rows.iter().enumerate() {
        for (x, ch) in row.bytes().enumerate() {
            if ch == b'1' {
                image.set(x as u32, y as u32, true);
            }
        }
    }
    image
}

#[cfg(feature = "refine")]
#[test]
fn refinement_layout_collapses_to_prototypes() {
    let base = symbol_from_rows(&["0110", "1001", "1111", "1001", "1001"]);
    let variant = symbol_from_rows(&["0110", "1001", "1111", "1001", "1001"]);
    let symbols = vec![&base, &variant];

    let mut config = Jbig2Config::text();
    config.refine = true;
    config.text_refine = false;

    let layout = plan_symbol_dictionary_layout(&symbols, &config, None).expect("layout");
    assert_eq!(layout.segment_count(), 1);
    assert_eq!(layout.export_input_indices.len(), 1);
    assert!(layout.refinements[1].is_some());

    let encoded = encode_symbol_dictionary_segments(&symbols, &config, &layout).expect("encode");
    assert_eq!(encoded.exported_symbol_count, 1);
    assert!(
        encoded
            .input_to_exported_pos
            .iter()
            .all(|&pos| pos != u32::MAX)
    );
}
