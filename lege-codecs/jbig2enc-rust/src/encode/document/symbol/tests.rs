#![cfg(feature = "refine")]

use super::dictionary::{
    build_refinement_family_layout, canonicalize_dict_symbols, encode_symbol_dictionary_segments,
};
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
    // The symbol must be large enough (> 64 pixels ⇒ entry cost > 12 bytes) for
    // `family_should_refine`'s gain heuristic (gain > 12) to prefer refining the
    // duplicate over exporting it standalone; a tiny symbol never clears the
    // threshold. This 12×9 box is 108 pixels.
    let rows = [
        "011111111110",
        "100000000001",
        "101111111101",
        "101000000101",
        "101011110101",
        "101000000101",
        "101111111101",
        "100000000001",
        "011111111110",
    ];
    let base = symbol_from_rows(&rows);
    let variant = symbol_from_rows(&rows);
    let symbols = vec![&base, &variant];

    let mut config = Jbig2Config::text();
    config.refine = true;
    config.text_refine = false;

    // The dictionary-level refinement-family collapse policy lives in
    // `build_refinement_family_layout`. `plan_symbol_dictionary_layout` is a
    // deliberate stub that never collapses families (it has ignored `config`
    // since the code was first written — commit 4977e24 — because the encoder
    // performs refinement at the text-region instance level instead, not in the
    // dictionary; see the Phase 3 findings in jbig2dec-gaps-plan.md). This test
    // exercises the family-collapse policy directly, against the function that
    // actually implements it, so it does not depend on the stub and changes no
    // encoder output.
    let canonical_order = canonicalize_dict_symbols(&symbols);
    let layout = build_refinement_family_layout(&symbols, &canonical_order, None);
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
