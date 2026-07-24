//! Opt-in runtime instrumentation for Phase 0 performance baselines.
//!
//! Nothing is allocated and no sampler thread is started unless the CLI calls
//! [`enable`]. The final report is a single JSON object on stderr so GUI worker
//! stdout remains newline-delimited progress JSON.

use serde::Serialize;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const SAMPLE_INTERVAL: Duration = Duration::from_millis(250);
pub const MAX_BLOCKING_THREADS: usize = 4;

/// Build the small Tokio control runtime used by synchronous CLI entry points.
/// CPU work belongs on Rayon; Tokio owns channels, timers, cancellation and
/// true blocking I/O only.
pub fn build_control_runtime() -> std::io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread()
        .max_blocking_threads(MAX_BLOCKING_THREADS)
        .enable_all()
        .build()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum Stage {
    Render,
    Inference,
    Processing,
    Encode,
    Ocr,
    Writer,
}

const STAGES: [Stage; 6] = [
    Stage::Render,
    Stage::Inference,
    Stage::Processing,
    Stage::Encode,
    Stage::Ocr,
    Stage::Writer,
];

impl Stage {
    const fn name(self) -> &'static str {
        match self {
            Self::Render => "render",
            Self::Inference => "inference",
            Self::Processing => "processing",
            Self::Encode => "encode",
            Self::Ocr => "ocr",
            Self::Writer => "writer",
        }
    }
}

#[derive(Default)]
struct ActiveTime {
    started: Option<Instant>,
    elapsed: Duration,
}

#[derive(Default)]
struct Gauge {
    current: AtomicUsize,
    peak: AtomicUsize,
    entries: AtomicUsize,
    total_job_nanos: Mutex<u128>,
    active: Mutex<ActiveTime>,
}

impl Gauge {
    fn enter(&self) -> Instant {
        self.entries.fetch_add(1, Ordering::Relaxed);
        let current = self.current.fetch_add(1, Ordering::AcqRel) + 1;
        self.peak.fetch_max(current, Ordering::Relaxed);
        if current == 1 {
            let mut active = self.active.lock().unwrap_or_else(|e| e.into_inner());
            active.started = Some(Instant::now());
        }
        Instant::now()
    }

    fn exit(&self, job_started: Instant) {
        let elapsed = job_started.elapsed().as_nanos();
        let mut total = self
            .total_job_nanos
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *total = total.saturating_add(elapsed);
        drop(total);

        let previous = self.current.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0, "runtime stats gauge underflow");
        if previous == 1 {
            let mut active = self.active.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(started) = active.started.take() {
                active.elapsed = active.elapsed.saturating_add(started.elapsed());
            }
        }
    }

    fn snapshot(&self, now: Instant) -> GaugeSnapshot {
        let active = self.active.lock().unwrap_or_else(|e| e.into_inner());
        let active_elapsed = active.elapsed.saturating_add(
            active
                .started
                .map(|started| now.saturating_duration_since(started))
                .unwrap_or_default(),
        );
        let total_job_nanos = *self
            .total_job_nanos
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let current = self.current.load(Ordering::Relaxed);
        let entries = self.entries.load(Ordering::Relaxed);
        GaugeSnapshot {
            current,
            peak: self.peak.load(Ordering::Relaxed),
            entries,
            exits: entries.saturating_sub(current),
            active_wall_seconds: active_elapsed.as_secs_f64(),
            summed_job_seconds: total_job_nanos as f64 / 1_000_000_000.0,
        }
    }
}

struct Collector {
    started: Instant,
    stop: AtomicBool,
    dumped: AtomicBool,
    peak_threads: AtomicUsize,
    peak_rss_kib: AtomicUsize,
    stages: [Gauge; STAGES.len()],
    blocking: Gauge,
    samples: Mutex<Vec<RuntimeSample>>,
    sampler: Mutex<Option<JoinHandle<()>>>,
}

