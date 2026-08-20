//! wgpu device context and core GPU dispatch primitives.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use anyhow::{Context, Result, bail};
use async_lock::Mutex;

#[derive(Clone)]
struct GpuPoller {
    sender: std::sync::mpsc::Sender<PollRequest>,
}

struct PollRequest {
    submission: Option<crate::vision::wgpu::SubmissionIndex>,
    done: std::sync::mpsc::Sender<Result<(), String>>,
}

impl GpuPoller {
    fn new(device: Arc<crate::vision::wgpu::Device>) -> Result<Self> {
        let (sender, receiver) = std::sync::mpsc::channel::<PollRequest>();
        std::thread::Builder::new()
            .name("gpu-poll".to_string())
            .spawn(move || {
                while let Ok(request) = receiver.recv() {
                    let poll_type = match request.submission {
                        Some(submission_index) => crate::vision::wgpu::PollType::Wait {
                            submission_index: Some(submission_index),
                            timeout: None,
                        },
                        None => crate::vision::wgpu::PollType::wait_indefinitely(),
                    };
                    let result = device
                        .poll(poll_type)
                        .map(|_| ())
                        .map_err(|error| format!("{error:?}"));
                    let _ = request.done.send(result);
                }
            })
            .context("failed to spawn shared GPU poll thread")?;
        Ok(Self { sender })
    }

    fn wait(&self) -> Result<()> {
        self.wait_for(None)
    }

    fn wait_for_submission(&self, submission: crate::vision::wgpu::SubmissionIndex) -> Result<()> {
        self.wait_for(Some(submission))
    }

    fn wait_for(&self, submission: Option<crate::vision::wgpu::SubmissionIndex>) -> Result<()> {
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        self.sender
            .send(PollRequest {
                submission,
                done: done_tx,
            })
            .context("shared GPU poll thread stopped")?;
        done_rx
            .recv()
            .context("shared GPU poll thread dropped completion")?
            .map_err(anyhow::Error::msg)
    }
}

#[derive(Default)]
struct DeviceHealth {
    lost: AtomicBool,
    reason: std::sync::Mutex<Option<Arc<str>>>,
}

impl DeviceHealth {
    fn mark_lost(&self, reason: crate::vision::wgpu::DeviceLostReason, message: String) {
        let detail: Arc<str> = Arc::from(if message.is_empty() {
            format!("{reason:?}")
        } else {
            format!("{reason:?}: {message}")
        });
        *self
            .reason
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(Arc::clone(&detail));
        self.lost.store(true, Ordering::Release);
        eprintln!("wgpu: shared device lost: {detail}");
    }
}

#[derive(Clone)]
pub(crate) struct GpuContext {
    pub(crate) instance: Arc<crate::vision::wgpu::Instance>,
    pub(crate) adapter: Arc<crate::vision::wgpu::Adapter>,
    pub(crate) device: Arc<crate::vision::wgpu::Device>,
    pub(crate) queue: Arc<crate::vision::wgpu::Queue>,
    pub(crate) is_cpu_adapter: bool,
    pub(crate) supports_timestamps: bool,
    pub(crate) adapter_name: Arc<str>,
    pub(crate) adapter_backend: crate::vision::wgpu::Backend,
    pub(crate) adapter_device_type: crate::vision::wgpu::DeviceType,
    poller: GpuPoller,
    health: Arc<DeviceHealth>,
}

