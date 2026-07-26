//! Fuzz the full open + page-0 compile path: `DocumentSnapshot::open`
//! followed by `PageCompiler::compile` under small `DocumentLimits`.
//! Every input must produce a `CompiledPage` or a typed error.

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
        max_decoded_bytes_per_context: 1 << 22, // 4 MiB
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
    let mut ctx = ParseContext::new();
    let _ = PageCompiler::new().compile(&snapshot, PageIndex(0), &mut ctx);
});
