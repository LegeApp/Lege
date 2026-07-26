#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panics are the assertion mechanism

//! Phase 5 exit gate (roadmap §7 Phase 5): pages compile and render
//! concurrently through the two-stage pipeline, emit in page order, are
//! deterministic across worker counts, are cancellable, and one page's failure
//! never corrupts another's.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use pdf_document::{DocumentLimits, DocumentSnapshot, PageIndex, ParseContext};
use pdf_page_ir::{DeviceSize, Matrix};
use pdf_render_api::{
    AnnotationMode, BackendCapabilities, BackendId, Background, CancellationToken, HostPage,
    OutputFormat, OutputResidency, PageTransform, PostprocessCapabilities, RenderBackend,
    RenderError, RenderLimits, RenderQuality, RenderRequest, RenderTicket, RenderedPage,
    SubmitError, SupportLevel,
};
use pdf_render_scheduler::{PipelineOutput, RenderScheduler, SchedulerOptions};
use pdf_source::{OwnedBytesSource, PdfSource};
use pdf_test_support::builder;

/// A backend that renders synchronously (like the real CPU backend does inside
/// the scheduler's render workers), sleeps to force overlap, tracks the peak
/// concurrent render count, and echoes the request's output width so per-page
/// output identity is checkable.
#[derive(Debug, Default)]
struct MockBackend {
    concurrent: AtomicUsize,
    peak: AtomicUsize,
    sleep: Duration,
    /// Panic when the request output width equals this (isolation test).
    panic_width: Option<u32>,
    /// Requests that arrived carrying a cancellation token.
    saw_token: AtomicUsize,
}

struct ConcGuard<'a>(&'a AtomicUsize);
impl Drop for ConcGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl RenderBackend for MockBackend {
    fn id(&self) -> BackendId {
        BackendId::Other(1)
    }
    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            formats: vec![OutputFormat::Rgba8PremultipliedSrgb],
            max_surface: DeviceSize {
                width: 1 << 16,
                height: 1 << 16,
            },
            features: pdf_page_ir::PageFeatures::all(),
            resident_surfaces: false,
            postprocess: PostprocessCapabilities::NONE,
        }
    }
    fn supports(&self, _page: &pdf_page_ir::CompiledPage, _req: &RenderRequest) -> SupportLevel {
        SupportLevel::Native
    }
    fn submit(&self, request: RenderRequest) -> Result<RenderTicket, SubmitError> {
        if request.limits.cancellation.is_some() {
            self.saw_token.fetch_add(1, Ordering::SeqCst);
        }
        let (ticket, tx) = RenderTicket::new(0);
        let c = self.concurrent.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(c, Ordering::SeqCst);
        let _guard = ConcGuard(&self.concurrent); // decrements even on panic
        std::thread::sleep(self.sleep);
        let w = request.output_size.width;
        if self.panic_width == Some(w) {
            panic!("mock backend panic on width {w}");
        }
        let host = HostPage {
            width: w,
            height: 1,
            stride: w as usize * 4,
            format: OutputFormat::Rgba8PremultipliedSrgb,
            pixels: Arc::from(vec![0u8; w as usize * 4]),
        };
        let _ = tx.send(Ok(RenderedPage::Host(host)));
        Ok(ticket)
    }
}

fn snapshot(pages: u32) -> DocumentSnapshot {
    let source: Arc<dyn PdfSource> =
        Arc::new(OwnedBytesSource::new(builder::multipage_classic(pages)));
    DocumentSnapshot::open(source, DocumentLimits::default()).expect("open")
}