impl GpuContext {
    /// Marker embedded in errors when every adapter fails `request_device`.
    /// Used by the pipeline to distinguish GPU-unavailable from model errors.
    pub(crate) fn gpu_device_unavailable_marker() -> &'static str {
        "wgpu GPU device unavailable"
    }

    pub(crate) async fn new() -> Result<Self> {
        let backends = crate::wgpu_setup::requested_backends();
        let instance = Arc::new(crate::wgpu_setup::create_instance());
        let adapter_name_filter = std::env::var("WGPU_ADAPTER_NAME").ok();
        let adapter_skip = crate::wgpu_setup::adapter_skip_filters();

        let enumerated: Vec<crate::vision::wgpu::Adapter> =
            instance.enumerate_adapters(backends).await;

        eprintln!("wgpu: found {} adapter(s)", enumerated.len());
        #[cfg(feature = "debug-logging")]
        for adapter in &enumerated {
            let info = adapter.get_info();
            eprintln!(
                "  - {} ({:?}, {:?})",
                info.name, info.backend, info.device_type
            );
        }

        // Build an ordered candidate list: the explicit WGPU_ADAPTER_NAME filter
        // selects a single candidate; otherwise enumerate and explicitly order
        // by device type (Discrete → Integrated → Other → Virtual → Cpu).
        // PowerPreference::HighPerformance is only a hint and has returned the
        // integrated GPU first on Linux hybrid-graphics systems.
        let candidates: Vec<crate::vision::wgpu::Adapter> =
            if let Some(filter) = adapter_name_filter.as_deref() {
                let matched = enumerated
                    .into_iter()
                    .find(|a| crate::wgpu_setup::adapter_name_matches(&a.get_info().name, filter))
                    .with_context(|| {
                        format!("WGPU_ADAPTER_NAME=`{filter}` did not match any wgpu adapter")
                    })?;
                vec![matched]
            } else {
                let mut list = enumerated;
                crate::wgpu_setup::sort_adapters_by_preference(&mut list);
                list
            };

        if candidates.is_empty() {
            bail!(
                "{}: no adapters found for backends {:?}",
                Self::gpu_device_unavailable_marker(),
                backends
            );
        }

        let require_real_gpu = std::env::var("WGPU_REQUIRE_REAL_GPU")
            .map(|v| v != "0" && !v.eq_ignore_ascii_case("false"))
            .unwrap_or(false);

        let mut adapter_errors: Vec<String> = Vec::new();

        for adapter in candidates {
            let info = adapter.get_info();
            let adapter_desc =
                format!("{} ({:?}, {:?})", info.name, info.backend, info.device_type);
            let is_cpu = info.device_type == crate::vision::wgpu::DeviceType::Cpu;

            if crate::wgpu_setup::adapter_is_skipped(&info.name, &adapter_skip) {
                adapter_errors.push(format!(
                    "`{adapter_desc}` skipped: matches WGPU_ADAPTER_SKIP"
                ));
                continue;
            }

            if is_cpu && require_real_gpu {
                adapter_errors.push(format!(
                    "`{adapter_desc}` skipped: CPU/software adapter not allowed (WGPU_REQUIRE_REAL_GPU)"
                ));
                continue;
            }

            let adapter_features = adapter.features();
            let supports_timestamps =
                adapter_features.contains(crate::vision::wgpu::Features::TIMESTAMP_QUERY);
            let required_features = if supports_timestamps {
                crate::vision::wgpu::Features::TIMESTAMP_QUERY
            } else {
                crate::vision::wgpu::Features::empty()
            };

            match adapter
                .request_device(&crate::vision::wgpu::DeviceDescriptor {
                    label: Some("wgpu-layout"),
                    required_features,
                    required_limits: adapter.limits(),
                    memory_hints: crate::vision::wgpu::MemoryHints::Performance,
                    ..Default::default()
                })
                .await
            {
                Ok((device, queue)) => {
                    eprintln!("wgpu: selected adapter: {adapter_desc}");
                    if is_cpu {
                        eprintln!(
                            "wgpu: WARNING — no hardware GPU adapter was usable; \
                             falling back to CPU/software rendering (`{}`). \
                             Layout detection will be disabled. \
                             Install or update your GPU driver, or set WGPU_REQUIRE_REAL_GPU=1 \
                             to surface this as an error instead.",
                            info.name
                        );
                    }
                    let health = Arc::new(DeviceHealth::default());
                    let callback_health = Arc::clone(&health);
                    device.set_device_lost_callback(move |reason, message| {
                        callback_health.mark_lost(reason, message);
                    });
                    let device = Arc::new(device);
                    let poller = GpuPoller::new(Arc::clone(&device))?;
                    return Ok(Self {
                        instance,
                        adapter: Arc::new(adapter),
                        device,
                        queue: Arc::new(queue),
                        is_cpu_adapter: is_cpu,
                        supports_timestamps,
                        adapter_name: Arc::from(info.name),
                        adapter_backend: info.backend,
                        adapter_device_type: info.device_type,
                        poller,
                        health,
                    });
                }
                Err(e) => {
                    eprintln!("wgpu: adapter `{adapter_desc}` rejected by driver: {e}");
                    adapter_errors.push(format!("`{adapter_desc}`: {e}"));
                }
            }
        }

        bail!(
            "{}: every adapter failed request_device — {}",
            Self::gpu_device_unavailable_marker(),
            adapter_errors.join("; ")
        )
    }

    pub(crate) async fn shared() -> Result<Self> {
        static SHARED: OnceLock<std::sync::Mutex<Option<GpuContext>>> = OnceLock::new();
        static INITIALIZING: Mutex<()> = Mutex::new(());

        let shared = SHARED.get_or_init(|| std::sync::Mutex::new(None));
        if let Some(ctx) = shared
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .filter(|ctx| !ctx.is_lost())
            .cloned()
        {
            return Ok(ctx);
        }

        // Concurrent first callers or recovery callers must not each create a
        // device. Hold a narrow async lock, but never a synchronous slot lock
        // across adapter/device discovery.
        let _initializing = INITIALIZING.lock().await;
        if let Some(ctx) = shared
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .filter(|ctx| !ctx.is_lost())
            .cloned()
        {
            return Ok(ctx);
        }

        let ctx = Self::new().await?;
        *shared.lock().unwrap_or_else(|error| error.into_inner()) = Some(ctx.clone());
        Ok(ctx)
    }

    pub(crate) fn wait(&self) -> Result<()> {
        self.poller.wait()
    }

    /// Wait for one caller's submission without also waiting for unrelated
    /// work that another shared-device client queued afterward.
    pub(crate) fn wait_for_submission(
        &self,
        submission: crate::vision::wgpu::SubmissionIndex,
    ) -> Result<()> {
        self.poller.wait_for_submission(submission)
    }

    pub(crate) fn is_lost(&self) -> bool {
        self.health.lost.load(Ordering::Acquire)
    }

    pub(crate) fn device_loss_reason(&self) -> Option<Arc<str>> {
        self.health
            .reason
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }
}