impl Collector {
    fn new() -> Self {
        Self {
            started: Instant::now(),
            stop: AtomicBool::new(false),
            dumped: AtomicBool::new(false),
            peak_threads: AtomicUsize::new(0),
            peak_rss_kib: AtomicUsize::new(0),
            stages: std::array::from_fn(|_| Gauge::default()),
            blocking: Gauge::default(),
            samples: Mutex::new(Vec::new()),
            sampler: Mutex::new(None),
        }
    }

    fn sample(&self) {
        let status = read_process_status();
        self.peak_threads
            .fetch_max(status.threads, Ordering::Relaxed);
        self.peak_rss_kib
            .fetch_max(status.peak_rss_kib, Ordering::Relaxed);
        let stages = STAGES
            .iter()
            .map(|stage| StageCurrent {
                name: stage.name(),
                in_flight: self.stages[*stage as usize].current.load(Ordering::Relaxed),
            })
            .collect();
        self.samples
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(RuntimeSample {
                elapsed_seconds: self.started.elapsed().as_secs_f64(),
                threads: status.threads,
                rss_kib: status.rss_kib,
                blocking_in_flight: self.blocking.current.load(Ordering::Relaxed),
                stages,
            });
    }
}

static COLLECTOR: OnceLock<Arc<Collector>> = OnceLock::new();

/// Enable collection. Calling this more than once is harmless.
pub fn enable() {
    let collector = COLLECTOR.get_or_init(|| Arc::new(Collector::new())).clone();
    let mut sampler = collector.sampler.lock().unwrap_or_else(|e| e.into_inner());
    if sampler.is_some() {
        return;
    }
    collector.sample();
    let sampled = collector.clone();
    *sampler = std::thread::Builder::new()
        .name("lege-runtime-stats".to_string())
        .spawn(move || {
            while !sampled.stop.load(Ordering::Acquire) {
                std::thread::sleep(SAMPLE_INTERVAL);
                if sampled.stop.load(Ordering::Acquire) {
                    break;
                }
                sampled.sample();
            }
        })
        .ok();
}

pub fn enabled() -> bool {
    COLLECTOR.get().is_some()
}

/// Mark one unit of stage work as in flight until the returned guard drops.
pub fn enter_stage(stage: Stage) -> StageGuard {
    let collector = COLLECTOR.get().cloned();
    let started = collector
        .as_ref()
        .map(|collector| collector.stages[stage as usize].enter());
    StageGuard {
        collector,
        stage,
        started,
    }
}

pub struct StageGuard {
    collector: Option<Arc<Collector>>,
    stage: Stage,
    started: Option<Instant>,
}

impl Drop for StageGuard {
    fn drop(&mut self) {
        if let (Some(collector), Some(started)) = (&self.collector, self.started) {
            collector.stages[self.stage as usize].exit(started);
        }
    }
}

/// Track an async unit of work without changing its result or cancellation behavior.
pub async fn track_future<F: Future>(stage: Stage, future: F) -> F::Output {
    let _guard = enter_stage(stage);
    future.await
}

struct BlockingGuard {
    collector: Option<Arc<Collector>>,
    started: Option<Instant>,
}

impl BlockingGuard {
    fn enter() -> Self {
        let collector = COLLECTOR.get().cloned();
        let started = collector
            .as_ref()
            .map(|collector| collector.blocking.enter());
        Self { collector, started }
    }
}

impl Drop for BlockingGuard {
    fn drop(&mut self) {
        if let (Some(collector), Some(started)) = (&self.collector, self.started) {
            collector.blocking.exit(started);
        }
    }
}

/// Tokio blocking-pool wrapper used to count task entries, exits, and peak use.
pub fn spawn_blocking<F, R>(work: F) -> tokio::task::JoinHandle<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        let _blocking = BlockingGuard::enter();
        work()
    })
}

/// Blocking-pool wrapper that also attributes the work to one pipeline stage.
pub fn spawn_blocking_stage<F, R>(stage: Stage, work: F) -> tokio::task::JoinHandle<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        let _blocking = BlockingGuard::enter();
        let _stage = enter_stage(stage);
        work()
    })
}