/// A request builder that compiles the page (real, concurrent) and encodes the
/// page index in the output width so results are page-identifiable. `fail_page`
/// makes one page's compilation return an error.
fn make_requester(
    compiles: Arc<AtomicUsize>,
    compile_peak: Arc<AtomicUsize>,
    fail_page: Option<u32>,
) -> impl Fn(&DocumentSnapshot, PageIndex) -> Result<RenderRequest, RenderError> + Sync {
    move |snapshot, page| {
        if fail_page == Some(page.0) {
            return Err(RenderError::Backend("intentional compile failure".into()));
        }
        let c = compiles.fetch_add(1, Ordering::SeqCst) + 1;
        compile_peak.fetch_max(c, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(2));
        let mut ctx = ParseContext::new();
        let compiled = pdf_content::PageCompiler::new()
            .compile(snapshot, page, &mut ctx)
            .map_err(|e| RenderError::Backend(e.to_string()))?;
        compiles.fetch_sub(1, Ordering::SeqCst);
        Ok(RenderRequest {
            page: Arc::new(compiled),
            transform: PageTransform {
                matrix: Matrix::IDENTITY,
            },
            crop: None,
            output_size: DeviceSize {
                width: 4 + page.0,
                height: 1,
            },
            output_format: OutputFormat::Rgba8PremultipliedSrgb,
            background: Background::White,
            annotations: AnnotationMode::None,
            color_policy: pdf_render_api::RenderColorPolicy::Original,
            quality: RenderQuality::Normal,
            limits: RenderLimits::default(),
            residency: OutputResidency::HostRequired,
        })
    }
}

fn collect(
    scheduler: &RenderScheduler,
    snap: &DocumentSnapshot,
    pages: u32,
    make: &(dyn Fn(&DocumentSnapshot, PageIndex) -> Result<RenderRequest, RenderError> + Sync),
    cancel: Option<&CancellationToken>,
) -> Vec<(u32, Result<u32, String>)> {
    let mut out = Vec::new();
    scheduler.render_range(snap, 0..pages, make, cancel, &mut |o: PipelineOutput| {
        let r = match o.result {
            Ok(RenderedPage::Host(h)) => Ok(h.width),
            Ok(_) => Ok(0),
            Err(e) => Err(e.to_string()),
        };
        out.push((o.page.0, r));
    });
    out
}

#[test]
fn six_pages_compile_and_render_concurrently_in_order() {
    let snap = snapshot(6);
    let backend = Arc::new(MockBackend {
        sleep: Duration::from_millis(15),
        ..Default::default()
    });
    let compiles = Arc::new(AtomicUsize::new(0));
    let compile_peak = Arc::new(AtomicUsize::new(0));
    let make = make_requester(compiles, compile_peak.clone(), None);
    let opts = SchedulerOptions {
        compile_workers: 6,
        render_workers: 6,
        compiled_queue_depth: 6,
        memory_limit_bytes: 1 << 30,
    };
    let scheduler = RenderScheduler::new(backend.clone(), opts);

    let out = collect(&scheduler, &snap, 6, &make, None);

    // Emitted in page order, one per page.
    let pages: Vec<u32> = out.iter().map(|(p, _)| *p).collect();
    assert_eq!(pages, vec![0, 1, 2, 3, 4, 5]);
    // Each output carries its page identity (width = 4 + page).
    for (p, r) in &out {
        assert_eq!(*r, Ok(4 + *p));
    }
    // Real concurrency was observed in both stages.
    assert!(
        backend.peak.load(Ordering::SeqCst) >= 2,
        "renders overlapped"
    );
    assert!(
        compile_peak.load(Ordering::SeqCst) >= 2,
        "compiles overlapped"
    );
}

#[test]
fn output_is_deterministic_across_worker_counts() {
    let snap = snapshot(8);
    let run = |cw: usize, rw: usize| {
        let backend = Arc::new(MockBackend {
            sleep: Duration::from_millis(1),
            ..Default::default()
        });
        let make = make_requester(
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicUsize::new(0)),
            None,
        );
        let opts = SchedulerOptions {
            compile_workers: cw,
            render_workers: rw,
            compiled_queue_depth: 4,
            memory_limit_bytes: 1 << 30,
        };
        collect(&RenderScheduler::new(backend, opts), &snap, 8, &make, None)
    };
    let single = run(1, 1);
    let many = run(6, 6);
    assert_eq!(single, many, "identical output regardless of worker counts");
    assert_eq!(single.len(), 8);
}