// ── Core GPU dispatch ─────────────────────────────────────────────────────────
//
// Binding layout:
//   0..N-1   : read-only storage (inputs)
//   N        : read_write storage (output)
//   N+1      : read-only storage (params)

#[cfg(test)]
pub(crate) async fn dispatch_compute(
    ctx: &GpuContext,
    wgsl: &str,
    inputs: &[&[u8]],
    output_bytes: usize,
    params: &[u8],
    workgroups: (u32, u32, u32),
) -> Result<Vec<u8>> {
    let (wg_x, wg_y, wg_z) = workgroups;
    use crate::vision::wgpu::util::DeviceExt;

    let n_inputs = inputs.len();
    let mut read_flags = vec![true; n_inputs];
    read_flags.push(false); // output
    read_flags.push(true); // params

    let bgl =
        ctx.device
            .create_bind_group_layout(&crate::vision::wgpu::BindGroupLayoutDescriptor {
                label: None,
                entries: &storage_bgl_entries(&read_flags),
            });
    let pipeline =
        ctx.device
            .create_compute_pipeline(&crate::vision::wgpu::ComputePipelineDescriptor {
                label: None,
                layout: Some(&ctx.device.create_pipeline_layout(
                    &crate::vision::wgpu::PipelineLayoutDescriptor {
                        label: None,
                        bind_group_layouts: &[Some(&bgl)],
                        immediate_size: 0,
                    },
                )),
                module: &ctx.device.create_shader_module(
                    crate::vision::wgpu::ShaderModuleDescriptor {
                        label: None,
                        source: crate::vision::wgpu::ShaderSource::Wgsl(wgsl.into()),
                    },
                ),
                entry_point: Some("main"),
                compilation_options: crate::vision::wgpu::PipelineCompilationOptions::default(),
                cache: None,
            });

    let mut buffers: Vec<crate::vision::wgpu::Buffer> = inputs
        .iter()
        .enumerate()
        .map(|(i, data)| {
            ctx.device
                .create_buffer_init(&crate::vision::wgpu::util::BufferInitDescriptor {
                    label: Some(&format!("input {i}")),
                    contents: data,
                    usage: crate::vision::wgpu::BufferUsages::STORAGE,
                })
        })
        .collect();

    let out_buf = ctx
        .device
        .create_buffer(&crate::vision::wgpu::BufferDescriptor {
            label: Some("output"),
            size: output_bytes as u64,
            usage: crate::vision::wgpu::BufferUsages::STORAGE
                | crate::vision::wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
    let readback = ctx
        .device
        .create_buffer(&crate::vision::wgpu::BufferDescriptor {
            label: Some("readback"),
            size: output_bytes as u64,
            usage: crate::vision::wgpu::BufferUsages::MAP_READ
                | crate::vision::wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
    let params_buf =
        ctx.device
            .create_buffer_init(&crate::vision::wgpu::util::BufferInitDescriptor {
                label: Some("params"),
                contents: params,
                usage: crate::vision::wgpu::BufferUsages::STORAGE,
            });

    buffers.push(out_buf);

    let bg_entries: Vec<crate::vision::wgpu::BindGroupEntry> = buffers
        .iter()
        .enumerate()
        .map(|(i, buf)| crate::vision::wgpu::BindGroupEntry {
            binding: i as u32,
            resource: buf.as_entire_binding(),
        })
        .chain(std::iter::once(crate::vision::wgpu::BindGroupEntry {
            binding: (n_inputs + 1) as u32,
            resource: params_buf.as_entire_binding(),
        }))
        .collect();

    let bg = ctx
        .device
        .create_bind_group(&crate::vision::wgpu::BindGroupDescriptor {
            label: None,
            layout: &bgl,
            entries: &bg_entries,
        });

    let mut encoder = ctx
        .device
        .create_command_encoder(&crate::vision::wgpu::CommandEncoderDescriptor { label: None });
    {
        let mut pass = encoder.begin_compute_pass(&crate::vision::wgpu::ComputePassDescriptor {
            label: None,
            timestamp_writes: None,
        });
        pass.set_pipeline(&pipeline);
        pass.set_bind_group(0, &bg, &[]);
        pass.dispatch_workgroups(wg_x, wg_y, wg_z);
    }
    let out_buf = &buffers[n_inputs];
    encoder.copy_buffer_to_buffer(out_buf, 0, &readback, 0, output_bytes as u64);
    let submission = ctx.queue.submit(Some(encoder.finish()));

    map_readback(ctx, &readback, output_bytes, submission).await
}

pub(crate) fn storage_bgl_entries(
    read_flags: &[bool],
) -> Vec<crate::vision::wgpu::BindGroupLayoutEntry> {
    read_flags
        .iter()
        .enumerate()
        .map(
            |(i, &read_only)| crate::vision::wgpu::BindGroupLayoutEntry {
                binding: i as u32,
                visibility: crate::vision::wgpu::ShaderStages::COMPUTE,
                ty: crate::vision::wgpu::BindingType::Buffer {
                    ty: crate::vision::wgpu::BufferBindingType::Storage { read_only },
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            },
        )
        .collect()
}

pub(crate) async fn map_readback(
    ctx: &GpuContext,
    buf: &crate::vision::wgpu::Buffer,
    bytes: usize,
    submission: crate::vision::wgpu::SubmissionIndex,
) -> Result<Vec<u8>> {
    let slice = buf.slice(..);
    let (sender, receiver) = std::sync::mpsc::channel();
    slice.map_async(crate::vision::wgpu::MapMode::Read, move |r| {
        let _ = sender.send(r);
    });
    ctx.wait_for_submission(submission)?;
    receiver
        .recv()
        .context("readback callback was not called")?
        .context("failed to map readback buffer")?;
    let mapped = slice
        .get_mapped_range()
        .context("failed to access mapped readback buffer")?;
    if mapped.len() != bytes {
        bail!(
            "readback size mismatch: got {} expected {}",
            mapped.len(),
            bytes
        );
    }
    let data = mapped.to_vec();
    drop(mapped);
    buf.unmap();
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_health_retains_driver_loss_detail() {
        let health = DeviceHealth::default();
        health.mark_lost(
            crate::vision::wgpu::DeviceLostReason::Unknown,
            "simulated driver reset".to_owned(),
        );

        assert!(health.lost.load(Ordering::Acquire));
        assert_eq!(
            health
                .reason
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .as_deref(),
            Some("Unknown: simulated driver reset")
        );
    }

    #[test]
    fn shared_context_prefers_discrete_when_available() {
        if std::env::var("LEGE_TEST_GPU_SELECTION").as_deref() != Ok("1") {
            eprintln!("skipping hardware adapter test; set LEGE_TEST_GPU_SELECTION=1");
            return;
        }
        if let Some(adapter_name) = std::env::var_os("WGPU_ADAPTER_NAME") {
            eprintln!(
                "skipping hardware policy test; unset WGPU_ADAPTER_NAME to test automatic selection (current: {:?})",
                adapter_name
            );
            return;
        }

        let instance = crate::wgpu_setup::create_instance();
        let adapters = pollster::block_on(
            instance.enumerate_adapters(crate::wgpu_setup::requested_backends()),
        );
        let enumerated: Vec<_> = adapters
            .iter()
            .map(crate::vision::wgpu::Adapter::get_info)
            .map(|info| format!("{} ({:?}, {:?})", info.name, info.backend, info.device_type))
            .collect();
        eprintln!("hardware test enumerated adapter(s): {enumerated:?}");
        let discrete: Vec<_> = adapters
            .iter()
            .map(crate::vision::wgpu::Adapter::get_info)
            .filter(|info| info.device_type == crate::vision::wgpu::DeviceType::DiscreteGpu)
            .map(|info| info.name)
            .collect();
        if discrete.is_empty() {
            eprintln!("skipping hardware adapter test; no discrete adapter enumerated");
            return;
        }
        eprintln!("hardware test enumerated discrete adapter(s): {discrete:?}");

        let selected = pollster::block_on(GpuContext::shared()).expect("initialize GPU context");
        assert_eq!(
            selected.adapter_device_type,
            crate::vision::wgpu::DeviceType::DiscreteGpu,
            "selected {} ({:?}) despite enumerated discrete adapter(s) {discrete:?}",
            selected.adapter_name,
            selected.adapter_device_type
        );
    }
}
