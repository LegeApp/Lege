//! Parallel page pipeline (roadmap §7 Phase 5):
//!
//! ```text
//! DocumentSnapshot → compile pool → bounded CompiledPage queue
//!                  → render pool  → reorder buffer → in-order output
//! ```
//!
//! Separate bounded stages so slow rendering doesn't block parsing, parsing
//! can't run unboundedly ahead, and memory stays bounded. Worker count
//! alone does NOT bound memory — jobs acquire [`MemoryBudget`] permits for
//! their estimated peak bytes before running.
//!
//! The pipeline internals are Phase 5; the *shape* (these types) is fixed
//! now, and the two genuinely load-bearing primitives — the budget and the
//! reorder buffer — are fully implemented and tested.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use pdf_document::{DocumentSnapshot, PageIndex};
use pdf_render_api::{CancellationToken, RenderBackend, RenderError, RenderRequest, RenderedPage};

mod budget;
mod reorder;

pub use budget::{MemoryBudget, MemoryPermit};
pub use reorder::ReorderBuffer;

/// Scheduler configuration.
#[derive(Debug, Clone)]
pub struct SchedulerOptions {
    /// Page-compilation workers.
    pub compile_workers: usize,
    /// Concurrent render jobs.
    pub render_workers: usize,
    /// Bound on compiled pages waiting for a render slot.
    pub compiled_queue_depth: usize,
    /// Maximum number of pages admitted to the pipeline but not yet emitted.
    ///
    /// This is an ordered sliding window, not a bounded result channel: the
    /// next expected page is always admitted, so a slow early page can still
    /// complete and release later completed surfaces from the reorder buffer.
    pub reorder_window_depth: usize,
    /// Total bytes all in-flight jobs may hold.
    pub memory_limit_bytes: u64,
}

impl Default for SchedulerOptions {
    fn default() -> Self {
        // One partitioned physical pool (performance advice §12): compile +
        // render workers sum to ≈ the core count rather than 2×cores. Rendering
        // is the heavier stage, so it gets the larger share.
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        let render_workers = (cores * 2 / 3).max(1);
        let compile_workers = cores.saturating_sub(render_workers).max(1);
        Self {
            compile_workers,
            render_workers,
            compiled_queue_depth: 12,
            reorder_window_depth: 12,
            memory_limit_bytes: 4 << 30,
        }
    }
}

/// A finished page emitted by the pipeline, tagged with its index so
/// downstream stages can rely on ordering.
#[derive(Debug)]
pub struct PipelineOutput {
    pub page: PageIndex,
    pub result: Result<RenderedPage, RenderError>,
    /// Time from pipeline start until this page finished rendering, captured
    /// before in-order delivery can hold it behind an earlier page.
    #[cfg(feature = "profiling")]
    pub completion_latency: std::time::Duration,
}

/// The page-render pipeline. Owns worker pools and stage queues.
#[derive(Debug)]
pub struct RenderScheduler {
    options: SchedulerOptions,
    backend: Arc<dyn RenderBackend>,
    budget: MemoryBudget,
}

impl RenderScheduler {
    pub fn new(backend: Arc<dyn RenderBackend>, options: SchedulerOptions) -> Self {
        let budget = MemoryBudget::new(options.memory_limit_bytes);
        Self {
            options,
            backend,
            budget,
        }
    }

    pub fn options(&self) -> &SchedulerOptions {
        &self.options
    }

    pub fn budget(&self) -> &MemoryBudget {
        &self.budget
    }

