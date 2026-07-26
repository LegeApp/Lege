//! Fuzz open → semantic compile → text extraction under small limits.

#![no_main]

use std::sync::Arc;

use libfuzzer_sys::fuzz_target;
use pdf_content::PageCompiler;
use pdf_document::{DocumentLimits, DocumentSnapshot, PageIndex, ParseContext};
use pdf_source::{OwnedBytesSource, PdfSource};

fuzz_target!(|data: &[u8]| {
    let source: Arc<dyn PdfSource> = Arc::new(OwnedBytesSource::new(data.to_vec()));
    let limits = DocumentLimits {
        max_reference_chain: 64,
        max_decoded_bytes_per_context: 1 << 22,
        max_pages: 64,
        max_revisions: 32,
        max_objects: 1 << 16,
        ..DocumentLimits::default()
    };
    let Ok(snapshot) = DocumentSnapshot::open(source, limits) else {
        return;
    };
    if snapshot.page_count() == 0 {
        return;
    }
    let mut context = ParseContext::new();
    let Ok(page) = PageCompiler::new().compile_semantic(&snapshot, PageIndex(0), &mut context)
    else {
        return;
    };
    let text = pdf_text::TextPage::build(&page, &pdf_text::TextPageOptions::default());
    let _ = text.all_text_utf16();
    let _ = text.words();
    let _ = text.rects(0, text.char_count());
});
