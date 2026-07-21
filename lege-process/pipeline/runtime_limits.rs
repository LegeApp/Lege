use super::config::PipelineConfig;

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

    pub fn djvu_from_config(config: &PipelineConfig) -> Self {
        let requested_workers = config.max_parallel_pages().unwrap_or(4).max(1);
        let adaptive = AdaptiveConcurrency::detect();
        let cpu_workers = requested_workers.min(adaptive.cpu_workers).max(1);
        let encode_workers = (cpu_workers / 3).max(1);
        let process_workers = cpu_workers.saturating_sub(encode_workers).max(1);
        let channel_capacity = config
            .channel_buffer_size()
            .max(cpu_workers)
            .min(adaptive.io_workers.max(cpu_workers));

        Self {
            page_workers: cpu_workers,
            channel_capacity,
            render_buffer: channel_capacity,
            inference_buffer: channel_capacity,
            process_workers,
            djvu_encode_workers: encode_workers,
        }
    }
}

/// Per-stage concurrency caps for the image-folder (PNG) pipeline. These are
/// derived from system specs at runtime so the program adapts to the host
/// instead of using hard-coded numbers tuned to the developer's box.
#[derive(Clone, Copy, Debug)]
pub struct AdaptiveConcurrency {
    /// Max parallel PNG decodes. Decode is single-threaded per page, so a
    /// wider count helps — bounded by RAM (each in-flight RGB buffer can be
    /// ~50–200 MB for high-res scans).
    pub io_workers: usize,
    /// Max parallel rayon-heavy stages (binarize + encode). Each
    /// stage is internally parallel via rayon, so running too many at once
    /// oversubscribes the global rayon pool.
    pub cpu_workers: usize,
}

impl AdaptiveConcurrency {
    /// Pick concurrency caps from detected CPU cores and total system RAM.
    ///
    /// Heuristics (rough page memory budget ~250 MB per in-flight page incl.
    /// decoded RGB + binarized + encode scratch):
    /// - `cpu_workers` scales with cores: 1 on 1–3 cores, 2 on 4–7 cores,
    ///   3 on 8–15 cores, 4 on 16+ cores. Capped by RAM/2GB so a 4 GB box
    ///   can't run 4 concurrent rayon stages.
    /// - `io_workers` is `cpu_workers * 2` (decode is cheap; we want a
    ///   queue feeding the CPU stages), bounded by `cores` and by
    ///   `RAM_gb / 0.5` (≈500 MB headroom per buffered page).
    pub fn detect() -> Self {
        let cores = std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(2)
            .max(1);
        let ram_gb = crate::pipeline::helper_functions::get_available_ram_gb();
        Self::from_specs(cores, ram_gb)
    }

    pub fn from_specs(cores: usize, ram_gb: usize) -> Self {
        let cores = cores.max(1);
        // Treat unknown RAM (0) as a conservative 4 GB box rather than infinite.
        let ram_gb = if ram_gb == 0 { 4 } else { ram_gb };

        // CPU stage: scale by cores, then trim by RAM (≥2 GB per concurrent stage).
        let cpu_by_cores = if cores >= 16 {
            4
        } else if cores >= 8 {
            3
        } else if cores >= 4 {
            2
        } else {
            1
        };
        let cpu_by_ram = (ram_gb / 2).max(1);
        let cpu_workers = cpu_by_cores.min(cpu_by_ram).max(1);

        // I/O stage: feed the CPU stage with a small queue. Decoded RGB
        // buffers are the biggest in-flight cost, so cap by RAM headroom.
        let io_target = cpu_workers.saturating_mul(2).max(2);
        let io_by_ram = (ram_gb * 2).max(2); // ~0.5 GB budget per buffered page
        let io_workers = io_target.min(cores).min(io_by_ram).min(16).max(cpu_workers);

        Self {
            io_workers,
            cpu_workers,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{AdaptiveConcurrency, PipelineRuntimeLimits};
    use crate::pipeline::config::PipelineConfig;

    #[test]
    fn adaptive_concurrency_low_end_laptop() {
        // 4 cores / 8 GB — typical low-end laptop.
        let a = AdaptiveConcurrency::from_specs(4, 8);
        assert_eq!(a.cpu_workers, 2);
        assert!(a.io_workers >= a.cpu_workers && a.io_workers <= 4);
    }

    #[test]
    fn adaptive_concurrency_dev_box() {
        // 16 cores / 32 GB — the developer's machine.
        let a = AdaptiveConcurrency::from_specs(16, 32);
        assert_eq!(a.cpu_workers, 4);
        assert!(a.io_workers >= 4 && a.io_workers <= 16);
    }

    #[test]
    fn adaptive_concurrency_tiny_box_is_safe() {
        // 2 cores / 2 GB — must not oversubscribe.
        let a = AdaptiveConcurrency::from_specs(2, 2);
        assert_eq!(a.cpu_workers, 1);
        assert!(a.io_workers >= 1);
        assert!(a.io_workers <= 2);
    }

    #[test]
    fn adaptive_concurrency_ram_starved_high_core() {
        // 24 cores but only 4 GB — RAM, not cores, should be the bottleneck.
        let a = AdaptiveConcurrency::from_specs(24, 4);
        assert_eq!(a.cpu_workers, 2);
    }

    #[test]
    fn adaptive_concurrency_unknown_ram_falls_back_conservatively() {
        // 0 GB == detection failure; treat as ~4 GB.
        let a = AdaptiveConcurrency::from_specs(8, 0);
        assert_eq!(a.cpu_workers, 2);
        assert!(a.io_workers >= 2);
    }

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
    fn djvu_runtime_limits_split_rayon_heavy_stages() {
        let mut config = PipelineConfig::new().expect("config");
        config.max_parallel_pages = Some(4);
        config.channel_buffer_size = Some(16);

        let limits = PipelineRuntimeLimits::djvu_from_config(&config);

        assert!(limits.page_workers >= 1);
        assert!(limits.process_workers >= 1);
        assert!(limits.djvu_encode_workers >= 1);
        assert!(limits.process_workers + limits.djvu_encode_workers <= limits.page_workers.max(2));
        assert!(limits.channel_capacity >= limits.page_workers);
    }
}
