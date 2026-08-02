//! Build-time op descriptors for the resident GPU executor.
//! Maps each PlannedOp to one or more StepSpecs without touching the GPU.

use anyhow::{Result, bail};

use super::common::{broadcast_strides_u32, c_strides_raw, c_strides_u32, linear_grid};
use super::conv::conv_out_usize;
use crate::vision::onnx::types::{ElementwiseKind, PlannedOp, PlannedOpKind, UnaryKind};
use crate::vision::reference;

/// Wraps `linear_grid` into the `[u32; 3]` shape StepSpec stores.
fn grid(groups: usize) -> [u32; 3] {
    let (x, y, z) = linear_grid(groups);
    [x, y, z]
}

/// A single GPU dispatch step: which shader, which buffers, precomputed params.
pub(crate) struct StepSpec {
    pub wgsl: &'static str,
    /// Number of read-only input bindings (bindings 0..n_read_inputs).
    pub n_read_inputs: usize,
    /// Tensor buffer names to bind as read-only inputs (binding order matches).
    pub input_buf_names: Vec<String>,
    /// Tensor buffer name to bind as the read_write output.
    pub output_buf_name: String,
    /// Pre-computed params bytes (passed as a storage buffer).
    pub params: Vec<u8>,
    /// dispatch_workgroups(x, y, z).
    pub dispatch: [u32; 3],
}

