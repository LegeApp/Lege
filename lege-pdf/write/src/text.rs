//! Invisible OCR text layer: consumes PreparedTextLayer (PDF-space runs),
//! emits BT/Tr 3/Tf/Tm/Tj into the page ContentWriter. No hOCR/XML here —
//! parsing and positioning stay in Lege.
//!
//! Runs arrive already positioned in PDF user space (bottom-left origin); this
//! module only encodes and emits. The text render mode is 3 (invisible), the
//! font is selected at size 1.0, and each run's own `size` drives the text
//! matrix scale — matching the current writer's `emit_invisible_text`.

use crate::artifact::{PreparedTextLayer, TextFont};
use crate::content::ContentWriter;
use crate::types::Affine;

/// Emit the whole text layer into `content`. A no-op if there are no runs.
pub fn emit_text_layer(content: &mut ContentWriter, layer: &PreparedTextLayer) {
    if layer.runs.is_empty() {
        return;
    }

    content.begin_text();
    content.set_text_render_mode(3); // invisible
    content.set_char_spacing(0.0);
    content.set_word_spacing(0.0);
    content.set_horizontal_scale(100.0);
    content.set_font(layer.font.resource_name(), 1.0);

    for run in layer.runs.iter() {
        if run.text.is_empty() {
            continue;
        }
        content.set_text_matrix(Affine::scale_translate(run.size, run.size, run.x, run.y));
        let encoded = encode(&run.text, layer.font);
        content.show_text_hex(&encoded);
    }

    content.end_text();
}

/// Encode run text for the layer's font. The embedded Type0/Identity-H font
/// takes UTF-16BE code units (CID = code unit); the Helvetica fallback takes a
/// single-byte encoding.
fn encode(text: &str, font: TextFont) -> Vec<u8> {
    match font {
        TextFont::Embedded => {
            let mut bytes = Vec::with_capacity(text.len() * 2);
            for u in text.encode_utf16() {
                bytes.extend_from_slice(&u.to_be_bytes());
            }
            bytes
        }
        TextFont::HelveticaFallback => {
            // WINDOWS-1252 is Latin-1 for the printable range Lege actually
            // emits on this near-dead path; codepoints above U+00FF become '?'.
            // (The glyphless embedded font is always available in Lege, so this
            // branch is effectively unused; exact 0x80–0x9F parity is deferred.)
            text.chars()
                .map(|c| {
                    let v = c as u32;
                    if v <= 0xFF { v as u8 } else { b'?' }
                })
                .collect()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifact::TextRun;

    fn layer(font: TextFont, runs: Vec<TextRun>) -> PreparedTextLayer {
        PreparedTextLayer {
            runs: runs.into_boxed_slice(),
            font,
        }
    }

    #[test]
    fn empty_layer_emits_nothing() {
        let mut c = ContentWriter::new();
        emit_text_layer(&mut c, &layer(TextFont::Embedded, vec![]));
        assert!(c.is_empty());
    }

    #[test]
    fn embedded_run_is_utf16be_hex() {
        let mut c = ContentWriter::new();
        emit_text_layer(
            &mut c,
            &layer(
                TextFont::Embedded,
                vec![TextRun {
                    text: "AB".to_string(),
                    x: 72.0,
                    y: 700.0,
                    size: 12.0,
                }],
            ),
        );
        let t = String::from_utf8_lossy(c.as_slice()).into_owned();
        assert!(
            t.starts_with("BT\n3 Tr\n0 Tc\n0 Tw\n100 Tz\n/F0 1 Tf\n"),
            "{t}"
        );
        assert!(t.contains("12 0 0 12 72 700 Tm\n"));
        assert!(t.contains("<00410042> Tj\n"), "{t}");
        assert!(t.trim_end().ends_with("ET"));
    }

    #[test]
    fn non_bmp_encodes_as_surrogate_pair() {
        // U+1F600 GRINNING FACE => surrogate pair D83D DE00.
        let mut c = ContentWriter::new();
        emit_text_layer(
            &mut c,
            &layer(
                TextFont::Embedded,
                vec![TextRun {
                    text: "😀".to_string(),
                    x: 0.0,
                    y: 0.0,
                    size: 10.0,
                }],
            ),
        );
        let t = String::from_utf8_lossy(c.as_slice()).into_owned();
        assert!(t.contains("<D83DDE00> Tj\n"), "{t}");
    }

    #[test]
    fn fallback_uses_single_bytes() {
        let mut c = ContentWriter::new();
        emit_text_layer(
            &mut c,
            &layer(
                TextFont::HelveticaFallback,
                vec![TextRun {
                    text: "Aé".to_string(),
                    x: 0.0,
                    y: 0.0,
                    size: 10.0,
                }],
            ),
        );
        let t = String::from_utf8_lossy(c.as_slice()).into_owned();
        // 'A' = 0x41, 'é' = 0xE9
        assert!(t.contains("/F1 1 Tf\n"), "{t}");
        assert!(t.contains("<41E9> Tj\n"), "{t}");
    }
}
