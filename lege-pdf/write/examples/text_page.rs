//! M2 text-layer smoke driver: emit a one-page PDF with an invisible text
//! layer over a blank page, using the Helvetica fallback font (no embedded
//! program needed), so `pdftotext` can confirm the text extracts.
//! Usage: `text_page OUT.pdf WORD1 WORD2 ...`.

use std::fs::File;
use std::io::BufWriter;

use lege_pdf_write::artifact::{PdfPageArtifact, PreparedTextLayer, TextFont, TextRun};
use lege_pdf_write::types::PdfRect;
use lege_pdf_write::writer::DocumentWriter;

fn main() {
    let mut args = std::env::args().skip(1);
    let out = args.next().expect("usage: text_page OUT.pdf WORD...");
    let words: Vec<String> = args.collect();
    assert!(!words.is_empty(), "need at least one word");

    const W: f64 = 612.0;
    const H: f64 = 792.0;

    // Lay the words out on one baseline near the top of the page.
    let mut runs = Vec::new();
    let mut x = 72.0;
    let y = H - 72.0;
    for word in &words {
        runs.push(TextRun {
            text: word.clone(),
            x,
            y,
            size: 12.0,
        });
        x += 12.0 * 0.6 * (word.chars().count() as f64 + 1.0);
    }

    let art = PdfPageArtifact {
        index: 0,
        media_box: PdfRect::from_size(W, H),
        elements: Box::new([]),
        text_layer: Some(PreparedTextLayer {
            runs: runs.into_boxed_slice(),
            font: TextFont::HelveticaFallback,
        }),
    };

    let file = BufWriter::new(File::create(&out).expect("create output"));
    let mut writer = DocumentWriter::new(file, 1).expect("new writer");
    writer.add_page(&art).expect("add page");
    writer.finalize().expect("finalize");
    eprintln!("wrote {out}");
}