/// Decomposes a PlannedOp into zero or more StepSpecs.
///
/// Returns an empty Vec for ops that are purely metadata (Identity, Reshape) —
/// the caller must record the output as an alias of the input buffer.
///
/// `dummy_bias_name` is the name of a pre-allocated 1-element zero buffer used
/// for Conv ops that have no bias tensor.
pub(crate) fn op_steps(op: &PlannedOp, dummy_bias_name: &str) -> Result<Vec<StepSpec>> {
    let in_shapes: Vec<Vec<usize>> = op
        .input_shapes
        .iter()
        .map(|s| s.iter().map(|&d| d as usize).collect())
        .collect();
    let out_shapes: Vec<Vec<usize>> = op
        .output_shapes
        .iter()
        .map(|s| s.iter().map(|&d| d as usize).collect())
        .collect();

    match &op.kind {
        // ── No GPU work: caller records alias ─────────────────────────────
        PlannedOpKind::Unary(UnaryKind::Identity)
        | PlannedOpKind::Reshape { .. }
        | PlannedOpKind::Unsqueeze { .. }
        | PlannedOpKind::Squeeze { .. } => Ok(vec![]),

        // ── Winograd F(2,3) input transform ──────────────────────────────
        PlannedOpKind::WinogradInputTransform => {
            let xs = &in_shapes[0];
            let cin = xs[1];
            let h = xs[2];
            let w = xs[3];
            let ntw = w.div_ceil(2);
            let nth = h.div_ceil(2);
            let p = ntw * nth;
            let params = [cin as u32, h as u32, w as u32, ntw as u32, nth as u32];
            Ok(vec![StepSpec {
                wgsl: super::winograd::WINOGRAD_INPUT_TRANSFORM_WGSL,
                n_read_inputs: 1,
                input_buf_names: vec![op.inputs[0].clone()],
                output_buf_name: op.outputs[0].clone(),
                params: bytemuck::cast_slice(&params).to_vec(),
                dispatch: [(cin * p).div_ceil(256) as u32, 1, 1],
            }])
        }

        // ── Winograd F(2,3) batched 16-GEMM ──────────────────────────────
        PlannedOpKind::WinogradBatchedGemm => {
            let us = &in_shapes[0];
            let vs = &in_shapes[1];
            let cout = us[1];
            let cin = us[2];
            let p = vs[2];
            let params = [cout as u32, cin as u32, p as u32];
            Ok(vec![StepSpec {
                wgsl: super::winograd::WINOGRAD_BATCHED_GEMM_WGSL,
                n_read_inputs: 2,
                input_buf_names: vec![op.inputs[0].clone(), op.inputs[1].clone()],
                output_buf_name: op.outputs[0].clone(),
                params: bytemuck::cast_slice(&params).to_vec(),
                dispatch: [cout.div_ceil(64) as u32, p.div_ceil(64) as u32, 16],
            }])
        }

        // ── Winograd F(2,3) output transform ─────────────────────────────
        PlannedOpKind::WinogradOutputTransform { use_bias, .. } => {
            let ms = &in_shapes[0];
            let ys = &out_shapes[0];
            let cout = ms[1];
            let h = ys[2];
            let w = ys[3];
            let ntw = w.div_ceil(2);
            let nth = h.div_ceil(2);
            let p = ntw * nth;
            let bias_name = if *use_bias {
                op.inputs[1].clone()
            } else {
                dummy_bias_name.to_owned()
            };
            let params = [
                cout as u32,
                h as u32,
                w as u32,
                ntw as u32,
                nth as u32,
                *use_bias as u32,
            ];
            Ok(vec![StepSpec {
                wgsl: super::winograd::WINOGRAD_OUTPUT_TRANSFORM_WGSL,
                n_read_inputs: 2,
                input_buf_names: vec![op.inputs[0].clone(), bias_name],
                output_buf_name: op.outputs[0].clone(),
                params: bytemuck::cast_slice(&params).to_vec(),
                dispatch: [(cout * p).div_ceil(256) as u32, 1, 1],
            }])
        }

        // ── Sigmoid ───────────────────────────────────────────────────────
        PlannedOpKind::Unary(UnaryKind::Sigmoid) => {
            let len = in_shapes[0].iter().product::<usize>();
            // Use vec4 when the element count is 4-aligned; the fuse logic in
            // compiled.rs detects this pointer and picks SILU_VEC4_WGSL instead.
            let (wgsl, p, disp) = if len % 4 == 0 {
                let len4 = (len / 4) as u32;
                (
                    super::sigmoid::SIGMOID_VEC4_WGSL,
                    len4.to_le_bytes().to_vec(),
                    [(len / 4).div_ceil(256) as u32, 1, 1],
                )
            } else {
                (
                    super::sigmoid::SIGMOID_WGSL,
                    (len as u32).to_le_bytes().to_vec(),
                    [len.div_ceil(256) as u32, 1, 1],
                )
            };
            Ok(vec![StepSpec {
                wgsl,
                n_read_inputs: 1,
                input_buf_names: vec![op.inputs[0].clone()],
                output_buf_name: op.outputs[0].clone(),
                params: p,
                dispatch: disp,
            }])
        }

        // ── Pointwise activations (Relu / HardSwish / HardSigmoid / Sqrt / Pow) ─
        PlannedOpKind::Unary(kind @ (UnaryKind::Relu | UnaryKind::HardSwish | UnaryKind::Sqrt))
        | PlannedOpKind::Unary(kind @ UnaryKind::HardSigmoid { .. })
        | PlannedOpKind::Unary(kind @ UnaryKind::Pow { .. }) => {
            let len = in_shapes[0].iter().product::<usize>();
            let (wgsl, params) = match kind {
                UnaryKind::Relu => (super::activations::RELU_WGSL, vec![len as u32]),
                UnaryKind::HardSwish => (super::activations::HARDSWISH_WGSL, vec![len as u32]),
                UnaryKind::Sqrt => (super::activations::SQRT_WGSL, vec![len as u32]),
                UnaryKind::HardSigmoid { alpha, beta } => (
                    super::activations::HARDSIGMOID_WGSL,
                    vec![len as u32, alpha.to_bits(), beta.to_bits()],
                ),
                UnaryKind::Pow { exponent } => (
                    super::activations::POW_WGSL,
                    vec![len as u32, exponent.to_bits()],
                ),
                _ => unreachable!(),
            };
            Ok(vec![StepSpec {
                wgsl,
                n_read_inputs: 1,
                input_buf_names: vec![op.inputs[0].clone()],
                output_buf_name: op.outputs[0].clone(),
                params: bytemuck::cast_slice(&params).to_vec(),
                dispatch: grid(len.div_ceil(256)),
            }])
        }

        // ── GlobalAveragePool ─────────────────────────────────────────────
        PlannedOpKind::GlobalAveragePool => {
            let shape = &in_shapes[0];
            if shape.len() < 2 {
                bail!("GlobalAveragePool expects rank >= 2");
            }
            let num_planes = shape[0] * shape[1];
            let plane = shape[2..].iter().product::<usize>().max(1);
            Ok(vec![StepSpec {
                wgsl: super::globalavgpool::GLOBALAVGPOOL_WGSL,
                n_read_inputs: 1,
                input_buf_names: vec![op.inputs[0].clone()],
                output_buf_name: op.outputs[0].clone(),
                params: bytemuck::cast_slice(&[num_planes as u32, plane as u32]).to_vec(),
                dispatch: [num_planes.div_ceil(256) as u32, 1, 1],
            }])
        }

        // ── Gemm (2D matmul + bias) ───────────────────────────────────────
        PlannedOpKind::Gemm {
            alpha,
            beta,
            trans_a,
            trans_b,
        } => {
            let a = &in_shapes[0];
            let b = &in_shapes[1];
            let (m, ka) = if *trans_a { (a[1], a[0]) } else { (a[0], a[1]) };
            let (_kb, n) = if *trans_b { (b[1], b[0]) } else { (b[0], b[1]) };
            let has_bias = op.inputs.len() >= 3;
            let bias_name = if has_bias {
                op.inputs[2].clone()
            } else {
                dummy_bias_name.to_owned()
            };
            let bias_len = if has_bias {
                in_shapes[2].iter().product::<usize>()
            } else {
                0
            };
            let params = [
                m as u32,
                n as u32,
                ka as u32,
                *trans_a as u32,
                *trans_b as u32,
                has_bias as u32,
                bias_len as u32,
                alpha.to_bits(),
                beta.to_bits(),
            ];
            Ok(vec![StepSpec {
                wgsl: super::gemm::GEMM_WGSL,
                n_read_inputs: 3,
                input_buf_names: vec![op.inputs[0].clone(), op.inputs[1].clone(), bias_name],
                output_buf_name: op.outputs[0].clone(),
                params: bytemuck::cast_slice(&params).to_vec(),
                dispatch: [(m * n).div_ceil(256) as u32, 1, 1],
            }])
        }

        // ── Elementwise binary ────────────────────────────────────────────
        PlannedOpKind::Elementwise(k) => {
            let out_shape = reference::broadcast_shape(&in_shapes[0], &in_shapes[1])
                .map_err(|e| anyhow::anyhow!("elemwise compile: {e}"))?;
            let num_elems = out_shape.iter().product::<usize>();
            let op_code: u32 = match k {
                ElementwiseKind::Add => 0,
                ElementwiseKind::Mul => 1,
                ElementwiseKind::Sub => 2,
                ElementwiseKind::Div => 3,
                ElementwiseKind::Max => 4,
            };

            // vec4 fast path: same-shape (no broadcast), element count 4-aligned.
            if in_shapes[0] == out_shape && in_shapes[1] == out_shape && num_elems % 4 == 0 {
                let len4 = (num_elems / 4) as u32;
                return Ok(vec![StepSpec {
                    wgsl: super::elemwise::ELEMENTWISE_SAME_VEC4_WGSL,
                    n_read_inputs: 2,
                    input_buf_names: op.inputs.clone(),
                    output_buf_name: op.outputs[0].clone(),
                    params: bytemuck::cast_slice(&[len4, op_code]).to_vec(),
                    dispatch: grid((num_elems / 4).div_ceil(256)),
                }]);
            }

            // General broadcast path (scalar).
            let rank = out_shape.len();
            if rank > 6 {
                bail!("elemwise compile: rank {rank} > 6");
            }
            let out_s = c_strides_u32(&out_shape);
            let in0_s = broadcast_strides_u32(&out_shape, &in_shapes[0]);
            let in1_s = broadcast_strides_u32(&out_shape, &in_shapes[1]);
            let mut p = [0u32; 22];
            p[0] = num_elems as u32;
            p[1] = rank as u32;
            p[2] = op_code;
            p[4..10].copy_from_slice(&out_s);
            p[10..16].copy_from_slice(&in0_s);
            p[16..22].copy_from_slice(&in1_s);
            Ok(vec![StepSpec {
                wgsl: super::elemwise::ELEMENTWISE_WGSL,
                n_read_inputs: 2,
                input_buf_names: op.inputs.clone(),
                output_buf_name: op.outputs[0].clone(),
                params: bytemuck::cast_slice(&p).to_vec(),
                dispatch: grid(num_elems.div_ceil(256)),
            }])
        }

        // ── Transpose ─────────────────────────────────────────────────────
        PlannedOpKind::Transpose { perm } => {
            let shape = &in_shapes[0];
            let rank = shape.len();
            if rank > 6 {
                bail!("transpose compile: rank {rank} > 6");
            }
            let out_shape: Vec<usize> = perm.iter().map(|&p| shape[p]).collect();
            let num_elems = out_shape.iter().product::<usize>();
            let out_s = c_strides_u32(&out_shape);
            let in_s_raw = c_strides_raw(shape);
            let mut in_perm_s = [0u32; 6];
            for d in 0..rank {
                in_perm_s[d] = in_s_raw[perm[d]] as u32;
            }
            let mut p = [0u32; 16];
            p[0] = num_elems as u32;
            p[1] = rank as u32;
            p[4..10].copy_from_slice(&out_s);
            p[10..16].copy_from_slice(&in_perm_s);
            Ok(vec![StepSpec {
                wgsl: super::transpose::TRANSPOSE_WGSL,
                n_read_inputs: 1,
                input_buf_names: vec![op.inputs[0].clone()],
                output_buf_name: op.outputs[0].clone(),
                params: bytemuck::cast_slice(&p).to_vec(),
                dispatch: grid(num_elems.div_ceil(256)),
            }])
        }

        // ── Concat: one sub-dispatch per input ────────────────────────────
        PlannedOpKind::Concat { axis } => {
            let axis = *axis;
            let out_shape = &out_shapes[0];
            let total_axis = out_shape[axis];
            let inner_stride: usize = out_shape[axis + 1..].iter().product();
            let mut steps = Vec::with_capacity(op.inputs.len());
            let mut axis_offset: u32 = 0;
            for (i, inp_shape) in in_shapes.iter().enumerate() {
                let local_elems: usize = inp_shape.iter().product();
                let local_axis = inp_shape[axis] as u32;
                let p: [u32; 8] = [
                    local_elems as u32,
                    inner_stride as u32,
                    local_axis,
                    axis_offset,
                    total_axis as u32,
                    0,
                    0,
                    0,
                ];
                steps.push(StepSpec {
                    wgsl: super::concat::CONCAT_SLICE_WGSL,
                    n_read_inputs: 1,
                    input_buf_names: vec![op.inputs[i].clone()],
                    output_buf_name: op.outputs[0].clone(),
                    params: bytemuck::cast_slice(&p).to_vec(),
                    dispatch: grid(local_elems.div_ceil(256)),
                });
                axis_offset += local_axis;
            }
            Ok(steps)
        }

        // ── Split: one sub-dispatch per output ────────────────────────────
        PlannedOpKind::Split { axis, sizes } => {
            let axis = *axis;
            let in_shape = &in_shapes[0];
            let total_axis = in_shape[axis];
            let inner_stride: usize = in_shape[axis + 1..].iter().product();
            let mut steps = Vec::with_capacity(sizes.len());
            let mut axis_offset: u32 = 0;
            for (i, &size) in sizes.iter().enumerate() {
                let local_axis = size as usize;
                let outer: usize = in_shape[..axis].iter().product();
                let num_elems = outer * local_axis * inner_stride;
                let p: [u32; 8] = [
                    num_elems as u32,
                    inner_stride as u32,
                    local_axis as u32,
                    axis_offset,
                    total_axis as u32,
                    0,
                    0,
                    0,
                ];
                steps.push(StepSpec {
                    wgsl: super::split::SPLIT_SLICE_WGSL,
                    n_read_inputs: 1,
                    input_buf_names: vec![op.inputs[0].clone()],
                    output_buf_name: op.outputs[i].clone(),
                    params: bytemuck::cast_slice(&p).to_vec(),
                    dispatch: [num_elems.div_ceil(256) as u32, 1, 1],
                });
                axis_offset += local_axis as u32;
            }
            Ok(steps)
        }

        // ── Slice ─────────────────────────────────────────────────────────
        PlannedOpKind::Slice {
            axes,
            starts,
            ends,
            steps,
        } => {
            let in_shape = &in_shapes[0];
            let rank = in_shape.len();
            if rank > 6 {
                bail!("slice compile: rank {rank} > 6");
            }
            let mut out_shape = in_shape.clone();
            let mut in_start_arr = [0u32; 6];
            let mut in_step_arr = [1u32; 6];
            for (&axis, (&s_raw, (&e_raw, &step_raw))) in axes
                .iter()
                .zip(starts.iter().zip(ends.iter().zip(steps.iter())))
            {
                let dim = in_shape[axis];
                let start = norm_slice(s_raw, dim);
                let end = norm_slice(e_raw, dim);
                let step = step_raw as usize;
                out_shape[axis] = end.saturating_sub(start).div_ceil(step);
                in_start_arr[axis] = start as u32;
                in_step_arr[axis] = step as u32;
            }
            let num_out = out_shape.iter().product::<usize>();
            let out_s = c_strides_u32(&out_shape);
            let in_s = c_strides_u32(in_shape);
            let mut p = [0u32; 28];
            p[0] = num_out as u32;
            p[1] = rank as u32;
            p[4..10].copy_from_slice(&out_s);
            p[10..16].copy_from_slice(&in_start_arr);
            p[16..22].copy_from_slice(&in_step_arr);
            p[22..28].copy_from_slice(&in_s);
            Ok(vec![StepSpec {
                wgsl: super::slice::SLICE_WGSL,
                n_read_inputs: 1,
                input_buf_names: vec![op.inputs[0].clone()],
                output_buf_name: op.outputs[0].clone(),
                params: bytemuck::cast_slice(&p).to_vec(),
                dispatch: grid(num_out.div_ceil(256)),
            }])
        }

        // ── Conv2d ────────────────────────────────────────────────────────
        PlannedOpKind::Conv2d(plan) => {
            let xs = &in_shapes[0];
            let ws = &in_shapes[1];
            let n = xs[0];
            let cin = xs[1];
            let hin = xs[2];
            let win = xs[3];
            let cout = ws[0];
            let cin_per_group = ws[1];
            let kh = ws[2];
            let kw = ws[3];
            let group = plan.group as usize;
            let hout = conv_out_usize(
                hin,
                plan.pads[0],
                plan.pads[2],
                plan.dilations[0],
                kh,
                plan.strides[0],
            )?;
            let wout = conv_out_usize(
                win,
                plan.pads[1],
                plan.pads[3],
                plan.dilations[1],
                kw,
                plan.strides[1],
            )?;
            let has_bias = op.inputs.len() >= 3;
            let bias_name = if has_bias {
                op.inputs[2].clone()
            } else {
                dummy_bias_name.to_owned()
            };

            // Optimized kernels below assume batch 1; route N>1 to the
            // batch-aware naive convolution.
            if n > 1 {
                let cout_per_group = cout / group;
                let num_out = n * cout * hout * wout;
                let p = [
                    num_out as u32,
                    cin as u32,
                    hin as u32,
                    win as u32,
                    cout as u32,
                    cin_per_group as u32,
                    kh as u32,
                    kw as u32,
                    hout as u32,
                    wout as u32,
                    plan.strides[0] as u32,
                    plan.strides[1] as u32,
                    plan.pads[0] as u32,
                    plan.pads[1] as u32,
                    plan.dilations[0] as u32,
                    plan.dilations[1] as u32,
                    has_bias as u32,
                    cout_per_group as u32,
                    n as u32,
                ];
                return Ok(vec![StepSpec {
                    wgsl: super::conv::CONV2D_WGSL,
                    n_read_inputs: 3,
                    input_buf_names: vec![op.inputs[0].clone(), op.inputs[1].clone(), bias_name],
                    output_buf_name: op.outputs[0].clone(),
                    params: bytemuck::cast_slice(&p).to_vec(),
                    dispatch: grid(num_out.div_ceil(256)),
                }]);
            }

            // 1×1, group=1, no padding, no dilation → tiled GEMM
            if kh == 1
                && kw == 1
                && group == 1
                && plan.pads == [0, 0, 0, 0]
                && plan.dilations == [1, 1]
            {
                let s1 = plan.strides == [1, 1] && hin == hout && win == wout;
                let plane = hout * wout;
                // vec4 path when both contraction (cin) and plane are 4-aligned,
                // so the buffers can be viewed as array<vec4<f32>>.
                let gemm_wgsl = if cin % 4 == 0 && plane % 4 == 0 {
                    super::conv::CONV1X1_GEMM_S1_VEC4_WGSL
                } else {
                    super::conv::CONV1X1_GEMM_S1_WGSL
                };
                let (wgsl, p, dispatch) = if s1 {
                    (
                        gemm_wgsl,
                        vec![
                            cout as u32,
                            cin as u32,
                            (hout * wout) as u32,
                            has_bias as u32,
                        ],
                        [
                            cout.div_ceil(64) as u32,
                            (hout * wout).div_ceil(64) as u32,
                            1,
                        ],
                    )
                } else {
                    (
                        super::conv::CONV1X1_BLOCK2X4_WGSL,
                        vec![
                            cout as u32,
                            cin as u32,
                            hout as u32,
                            wout as u32,
                            hin as u32,
                            win as u32,
                            plan.strides[0] as u32,
                            plan.strides[1] as u32,
                            has_bias as u32,
                        ],
                        [
                            cout.div_ceil(32) as u32,
                            (hout * wout).div_ceil(64) as u32,
                            1,
                        ],
                    )
                };
                return Ok(vec![StepSpec {
                    wgsl,
                    n_read_inputs: 3,
                    input_buf_names: vec![op.inputs[0].clone(), op.inputs[1].clone(), bias_name],
                    output_buf_name: op.outputs[0].clone(),
                    params: bytemuck::cast_slice(&p).to_vec(),
                    dispatch,
                }]);
            }

            // Depthwise 3×3 → one output element per invocation, no shared barriers.
            if kh == 3
                && kw == 3
                && group == cin
                && cout == cin
                && cin_per_group == 1
                && plan.dilations == [1, 1]
            {
                let num_out = cout * hout * wout;
                let p = [
                    num_out as u32,
                    cout as u32,
                    hin as u32,
                    win as u32,
                    hout as u32,
                    wout as u32,
                    plan.strides[0] as u32,
                    plan.strides[1] as u32,
                    plan.pads[0] as u32,
                    plan.pads[1] as u32,
                    has_bias as u32,
                ];
                return Ok(vec![StepSpec {
                    wgsl: super::conv::DEPTHWISE3X3_WGSL,
                    n_read_inputs: 3,
                    input_buf_names: vec![op.inputs[0].clone(), op.inputs[1].clone(), bias_name],
                    output_buf_name: op.outputs[0].clone(),
                    params: bytemuck::cast_slice(&p).to_vec(),
                    dispatch: [num_out.div_ceil(256) as u32, 1, 1],
                }]);
            }

            // Group=1 3×3 stride-1 dilation-1 → 2×2 spatial × 8-channel register tile.
            if kh == 3
                && kw == 3
                && group == 1
                && plan.strides == [1, 1]
                && plan.dilations == [1, 1]
            {
                let p = [
                    cout as u32,
                    cin as u32,
                    hin as u32,
                    win as u32,
                    hout as u32,
                    wout as u32,
                    plan.pads[0] as u32,
                    plan.pads[1] as u32,
                    has_bias as u32,
                ];
                return Ok(vec![StepSpec {
                    wgsl: super::conv::CONV3X3_CO8_SP2X2_S1D1_WGSL,
                    n_read_inputs: 3,
                    input_buf_names: vec![op.inputs[0].clone(), op.inputs[1].clone(), bias_name],
                    output_buf_name: op.outputs[0].clone(),
                    params: bytemuck::cast_slice(&p).to_vec(),
                    dispatch: [
                        wout.div_ceil(32) as u32,
                        hout.div_ceil(32) as u32,
                        cout.div_ceil(8) as u32,
                    ],
                }]);
            }

            // 3×3 stride-1, dilation 2..5: 2×2 spatial × 8-channel register tile
            // (32 accumulators), same arithmetic intensity as the s1d1 tile. Patch
            // (32+2d)² (d=5→42²=1764) fits the 1764-float smem budget.
            let dil = plan.dilations[0];
            if kh == 3
                && kw == 3
                && group == 1
                && plan.strides == [1, 1]
                && plan.dilations[0] == plan.dilations[1]
                && dil >= 2
                && dil <= super::conv::CONV3X3_DIL_MAX
            {
                // The shader's workgroup smem is sized for the max routed dilation;
                // a relaxed ceiling without a matching array resize is silent UB.
                debug_assert!(
                    (32 + 2 * dil as usize).pow(2) <= super::conv::CONV3X3_DIL_SMEM,
                    "CONV3X3_CO8_SP2X2_DIL smem overflow at dil={dil}"
                );
                let p = [
                    cout as u32,
                    cin as u32,
                    hin as u32,
                    win as u32,
                    hout as u32,
                    wout as u32,
                    plan.pads[0] as u32,
                    plan.pads[1] as u32,
                    has_bias as u32,
                    dil as u32,
                ];
                return Ok(vec![StepSpec {
                    wgsl: super::conv::CONV3X3_CO8_SP2X2_DIL_WGSL,
                    n_read_inputs: 3,
                    input_buf_names: vec![op.inputs[0].clone(), op.inputs[1].clone(), bias_name],
                    output_buf_name: op.outputs[0].clone(),
                    params: bytemuck::cast_slice(&p).to_vec(),
                    dispatch: [
                        wout.div_ceil(32) as u32,
                        hout.div_ceil(32) as u32,
                        cout.div_ceil(8) as u32,
                    ],
                }]);
            }

            // Group=1 3×3 → shared-memory tiled direct conv, eight output channels per workgroup.
            if kh == 3 && kw == 3 && group == 1 && super::conv::conv3x3_tile_patch_fits(plan) {
                let p = [
                    cout as u32,
                    cin as u32,
                    hin as u32,
                    win as u32,
                    hout as u32,
                    wout as u32,
                    plan.strides[0] as u32,
                    plan.strides[1] as u32,
                    plan.pads[0] as u32,
                    plan.pads[1] as u32,
                    plan.dilations[0] as u32,
                    plan.dilations[1] as u32,
                    has_bias as u32,
                    0,
                    0,
                ];
                return Ok(vec![StepSpec {
                    wgsl: super::conv::CONV3X3_COBLOCK8_WGSL,
                    n_read_inputs: 3,
                    input_buf_names: vec![op.inputs[0].clone(), op.inputs[1].clone(), bias_name],
                    output_buf_name: op.outputs[0].clone(),
                    params: bytemuck::cast_slice(&p).to_vec(),
                    dispatch: [
                        wout.div_ceil(16) as u32,
                        hout.div_ceil(16) as u32,
                        cout.div_ceil(8) as u32,
                    ],
                }]);
            }

            // 5×5 stride-1 dilation-1: 2×2 spatial × 8ch register tile (32 accumulators).
            // 36×36 smem = 1296 floats (exact same budget as generic 5×5 kernel).
            if kh == 5
                && kw == 5
                && group == 1
                && plan.strides == [1, 1]
                && plan.dilations == [1, 1]
            {
                let p = [
                    cout as u32,
                    cin as u32,
                    hin as u32,
                    win as u32,
                    hout as u32,
                    wout as u32,
                    plan.pads[0] as u32,
                    plan.pads[1] as u32,
                    has_bias as u32,
                ];
                return Ok(vec![StepSpec {
                    wgsl: super::conv::CONV5X5_CO8_SP2X2_D1_WGSL,
                    n_read_inputs: 3,
                    input_buf_names: vec![op.inputs[0].clone(), op.inputs[1].clone(), bias_name],
                    output_buf_name: op.outputs[0].clone(),
                    params: bytemuck::cast_slice(&p).to_vec(),
                    dispatch: [
                        wout.div_ceil(32) as u32,
                        hout.div_ceil(32) as u32,
                        cout.div_ceil(8) as u32,
                    ],
                }]);
            }

            // 5×5 stride-1, dilation 2..3: 2×2 spatial × 8ch (32 accumulators per
            // thread), same arithmetic intensity as the d1 tile. Patch (32+4d)²
            // (d=2→40²=1600, d=3→44²=1936) fits the 1936-float smem budget.
            let dil = plan.dilations[0];
            if kh == 5
                && kw == 5
                && group == 1
                && plan.strides == [1, 1]
                && plan.dilations[0] == plan.dilations[1]
                && dil >= 2
                && dil <= super::conv::CONV5X5_DIL_MAX
            {
                // The shader's workgroup smem is sized for the max routed dilation;
                // a relaxed ceiling without a matching array resize is silent UB.
                debug_assert!(
                    (32 + 4 * dil as usize).pow(2) <= super::conv::CONV5X5_DIL_SMEM,
                    "CONV5X5_CO8_SP2X2_DIL smem overflow at dil={dil}"
                );
                let p = [
                    cout as u32,
                    cin as u32,
                    hin as u32,
                    win as u32,
                    hout as u32,
                    wout as u32,
                    plan.pads[0] as u32,
                    plan.pads[1] as u32,
                    has_bias as u32,
                    dil as u32,
                ];
                return Ok(vec![StepSpec {
                    wgsl: super::conv::CONV5X5_CO8_SP2X2_DIL_WGSL,
                    n_read_inputs: 3,
                    input_buf_names: vec![op.inputs[0].clone(), op.inputs[1].clone(), bias_name],
                    output_buf_name: op.outputs[0].clone(),
                    params: bytemuck::cast_slice(&p).to_vec(),
                    dispatch: [
                        wout.div_ceil(32) as u32,
                        hout.div_ceil(32) as u32,
                        cout.div_ceil(8) as u32,
                    ],
                }]);
            }

            // Group=1 5×5 → shared-memory tiled direct conv, eight output channels per workgroup.
            if kh == 5 && kw == 5 && group == 1 {
                let p = [
                    cout as u32,
                    cin as u32,
                    hin as u32,
                    win as u32,
                    hout as u32,
                    wout as u32,
                    plan.strides[0] as u32,
                    plan.strides[1] as u32,
                    plan.pads[0] as u32,
                    plan.pads[1] as u32,
                    plan.dilations[0] as u32,
                    plan.dilations[1] as u32,
                    has_bias as u32,
                    0,
                    0,
                ];
                return Ok(vec![StepSpec {
                    wgsl: super::conv::CONV5X5_COBLOCK8_WGSL,
                    n_read_inputs: 3,
                    input_buf_names: vec![op.inputs[0].clone(), op.inputs[1].clone(), bias_name],
                    output_buf_name: op.outputs[0].clone(),
                    params: bytemuck::cast_slice(&p).to_vec(),
                    dispatch: [
                        wout.div_ceil(16) as u32,
                        hout.div_ceil(16) as u32,
                        cout.div_ceil(8) as u32,
                    ],
                }]);
            }

            // 3×3 → shared-memory tiled direct conv
            if kh == 3 && kw == 3 && super::conv::conv3x3_tile_patch_fits(plan) {
                let p = [
                    cout as u32,
                    cin as u32,
                    hin as u32,
                    win as u32,
                    hout as u32,
                    wout as u32,
                    kh as u32,
                    kw as u32,
                    plan.strides[0] as u32,
                    plan.strides[1] as u32,
                    plan.pads[0] as u32,
                    plan.pads[1] as u32,
                    plan.dilations[0] as u32,
                    plan.dilations[1] as u32,
                    has_bias as u32,
                    cin_per_group as u32,
                ];
                return Ok(vec![StepSpec {
                    wgsl: super::conv::CONV3X3_TILED_WGSL,
                    n_read_inputs: 3,
                    input_buf_names: vec![op.inputs[0].clone(), op.inputs[1].clone(), bias_name],
                    output_buf_name: op.outputs[0].clone(),
                    params: bytemuck::cast_slice(&p).to_vec(),
                    dispatch: [
                        wout.div_ceil(16) as u32,
                        hout.div_ceil(16) as u32,
                        cout as u32,
                    ],
                }]);
            }

            // Fallback: naive direct convolution (batch 1 here; N>1 handled above)
            let cout_per_group = cout / group;
            let num_out = cout * hout * wout;
            let p = [
                num_out as u32,
                cin as u32,
                hin as u32,
                win as u32,
                cout as u32,
                cin_per_group as u32,
                kh as u32,
                kw as u32,
                hout as u32,
                wout as u32,
                plan.strides[0] as u32,
                plan.strides[1] as u32,
                plan.pads[0] as u32,
                plan.pads[1] as u32,
                plan.dilations[0] as u32,
                plan.dilations[1] as u32,
                has_bias as u32,
                cout_per_group as u32,
                1u32,
            ];
            Ok(vec![StepSpec {
                wgsl: super::conv::CONV2D_WGSL,
                n_read_inputs: 3,
                input_buf_names: vec![op.inputs[0].clone(), op.inputs[1].clone(), bias_name],
                output_buf_name: op.outputs[0].clone(),
                params: bytemuck::cast_slice(&p).to_vec(),
                dispatch: grid(num_out.div_ceil(256)),
            }])
        }

        // ── MaxPool2d / AvgPool2d ─────────────────────────────────────────
        PlannedOpKind::MaxPool2d(plan) | PlannedOpKind::AvgPool2d(plan) => {
            let wgsl = if matches!(op.kind, PlannedOpKind::AvgPool2d(_)) {
                super::maxpool::AVGPOOL2D_WGSL
            } else {
                super::maxpool::MAXPOOL2D_WGSL
            };
            let s = &in_shapes[0];
            let channels = s[1];
            let hin = s[2];
            let win = s[3];
            let kh = plan.kernel_shape[0] as usize;
            let kw = plan.kernel_shape[1] as usize;
            let hout = conv_out_usize(
                hin,
                plan.pads[0],
                plan.pads[2],
                plan.dilations[0],
                kh,
                plan.strides[0],
            )?;
            let wout = conv_out_usize(
                win,
                plan.pads[1],
                plan.pads[3],
                plan.dilations[1],
                kw,
                plan.strides[1],
            )?;
            // Include N as well as C/H/W. The shader treats N*C as one flat
            // channel axis, so this also makes the supported batched rec graphs
            // compute every sample instead of only batch zero.
            let num_out = s[0] * channels * hout * wout;
            let p = [
                num_out as u32,
                channels as u32,
                hin as u32,
                win as u32,
                hout as u32,
                wout as u32,
                kh as u32,
                kw as u32,
                plan.strides[0] as u32,
                plan.strides[1] as u32,
                plan.pads[0] as u32,
                plan.pads[1] as u32,
                plan.dilations[0] as u32,
                plan.dilations[1] as u32,
            ];
            Ok(vec![StepSpec {
                wgsl,
                n_read_inputs: 1,
                input_buf_names: vec![op.inputs[0].clone()],
                output_buf_name: op.outputs[0].clone(),
                params: bytemuck::cast_slice(&p).to_vec(),
                dispatch: grid(num_out.div_ceil(256)),
            }])
        }

        // ── ResizeNearest ─────────────────────────────────────────────────
        PlannedOpKind::ResizeNearest { scales } => {
            let s = &in_shapes[0];
            let planes = s[0] * s[1];
            let hin = s[2];
            let win = s[3];
            let hout = (hin as f32 * scales[2]).floor() as usize;
            let wout = (win as f32 * scales[3]).floor() as usize;
            let num_out = planes * hout * wout;
            let p = [
                num_out as u32,
                planes as u32,
                hin as u32,
                win as u32,
                hout as u32,
                wout as u32,
                scales[2].to_bits(),
                scales[3].to_bits(),
            ];
            Ok(vec![StepSpec {
                wgsl: super::resize::RESIZE_NEAREST_WGSL,
                n_read_inputs: 1,
                input_buf_names: vec![op.inputs[0].clone()],
                output_buf_name: op.outputs[0].clone(),
                params: bytemuck::cast_slice(&p).to_vec(),
                dispatch: [num_out.div_ceil(256) as u32, 1, 1],
            }])
        }

        // ── MatMul ────────────────────────────────────────────────────────
        PlannedOpKind::MatMul => {
            // ONNX 1-D operand promotion (lhs [k]->[1,k], rhs [k]->[k,1]). The
            // squeezed output has the same contiguous element layout as the
            // promoted [.,m,n], so the kernel writes the right buffer either way.
            let (lhs, rhs) = {
                let l = &in_shapes[0];
                let r = &in_shapes[1];
                let lv = if l.len() == 1 {
                    vec![1, l[0]]
                } else {
                    l.clone()
                };
                let rv = if r.len() == 1 {
                    vec![r[0], 1]
                } else {
                    r.clone()
                };
                (lv, rv)
            };
            let lhs = &lhs;
            let rhs = &rhs;
            let m = lhs[lhs.len() - 2];
            let k = lhs[lhs.len() - 1];
            let n = rhs[rhs.len() - 1];
            let batch_shape =
                reference::broadcast_shape(&lhs[..lhs.len() - 2], &rhs[..rhs.len() - 2])
                    .map_err(|e| anyhow::anyhow!("matmul compile batch: {e}"))?;
            let mut out_shape = batch_shape.clone();
            out_shape.push(m);
            out_shape.push(n);
            if out_shape.len() > 6 {
                bail!("matmul compile: out rank {} > 6", out_shape.len());
            }
            let total_out = out_shape.iter().product::<usize>();
            let out_strides = c_strides_u32(&out_shape);
            let lhs_batch = matmul_batch_strides(&batch_shape, &lhs[..lhs.len() - 2], m * k)?;
            let rhs_batch = matmul_batch_strides(&batch_shape, &rhs[..rhs.len() - 2], k * n)?;
            let mut p = [0u32; 30];
            p[0] = total_out as u32;
            p[1] = out_shape.len() as u32;
            p[2] = lhs.len() as u32;
            p[3] = rhs.len() as u32;
            p[4] = batch_shape.len() as u32;
            p[5] = m as u32;
            p[6] = k as u32;
            p[7] = n as u32;
            p[8..14].copy_from_slice(&out_strides);
            p[14..20].copy_from_slice(&lhs_batch);
            p[20] = k as u32;
            p[21] = 1;
            p[22..28].copy_from_slice(&rhs_batch);
            p[28] = n as u32;
            p[29] = 1;
            Ok(vec![StepSpec {
                wgsl: super::matmul::MATMUL_WGSL,
                n_read_inputs: 2,
                input_buf_names: op.inputs.clone(),
                output_buf_name: op.outputs[0].clone(),
                params: bytemuck::cast_slice(&p).to_vec(),
                dispatch: [total_out.div_ceil(256) as u32, 1, 1],
            }])
        }

        // ── Softmax ───────────────────────────────────────────────────────
        PlannedOpKind::Softmax { axis } => {
            let s = &in_shapes[0];
            let axis = *axis;
            let outer: usize = s[..axis].iter().product();
            let dim = s[axis];
            let inner: usize = s[axis + 1..].iter().product();
            let row_count = outer * inner;
            let p = [row_count as u32, dim as u32, inner as u32, 0u32];
            Ok(vec![StepSpec {
                wgsl: super::softmax::SOFTMAX_WGSL,
                n_read_inputs: 1,
                input_buf_names: vec![op.inputs[0].clone()],
                output_buf_name: op.outputs[0].clone(),
                params: bytemuck::cast_slice(&p).to_vec(),
                dispatch: [row_count.div_ceil(256) as u32, 1, 1],
            }])
        }
        PlannedOpKind::PRelu => {
            let len = out_shapes[0].iter().product::<usize>();
            let mut p = [0u32; 22];
            p[0] = len as u32;
            p[1] = out_shapes[0].len() as u32;
            p[4..10].copy_from_slice(&c_strides_u32(&out_shapes[0]));
            p[10..16].copy_from_slice(&c_strides_u32(&in_shapes[0]));
            p[16..22].copy_from_slice(&broadcast_strides_u32(&out_shapes[0], &in_shapes[1]));
            Ok(vec![StepSpec {
                wgsl: super::deskew::PRELU_WGSL,
                n_read_inputs: 2,
                input_buf_names: op.inputs.clone(),
                output_buf_name: op.outputs[0].clone(),
                params: bytemuck::cast_slice(&p).to_vec(),
                dispatch: [len.div_ceil(256) as u32, 1, 1],
            }])
        }
        PlannedOpKind::Pad { pads, mode, value } => {
            let rank = in_shapes[0].len();
            if rank > 6 {
                bail!("Pad rank exceeds 6");
            }
            let len = out_shapes[0].iter().product::<usize>();
            let mode_u32 = match mode {
                crate::vision::onnx::types::PadMode::Constant => 0u32,
                crate::vision::onnx::types::PadMode::Reflect => 1u32,
            };
            let mut p = [0u32; 34];
            p[0] = len as u32;
            p[1] = rank as u32;
            p[2] = mode_u32;
            p[3] = value.to_bits();
            for axis in 0..rank {
                p[4 + axis] = out_shapes[0][axis] as u32;
                p[10 + axis] = in_shapes[0][axis] as u32;
                p[16 + axis] = pads[axis] as u32;
            }
            p[22..28].copy_from_slice(&c_strides_u32(&out_shapes[0]));
            p[28..34].copy_from_slice(&c_strides_u32(&in_shapes[0]));
            Ok(vec![StepSpec {
                wgsl: super::deskew::PAD_WGSL,
                n_read_inputs: 1,
                input_buf_names: vec![op.inputs[0].clone()],
                output_buf_name: op.outputs[0].clone(),
                params: bytemuck::cast_slice(&p).to_vec(),
                dispatch: grid(len.div_ceil(256)),
            }])
        }
        PlannedOpKind::ResizeLinear {
            sizes,
            align_corners,
        } => {
            let len = out_shapes[0].iter().product::<usize>();
            let p = [
                len as u32,
                in_shapes[0][0] as u32,
                in_shapes[0][1] as u32,
                in_shapes[0][2] as u32,
                in_shapes[0][3] as u32,
                sizes[2] as u32,
                sizes[3] as u32,
                *align_corners as u32,
            ];
            Ok(vec![StepSpec {
                wgsl: super::deskew::RESIZE_LINEAR_WGSL,
                n_read_inputs: 1,
                input_buf_names: vec![op.inputs[0].clone()],
                output_buf_name: op.outputs[0].clone(),
                params: bytemuck::cast_slice(&p).to_vec(),
                dispatch: [len.div_ceil(256) as u32, 1, 1],
            }])
        }
        PlannedOpKind::GridSample { align_corners } => {
            let len = out_shapes[0].iter().product::<usize>();
            let p = [
                len as u32,
                in_shapes[0][0] as u32,
                in_shapes[0][1] as u32,
                in_shapes[0][2] as u32,
                in_shapes[0][3] as u32,
                in_shapes[1][1] as u32,
                in_shapes[1][2] as u32,
                *align_corners as u32,
            ];
            Ok(vec![StepSpec {
                wgsl: super::deskew::GRID_SAMPLE_WGSL,
                n_read_inputs: 2,
                input_buf_names: op.inputs.clone(),
                output_buf_name: op.outputs[0].clone(),
                params: bytemuck::cast_slice(&p).to_vec(),
                dispatch: [len.div_ceil(256) as u32, 1, 1],
            }])
        }
        PlannedOpKind::CumSum { axis } => {
            let shape = &in_shapes[0];
            let outer = shape[..*axis].iter().product::<usize>();
            let axis_dim = shape[*axis];
            let inner = shape[*axis + 1..].iter().product::<usize>();
            let lines = outer * inner;
            Ok(vec![StepSpec {
                wgsl: super::sauvola::CUMSUM_WGSL,
                n_read_inputs: 1,
                input_buf_names: vec![op.inputs[0].clone()],
                output_buf_name: op.outputs[0].clone(),
                params: bytemuck::cast_slice(&[lines as u32, axis_dim as u32, inner as u32])
                    .to_vec(),
                dispatch: grid(lines.div_ceil(256)),
            }])
        }
        PlannedOpKind::ReduceSum { axis, mean, .. } => {
            let shape = &in_shapes[0];
            let outer = shape[..*axis].iter().product::<usize>();
            let axis_dim = shape[*axis];
            let inner = shape[*axis + 1..].iter().product::<usize>();
            let num_out = outer * inner;
            let is_mean = u32::from(*mean);
            Ok(vec![StepSpec {
                wgsl: super::sauvola::REDUCESUM_WGSL,
                n_read_inputs: 1,
                input_buf_names: vec![op.inputs[0].clone()],
                output_buf_name: op.outputs[0].clone(),
                params: bytemuck::cast_slice(&[
                    num_out as u32,
                    axis_dim as u32,
                    inner as u32,
                    is_mean,
                ])
                .to_vec(),
                dispatch: grid(num_out.div_ceil(256)),
            }])
        }
        PlannedOpKind::SpaceToDepth { blocksize } | PlannedOpKind::DepthToSpace { blocksize } => {
            let s = &in_shapes[0];
            if s.len() != 4 {
                bail!("SpaceToDepth/DepthToSpace expect rank-4 input");
            }
            let num_out = out_shapes[0].iter().product::<usize>();
            let wgsl = match &op.kind {
                PlannedOpKind::SpaceToDepth { .. } => super::sauvola::SPACE_TO_DEPTH_WGSL,
                _ => super::sauvola::DEPTH_TO_SPACE_WGSL,
            };
            let p = [
                num_out as u32,
                s[1] as u32,
                s[2] as u32,
                s[3] as u32,
                *blocksize as u32,
            ];
            Ok(vec![StepSpec {
                wgsl,
                n_read_inputs: 1,
                input_buf_names: vec![op.inputs[0].clone()],
                output_buf_name: op.outputs[0].clone(),
                params: bytemuck::cast_slice(&p).to_vec(),
                dispatch: grid(num_out.div_ceil(256)),
            }])
        }
    }
}