    /// Render a contiguous page range, delivering results **in page order** to
    /// `emit` while compiling and rendering out of order internally.
    ///
    /// The pipeline (roadmap §7 Phase 5): a compile-worker set turns pages into
    /// `RenderRequest`s and feeds a **bounded** compiled queue (backpressure —
    /// parsing cannot run unboundedly ahead of rendering); a render-worker set
    /// drains it, reserving a [`MemoryBudget`] permit per job so worker count
    /// alone does not bound memory; a reorder buffer emits in page order.
    ///
    /// Guarantees: deterministic output for any worker counts (each page is
    /// independent — its own parse/render context, the snapshot is immutable,
    /// no document-wide lock); `cancel` stops queued and active jobs (it is
    /// checked before each stage *and* injected into each request's
    /// `limits.cancellation` so backends can stop mid-render); a panic or
    /// error on one page becomes that page's `Err` and never corrupts
    /// another page's job.
    pub fn render_range(
        &self,
        snapshot: &DocumentSnapshot,
        pages: std::ops::Range<u32>,
        make_request: &(
             dyn Fn(&DocumentSnapshot, PageIndex) -> Result<RenderRequest, RenderError> + Sync
         ),
        cancel: Option<&CancellationToken>,
        emit: &mut dyn FnMut(PipelineOutput),
    ) {
        use crossbeam_channel::{bounded, unbounded};

        let start_seq = pages.start as u64;
        let cancelled = || cancel.is_some_and(|t| t.is_cancelled());
        #[cfg(feature = "profiling")]
        let pipeline_start = std::time::Instant::now();

        // Stage 0: page indices to compile. Keep this bounded too: eagerly
        // enqueueing a caller-controlled `u32` range before any worker starts
        // can otherwise consume gigabytes before the actual pipeline runs.
        let (page_tx, page_rx) = bounded::<u32>(self.options.compiled_queue_depth.max(1));

        // Stage 1→2: bounded compiled queue (bounds compiled-page memory).
        type Compiled = (u32, Result<RenderRequest, RenderError>);
        let (compiled_tx, compiled_rx) =
            bounded::<Compiled>(self.options.compiled_queue_depth.max(1));
        // Stage 2→collector.
        #[cfg(feature = "profiling")]
        type PipelineResult = (u32, Result<RenderedPage, RenderError>, std::time::Duration);
        #[cfg(not(feature = "profiling"))]
        type PipelineResult = (u32, Result<RenderedPage, RenderError>);
        let (result_tx, result_rx) = unbounded::<PipelineResult>();
        // Advancing this channel means one in-order output was emitted. The
        // feeder uses it to extend the admission window. It only carries
        // zero-sized completion credits, never rendered surfaces.
        let (emitted_tx, emitted_rx) = unbounded::<()>();
        let reorder_window = self.options.reorder_window_depth.max(1) as u64;

        std::thread::scope(|scope| {
            // Admit an ordered sliding window. This bounds *all* pages that
            // can reach the reorder buffer while reserving its first slot for
            // the page the collector is waiting for. Bounding `result_tx`
            // instead can deadlock when later workers fill it before that
            // early page finishes.
            scope.spawn(move || {
                let mut pages = pages;
                let mut admitted = 0_u64;
                let mut emitted = 0_u64;
                loop {
                    let limit = emitted.saturating_add(reorder_window);
                    while admitted < limit {
                        let Some(n) = pages.next() else {
                            return;
                        };
                        if page_tx.send(n).is_err() {
                            return;
                        }
                        admitted += 1;
                    }
                    match emitted_rx.recv() {
                        Ok(()) => emitted += 1,
                        Err(_) => return,
                    }
                }
            });

            // Compile workers.
            for _ in 0..self.options.compile_workers.max(1) {
                let page_rx = page_rx.clone();
                let compiled_tx = compiled_tx.clone();
                scope.spawn(move || {
                    while let Ok(n) = page_rx.recv() {
                        let item = if cancelled() {
                            Err(RenderError::Cancelled)
                        } else {
                            match catch_unwind(AssertUnwindSafe(|| {
                                make_request(snapshot, PageIndex(n))
                            })) {
                                Ok(r) => r,
                                Err(_) => Err(RenderError::Backend(format!(
                                    "compile panicked on page {n}"
                                ))),
                            }
                        };
                        if compiled_tx.send((n, item)).is_err() {
                            break;
                        }
                    }
                });
            }
            drop(compiled_tx);
            drop(page_rx);

            // Render workers.
            for _ in 0..self.options.render_workers.max(1) {
                let compiled_rx = compiled_rx.clone();
                let result_tx = result_tx.clone();
                let budget = self.budget.clone();
                let backend = self.backend.clone();
                scope.spawn(move || {
                    while let Ok((n, item)) = compiled_rx.recv() {
                        let result = match item {
                            Err(e) => Err(e),
                            Ok(request) if cancelled() => {
                                drop(request);
                                Err(RenderError::Cancelled)
                            }
                            Ok(mut request) => {
                                // Propagate the pipeline token into the request so the
                                // backend can observe cancellation *mid-render* (backends
                                // check `limits.cancellation` at operation boundaries).
                                // A token the caller already set wins.
                                if request.limits.cancellation.is_none() {
                                    request.limits.cancellation = cancel.cloned();
                                }
                                render_job(backend.as_ref(), &budget, request)
                            }
                        };
                        #[cfg(feature = "profiling")]
                        let sent = result_tx.send((n, result, pipeline_start.elapsed()));
                        #[cfg(not(feature = "profiling"))]
                        let sent = result_tx.send((n, result));
                        if sent.is_err() {
                            break;
                        }
                    }
                });
            }
            drop(result_tx);
            drop(compiled_rx);

            // Collector (this thread): reorder → in-order emit.
            let mut reorder = ReorderBuffer::new(start_seq);
            while let Ok(item) = result_rx.recv() {
                #[cfg(feature = "profiling")]
                let (n, result, completion_latency) = item;
                #[cfg(not(feature = "profiling"))]
                let (n, result) = item;
                reorder.insert(
                    n as u64,
                    PipelineOutput {
                        page: PageIndex(n),
                        result,
                        #[cfg(feature = "profiling")]
                        completion_latency,
                    },
                );
                for (_, out) in reorder.drain_ready() {
                    emit(out);
                    let _ = emitted_tx.send(());
                }
            }
            for (_, out) in reorder.drain_ready() {
                emit(out);
                let _ = emitted_tx.send(());
            }
        });
    }

