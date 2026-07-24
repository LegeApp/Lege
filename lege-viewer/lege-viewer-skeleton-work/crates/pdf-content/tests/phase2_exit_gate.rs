#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panics are the assertion mechanism

//! Phase 2 exit gate (roadmap §7 Phase 2): supported pages compile
//! concurrently into deterministic semantic representations without invoking
//! any raster backend.
//!
//! "Without a raster backend" is structural here: `pdf-content` does not
//! depend on `pdf-render-api` or any backend crate (see its Cargo.toml), so a
//! semantic compile *cannot* reach one. What this gate proves is the other
//! half — that the semantic dump is identical across worker counts and across
//! repeated runs on one shared, immutable snapshot.

use std::sync::Arc;

use pdf_content::dump::dump_semantic;
use pdf_content::PageCompiler;
use pdf_document::{DocumentLimits, DocumentSnapshot, PageIndex, ParseContext};
use pdf_source::{OwnedBytesSource, PdfSource};
use pdf_test_support::builder::{self, PdfBuilder};

fn open(bytes: Vec<u8>) -> DocumentSnapshot {
    let source: Arc<dyn PdfSource> = Arc::new(OwnedBytesSource::new(bytes));
    DocumentSnapshot::open(source, DocumentLimits::default()).expect("open failed")
}

/// A document whose pages exercise the full Phase 2 operator surface —
/// transforms, colors, path fill/stroke, clipping, a shared Form XObject, an
/// inline image, and text — with per-page variation so a page mix-up would be
/// caught. All pages share one font and one form (concurrent resource
/// resolution across workers).
fn varied_fixture(page_count: u32) -> Vec<u8> {
    const CATALOG: u32 = 1;
    const PAGES: u32 = 2;
    const FONT: u32 = 3;
    const FORM: u32 = 4;
    let page = |i: u32| 10 + 2 * i;
    let content = |i: u32| 11 + 2 * i;

    let mut b = PdfBuilder::new();
    b.add_object(CATALOG, &format!("<</Type/Catalog/Pages {PAGES} 0 R>>"));
    let kids: Vec<String> = (0..page_count).map(|i| format!("{} 0 R", page(i))).collect();
    b.add_object(
        PAGES,
        &format!(
            "<</Type/Pages/Kids[{}]/Count {page_count}/MediaBox[0 0 612 792]>>",
            kids.join(" ")
        ),
    );
    b.add_object(FONT, "<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>");
    b.add_stream(
        FORM,
        "/Type/XObject/Subtype/Form/BBox[0 0 20 20]/Matrix[1 0 0 1 5 5]",
        b"0 0 1 rg 0 0 20 20 re f",
    );

    for i in 0..page_count {
        b.add_object(
            page(i),
            &format!(
                "<</Type/Page/Parent {PAGES} 0 R/Contents {} 0 R\
                 /Resources<</Font<</F1 {FONT} 0 R>>/XObject<</Fm {FORM} 0 R>>>>>>",
                content(i)
            ),
        );
        // Per-page variation: the translate, fill color, and text differ by i.
        let mut c = format!(
            "q 1 0 0 1 {i} {} cm 0.{} 0.2 0.3 rg 10 20 30 40 re f \
             1 w 0 0 1 RG 5 5 m 60 60 l S 0 0 100 100 re W n /Fm Do \
             q 8 0 0 8 0 0 cm BI /W 2 /H 2 /BPC 8 /CS /G /F /AHx ID 00112233 EI Q ",
            i * 2,
            (i % 9) + 1,
        );
        c.push_str(&format!("BT /F1 12 Tf 72 {} Td (Page {i}) Tj ET Q", 700 - i * 5));
        b.add_stream(content(i), "", c.as_bytes());
    }
    b.finish_classic_xref(&format!("/Root {CATALOG} 0 R"));
    b.into_bytes()
}

fn probe(snapshot: &DocumentSnapshot, index: u32) -> String {
    let mut ctx = ParseContext::new();
    let page = PageCompiler::new()
        .compile_semantic(snapshot, PageIndex(index), &mut ctx)
        .expect("compile failed");
    dump_semantic(&page, snapshot.names())
}

/// Compile every page with `threads` workers, each owning its own
/// `ParseContext`, one page at a time. Returns per-page dumps in page order.
fn probe_all(snapshot: &DocumentSnapshot, threads: usize) -> Vec<String> {
    let page_count = snapshot.page_count() as usize;
    let next = std::sync::atomic::AtomicUsize::new(0);
    let slots: Vec<std::sync::Mutex<Option<String>>> =
        (0..page_count).map(|_| std::sync::Mutex::new(None)).collect();
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| loop {
                let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if i >= page_count {
                    return;
                }
                *slots[i].lock().unwrap() = Some(probe(snapshot, i as u32));
            });
        }
    });
    slots.into_iter().map(|s| s.into_inner().unwrap().expect("page not probed")).collect()
}

#[test]
fn varied_pages_compile_deterministically_across_worker_counts() {
    let snapshot = open(varied_fixture(6));
    assert_eq!(snapshot.page_count(), 6);

    let baseline = probe_all(&snapshot, 1);

    // Sanity: the baseline actually contains the operator surface we expect,
    // and pages differ from one another.
    assert!(baseline[0].contains("show-text run#0"));
    assert!(baseline[0].contains("fill path#"));
    assert!(baseline[0].contains("stroke path#"));
    assert!(baseline[0].contains("clip path#"));
    assert!(baseline[0].contains("draw-image image#")); // the inline image
    assert_ne!(baseline[0], baseline[1], "pages must not be identical");

    for threads in [1usize, 2, 6] {
        for run in 0..3 {
            let probes = probe_all(&snapshot, threads);
            assert_eq!(probes, baseline, "threads={threads} run={run} diverged");
        }
    }
}

#[test]
fn concurrent_compilation_of_the_same_page_is_stable() {
    // Every worker compiles the SAME page — sharing the font and form
    // resolution — and must agree byte-for-byte.
    let snapshot = open(varied_fixture(1));
    let baseline = probe(&snapshot, 0);
    let results: Vec<String> = std::thread::scope(|scope| {
        let handles: Vec<_> =
            (0..8).map(|_| scope.spawn(|| probe(&snapshot, 0))).collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    for r in results {
        assert_eq!(r, baseline);
    }
}

#[test]
fn exit_gate_over_object_stream_fixture() {
    // The Phase 1 exit fixture reaches the shared font through an object
    // stream; compiling its pages concurrently exercises that path from the
    // content layer. Determinism must hold there too.
    let snapshot = open(builder::phase1_exit_fixture(6));
    let baseline = probe_all(&snapshot, 1);
    for threads in [2usize, 6] {
        for _ in 0..3 {
            assert_eq!(probe_all(&snapshot, threads), baseline, "threads={threads}");
        }
    }
    // Page 0 was rewritten by the incremental update.
    assert!(baseline[0].contains("Updated page 0"));
    assert!(baseline[1].contains("Page 1"));
}