/// Stop sampling and emit the final JSON report once.
pub fn dump() {
    let Some(collector) = COLLECTOR.get() else {
        return;
    };
    if collector.dumped.swap(true, Ordering::AcqRel) {
        return;
    }
    collector.stop.store(true, Ordering::Release);
    if let Some(handle) = collector
        .sampler
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
    {
        let _ = handle.join();
    }
    collector.sample();

    let now = Instant::now();
    let elapsed_seconds = collector.started.elapsed().as_secs_f64();
    let report = RuntimeReport {
        schema: 1,
        elapsed_seconds,
        peak_threads: collector.peak_threads.load(Ordering::Relaxed),
        peak_rss_kib: collector.peak_rss_kib.load(Ordering::Relaxed),
        rayon_pool_size: rayon::current_num_threads(),
        blocking_pool: collector.blocking.snapshot(now),
        stages: STAGES
            .iter()
            .map(|stage| {
                let gauge = collector.stages[*stage as usize].snapshot(now);
                StageReport {
                    name: stage.name(),
                    active_wall_percent: if elapsed_seconds > 0.0 {
                        gauge.active_wall_seconds * 100.0 / elapsed_seconds
                    } else {
                        0.0
                    },
                    gauge,
                }
            })
            .collect(),
        samples: collector
            .samples
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone(),
    };
    match serde_json::to_string(&report) {
        Ok(json) => eprintln!("LEGE_RUNTIME_STATS {json}"),
        Err(error) => eprintln!("LEGE_RUNTIME_STATS serialization_error={error}"),
    }
}

#[derive(Clone, Serialize)]
struct RuntimeSample {
    elapsed_seconds: f64,
    threads: usize,
    rss_kib: usize,
    blocking_in_flight: usize,
    stages: Vec<StageCurrent>,
}

#[derive(Clone, Serialize)]
struct StageCurrent {
    name: &'static str,
    in_flight: usize,
}

#[derive(Serialize)]
struct RuntimeReport {
    schema: u8,
    elapsed_seconds: f64,
    peak_threads: usize,
    peak_rss_kib: usize,
    rayon_pool_size: usize,
    blocking_pool: GaugeSnapshot,
    stages: Vec<StageReport>,
    samples: Vec<RuntimeSample>,
}

#[derive(Serialize)]
struct StageReport {
    name: &'static str,
    active_wall_percent: f64,
    #[serde(flatten)]
    gauge: GaugeSnapshot,
}

#[derive(Serialize)]
struct GaugeSnapshot {
    current: usize,
    peak: usize,
    entries: usize,
    exits: usize,
    active_wall_seconds: f64,
    summed_job_seconds: f64,
}

#[derive(Default)]
struct ProcessStatus {
    threads: usize,
    rss_kib: usize,
    peak_rss_kib: usize,
}

#[cfg(target_os = "linux")]
fn read_process_status() -> ProcessStatus {
    let Ok(status) = std::fs::read_to_string("/proc/self/status") else {
        return ProcessStatus::default();
    };
    let mut result = ProcessStatus::default();
    for line in status.lines() {
        if let Some(value) = line.strip_prefix("Threads:") {
            result.threads = value.trim().parse().unwrap_or(0);
        } else if let Some(value) = line.strip_prefix("VmRSS:") {
            result.rss_kib = value
                .split_ascii_whitespace()
                .next()
                .and_then(|value| value.parse().ok())
                .unwrap_or(0);
        } else if let Some(value) = line.strip_prefix("VmHWM:") {
            result.peak_rss_kib = value
                .split_ascii_whitespace()
                .next()
                .and_then(|value| value.parse().ok())
                .unwrap_or(0);
        }
    }
    result.peak_rss_kib = result.peak_rss_kib.max(result.rss_kib);
    result
}

#[cfg(not(target_os = "linux"))]
fn read_process_status() -> ProcessStatus {
    // `/proc/self/status` is the canonical Phase 0 measurement source. Keep
    // unsupported fields explicit on other platforms rather than mixing units
    // or changing semantics between baseline hosts.
    ProcessStatus::default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inactive_guards_are_noops() {
        let guard = enter_stage(Stage::Processing);
        drop(guard);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_process_status_has_threads() {
        let status = read_process_status();
        assert!(status.threads >= 1);
    }
}