    /// Run the existing pipeline while collecting operation-level throughput
    /// attribution. Per-page compilation/render reports remain owned by the
    /// caller's profiled request builder/backend; this method records the
    /// pipeline envelope without introducing shared counters into hot workers.
    #[cfg(feature = "profiling")]
    pub fn render_range_profiled(
        &self,
        snapshot: &DocumentSnapshot,
        pages: std::ops::Range<u32>,
        make_request: &(
             dyn Fn(&DocumentSnapshot, PageIndex) -> Result<RenderRequest, RenderError> + Sync
         ),
        cancel: Option<&CancellationToken>,
        emit: &mut dyn FnMut(PipelineOutput),
    ) -> pdf_profiling::ProfileReport {
        let requested = pages.end.saturating_sub(pages.start) as u64;
        let start = std::time::Instant::now();
        let mut completed = 0u64;
        let mut succeeded = 0u64;
        let mut completion_ns = Vec::with_capacity(requested as usize);
        self.render_range(snapshot, pages, make_request, cancel, &mut |out| {
            completed += 1;
            if out.result.is_ok() {
                succeeded += 1;
            }
            completion_ns.push(out.completion_latency.as_nanos() as u64);
            emit(out);
        });
        completion_ns.sort_unstable();
        let mut profile = pdf_profiling::ProfileReport::new();
        profile.add_duration("scheduler.total", start.elapsed());
        profile.increment("scheduler.requested_pages", requested);
        profile.increment("scheduler.completed_pages", completed);
        profile.increment("scheduler.succeeded_pages", succeeded);
        profile.increment(
            "scheduler.compile_workers",
            self.options.compile_workers as u64,
        );
        profile.increment(
            "scheduler.render_workers",
            self.options.render_workers as u64,
        );
        profile.increment(
            "scheduler.memory_limit_bytes",
            self.options.memory_limit_bytes,
        );
        if !completion_ns.is_empty() {
            profile.increment(
                "scheduler.page_latency_p50_ns",
                percentile(&completion_ns, 0.50),
            );
            profile.increment(
                "scheduler.page_latency_p90_ns",
                percentile(&completion_ns, 0.90),
            );
            profile.increment(
                "scheduler.page_latency_p99_ns",
                percentile(&completion_ns, 0.99),
            );
        }
        profile
    }
}

#[cfg(feature = "profiling")]
fn percentile(sorted: &[u64], quantile: f64) -> u64 {
    let index = ((sorted.len().saturating_sub(1)) as f64 * quantile).ceil() as usize;
    sorted[index.min(sorted.len() - 1)]
}

/// Reserve budget for a job's estimated peak, render it, and release on
/// return. A panic in the backend becomes an isolated `Err` for this page.
fn render_job(
    backend: &dyn RenderBackend,
    budget: &MemoryBudget,
    request: RenderRequest,
) -> Result<RenderedPage, RenderError> {
    let estimate = estimate_job_bytes(&request);
    let _permit = budget
        .acquire(estimate)
        .map_err(|_| RenderError::LimitExceeded("memory budget"))?;
    match catch_unwind(AssertUnwindSafe(|| {
        let ticket = backend
            .submit(request)
            .map_err(|e| RenderError::Backend(e.to_string()))?;
        ticket.wait()
    })) {
        Ok(result) => result,
        // Same typed panic taxonomy as the API boundary
        // (`pdf_render_api::render_blocking`): the payload message survives.
        Err(payload) => Err(RenderError::Panic {
            message: pdf_render_api::panic_message(payload),
        }),
    }
}

/// Peak-byte estimate for budgeting. Refined in Phase 5 using
/// `PageComplexity`; the output surface is the floor.
pub fn estimate_job_bytes(request: &RenderRequest) -> u64 {
    let surface = (request.output_size.width as u64)
        .saturating_mul(request.output_size.height as u64)
        .saturating_mul(request.output_format.bytes_per_pixel() as u64);
    surface.saturating_add(request.page.complexity.estimated_peak_bytes)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    const fn assert_send_sync<T: Send + Sync>() {}
    const _: () = assert_send_sync::<RenderScheduler>();
}
