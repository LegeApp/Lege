//! Minimal M1 smoke driver: wrap one or more JPEG files into an image-only PDF
//! via `DocumentWriter`. Usage: `wrap_jpeg OUT.pdf IN1.jpg [IN2.jpg ...]`.
//! Each JPEG becomes one full-page image at US-Letter size.

use std::fs::File;
use std::io::BufWriter;
use std::sync::Arc;

use lege_pdf_write::artifact::{ColorModel, PdfImageElement, PdfImageResource, PdfPageArtifact};
use lege_pdf_write::types::{Affine, PdfRect};
use lege_pdf_write::writer::DocumentWriter;

fn main() {
    let mut args = std::env::args().skip(1);
    let out = args.next().expect("usage: wrap_jpeg OUT.pdf IN1.jpg ...");
    let inputs: Vec<String> = args.collect();
    assert!(!inputs.is_empty(), "need at least one JPEG");

    const W: f64 = 612.0;
    const H: f64 = 792.0;

    let file = BufWriter::new(File::create(&out).expect("create output"));
    let mut writer = DocumentWriter::new(file, inputs.len()).expect("new writer");

    for (i, path) in inputs.iter().enumerate() {
        let bytes = std::fs::read(path).expect("read jpeg");
        // Dimensions are not needed for placement (the cm matrix drives it); use
        // nominal pixel dims. A real pipeline passes true pixel size.
        let art = PdfPageArtifact {
            index: i as u32,
            media_box: PdfRect::from_size(W, H),
            elements: Box::new([PdfImageElement {
                transform: Affine::scale_translate(W, H, 0.0, 0.0),
                image: PdfImageResource::Jpeg {
                    data: Arc::from(bytes.into_boxed_slice()),
                    width: 200,
                    height: 260,
                    color: ColorModel::Rgb,
                },
            }]),
            text_layer: None,
        };
        writer.add_page(&art).expect("add page");
    }

    writer.finalize().expect("finalize");
    eprintln!("wrote {out}");
}
