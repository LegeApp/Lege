//! Resident compiled GPU executor.
//!
//! At build time: allocate all tensor buffers, upload all weights, compile one
//! pipeline per unique shader, pre-create all bind groups.
//!
//! At run time (per page): write_buffer(input) → one submit(all dispatches) →
//! 3 map+readback for the model output heads.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use crate::vision::wgpu::util::DeviceExt;
use anyhow::{Context, Result, bail};
use rayon::prelude::*;

use crate::vision::onnx::attrs::shape_i64_to_usize;
use crate::vision::onnx::graph::PreparedGraph;
use crate::vision::onnx::types::{ElementwiseKind, PlannedOpKind, UnaryKind};
use crate::vision::ops::compile::op_steps;
use crate::vision::ops::sigmoid::{SILU_VEC4_WGSL, SILU_WGSL};
use crate::vision::reference;
use crate::vision::runtime::device::{GpuContext, map_readback, storage_bgl_entries};
use crate::vision::runtime::memory_plan::ResidentMemoryPlan;

const DUMMY_BIAS: &str = "__dummy_zero_bias__";

fn build_pipeline_for_shader(
    device: &crate::vision::wgpu::Device,
    wgsl: &'static str,
    n_read_inputs: usize,
) -> (
    Arc<crate::vision::wgpu::ComputePipeline>,
    Arc<crate::vision::wgpu::BindGroupLayout>,
) {
    let mut read_flags: Vec<bool> = vec![true; n_read_inputs];
    read_flags.push(false); // output (read_write)
    read_flags.push(true); // params (read)
    let bgl = Arc::new(device.create_bind_group_layout(
        &crate::vision::wgpu::BindGroupLayoutDescriptor {
            label: None,
            entries: &storage_bgl_entries(&read_flags),
        },
    ));
    let pipeline_layout =
        device.create_pipeline_layout(&crate::vision::wgpu::PipelineLayoutDescriptor {
            label: None,
            bind_group_layouts: &[Some(&bgl)],
            immediate_size: 0,
        });
    let shader = device.create_shader_module(crate::vision::wgpu::ShaderModuleDescriptor {
        label: None,
        source: crate::vision::wgpu::ShaderSource::Wgsl(wgsl.into()),
    });
    let pipeline = Arc::new(device.create_compute_pipeline(
        &crate::vision::wgpu::ComputePipelineDescriptor {
            label: None,
            layout: Some(&pipeline_layout),
            module: &shader,
            entry_point: Some("main"),
            compilation_options: crate::vision::wgpu::PipelineCompilationOptions::default(),
            cache: None,
        },
    ));
    (pipeline, bgl)
}

struct CompiledStep {
    step_index: usize,
    op_index: usize,
    op_name: String,
    op_kind: String,
    output_shape: Vec<usize>,
    pipeline: Arc<crate::vision::wgpu::ComputePipeline>,
    bind_group: crate::vision::wgpu::BindGroup,
    dispatch: [u32; 3],
}

pub(crate) struct StepProfile {
    pub(crate) step_index: usize,
    pub(crate) op_index: usize,
    pub(crate) op_name: String,
    pub(crate) op_kind: String,
    pub(crate) output_shape: Vec<usize>,
    pub(crate) dispatch: [u32; 3],
    pub(crate) gpu_ms: f64,
}

pub(crate) struct CompiledGraph {
    ctx: GpuContext,
    slot_buffers: Vec<Arc<crate::vision::wgpu::Buffer>>,
    tensor_slots: HashMap<String, usize>,
    _dummy_bias_buffer: crate::vision::wgpu::Buffer,
    aliases: HashMap<String, String>,
    input_name: String,
    steps: Vec<CompiledStep>,
    output_names: Vec<String>,
    output_buf_names: Vec<String>, // canonical (alias-resolved) names for output buffers
    output_readback_bufs: Vec<crate::vision::wgpu::Buffer>,
    output_shapes: Vec<Vec<usize>>,
    output_bytes: Vec<usize>,
    // Keep params buffers alive for their bind groups
    _params_bufs: Vec<crate::vision::wgpu::Buffer>,
    // Immutable per-shader GPU objects, shared with sibling sessions so only the
    // first session for a document pays the shader-compilation cost.
    pipeline_cache: HashMap<&'static str, Arc<crate::vision::wgpu::ComputePipeline>>,
    bgl_cache: HashMap<&'static str, Arc<crate::vision::wgpu::BindGroupLayout>>,
}

pub(crate) struct SubmittedRun<'a> {
    graph: &'a CompiledGraph,
    submit_elapsed: Duration,
}

impl CompiledGraph {
    #[cfg(feature = "layout-detection")]
    pub(crate) const LAYOUT_SOFTWARE_ADAPTER_ERROR: &'static str =
        "software WGPU adapter selected; continuing without layout detection";

    pub(crate) async fn build(graph: &PreparedGraph) -> Result<Self> {
        let t0 = std::time::Instant::now();
        let ctx = GpuContext::shared().await?;
        #[cfg(feature = "debug-logging")]
        eprintln!(
            "  compiled.build: gpu_init={:.0}ms",
            t0.elapsed().as_millis()
        );
        Self::build_from_context(graph, ctx, t0, None)
    }

    #[cfg(feature = "layout-detection")]
    pub(crate) async fn build_layout(graph: &PreparedGraph) -> Result<Self> {
        let t0 = std::time::Instant::now();
        let ctx = GpuContext::shared().await?;
        if ctx.is_cpu_adapter {
            bail!("{}", Self::LAYOUT_SOFTWARE_ADAPTER_ERROR);
        }
        #[cfg(feature = "debug-logging")]
        eprintln!(
            "  compiled.layout_build: gpu_init={:.0}ms",
            t0.elapsed().as_millis()
        );
        Self::build_from_context(graph, ctx, t0, None)
    }

