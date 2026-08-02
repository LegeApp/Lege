//! Fuzz the entire pipeline: open → compile page 0 → CPU raster at 64×64.
//! The end-to-end never-panic invariant over arbitrary bytes.

#![no_main]

use std::sync::Arc;

use libfuzzer_sys::fuzz_target;
use pdf_content::PageCompiler;
use pdf_document::{DocumentLimits, DocumentSnapshot, PageIndex, ParseContext};
use pdf_page_ir::{DeviceSize, Matrix};
use pdf_render_api::{
    AnnotationMode, Background, OutputFormat, OutputResidency, PageTransform, RenderLimits,
    RenderColorPolicy, RenderQuality, RenderRequest,
};
use pdf_render_cpu::CpuBackend;
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
    let Ok(page) = PageCompiler::new().compile(&snapshot, PageIndex(0), &mut ctx) else {
        return;
    };

    let dim = 64u32;
    let crop = page.bounds.crop;
    let (w, h) = (crop.x1 - crop.x0, crop.y1 - crop.y0);
    let scale = if w > 0.0 && h > 0.0 { (dim as f64 / w).min(dim as f64 / h) } else { 1.0 };
    let matrix = Matrix {
        a: scale,
        b: 0.0,
        c: 0.0,
        d: -scale,
        e: -crop.x0 * scale,
        f: crop.y1 * scale,
    };
    let request = RenderRequest {
        page: Arc::new(page),
        transform: PageTransform { matrix },
        crop: None,
        output_size: DeviceSize { width: dim, height: dim },
        output_format: OutputFormat::Rgba8PremultipliedSrgb,
        background: Background::White,
        color_policy: RenderColorPolicy::default(),
        annotations: AnnotationMode::None,
        quality: RenderQuality::Normal,
        limits: RenderLimits {
            max_page_bytes: 1 << 24, // 16 MiB
            max_group_depth: 16,
            cancellation: None,
        },
        residency: OutputResidency::HostRequired,
    };
    let backend = CpuBackend::default();
    let _ = pdf_render_api::render_blocking(&backend, request);
});
