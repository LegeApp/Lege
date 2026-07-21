//! wgpu device context and core GPU dispatch primitives.

use std::sync::{Arc, OnceLock};

use anyhow::{Context, Result, bail};
use async_lock::Mutex;

#[derive(Clone)]
pub(crate) struct GpuContext {
    pub(crate) device: Arc<crate::vision::wgpu::Device>,
    pub(crate) queue: Arc<crate::vision::wgpu::Queue>,
    pub(crate) is_cpu_adapter: bool,
    pub(crate) supports_timestamps: bool,
}

impl GpuContext {
    /// Marker embedded in errors when every adapter fails `request_device`.
    /// Used by the pipeline to distinguish GPU-unavailable from model errors.
    pub(crate) fn gpu_device_unavailable_marker() -> &'static str {
        "wgpu GPU device unavailable"
    }

    pub(crate) async fn new() -> Result<Self> {
        let backends = crate::wgpu_setup::requested_backends();
        let instance = crate::wgpu_setup::create_instance();
        let adapter_name_filter = std::env::var("WGPU_ADAPTER_NAME").ok();

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
        // selects a single candidate; otherwise the HighPerformance preference
        // adapter goes first, followed by remaining enumerated adapters sorted by
        // device type (Discrete → Integrated → Other → Virtual → Cpu) so that if
        // the preferred adapter's driver rejects request_device we fall to a real
        // iGPU before landing on a software rasterizer.
        let candidates: Vec<crate::vision::wgpu::Adapter> =
            if let Some(filter) = adapter_name_filter.as_deref() {
                let needle = filter.to_ascii_lowercase();
                let matched = enumerated
                    .into_iter()
                    .find(|a| a.get_info().name.to_ascii_lowercase().contains(&needle))
                    .with_context(|| {
                        format!("WGPU_ADAPTER_NAME=`{filter}` did not match any wgpu adapter")
                    })?;
                vec![matched]
            } else {
                let sort_key = |a: &crate::vision::wgpu::Adapter| match a.get_info().device_type {
                    crate::vision::wgpu::DeviceType::DiscreteGpu => 0u8,
                    crate::vision::wgpu::DeviceType::IntegratedGpu => 1,
                    crate::vision::wgpu::DeviceType::Other => 2,
                    crate::vision::wgpu::DeviceType::VirtualGpu => 3,
                    crate::vision::wgpu::DeviceType::Cpu => 4,
                };
                match instance
                    .request_adapter(&crate::vision::wgpu::RequestAdapterOptions {
                        power_preference: crate::vision::wgpu::PowerPreference::HighPerformance,
                        compatible_surface: None,
                        force_fallback_adapter: false,
                        apply_limit_buckets: false,
                    })
                    .await
                {
                    Ok(preferred) => {
                        let preferred_name = preferred.get_info().name.clone();
                        // Keep the preferred adapter first; sort the rest so
                        // integrated GPUs are tried before software adapters.
                        let mut rest: Vec<_> = enumerated
                            .into_iter()
                            .filter(|a| a.get_info().name != preferred_name)
                            .collect();
                        rest.sort_by_key(|a| sort_key(a));
                        let mut list = vec![preferred];
                        list.extend(rest);
                        list
                    }
                    Err(_) => {
                        // No HighPerformance preference returned — order all by
                        // device type so we prefer real hardware over software.
                        let mut list = enumerated;
                        list.sort_by_key(|a| sort_key(&a));
                        list
                    }
                }
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
                    return Ok(Self {
                        device: Arc::new(device),
                        queue: Arc::new(queue),
                        is_cpu_adapter: is_cpu,
                        supports_timestamps,
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
        static SHARED: OnceLock<GpuContext> = OnceLock::new();
        static INITIALIZING: Mutex<()> = Mutex::new(());

        if let Some(ctx) = SHARED.get() {
            return Ok(ctx.clone());
        }

        // OnceLock::set alone is not an initializer: concurrent first callers
        // would each create a device, then immediately drop every device but the
        // winner. Besides wasting substantial work, concurrent Vulkan device
        // creation/destruction has proven unstable on software adapters. Hold a
        // narrow process-wide initialization lock and check again after waiting.
        let _initializing = INITIALIZING.lock().await;
        if let Some(ctx) = SHARED.get() {
            return Ok(ctx.clone());
        }

        let ctx = Self::new().await?;
        let _ = SHARED.set(ctx.clone());
        Ok(SHARED
            .get()
            .expect("shared GPU context was just initialized")
            .clone())
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
    ctx.queue.submit(Some(encoder.finish()));

    map_readback(&ctx.device, &readback, output_bytes).await
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
    device: &crate::vision::wgpu::Device,
    buf: &crate::vision::wgpu::Buffer,
    bytes: usize,
) -> Result<Vec<u8>> {
    let slice = buf.slice(..);
    let (sender, receiver) = std::sync::mpsc::channel();
    slice.map_async(crate::vision::wgpu::MapMode::Read, move |r| {
        let _ = sender.send(r);
    });
    let _ = device.poll(crate::vision::wgpu::PollType::wait_indefinitely());
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