#[test]
fn one_page_failure_does_not_affect_others() {
    let snap = snapshot(5);
    let backend = Arc::new(MockBackend {
        sleep: Duration::from_millis(1),
        ..Default::default()
    });
    // Compilation fails for page 2 only.
    let make = make_requester(
        Arc::new(AtomicUsize::new(0)),
        Arc::new(AtomicUsize::new(0)),
        Some(2),
    );
    let opts = SchedulerOptions {
        compile_workers: 4,
        render_workers: 4,
        compiled_queue_depth: 4,
        memory_limit_bytes: 1 << 30,
    };
    let out = collect(&RenderScheduler::new(backend, opts), &snap, 5, &make, None);

    assert_eq!(out.len(), 5);
    assert!(out[2].1.is_err(), "page 2 failed");
    for p in [0u32, 1, 3, 4] {
        assert_eq!(out[p as usize].1, Ok(4 + p), "page {p} unaffected");
    }
}

#[test]
fn backend_panic_on_one_page_is_isolated() {
    let snap = snapshot(4);
    // The backend panics rendering page 1 (its output width is 4+1=5).
    let backend = Arc::new(MockBackend {
        sleep: Duration::from_millis(1),
        panic_width: Some(5),
        ..Default::default()
    });
    let make = make_requester(
        Arc::new(AtomicUsize::new(0)),
        Arc::new(AtomicUsize::new(0)),
        None,
    );
    let opts = SchedulerOptions {
        compile_workers: 4,
        render_workers: 4,
        compiled_queue_depth: 4,
        memory_limit_bytes: 1 << 30,
    };
    let out = collect(&RenderScheduler::new(backend, opts), &snap, 4, &make, None);

    assert_eq!(out.len(), 4);
    // The caught panic surfaces as the typed RenderError::Panic — payload
    // message preserved — matching the pdf-render-api boundary taxonomy.
    let err = out[1]
        .1
        .as_ref()
        .expect_err("page 1 render panicked → isolated error");
    assert!(
        err.contains("backend panicked") && err.contains("mock backend panic on width 5"),
        "expected RenderError::Panic with payload, got: {err}"
    );
    for p in [0u32, 2, 3] {
        assert_eq!(out[p as usize].1, Ok(4 + p), "page {p} still succeeded");
    }
}

#[test]
fn pipeline_token_is_injected_into_backend_requests() {
    // The scheduler must propagate its token into `limits.cancellation` so a
    // backend can observe cancellation *mid-render* (active jobs, not just
    // queued ones). A live token must not disturb normal output.
    let snap = snapshot(4);
    let backend = Arc::new(MockBackend {
        sleep: Duration::from_millis(1),
        ..Default::default()
    });
    let make = make_requester(
        Arc::new(AtomicUsize::new(0)),
        Arc::new(AtomicUsize::new(0)),
        None,
    );
    let opts = SchedulerOptions {
        compile_workers: 2,
        render_workers: 2,
        compiled_queue_depth: 2,
        memory_limit_bytes: 1 << 30,
    };
    let token = CancellationToken::new(); // live — never cancelled

    let out = collect(
        &RenderScheduler::new(backend.clone(), opts),
        &snap,
        4,
        &make,
        Some(&token),
    );

    assert_eq!(out.len(), 4);
    for (p, r) in &out {
        assert_eq!(*r, Ok(4 + *p));
    }
    assert_eq!(
        backend.saw_token.load(Ordering::SeqCst),
        4,
        "every request carried the pipeline token"
    );
}

#[test]
fn cancellation_stops_the_pipeline() {
    let snap = snapshot(6);
    let backend = Arc::new(MockBackend {
        sleep: Duration::from_millis(1),
        ..Default::default()
    });
    let make = make_requester(
        Arc::new(AtomicUsize::new(0)),
        Arc::new(AtomicUsize::new(0)),
        None,
    );
    let opts = SchedulerOptions {
        compile_workers: 2,
        render_workers: 2,
        compiled_queue_depth: 2,
        memory_limit_bytes: 1 << 30,
    };
    let token = CancellationToken::new();
    token.cancel(); // cancelled up front → every page short-circuits

    let out = collect(
        &RenderScheduler::new(backend, opts),
        &snap,
        6,
        &make,
        Some(&token),
    );

    assert_eq!(
        out.len(),
        6,
        "every page still produces a (cancelled) result in order"
    );
    for (_, r) in &out {
        assert!(
            matches!(r, Err(e) if e.contains("cancel")),
            "cancelled: {r:?}"
        );
    }
}
