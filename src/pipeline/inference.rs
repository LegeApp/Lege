// inference.rs

use crate::engine::{Detection, PaddleXConfig, PaddleXEngine};
use crate::pipeline::config::PipelineConfig;
use anyhow::Result;
use image::RgbImage;
use log::{error, info};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};

const DEFAULT_MAX_BATCH_SIZE: usize = 16;
const DEFAULT_BATCH_TIMEOUT_MS: u64 = 10; // Reduced from higher values

pub struct InferenceJob {
    pub page_index: usize,
    pub image: Arc<RgbImage>,
    pub response_tx: oneshot::Sender<Result<Vec<Detection>>>,
}

/// Fire-and-forget job - results sent back via callback channel
pub struct InferenceJobAsync {
    pub page_index: usize,
    pub image: Arc<RgbImage>,
    pub result_tx: mpsc::Sender<(usize, Result<Vec<Detection>>)>,
}

pub struct InferenceActor {
    receiver: mpsc::Receiver<InferenceJob>,
    engine: PaddleXEngine,
    max_batch_size: usize,
    batch_timeout_ms: u64,
    initial_single_pages: usize,
}

impl InferenceActor {
    fn new(receiver: mpsc::Receiver<InferenceJob>, config: &PipelineConfig) -> Result<Self> {
        let engine_config = PaddleXConfig {
            confidence_threshold: config.confidence_threshold(),
            nms_threshold: config.nms_threshold(),
            iou_threshold: config.nms_threshold(),
            batch_size: config.batch_size(),
        };

        let max_batch_size = config
            .max_inference_batch_size()
            .unwrap_or(DEFAULT_MAX_BATCH_SIZE);
        let batch_timeout_ms = config.batch_timeout_ms().min(DEFAULT_BATCH_TIMEOUT_MS); // Cap at 10ms
        let initial_single_pages = config.initial_single_pages();

        info!(
            "InferenceActor: Creating PaddleXEngine with model: {}",
            config.model_path()
        );
        info!(
            "InferenceActor: Max batch size: {}, timeout: {}ms",
            max_batch_size, batch_timeout_ms
        );

        let engine = PaddleXEngine::new(config.model_path(), engine_config)?;
        info!(
            "InferenceActor: Engine initialized with {}",
            engine.provider_name()
        );

        Ok(InferenceActor {
            receiver,
            engine,
            max_batch_size,
            batch_timeout_ms,
            initial_single_pages,
        })
    }

    /// Main run loop - simplified, no fake prefetching
    fn run(mut self) {
        info!("InferenceActor: Starting inference loop");

        let mut processed_count: usize = 0;
        let is_gpu = self.engine.provider_name() != "CPU";

        loop {
            // Wait for first job
            let first_job = match self.receiver.blocking_recv() {
                Some(job) => job,
                None => break,
            };

            // Try to form a batch (only if GPU and past warmup)
            let mut jobs = vec![first_job];

            if is_gpu && processed_count >= self.initial_single_pages && self.max_batch_size > 1 {
                let deadline = Instant::now() + Duration::from_millis(self.batch_timeout_ms);

                while jobs.len() < self.max_batch_size && Instant::now() < deadline {
                    match self.receiver.try_recv() {
                        Ok(job) => jobs.push(job),
                        Err(_) => {
                            // Brief sleep to avoid busy-spin, but very short
                            std::thread::sleep(Duration::from_micros(500));
                        }
                    }
                }
            }

            let batch_size = jobs.len();

            // Process the batch
            if batch_size == 1 {
                // Single image - most common case
                let job = jobs.pop().unwrap();
                let result = {
                    let _guard = crate::pipeline::runtime_limits::lock_ort_gate();
                    match _guard {
                        Ok(_guard) => self.engine.detect_single_blocking(&job.image),
                        Err(e) => Err(e),
                    }
                };
                let _ = job.response_tx.send(result);
            } else {
                // Batch processing
                let images: Vec<RgbImage> = jobs.iter().map(|j| (*j.image).clone()).collect();
                let indices: Vec<usize> = (0..images.len()).collect();

                let batch_result = {
                    let _guard = crate::pipeline::runtime_limits::lock_ort_gate();
                    match _guard {
                        Ok(_guard) => self
                            .engine
                            .detect_batch_with_indices_blocking(&images, &indices),
                        Err(e) => Err(e),
                    }
                };

                match batch_result {
                    Ok(results) => {
                        for (job, detections) in jobs.into_iter().zip(results) {
                            let _ = job.response_tx.send(Ok(detections));
                        }
                    }
                    Err(e) => {
                        let msg = format!("Batch inference failed: {}", e);
                        for job in jobs {
                            let _ = job.response_tx.send(Err(anyhow::anyhow!("{}", msg)));
                        }
                    }
                }
            }

            processed_count += batch_size;
        }

        info!(
            "InferenceActor: Shutting down after {} pages",
            processed_count
        );
    }
}