    #[cfg(feature = "layout-detection")]
    pub(crate) fn layout_software_adapter_error() -> &'static str {
        Self::LAYOUT_SOFTWARE_ADAPTER_ERROR
    }

    pub(crate) fn build_sibling(&self, graph: &PreparedGraph) -> Result<Self> {
        Self::build_from_context(
            graph,
            self.ctx.clone(),
            std::time::Instant::now(),
            Some(self),
        )
    }

    fn build_from_context(
        graph: &PreparedGraph,
        ctx: GpuContext,
        t0: std::time::Instant,
        shared_constants: Option<&CompiledGraph>,
    ) -> Result<Self> {
        #[cfg(not(feature = "debug-logging"))]
        let _ = t0;
        // ── Allocate GPU buffers ──────────────────────────────────────────
        let plan = ResidentMemoryPlan::from_graph(graph)?;
        let slot_sizes = plan.slot_sizes();
        let constant_slots = plan
            .initializer_entries()
            .filter_map(|entry| entry.reuse_slot)
            .collect::<HashSet<_>>();
        #[cfg(feature = "debug-logging")]
        let total_slot_bytes = slot_sizes.iter().sum::<u64>();
        let mut slot_buffers = Vec::with_capacity(slot_sizes.len());
        for (slot, byte_size) in slot_sizes.iter().enumerate() {
            if constant_slots.contains(&slot)
                && let Some(parent) = shared_constants
            {
                let parent_buffer = parent.slot_buffers.get(slot).with_context(|| {
                    format!("compiled sibling: parent is missing constant slot {slot}")
                })?;
                slot_buffers.push(Arc::clone(parent_buffer));
                continue;
            }
            let buf = Arc::new(
                ctx.device
                    .create_buffer(&crate::vision::wgpu::BufferDescriptor {
                        label: Some(&format!("resident slot {slot}")),
                        size: *byte_size,
                        usage: crate::vision::wgpu::BufferUsages::STORAGE
                            | crate::vision::wgpu::BufferUsages::COPY_SRC
                            | crate::vision::wgpu::BufferUsages::COPY_DST,
                        mapped_at_creation: false,
                    }),
            );
            slot_buffers.push(buf);
        }
        let mut tensor_slots = HashMap::new();
        for entry in plan.entries() {
            let slot = entry.reuse_slot.with_context(|| {
                format!("compiled: resident entry `{}` has no slot", entry.name)
            })?;
            tensor_slots.insert(entry.name.clone(), slot);
        }
        let dummy_bias_buffer = ctx
            .device
            .create_buffer(&crate::vision::wgpu::BufferDescriptor {
                label: Some(DUMMY_BIAS),
                size: 4,
                usage: crate::vision::wgpu::BufferUsages::STORAGE
                    | crate::vision::wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
        ctx.queue.write_buffer(&dummy_bias_buffer, 0, &[0u8; 4]);
        #[cfg(feature = "debug-logging")]
        eprintln!(
            "  compiled.build: allocated {} resident slots, {:.1}MB at {:.0}ms",
            slot_buffers.len(),
            total_slot_bytes as f64 / (1024.0 * 1024.0),
            t0.elapsed().as_millis()
        );

        // ── Upload all weight constants ───────────────────────────────────
        #[cfg(feature = "debug-logging")]
        let mut uploaded_mb = 0.0f64;
        for (name, tensor) in &graph.constants {
            if tensor.data.is_empty() {
                continue;
            }
            let slot = *tensor_slots.get(name).with_context(|| {
                format!("compiled: missing resident slot for constant `{name}`")
            })?;
            if shared_constants.is_some() && constant_slots.contains(&slot) {
                continue;
            }
            let buf = slot_buffers
                .get(slot)
                .with_context(|| format!("compiled: missing resident slot buffer {slot}"))?;
            let bytes: &[u8] = bytemuck::cast_slice(&tensor.data);
            ctx.queue.write_buffer(buf, 0, bytes);
            #[cfg(feature = "debug-logging")]
            {
                uploaded_mb += bytes.len() as f64 / (1024.0 * 1024.0);
            }
        }
        #[cfg(feature = "debug-logging")]
        eprintln!(
            "  compiled.build: uploaded {uploaded_mb:.1}MB constants at {:.0}ms",
            t0.elapsed().as_millis()
        );

        // ── Build alias map (Identity / Reshape / Unsqueeze / Squeeze → zero
        // GPU cost). Shared with the resident memory planner so buffer liveness
        // accounts for every aliased consumer.
        let aliases = crate::vision::runtime::memory_plan::build_alias_map(&graph.planned_ops);

        // ── Detect SiLU patterns: Sigmoid(x)→sig followed by Mul(x,sig)→out ──
        // When found: replace Sigmoid with SiLU writing to Mul's output; skip Mul.
        // Key: sigmoid op index → Mul's output buffer name
        let mut silu_overrides: HashMap<usize, String> = HashMap::new();
        // Set of Mul op indices to skip because they're fused into a preceding SiLU
        let mut fused_mul_ops: HashSet<usize> = HashSet::new();
        {
            // Map: sigmoid output name → (sigmoid op idx, sigmoid input name)
            let mut pending_sigs: HashMap<String, (usize, String)> = HashMap::new();
            for (idx, op) in graph.planned_ops.iter().enumerate() {
                match &op.kind {
                    PlannedOpKind::Unary(UnaryKind::Sigmoid) => {
                        pending_sigs.insert(op.outputs[0].clone(), (idx, op.inputs[0].clone()));
                    }
                    PlannedOpKind::Elementwise(ElementwiseKind::Mul) if op.inputs.len() == 2 => {
                        let a = &op.inputs[0];
                        let b = &op.inputs[1];
                        let fuse = if let Some((sig_idx, x)) = pending_sigs.get(a) {
                            if x == b {
                                Some((*sig_idx, op.outputs[0].clone()))
                            } else {
                                None
                            }
                        } else if let Some((sig_idx, x)) = pending_sigs.get(b) {
                            if x == a {
                                Some((*sig_idx, op.outputs[0].clone()))
                            } else {
                                None
                            }
                        } else {
                            None
                        };
                        if let Some((sig_idx, mul_out)) = fuse {
                            silu_overrides.insert(sig_idx, mul_out);
                            fused_mul_ops.insert(idx);
                        }
                    }
                    _ => {}
                }
            }
        }
        #[cfg(feature = "debug-logging")]
        eprintln!(
            "  compiled.build: SiLU fused {} sigmoid+mul pairs",
            silu_overrides.len()
        );

        // ── Pre-compile pipelines (one per unique WGSL source) ────────────
        // Keyed by shader source value, not pointer: `const &str` addresses
        // are not guaranteed stable across reference sites, and a missed hit
        // would compile (and pay naga validation for) the same WGSL twice.
        let mut pipeline_cache = shared_constants
            .map(|parent| parent.pipeline_cache.clone())
            .unwrap_or_default();
        let mut bgl_cache = shared_constants
            .map(|parent| parent.bgl_cache.clone())
            .unwrap_or_default();

        // The first session for a document has no parent to inherit pipelines
        // from, so it must compile every unique shader. naga validation and
        // backend codegen (HLSL/FXC on DX12, SPIR-V on Vulkan) dominate cold
        // start, and each unique WGSL source is independent, so compile them in
        // parallel across Rayon workers instead of serially inside the step
        // loop. wgpu's `Device` is `Sync`, which makes concurrent pipeline
        // creation safe. Sibling sessions skip this entirely: their caches were
        // cloned from the parent above (Arc clones of the same GPU objects).
        if shared_constants.is_none() {
            use crate::vision::ops::sigmoid::SIGMOID_VEC4_WGSL;
            // Collect the unique (shader source → read-input count) set, honoring
            // the same SiLU rewrite the step loop applies so the precompiled
            // pipelines match exactly what the loop will look up.
            let mut unique_shaders: HashMap<&'static str, usize> = HashMap::new();
            for (op_idx, op) in graph.planned_ops.iter().enumerate() {
                if fused_mul_ops.contains(&op_idx) {
                    continue;
                }
                let mut step_specs = op_steps(op, DUMMY_BIAS)?;
                if silu_overrides.contains_key(&op_idx) {
                    for spec in &mut step_specs {
                        let is_vec4 = spec.wgsl == SIGMOID_VEC4_WGSL;
                        spec.wgsl = if is_vec4 { SILU_VEC4_WGSL } else { SILU_WGSL };
                    }
                }
                for spec in &step_specs {
                    unique_shaders
                        .entry(spec.wgsl)
                        .or_insert(spec.n_read_inputs);
                }
            }

            let device: &crate::vision::wgpu::Device = &ctx.device;
            let compiled: Vec<(
                &'static str,
                Arc<crate::vision::wgpu::ComputePipeline>,
                Arc<crate::vision::wgpu::BindGroupLayout>,
            )> = unique_shaders
                .par_iter()
                .map(|(&wgsl, &n_read_inputs)| {
                    let (pipeline, bgl) = build_pipeline_for_shader(device, wgsl, n_read_inputs);
                    (wgsl, pipeline, bgl)
                })
                .collect();
            for (wgsl, pipeline, bgl) in compiled {
                pipeline_cache.insert(wgsl, pipeline);
                bgl_cache.insert(wgsl, bgl);
            }
            #[cfg(feature = "debug-logging")]
            eprintln!(
                "  compiled.build: precompiled {} unique pipeline(s) in parallel at {:.0}ms",
                pipeline_cache.len(),
                t0.elapsed().as_millis()
            );
        }

        // ── Build compiled steps ──────────────────────────────────────────
        let mut steps: Vec<CompiledStep> = Vec::new();
        let mut params_bufs: Vec<crate::vision::wgpu::Buffer> = Vec::new();
        let dump_conv_shapes = std::env::var_os("LEGE_CONV_SHAPES").is_some();

        for (op_idx, op) in graph.planned_ops.iter().enumerate() {
            // Skip Mul ops that were fused into a preceding SiLU
            if fused_mul_ops.contains(&op_idx) {
                continue;
            }
            let mut step_specs = op_steps(op, DUMMY_BIAS)?;

            // Apply SiLU override: replace Sigmoid shader + redirect output to Mul's buffer.
            // Preserve the vec4/scalar variant that compile.rs already chose.
            if let Some(mul_out) = silu_overrides.get(&op_idx) {
                use crate::vision::ops::sigmoid::SIGMOID_VEC4_WGSL;
                for spec in &mut step_specs {
                    // Compare by value, not pointer: `const &str` addresses are not
                    // guaranteed stable across reference sites, so an as_ptr() check
                    // can misclassify a vec4 sigmoid as scalar (and vice versa),
                    // leaving the SiLU shader reading vec4 params as scalars.
                    let is_vec4 = spec.wgsl == SIGMOID_VEC4_WGSL;
                    spec.wgsl = if is_vec4 { SILU_VEC4_WGSL } else { SILU_WGSL };
                    spec.output_buf_name = mul_out.clone();
                }
            }

            for spec in step_specs {
                // Fail loudly at build time: an over-limit dispatch would
                // otherwise surface as a cryptic wgpu validation error (or,
                // for flat kernels without the 2D-grid recovery, silently
                // wrong indices).
                if spec.dispatch.iter().any(|&d| d > 65535) {
                    bail!(
                        "compiled: op {} ({}) dispatch {:?} exceeds the 65535 per-dimension workgroup limit",
                        op_idx,
                        op.name,
                        spec.dispatch
                    );
                }
                let op_kind = step_profile_kind(&op.kind, spec.wgsl);
                if dump_conv_shapes {
                    if let PlannedOpKind::Conv2d(_) = &op.kind {
                        eprintln!(
                            "  conv-shape op={} name={} shader={} input={:?} weight={:?} output={:?} dispatch={:?}",
                            op_idx,
                            op.name,
                            op_kind,
                            op.input_shapes.first(),
                            op.input_shapes.get(1),
                            op.output_shapes.first(),
                            spec.dispatch,
                        );
                    }
                }
                let wgsl_key = spec.wgsl;
                let (pipeline, bgl) = if let (Some(p), Some(b)) =
                    (pipeline_cache.get(&wgsl_key), bgl_cache.get(&wgsl_key))
                {
                    (p.clone(), b.clone())
                } else {
                    // Fallback for a shader the parallel pre-pass did not cover
                    // (e.g. a sibling whose parent cache somehow lacks it).
                    let (pipeline, bgl) =
                        build_pipeline_for_shader(&ctx.device, wgsl_key, spec.n_read_inputs);
                    pipeline_cache.insert(wgsl_key, pipeline.clone());
                    bgl_cache.insert(wgsl_key, bgl.clone());
                    (pipeline, bgl)
                };

                // Create params buffer (kept alive in params_bufs)
                let params_buf = ctx.device.create_buffer_init(
                    &crate::vision::wgpu::util::BufferInitDescriptor {
                        label: None,
                        contents: &spec.params,
                        usage: crate::vision::wgpu::BufferUsages::STORAGE,
                    },
                );

                // Build bind group referencing persistent tensor buffers
                let mut bg_entries: Vec<crate::vision::wgpu::BindGroupEntry> =
                    Vec::with_capacity(spec.n_read_inputs + 2);
                for (binding, inp_name) in spec.input_buf_names.iter().enumerate() {
                    let canonical = resolve_alias(inp_name, &aliases);
                    let buf = if canonical == DUMMY_BIAS {
                        &dummy_bias_buffer
                    } else {
                        let slot = *tensor_slots.get(&canonical).with_context(|| {
                            format!(
                                "compiled: missing resident slot for input `{canonical}` (op {})",
                                op.name
                            )
                        })?;
                        slot_buffers.get(slot).with_context(|| {
                            format!("compiled: missing resident slot buffer {slot}")
                        })?
                    };
                    bg_entries.push(crate::vision::wgpu::BindGroupEntry {
                        binding: binding as u32,
                        resource: buf.as_entire_binding(),
                    });
                }
                let out_canonical = resolve_alias(&spec.output_buf_name, &aliases);
                let out_slot = *tensor_slots.get(&out_canonical).with_context(|| {
                    format!(
                        "compiled: missing resident slot for output `{out_canonical}` (op {})",
                        op.name
                    )
                })?;
                for inp_name in &spec.input_buf_names {
                    let input_canonical = resolve_alias(inp_name, &aliases);
                    if input_canonical == DUMMY_BIAS {
                        continue;
                    }
                    let input_slot = *tensor_slots.get(&input_canonical).with_context(|| {
                        format!(
                            "compiled: missing resident slot for input `{input_canonical}` (op {})",
                            op.name
                        )
                    })?;
                    if input_slot == out_slot {
                        bail!(
                            "compiled: op {} ({}) binds resident slot {out_slot} as both input `{input_canonical}` and output `{out_canonical}`",
                            op_idx,
                            op.name
                        );
                    }
                }
                let out_buf = slot_buffers.get(out_slot).with_context(|| {
                    format!("compiled: missing resident slot buffer {out_slot}")
                })?;
                bg_entries.push(crate::vision::wgpu::BindGroupEntry {
                    binding: spec.n_read_inputs as u32,
                    resource: out_buf.as_entire_binding(),
                });
                bg_entries.push(crate::vision::wgpu::BindGroupEntry {
                    binding: (spec.n_read_inputs + 1) as u32,
                    resource: params_buf.as_entire_binding(),
                });

                let bind_group =
                    ctx.device
                        .create_bind_group(&crate::vision::wgpu::BindGroupDescriptor {
                            label: None,
                            layout: &bgl,
                            entries: &bg_entries,
                        });

                let step_index = steps.len();
                let output_shape = op
                    .outputs
                    .iter()
                    .position(|name| name == &spec.output_buf_name)
                    .and_then(|index| op.output_shapes.get(index))
                    .or_else(|| op.output_shapes.first())
                    .map(|shape| shape.iter().map(|&dim| dim as usize).collect())
                    .unwrap_or_default();

                params_bufs.push(params_buf);
                steps.push(CompiledStep {
                    step_index,
                    op_index: op_idx,
                    op_name: op.name.clone(),
                    op_kind,
                    output_shape,
                    pipeline,
                    bind_group,
                    dispatch: spec.dispatch,
                });
            }
        }

        #[cfg(feature = "debug-logging")]
        eprintln!(
            "  compiled.build: {} steps, {} pipelines, build total={:.0}ms",
            steps.len(),
            pipeline_cache.len(),
            t0.elapsed().as_millis()
        );

        // ── Input info ────────────────────────────────────────────────────
        let input_name = graph.inputs.first().context("model has no inputs")?.clone();

        // ── Readback buffers for model outputs ────────────────────────────
        let mut output_names = Vec::new();
        let mut output_buf_names = Vec::new();
        let mut output_readback_bufs = Vec::new();
        let mut output_shapes = Vec::new();
        let mut output_bytes = Vec::new();
        for name in &graph.outputs {
            let shape = graph
                .known_shapes
                .get(name)
                .with_context(|| format!("missing shape for output `{name}`"))?;
            let shape_u = shape_i64_to_usize(shape)?;
            let bytes = shape_u.iter().product::<usize>() * 4;
            let rb = ctx
                .device
                .create_buffer(&crate::vision::wgpu::BufferDescriptor {
                    label: Some(&format!("readback_{name}")),
                    size: bytes as u64,
                    usage: crate::vision::wgpu::BufferUsages::MAP_READ
                        | crate::vision::wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });
            let canonical = resolve_alias(name, &aliases);
            output_names.push(name.clone());
            output_buf_names.push(canonical);
            output_readback_bufs.push(rb);
            output_shapes.push(shape_u);
            output_bytes.push(bytes);
        }

        Ok(Self {
            ctx,
            slot_buffers,
            tensor_slots,
            _dummy_bias_buffer: dummy_bias_buffer,
            aliases,
            input_name,
            steps,
            output_names,
            output_buf_names,
            output_readback_bufs,
            output_shapes,
            output_bytes,
            _params_bufs: params_bufs,
            pipeline_cache,
            bgl_cache,
        })
    }

    pub(crate) fn step_count(&self) -> usize {
        self.steps.len()
    }

    /// Upload `input`, submit all dispatches plus output copies, and return
    /// immediately. The resident buffers and readback buffers must not be reused
    /// until `await_result` completes, so one `CompiledGraph` remains single-flight.
    pub(crate) fn submit(&self, input: &reference::Tensor) -> Result<SubmittedRun<'_>> {
        let t0 = std::time::Instant::now();

        // Write input tensor to its GPU buffer
        let input_canonical = resolve_alias(&self.input_name, &self.aliases);
        let input_slot = *self
            .tensor_slots
            .get(&input_canonical)
            .context("compiled: missing resident slot for input buffer")?;
        let input_buf = self
            .slot_buffers
            .get(input_slot)
            .with_context(|| format!("compiled: missing resident slot buffer {input_slot}"))?;
        self.ctx
            .queue
            .write_buffer(input_buf, 0, bytemuck::cast_slice(&input.data));

        let chunk_size = compiled_chunk_size(self.ctx.is_cpu_adapter, self.steps.len());
        for chunk in self.steps.chunks(chunk_size) {
            let mut encoder = self.ctx.device.create_command_encoder(
                &crate::vision::wgpu::CommandEncoderDescriptor {
                    label: Some("compiled_run_chunk"),
                },
            );
            for step in chunk {
                let mut pass =
                    encoder.begin_compute_pass(&crate::vision::wgpu::ComputePassDescriptor {
                        label: None,
                        timestamp_writes: None,
                    });
                pass.set_pipeline(&step.pipeline);
                pass.set_bind_group(0, &step.bind_group, &[]);
                pass.dispatch_workgroups(step.dispatch[0], step.dispatch[1], step.dispatch[2]);
            }
            self.ctx.queue.submit(Some(encoder.finish()));
            if self.ctx.is_cpu_adapter {
                self.ctx.wait()?;
            }
        }

        let mut encoder = self.ctx.device.create_command_encoder(
            &crate::vision::wgpu::CommandEncoderDescriptor {
                label: Some("compiled_readback"),
            },
        );
        for (i, canonical) in self.output_buf_names.iter().enumerate() {
            let out_slot = *self.tensor_slots.get(canonical).with_context(|| {
                format!("compiled run: missing resident slot for output `{canonical}`")
            })?;
            let out_buf = self.slot_buffers.get(out_slot).with_context(|| {
                format!("compiled run: missing resident slot buffer {out_slot}")
            })?;
            encoder.copy_buffer_to_buffer(
                out_buf,
                0,
                &self.output_readback_bufs[i],
                0,
                self.output_bytes[i] as u64,
            );
        }
        self.ctx.queue.submit(Some(encoder.finish()));

        Ok(SubmittedRun {
            graph: self,
            submit_elapsed: t0.elapsed(),
        })
    }

    /// Await a submitted inference and map/read the model output heads.
    pub(crate) async fn await_result(
        &self,
        submitted: SubmittedRun<'_>,
    ) -> Result<HashMap<String, reference::Tensor>> {
        if !std::ptr::eq(self, submitted.graph) {
            bail!("submitted run belongs to a different CompiledGraph");
        }
        #[cfg(feature = "debug-logging")]
        let t0 = std::time::Instant::now();
        let mut result = HashMap::new();
        let mut receivers = Vec::with_capacity(self.output_readback_bufs.len());
        for (i, name) in self.output_names.iter().enumerate() {
            let slice = self.output_readback_bufs[i].slice(..);
            let (sender, receiver) = std::sync::mpsc::channel();
            slice.map_async(crate::vision::wgpu::MapMode::Read, move |r| {
                let _ = sender.send(r);
            });
            receivers.push((i, name, receiver));
        }

        self.ctx.wait()?;

        for (i, name, receiver) in receivers {
            receiver
                .recv()
                .context("readback callback was not called")?
                .context("failed to map readback buffer")?;
            let slice = self.output_readback_bufs[i].slice(..);
            let mapped = slice
                .get_mapped_range()
                .context("failed to access mapped output buffer")?;
            if mapped.len() != self.output_bytes[i] {
                bail!(
                    "readback size mismatch for `{name}`: got {} expected {}",
                    mapped.len(),
                    self.output_bytes[i]
                );
            }
            // Single copy: mapped ranges are at least 8-byte aligned, so the
            // f32 view is valid and we can skip the intermediate Vec<u8>.
            let data: Vec<f32> = bytemuck::cast_slice(&mapped).to_vec();
            drop(mapped);
            self.output_readback_bufs[i].unmap();
            result.insert(
                name.clone(),
                reference::Tensor::new(self.output_shapes[i].clone(), data)?,
            );
        }

        #[cfg(feature = "debug-logging")]
        {
            let readback_elapsed = t0.elapsed();
            eprintln!(
                "  compiled.run: encode+submit={:.1}ms readback={:.1}ms total={:.1}ms",
                submitted.submit_elapsed.as_secs_f64() * 1000.0,
                readback_elapsed.as_secs_f64() * 1000.0,
                (submitted.submit_elapsed + readback_elapsed).as_secs_f64() * 1000.0,
            );
        }

        Ok(result)
    }

    /// Run one inference pass. Uploads `input`, submits all ops in one command
    /// buffer, reads back the 3 model output heads.
    pub(crate) async fn run(
        &self,
        input: &reference::Tensor,
    ) -> Result<HashMap<String, reference::Tensor>> {
        let submitted = self.submit(input)?;
        self.await_result(submitted).await
    }

    pub(crate) async fn profile(&self, input: &reference::Tensor) -> Result<Vec<StepProfile>> {
        if !self.ctx.supports_timestamps {
            bail!("selected WGPU adapter does not support timestamp queries");
        }
        let query_count = (self.steps.len() * 2) as u32;
        let query_bytes = query_count as usize * std::mem::size_of::<u64>();

        let input_canonical = resolve_alias(&self.input_name, &self.aliases);
        let input_slot = *self
            .tensor_slots
            .get(&input_canonical)
            .context("compiled profile: missing resident slot for input buffer")?;
        let input_buf = self.slot_buffers.get(input_slot).with_context(|| {
            format!("compiled profile: missing resident slot buffer {input_slot}")
        })?;
        self.ctx
            .queue
            .write_buffer(input_buf, 0, bytemuck::cast_slice(&input.data));

        let query_set =
            self.ctx
                .device
                .create_query_set(&crate::vision::wgpu::QuerySetDescriptor {
                    label: Some("compiled_step_timestamps"),
                    ty: crate::vision::wgpu::QueryType::Timestamp,
                    count: query_count,
                });
        let resolve_buffer =
            self.ctx
                .device
                .create_buffer(&crate::vision::wgpu::BufferDescriptor {
                    label: Some("compiled_step_timestamp_resolve"),
                    size: query_bytes as u64,
                    usage: crate::vision::wgpu::BufferUsages::QUERY_RESOLVE
                        | crate::vision::wgpu::BufferUsages::COPY_SRC,
                    mapped_at_creation: false,
                });
        let readback_buffer =
            self.ctx
                .device
                .create_buffer(&crate::vision::wgpu::BufferDescriptor {
                    label: Some("compiled_step_timestamp_readback"),
                    size: query_bytes as u64,
                    usage: crate::vision::wgpu::BufferUsages::MAP_READ
                        | crate::vision::wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });

        let mut encoder = self.ctx.device.create_command_encoder(
            &crate::vision::wgpu::CommandEncoderDescriptor {
                label: Some("compiled_profile"),
            },
        );
        for (index, step) in self.steps.iter().enumerate() {
            let begin = (index * 2) as u32;
            let end = begin + 1;
            let mut pass =
                encoder.begin_compute_pass(&crate::vision::wgpu::ComputePassDescriptor {
                    label: Some("compiled_profile_step"),
                    timestamp_writes: Some(crate::vision::wgpu::ComputePassTimestampWrites {
                        query_set: &query_set,
                        beginning_of_pass_write_index: Some(begin),
                        end_of_pass_write_index: Some(end),
                    }),
                });
            pass.set_pipeline(&step.pipeline);
            pass.set_bind_group(0, &step.bind_group, &[]);
            pass.dispatch_workgroups(step.dispatch[0], step.dispatch[1], step.dispatch[2]);
        }
        encoder.resolve_query_set(&query_set, 0..query_count, &resolve_buffer, 0);
        encoder.copy_buffer_to_buffer(&resolve_buffer, 0, &readback_buffer, 0, query_bytes as u64);
        self.ctx.queue.submit(Some(encoder.finish()));

        let raw = map_readback(&self.ctx, &readback_buffer, query_bytes).await?;
        let ticks: Vec<u64> = bytemuck::cast_slice(&raw).to_vec();
        let period_ns = self.ctx.queue.get_timestamp_period() as f64;
        let mut profiles = Vec::with_capacity(self.steps.len());
        for (index, step) in self.steps.iter().enumerate() {
            let start = ticks[index * 2];
            let end = ticks[index * 2 + 1];
            let gpu_ms = end.saturating_sub(start) as f64 * period_ns / 1_000_000.0;
            profiles.push(StepProfile {
                step_index: step.step_index,
                op_index: step.op_index,
                op_name: step.op_name.clone(),
                op_kind: step.op_kind.clone(),
                output_shape: step.output_shape.clone(),
                dispatch: step.dispatch,
                gpu_ms,
            });
        }
        Ok(profiles)
    }

    /// One-shot timestamp profile that decomposes per-page GPU time into kernel
    /// "busy" time vs inter-dispatch idle ("gaps"). Each op runs in its own compute
    /// pass — identical to the real `submit()` path — so pass-boundary barriers are
    /// captured. Gated behind `LEGE_INFERENCE_PROFILE`; prints a summary to stderr.
    ///
    /// Interpretation:
    /// - busy ≈ span ≈ wall  → kernels are the cost (optimize shaders).
    /// - busy ≪ span         → GPU sits idle between dispatches (barrier/sync stalls).
    /// - span ≪ wall         → cost is CPU-side submit/poll, not the GPU timeline.
    pub(crate) async fn profile_report(&self, input: &reference::Tensor) -> Result<String> {
        if !self.ctx.supports_timestamps {
            bail!("selected WGPU adapter does not support timestamp queries");
        }
        let wall0 = std::time::Instant::now();
        let profiles = self.profile(input).await?;
        let wall_ms = wall0.elapsed().as_secs_f64() * 1000.0;

        // Re-run capturing raw ticks so we can measure the GPU-timeline span (and
        // thus the idle gaps between dispatches), which the per-step durations alone
        // cannot show.
        let query_count = (self.steps.len() * 2) as u32;
        let query_bytes = query_count as usize * std::mem::size_of::<u64>();
        let input_canonical = resolve_alias(&self.input_name, &self.aliases);
        let input_slot = *self
            .tensor_slots
            .get(&input_canonical)
            .context("profile_report: missing resident slot for input buffer")?;
        let input_buf = self
            .slot_buffers
            .get(input_slot)
            .context("profile_report: missing resident input buffer")?;
        self.ctx
            .queue
            .write_buffer(input_buf, 0, bytemuck::cast_slice(&input.data));

        let query_set =
            self.ctx
                .device
                .create_query_set(&crate::vision::wgpu::QuerySetDescriptor {
                    label: Some("profile_report_ts"),
                    ty: crate::vision::wgpu::QueryType::Timestamp,
                    count: query_count,
                });
        let resolve_buffer =
            self.ctx
                .device
                .create_buffer(&crate::vision::wgpu::BufferDescriptor {
                    label: Some("profile_report_resolve"),
                    size: query_bytes as u64,
                    usage: crate::vision::wgpu::BufferUsages::QUERY_RESOLVE
                        | crate::vision::wgpu::BufferUsages::COPY_SRC,
                    mapped_at_creation: false,
                });
        let readback_buffer =
            self.ctx
                .device
                .create_buffer(&crate::vision::wgpu::BufferDescriptor {
                    label: Some("profile_report_readback"),
                    size: query_bytes as u64,
                    usage: crate::vision::wgpu::BufferUsages::MAP_READ
                        | crate::vision::wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });
        let mut encoder = self.ctx.device.create_command_encoder(
            &crate::vision::wgpu::CommandEncoderDescriptor {
                label: Some("profile_report"),
            },
        );
        for (index, step) in self.steps.iter().enumerate() {
            let begin = (index * 2) as u32;
            let mut pass =
                encoder.begin_compute_pass(&crate::vision::wgpu::ComputePassDescriptor {
                    label: Some("profile_report_step"),
                    timestamp_writes: Some(crate::vision::wgpu::ComputePassTimestampWrites {
                        query_set: &query_set,
                        beginning_of_pass_write_index: Some(begin),
                        end_of_pass_write_index: Some(begin + 1),
                    }),
                });
            pass.set_pipeline(&step.pipeline);
            pass.set_bind_group(0, &step.bind_group, &[]);
            pass.dispatch_workgroups(step.dispatch[0], step.dispatch[1], step.dispatch[2]);
        }
        encoder.resolve_query_set(&query_set, 0..query_count, &resolve_buffer, 0);
        encoder.copy_buffer_to_buffer(&resolve_buffer, 0, &readback_buffer, 0, query_bytes as u64);
        self.ctx.queue.submit(Some(encoder.finish()));
        let raw = map_readback(&self.ctx, &readback_buffer, query_bytes).await?;
        let ticks: Vec<u64> = bytemuck::cast_slice(&raw).to_vec();
        let period_ns = self.ctx.queue.get_timestamp_period() as f64;
        let to_ms = |ticks: u64| ticks as f64 * period_ns / 1_000_000.0;

        let busy_ms: f64 = profiles.iter().map(|p| p.gpu_ms).sum();
        let first_begin = ticks.iter().step_by(2).copied().min().unwrap_or(0);
        let last_end = ticks
            .iter()
            .skip(1)
            .step_by(2)
            .copied()
            .max()
            .unwrap_or(first_begin);
        let span_ms = to_ms(last_end.saturating_sub(first_begin));
        let gap_ms = (span_ms - busy_ms).max(0.0);

        // Rank ops by total GPU busy time.
        let mut by_kind: HashMap<String, (f64, usize)> = HashMap::new();
        for p in &profiles {
            let e = by_kind.entry(p.op_kind.clone()).or_insert((0.0, 0));
            e.0 += p.gpu_ms;
            e.1 += 1;
        }
        let mut ranked: Vec<(String, f64, usize)> =
            by_kind.into_iter().map(|(k, (ms, n))| (k, ms, n)).collect();
        ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        let mut out = String::new();
        out.push_str("\n=== LEGE_INFERENCE_PROFILE (one-shot) ===\n");
        out.push_str(&format!(
            "  steps={}  GPU busy(kernels)={:.1}ms  GPU span={:.1}ms  GPU idle gaps={:.1}ms  profile wall={:.1}ms\n",
            self.steps.len(),
            busy_ms,
            span_ms,
            gap_ms,
            wall_ms,
        ));
        let gap_pct = if span_ms > 0.0 {
            gap_ms / span_ms * 100.0
        } else {
            0.0
        };
        out.push_str(&format!(
            "  interpretation: {:.0}% of GPU-timeline span is inter-dispatch idle (barrier/sync). High => submission/barrier bound, not kernels.\n",
            gap_pct,
        ));
        out.push_str("  top ops by GPU busy time:\n");
        for (kind, ms, n) in ranked.into_iter().take(10) {
            out.push_str(&format!(
                "    {:<28} {:>7.1}ms  ({} dispatches)\n",
                kind, ms, n
            ));
        }
        out.push_str(
            "  compare GPU span above against the `compiled.run: ... readback=` wall time.\n",
        );
        out.push_str("=========================================\n");
        Ok(out)
    }
}

