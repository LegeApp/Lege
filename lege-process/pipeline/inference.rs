// GPU layout inference session pool.
//
// Each LayoutEngine/CompiledGraph remains single-flight because it owns
// activation and readback buffers. The pool creates K sibling sessions on the
// process-wide shared WGPU device and checks one out per page. This removes the
// serial InferenceActor and its image-cloning batch path while allowing pages
// to overlap until the GPU saturates.

use crate::engine::{Detection, LayoutEngine, LayoutEngineConfig};
use crate::pipeline::config::PipelineConfig;
use anyhow::{Context, Result, anyhow};
use image::RgbImage;
use log::info;
use parking_lot::Mutex;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use tokio::sync::{Semaphore, oneshot};

const DEFAULT_GPU_SESSIONS: usize = 2;
const MIN_GPU_SESSIONS: usize = 2;
const MAX_GPU_SESSIONS: usize = 4;
const DEFAULT_VRAM_BUDGET_MB: usize = 2 * 1024;
// Conservative admission estimate for one PP-DocLayout-M inference. Resident
// session buffers are allocated at pool creation; this covers transient
// command/readback/input pressure while a checked-out session is in flight.
const SESSION_TRANSIENT_MB: usize = 384;

struct SessionPool {
    sessions: Mutex<Vec<LayoutEngine>>,
    available: Arc<Semaphore>,
    vram: Arc<Semaphore>,
    vram_budget_mb: usize,
    session_count: usize,
}

impl SessionPool {
    fn new(config: &PipelineConfig) -> Result<Self> {
        let engine_config = LayoutEngineConfig {
            confidence_threshold: config.confidence_threshold(),
            nms_threshold: config.nms_threshold(),
            iou_threshold: config.nms_threshold(),
            batch_size: 1,
        };

        let requested_sessions = std::env::var("LEGE_GPU_SESSIONS")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(DEFAULT_GPU_SESSIONS)
            .clamp(MIN_GPU_SESSIONS, MAX_GPU_SESSIONS);
        let vram_budget_mb = std::env::var("LEGE_VRAM_BUDGET_MB")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(DEFAULT_VRAM_BUDGET_MB)
            .max(1);

        // Do not construct more resident sessions than the configured VRAM
        // budget can plausibly support. A deliberately tiny budget may reduce
        // the pool to one session instead of failing initialization.
        let budget_sessions = (vram_budget_mb / SESSION_TRANSIENT_MB).max(1);
        let session_count = requested_sessions.min(budget_sessions);

        info!(
            "InferenceSessionPool: creating {} session(s), VRAM budget={} MiB",
            session_count, vram_budget_mb
        );
        let first = LayoutEngine::new(config.model_path(), engine_config)
            .context("failed to create the first layout inference session")?;
        let provider = first.provider_name().to_string();
        let mut sessions = Vec::with_capacity(session_count);
        sessions.push(first);
        while sessions.len() < session_count {
            let sibling = sessions[0]
                .build_sibling()
                .with_context(|| format!("failed to create layout session {}", sessions.len()))?;
            sessions.push(sibling);
        }
        info!(
            "InferenceSessionPool: initialized {} {} session(s) on the shared device",
            sessions.len(),
            provider
        );

        Ok(Self {
            sessions: Mutex::new(sessions),
            available: Arc::new(Semaphore::new(session_count)),
            vram: Arc::new(Semaphore::new(vram_budget_mb)),
            vram_budget_mb,
            session_count,
        })
    }

    fn transient_mb_for(image: &RgbImage) -> usize {
        let input_bytes = (image.width() as usize)
            .saturating_mul(image.height() as usize)
            .saturating_mul(3);
        SESSION_TRANSIENT_MB.saturating_add(input_bytes.div_ceil(1024 * 1024))
    }

    fn run(&self, image: &RgbImage) -> Result<Vec<Detection>> {
        let mut engine = self
            .sessions
            .lock()
            .pop()
            .expect("session semaphore granted a permit without an engine");
        let result = catch_unwind(AssertUnwindSafe(|| engine.detect_single_blocking(image)))
            .unwrap_or_else(|payload| {
                Err(anyhow!(
                    "layout inference panicked: {}",
                    panic_payload_message(payload)
                ))
            });
        self.sessions.lock().push(engine);
        result
    }
}

#[derive(Clone)]
pub struct InferenceHandle {
    pool: Arc<SessionPool>,
}

impl InferenceHandle {
    pub fn new(config: &PipelineConfig) -> Result<Self> {
        Ok(Self {
            pool: Arc::new(SessionPool::new(config)?),
        })
    }

    /// Check out one GPU session and run a single page. Permit order is VRAM
    /// first, then session, matching the global byte-before-GPU ordering rule.
    pub async fn detect(&self, page_index: usize, image: Arc<RgbImage>) -> Result<Vec<Detection>> {
        let requested_mb =
            SessionPool::transient_mb_for(&image).min(self.pool.vram_budget_mb) as u32;
        let _vram = Arc::clone(&self.pool.vram)
            .acquire_many_owned(requested_mb.max(1))
            .await
            .map_err(|_| anyhow!("layout VRAM admission semaphore closed"))?;
        let _session = Arc::clone(&self.pool.available)
            .acquire_owned()
            .await
            .map_err(|_| anyhow!("layout inference session pool closed"))?;

        let pool = Arc::clone(&self.pool);
        crate::runtime_stats::spawn_blocking_stage(
            crate::runtime_stats::Stage::Inference,
            move || {
                pool.run(&image)
                    .with_context(|| format!("layout inference failed on page {page_index}"))
            },
        )
        .await
        .map_err(|error| anyhow!("layout inference worker panicked: {error}"))?
    }

    /// Compatibility seam for existing page-job code. Unlike the removed
    /// actor, this only spawns a lightweight waiter; GPU execution happens in
    /// the checked-out session pool.
    pub async fn submit(
        &self,
        page_index: usize,
        image: Arc<RgbImage>,
    ) -> Result<oneshot::Receiver<Result<Vec<Detection>>>> {
        let (response_tx, response_rx) = oneshot::channel();
        let handle = self.clone();
        tokio::spawn(async move {
            let _ = response_tx.send(handle.detect(page_index, image).await);
        });
        Ok(response_rx)
    }

    pub fn has_capacity(&self) -> bool {
        self.pool.available.available_permits() > 0
    }

    pub fn session_count(&self) -> usize {
        self.pool.session_count
    }
}

fn panic_payload_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "unknown panic payload".to_string()
    }
}

pub fn is_layout_software_adapter_error(error: &(dyn std::error::Error + 'static)) -> bool {
    lege_gpu::vision::is_layout_software_adapter_error(error)
}

pub fn is_gpu_device_error(error: &(dyn std::error::Error + 'static)) -> bool {
    lege_gpu::vision::is_layout_gpu_device_error(error)
}