fn norm_slice(idx: i64, dim: usize) -> usize {
    if idx < 0 {
        (idx + dim as i64).clamp(0, dim as i64) as usize
    } else {
        (idx as usize).min(dim)
    }
}

fn matmul_batch_strides(
    out_batch: &[usize],
    in_batch: &[usize],
    matrix_elems: usize,
) -> Result<[u32; 6]> {
    if out_batch.len() > 6 {
        bail!("matmul compile: batch rank {} > 6", out_batch.len());
    }
    let pad = out_batch.len() - in_batch.len();
    let padded: Vec<usize> = (0..pad)
        .map(|_| 1usize)
        .chain(in_batch.iter().copied())
        .collect();
    let strides = c_strides_raw(&padded);
    let mut out = [0u32; 6];
    for dim in 0..out_batch.len() {
        out[dim] = if padded[dim] == 1 {
            0
        } else {
            (strides[dim] * matrix_elems) as u32
        };
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vision::onnx::types::Pool2dPlan;

    #[test]
    fn pool_dispatch_covers_every_batch() {
        let op = PlannedOp {
            name: "pool".to_string(),
            inputs: vec!["input".to_string()],
            outputs: vec!["output".to_string()],
            input_shapes: vec![vec![2, 3, 8, 8]],
            output_shapes: vec![vec![2, 3, 4, 4]],
            kind: PlannedOpKind::AvgPool2d(Pool2dPlan {
                pads: [0; 4],
                strides: [2; 2],
                dilations: [1; 2],
                kernel_shape: [2; 2],
            }),
        };
        let steps = op_steps(&op, "zero").unwrap();
        assert_eq!(steps.len(), 1);
        let num_out = u32::from_le_bytes(steps[0].params[..4].try_into().unwrap());
        assert_eq!(num_out, 2 * 3 * 4 * 4);
        assert_eq!(steps[0].dispatch, [1, 1, 1]);
        assert!(steps[0].wgsl.contains("num_workgroups"));
    }

    #[test]
    fn nearest_resize_preserves_fractional_scales_and_batch_planes() {
        let op = PlannedOp {
            name: "resize".to_string(),
            inputs: vec!["input".to_string()],
            outputs: vec!["output".to_string()],
            input_shapes: vec![vec![2, 1, 5, 5]],
            output_shapes: vec![vec![2, 1, 3, 3]],
            kind: PlannedOpKind::ResizeNearest {
                scales: vec![1.0, 1.0, 0.7, 0.7],
            },
        };
        let steps = op_steps(&op, "zero").unwrap();
        let params = bytemuck::cast_slice::<u8, u32>(&steps[0].params);

        assert_eq!(params[0], 2 * 3 * 3);
        assert_eq!(params[1], 2);
        assert_eq!((params[4], params[5]), (3, 3));
        assert_eq!(f32::from_bits(params[6]), 0.7);
        assert_eq!(f32::from_bits(params[7]), 0.7);
    }
}