fn resolve_alias(name: &str, aliases: &HashMap<String, String>) -> String {
    let mut cur = name;
    for _ in 0..32 {
        // limit chain depth
        match aliases.get(cur) {
            Some(target) => cur = target,
            None => break,
        }
    }
    cur.to_owned()
}

fn step_profile_kind(kind: &PlannedOpKind, wgsl: &'static str) -> String {
    if wgsl == SILU_WGSL || wgsl == SILU_VEC4_WGSL {
        "SiLU".to_owned()
    } else if wgsl == crate::vision::ops::winograd::WINOGRAD_INPUT_TRANSFORM_WGSL {
        "WinogradInput".to_owned()
    } else if wgsl == crate::vision::ops::winograd::WINOGRAD_BATCHED_GEMM_WGSL {
        "WinogradGemm".to_owned()
    } else if wgsl == crate::vision::ops::winograd::WINOGRAD_OUTPUT_TRANSFORM_WGSL {
        "WinogradOutput".to_owned()
    } else if wgsl == crate::vision::ops::conv::CONV1X1_GEMM_S1_VEC4_WGSL {
        conv_profile_label("Conv1x1GemmS1Vec4", kind)
    } else if wgsl == crate::vision::ops::conv::CONV1X1_GEMM_S1_WGSL {
        conv_profile_label("Conv1x1GemmS1", kind)
    } else if wgsl == crate::vision::ops::conv::CONV1X1_BLOCK2X4_S1_WGSL {
        conv_profile_label("Conv1x1Block2x4S1", kind)
    } else if wgsl == crate::vision::ops::conv::CONV1X1_BLOCK2X4_WGSL {
        conv_profile_label("Conv1x1Block2x4", kind)
    } else if wgsl == crate::vision::ops::conv::CONV1X1_BLOCK4X2_WGSL {
        conv_profile_label("Conv1x1Block4x2", kind)
    } else if wgsl == crate::vision::ops::conv::CONV1X1_BLOCK2X2_WGSL {
        conv_profile_label("Conv1x1Block2x2", kind)
    } else if wgsl == crate::vision::ops::conv::CONV1X1_WGSL {
        conv_profile_label("Conv1x1Gemm", kind)
    } else if wgsl == crate::vision::ops::conv::CONV3X3_TILED_WGSL {
        conv_profile_label("Conv3x3Tiled", kind)
    } else if wgsl == crate::vision::ops::conv::CONV3X3_CO8_SP2X2_S1D1_VEC4LOAD_WGSL {
        conv_profile_label("Conv3x3Co8Sp2x2Vec4Load", kind)
    } else if wgsl == crate::vision::ops::conv::CONV3X3_CO8_SP2X2_S1D1_WGSL {
        conv_profile_label("Conv3x3Co8Sp2x2", kind)
    } else if wgsl == crate::vision::ops::conv::CONV3X3_CO8_SP2X2_DIL_WGSL {
        conv_profile_label("Conv3x3Co8Sp2x2Dil", kind)
    } else if wgsl == crate::vision::ops::conv::CONV3X3_CO8_SP1X2_WGSL {
        conv_profile_label("Conv3x3Co8Sp1x2", kind)
    } else if wgsl == crate::vision::ops::conv::CONV3X3_COBLOCK8_WGSL {
        conv_profile_label("Conv3x3Co8", kind)
    } else if wgsl == crate::vision::ops::conv::CONV3X3_COBLOCK4_WGSL {
        conv_profile_label("Conv3x3Co4", kind)
    } else if wgsl == crate::vision::ops::conv::CONV5X5_CO8_SP2X1_WGSL {
        conv_profile_label("Conv5x5Co8Sp2x1", kind)
    } else if wgsl == crate::vision::ops::conv::CONV5X5_CO8_SP1X2_WGSL {
        conv_profile_label("Conv5x5Co8Sp1x2", kind)
    } else if wgsl == crate::vision::ops::conv::CONV5X5_CO8_SP2X2_DIL_WGSL {
        conv_profile_label("Conv5x5Co8Sp2x2Dil", kind)
    } else if wgsl == crate::vision::ops::conv::CONV5X5_CO8_SP2X2_D1_WGSL {
        conv_profile_label("Conv5x5Co8Sp2x2D1", kind)
    } else if wgsl == crate::vision::ops::conv::CONV5X5_COBLOCK8_WGSL {
        conv_profile_label("Conv5x5Co8", kind)
    } else if wgsl == crate::vision::ops::conv::CONV5X5_COBLOCK4_WGSL {
        conv_profile_label("Conv5x5Co4", kind)
    } else if wgsl == crate::vision::ops::conv::DEPTHWISE3X3_WGSL {
        conv_profile_label("Depthwise3x3", kind)
    } else if wgsl == crate::vision::ops::conv::CONV2D_WGSL {
        conv_profile_label("Conv2dFallback", kind)
    } else {
        kind.label().to_owned()
    }
}

fn conv_profile_label(prefix: &str, kind: &PlannedOpKind) -> String {
    if let PlannedOpKind::Conv2d(plan) = kind {
        format!(
            "{} g{} k{}x{} s{}x{} d{}x{}",
            prefix,
            plan.group,
            plan.kernel_shape[0],
            plan.kernel_shape[1],
            plan.strides[0],
            plan.strides[1],
            plan.dilations[0],
            plan.dilations[1]
        )
    } else {
        prefix.to_owned()
    }
}

fn compiled_chunk_size(is_cpu_adapter: bool, step_count: usize) -> usize {
    let env_value = std::env::var("WGPU_COMPILED_CHUNK_SIZE")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|&value| value > 0);
    env_value.unwrap_or_else(|| if is_cpu_adapter { 1 } else { step_count.max(1) })
}
