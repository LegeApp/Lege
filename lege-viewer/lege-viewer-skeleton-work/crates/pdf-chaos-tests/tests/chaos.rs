#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panics are the assertion mechanism

//! The permanent never-panic regression gate (stable-toolchain chaos suite).
//!
//! Each small fixture PDF gets `MUTANTS_PER_FIXTURE` deterministic mutations
//! (fixed seeds, xorshift64* — see `pdf_chaos_tests::mutate`). Every mutant
//! must either complete open → compile → render (64×64) or return a typed
//! error. A panic anywhere in the pipeline fails this test with the fixture
//! name and seed needed to reproduce.
//!
//! This is the stable counterpart of the nightly-only `fuzz/` workspace: it
//! runs in plain `cargo test` on every machine, forever.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use pdf_chaos_tests::mutate;
use pdf_content::PageCompiler;
use pdf_document::{DocumentLimits, DocumentSnapshot, PageIndex, ParseContext};
use pdf_page_ir::{DeviceSize, Matrix};
use pdf_render_api::{
    AnnotationMode, Background, OutputFormat, OutputResidency, PageTransform, RenderLimits,
    RenderQuality, RenderRequest,
};
use pdf_render_cpu::CpuBackend;
use pdf_source::{OwnedBytesSource, PdfSource};
use pdf_test_support::builder;

/// Mutants per fixture. Total runtime target for the whole suite is well
/// under a minute; most mutants die cheaply at open.
const MUTANTS_PER_FIXTURE: u64 = 200;

/// Small DoS budgets: a mutant must not be able to buy a big allocation.
fn small_limits() -> DocumentLimits {
    DocumentLimits {
        max_reference_chain: 64,
        max_decoded_bytes_per_context: 1 << 22, // 4 MiB
        max_pages: 64,
        max_revisions: 32,
        max_objects: 1 << 16,
        ..DocumentLimits::default()
    }
}

/// Run the full pipeline over `bytes`. The return value is deliberately
/// ignored by callers: any typed outcome (success *or* error) satisfies the
/// invariant — only a panic is a failure.
fn pipeline(bytes: &[u8], backend: &CpuBackend) {
    let source: Arc<dyn PdfSource> = Arc::new(OwnedBytesSource::new(bytes.to_vec()));
    let Ok(snapshot) = DocumentSnapshot::open(source, small_limits()) else {
        return;
    };
    if snapshot.page_count() == 0 {
        return;
    }
    let compiler = PageCompiler::new();
    let mut text_ctx = ParseContext::new();
    if let Ok(semantic) = compiler.compile_semantic(&snapshot, PageIndex(0), &mut text_ctx) {
        let _ = pdf_text::TextPage::build(&semantic, &pdf_text::TextPageOptions::default());
    }
    let mut ctx = ParseContext::new();
    let Ok(page) = compiler.compile(&snapshot, PageIndex(0), &mut ctx) else {
        return;
    };

    let dim = 64u32;
    let crop = page.bounds.crop;
    let (w, h) = (crop.x1 - crop.x0, crop.y1 - crop.y0);
    let scale = if w > 0.0 && h > 0.0 { (dim as f64 / w).min(dim as f64 / h) } else { 1.0 };
    let matrix =
        Matrix { a: scale, b: 0.0, c: 0.0, d: -scale, e: -crop.x0 * scale, f: crop.y1 * scale };
    let request = RenderRequest {
        page: Arc::new(page),
        transform: PageTransform { matrix },
        crop: None,
        output_size: DeviceSize { width: dim, height: dim },
        output_format: OutputFormat::Rgba8PremultipliedSrgb,
        background: Background::White,
        annotations: AnnotationMode::None,
        quality: RenderQuality::Normal,
        limits: RenderLimits {
            max_page_bytes: 1 << 24, // 16 MiB
            max_group_depth: 16,
            cancellation: None,
        },
        residency: OutputResidency::HostRequired,
    };
    // render_blocking already converts backend panics to RenderError::Panic;
    // the catch_unwind in the caller is belt-and-braces for everything else.
    let _ = pdf_render_api::render_blocking(backend, request);
}

fn payload_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_owned()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_owned()
    }
}

/// The chaos gate proper, over one named fixture.
fn chaos_over(name: &str, fixture: &[u8], base_seed: u64) {
    // Silence the default panic hook while probing: a caught panic is the
    // *finding*, not console noise. Restored before asserting so genuine
    // test failures print normally. The hook is process-global, which is
    // why this whole suite runs as a single serial #[test].
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let backend = CpuBackend::default();
    let mut failures: Vec<String> = Vec::new();

    // Seed 0 slot: the pristine fixture itself must also not panic.
    if let Err(p) = catch_unwind(AssertUnwindSafe(|| pipeline(fixture, &backend))) {
        failures.push(format!("{name}: PRISTINE fixture panicked: {}", payload_message(p)));
    }
    for i in 0..MUTANTS_PER_FIXTURE {
        // Deterministic per-fixture, per-mutant seed.
        let seed = base_seed.wrapping_mul(1_000_003).wrapping_add(i + 1);
        let mutant = mutate(fixture, seed);
        if let Err(p) = catch_unwind(AssertUnwindSafe(|| pipeline(&mutant, &backend))) {
            failures.push(format!(
                "{name}: mutant seed={seed} (base_seed={base_seed}, index={i}, \
                 len={}) PANICKED: {}\n  reproduce: pdf_chaos_tests::mutate(fixture, {seed})",
                mutant.len(),
                payload_message(p),
            ));
        }
    }

    std::panic::set_hook(prev_hook);
    assert!(
        failures.is_empty(),
        "never-panic invariant violated by {} mutant(s):\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// Single serial test on purpose: it swaps the process-global panic hook.
#[test]
fn no_mutant_of_any_fixture_panics() {
    let fixtures: Vec<(&str, Vec<u8>)> = vec![
        ("hello_world.pdf", include_bytes!("fixtures/hello_world.pdf").to_vec()),
        ("classic_xref_2p", builder::phase1_exit_fixture(2)),
        ("xref_stream_2p", builder::xref_stream_fixture(2)),
        ("hybrid_xref_2p", builder::hybrid_fixture(2)),
        ("rc4_encrypted", builder::encrypted_fixture()),
        ("aes256_encrypted", builder::aes256_encrypted_fixture()),
    ];
    for (index, (name, bytes)) in fixtures.iter().enumerate() {
        chaos_over(name, bytes, 0xC0FFEE + index as u64);
    }
}