#[derive(Clone)]
pub struct InferenceHandle {
    sender: mpsc::Sender<InferenceJob>,
}

impl InferenceHandle {
    pub fn new(config: &PipelineConfig) -> Result<Self> {
        // Larger buffer for better throughput
        let (sender, receiver) = mpsc::channel(256);
        let config_clone = config.clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<()>>();

        std::thread::Builder::new()
            .name("inference-actor".to_string())
            .spawn(move || match InferenceActor::new(receiver, &config_clone) {
                Ok(actor) => {
                    let _ = ready_tx.send(Ok(()));
                    actor.run();
                }
                Err(e) => {
                    error!("InferenceActor failed to initialize: {}", e);
                    crate::warn_log!("InferenceActor failed to initialize: {}", e);
                    let _ = ready_tx.send(Err(anyhow::anyhow!(
                        "InferenceActor failed to initialize: {}",
                        e
                    )));
                }
            })?;

        ready_rx
            .recv()
            .map_err(|_| anyhow::anyhow!("Inference actor startup handshake failed"))??;

        info!("InferenceHandle: Actor thread spawned (buffer: 256)");
        Ok(Self { sender })
    }

    /// Standard detect - waits for result
    pub async fn detect(&self, page_index: usize, image: Arc<RgbImage>) -> Result<Vec<Detection>> {
        let (response_tx, response_rx) = oneshot::channel();
        let job = InferenceJob {
            page_index,
            image,
            response_tx,
        };

        self.sender
            .send(job)
            .await
            .map_err(|_| anyhow::anyhow!("Inference actor channel closed"))?;

        response_rx
            .await
            .map_err(|_| anyhow::anyhow!("Inference actor dropped response"))?
    }

    /// Fire-and-forget - submits job without waiting
    /// Useful when caller wants to overlap inference with other work
    pub async fn submit(
        &self,
        page_index: usize,
        image: Arc<RgbImage>,
    ) -> Result<oneshot::Receiver<Result<Vec<Detection>>>> {
        let (response_tx, response_rx) = oneshot::channel();
        let job = InferenceJob {
            page_index,
            image,
            response_tx,
        };

        self.sender
            .send(job)
            .await
            .map_err(|_| anyhow::anyhow!("Inference actor channel closed"))?;

        Ok(response_rx)
    }

    /// Check if the channel has capacity (non-blocking)
    pub fn has_capacity(&self) -> bool {
        self.sender.capacity() > 0
    }
}

//==============================================================================
// Alternative: Multi-threaded inference pool for CPU
//==============================================================================

/// For CPU-only inference, we can use multiple threads
/// Each thread has its own engine instance
pub struct InferencePool {
    senders: Vec<mpsc::Sender<InferenceJob>>,
    next_worker: std::sync::atomic::AtomicUsize,
}

impl InferencePool {
    pub fn new(config: &PipelineConfig, num_workers: usize) -> Result<Self> {
        let mut senders = Vec::with_capacity(num_workers);

        for i in 0..num_workers {
            let (sender, receiver) = mpsc::channel(64);
            let config_clone = config.clone();

            std::thread::Builder::new()
                .name(format!("inference-worker-{}", i))
                .spawn(move || match InferenceActor::new(receiver, &config_clone) {
                    Ok(actor) => actor.run(),
                    Err(e) => error!("InferenceWorker {} failed: {}", i, e),
                })?;

            senders.push(sender);
        }

        info!("InferencePool: Created {} workers", num_workers);
        Ok(Self {
            senders,
            next_worker: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    /// Round-robin job distribution
    pub async fn detect(&self, page_index: usize, image: Arc<RgbImage>) -> Result<Vec<Detection>> {
        let worker_idx = self
            .next_worker
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            % self.senders.len();

        let (response_tx, response_rx) = oneshot::channel();
        let job = InferenceJob {
            page_index,
            image,
            response_tx,
        };

        self.senders[worker_idx]
            .send(job)
            .await
            .map_err(|_| anyhow::anyhow!("Inference worker {} closed", worker_idx))?;

        response_rx
            .await
            .map_err(|_| anyhow::anyhow!("Inference worker dropped response"))?
    }
}
