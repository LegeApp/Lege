use std::sync::{Mutex, MutexGuard};
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Result, anyhow};

use super::config::PipelineConfig;

static ORT_GATE: Mutex<()> = Mutex::new(());

#[cfg(test)]
static ORT_CURRENT: AtomicUsize = AtomicUsize::new(0);
#[cfg(test)]
static ORT_MAX: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PipelineRuntimeLimits {
    pub page_workers: usize,
    pub channel_capacity: usize,
    pub render_buffer: usize,
    pub inference_buffer: usize,
    pub process_workers: usize,
    pub djvu_encode_workers: usize,
}

impl PipelineRuntimeLimits {
    pub fn from_config(config: &PipelineConfig) -> Self {
        let page_workers = config.max_parallel_pages().unwrap_or(4).max(1);
        let channel_capacity = config.channel_buffer_size().max(page_workers);

        Self {
            page_workers,
            channel_capacity,
            render_buffer: channel_capacity,
            inference_buffer: channel_capacity,
            process_workers: page_workers,
            djvu_encode_workers: page_workers,
        }
    }
}

pub fn lock_ort_gate() -> Result<OrtGateGuard> {
    let guard = ORT_GATE
        .lock()
        .map_err(|_| anyhow!("ONNX Runtime global lock poisoned"))?;
    Ok(OrtGateGuard::new(guard))
}

pub fn with_ort_lock<T>(f: impl FnOnce() -> T) -> Result<T> {
    let _guard = lock_ort_gate()?;
    Ok(f())
}

pub struct OrtGateGuard {
    _guard: MutexGuard<'static, ()>,
}

impl OrtGateGuard {
    fn new(guard: MutexGuard<'static, ()>) -> Self {
        #[cfg(test)]
        {
            let current = ORT_CURRENT.fetch_add(1, Ordering::SeqCst) + 1;
            let mut prev = ORT_MAX.load(Ordering::SeqCst);
            while current > prev {
                match ORT_MAX.compare_exchange(prev, current, Ordering::SeqCst, Ordering::SeqCst) {
                    Ok(_) => break,
                    Err(actual) => prev = actual,
                }
            }
        }

        Self { _guard: guard }
    }
}

impl Drop for OrtGateGuard {
    fn drop(&mut self) {
        #[cfg(test)]
        {
            ORT_CURRENT.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

#[cfg(test)]
pub(crate) fn reset_ort_test_state() {
    ORT_CURRENT.store(0, Ordering::SeqCst);
    ORT_MAX.store(0, Ordering::SeqCst);
}

#[cfg(test)]
pub(crate) fn max_ort_concurrency_seen() -> usize {
    ORT_MAX.load(Ordering::SeqCst)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier};
    use std::thread;
    use std::time::Duration;

    use super::{
        PipelineRuntimeLimits, lock_ort_gate, max_ort_concurrency_seen, reset_ort_test_state,
    };
    use crate::pipeline::config::PipelineConfig;

    #[test]
    fn runtime_limits_follow_config() {
        let mut config = PipelineConfig::new().expect("config");
        config.max_parallel_pages = Some(3);
        config.channel_buffer_size = Some(2);

        let limits = PipelineRuntimeLimits::from_config(&config);

        assert_eq!(limits.page_workers, 3);
        assert_eq!(limits.channel_capacity, 3);
        assert_eq!(limits.render_buffer, 3);
        assert_eq!(limits.inference_buffer, 3);
        assert_eq!(limits.process_workers, 3);
        assert_eq!(limits.djvu_encode_workers, 3);
    }

    #[test]
    fn ort_gate_serializes_concurrent_users() {
        reset_ort_test_state();

        let barrier = Arc::new(Barrier::new(3));
        let mut handles = Vec::new();

        for _ in 0..2 {
            let barrier = barrier.clone();
            handles.push(thread::spawn(move || {
                barrier.wait();
                let _guard = lock_ort_gate().expect("guard");
                thread::sleep(Duration::from_millis(40));
            }));
        }

        barrier.wait();

        for handle in handles {
            handle.join().expect("join");
        }

        assert_eq!(max_ort_concurrency_seen(), 1);
    }

    #[test]
    fn deskew_and_inference_labels_share_same_gate() {
        reset_ort_test_state();

        let barrier = Arc::new(Barrier::new(3));
        let mut handles = Vec::new();

        for _label in ["deskew", "inference"] {
            let barrier = barrier.clone();
            handles.push(thread::spawn(move || {
                barrier.wait();
                let _guard = lock_ort_gate().expect("guard");
                thread::sleep(Duration::from_millis(25));
            }));
        }

        barrier.wait();

        for handle in handles {
            handle.join().expect("join");
        }

        assert_eq!(max_ort_concurrency_seen(), 1);
    }
}
