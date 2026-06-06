#![allow(dead_code)]

use anyhow::{Context, Result, bail};
use rayon::prelude::*;

use crate::vision::onnx::types::{
    Conv2dPlan, ElementwiseKind, PadMode, PlannedOpKind, Pool2dPlan, UnaryKind,
};
use crate::vision::ops::winograd::{input_transform_f23, output_transform_f23};

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Tensor {
    pub(crate) shape: Vec<usize>,
    pub(crate) data: Vec<f32>,
}

impl Tensor {
    pub(crate) fn new(shape: Vec<usize>, data: Vec<f32>) -> Result<Self> {
        let expected = shape.iter().product::<usize>();
        if expected != data.len() {
            bail!(
                "tensor data length mismatch: shape {:?} expects {}, got {}",
                shape,
                expected,
                data.len()
            );
        }
        Ok(Self { shape, data })
    }

    fn zeros(shape: Vec<usize>) -> Self {
        let len = shape.iter().product();
        Self {
            shape,
            data: vec![0.0; len],
        }
    }

    fn rank(&self) -> usize {
        self.shape.len()
    }
}

pub(crate) fn run_op(kind: &PlannedOpKind, inputs: &[&Tensor]) -> Result<Vec<Tensor>> {
    match kind {
        PlannedOpKind::Conv2d(plan) => Ok(vec![conv2d(plan, inputs)?]),
        PlannedOpKind::WinogradInputTransform => Ok(vec![winograd_input_transform(inputs[0])?]),
        PlannedOpKind::WinogradBatchedGemm => {
            Ok(vec![winograd_batched_gemm(inputs[0], inputs[1])?])
        }
        PlannedOpKind::WinogradOutputTransform { use_bias, h, w } => {
            Ok(vec![winograd_output_transform(inputs, *use_bias, *h, *w)?])
        }
        PlannedOpKind::Elementwise(kind) => Ok(vec![elementwise(kind, inputs)?]),
        PlannedOpKind::Unary(kind) => Ok(vec![unary(kind, inputs)?]),
        PlannedOpKind::Concat { axis } => Ok(vec![concat(inputs, *axis)?]),
        PlannedOpKind::Split { axis, sizes } => split(inputs, *axis, sizes),
        PlannedOpKind::Slice {
            axes,
            starts,
            ends,
            steps,
        } => Ok(vec![slice(inputs[0], axes, starts, ends, steps)?]),
        PlannedOpKind::Reshape { target } => Ok(vec![reshape(inputs[0], target)?]),
        PlannedOpKind::Transpose { perm } => Ok(vec![transpose(inputs[0], perm)?]),
        PlannedOpKind::MaxPool2d(plan) => Ok(vec![maxpool2d(plan, inputs[0])?]),
        PlannedOpKind::ResizeNearest { scales } => Ok(vec![resize_nearest(inputs[0], scales)?]),
        PlannedOpKind::MatMul => Ok(vec![matmul(inputs[0], inputs[1])?]),
        PlannedOpKind::Gemm {
            alpha,
            beta,
            trans_a,
            trans_b,
        } => Ok(vec![gemm(inputs, *alpha, *beta, *trans_a, *trans_b)?]),
        PlannedOpKind::Softmax { axis } => Ok(vec![softmax(inputs[0], *axis)?]),
        PlannedOpKind::GlobalAveragePool => Ok(vec![global_avg_pool(inputs[0])?]),
        PlannedOpKind::CumSum { axis } => Ok(vec![cumsum(inputs[0], *axis)?]),
        PlannedOpKind::ReduceSum { axis, keepdims } => {
            Ok(vec![reduce_sum(inputs[0], *axis, *keepdims)?])
        }
        PlannedOpKind::SpaceToDepth { blocksize } => {
            Ok(vec![space_to_depth(inputs[0], *blocksize)?])
        }
        PlannedOpKind::DepthToSpace { blocksize } => {
            Ok(vec![depth_to_space(inputs[0], *blocksize)?])
        }
        PlannedOpKind::PRelu => Ok(vec![prelu(inputs)?]),
        PlannedOpKind::Unsqueeze { axes } => Ok(vec![unsqueeze(inputs[0], axes)?]),
        PlannedOpKind::Squeeze { axes } => Ok(vec![squeeze(inputs[0], axes)?]),
        PlannedOpKind::Pad { pads, mode, value } => Ok(vec![pad(inputs[0], pads, mode, *value)?]),
        PlannedOpKind::ResizeLinear {
            sizes,
            align_corners,
        } => Ok(vec![resize_linear(inputs[0], sizes, *align_corners)?]),
        PlannedOpKind::GridSample { align_corners } => {
            Ok(vec![grid_sample(inputs[0], inputs[1], *align_corners)?])
        }
    }
}

fn prelu(inputs: &[&Tensor]) -> Result<Tensor> {
    if inputs.len() != 2 {
        bail!("PRelu expects 2 inputs");
    }
    let input = inputs[0];
    let slope = inputs[1];
    let shape = broadcast_shape(&input.shape, &slope.shape)?;
    if shape != input.shape {
        bail!("PRelu output shape must match input shape");
    }
    let mut data = Vec::with_capacity(input.data.len());
    for index in 0..input.data.len() {
        let coords = unravel(index, &input.shape);
        let slope_index = broadcast_index(&coords, &input.shape, &slope.shape);
        let x = input.data[index];
        data.push(if x < 0.0 {
            x * slope.data[slope_index]
        } else {
            x
        });
    }
    Tensor::new(input.shape.clone(), data)
}

fn unsqueeze(input: &Tensor, axes: &[usize]) -> Result<Tensor> {
    let mut shape = input.shape.clone();
    let mut axes = axes.to_vec();
    axes.sort_unstable();
    for axis in axes {
        shape.insert(axis.min(shape.len()), 1);
    }
    Tensor::new(shape, input.data.clone())
}

fn squeeze(input: &Tensor, axes: &[usize]) -> Result<Tensor> {
    let shape = input
        .shape
        .iter()
        .enumerate()
        .filter_map(|(axis, dim)| (!axes.contains(&axis)).then_some(*dim))
        .collect::<Vec<_>>();
    Tensor::new(shape, input.data.clone())
}

fn gemm(inputs: &[&Tensor], alpha: f32, beta: f32, trans_a: bool, trans_b: bool) -> Result<Tensor> {
    if inputs.len() < 2 || inputs.len() > 3 {
        bail!("Gemm expects 2 or 3 inputs, got {}", inputs.len());
    }
    let a = inputs[0];
    let b = inputs[1];
    if a.rank() != 2 || b.rank() != 2 {
        bail!("Gemm expects 2D A and B");
    }
    let (m, ka) = if trans_a {
        (a.shape[1], a.shape[0])
    } else {
        (a.shape[0], a.shape[1])
    };
    let (kb, n) = if trans_b {
        (b.shape[1], b.shape[0])
    } else {
        (b.shape[0], b.shape[1])
    };
    if ka != kb {
        bail!("Gemm contracting dims mismatch: {ka} vs {kb}");
    }
    let bias = inputs.get(2);
    let mut data = vec![0.0f32; m * n];
    for row in 0..m {
        for col in 0..n {
            let mut acc = 0.0;
            for k in 0..ka {
                let av = if trans_a {
                    a.data[k * m + row]
                } else {
                    a.data[row * ka + k]
                };
                let bv = if trans_b {
                    b.data[col * ka + k]
                } else {
                    b.data[k * n + col]
                };
                acc += av * bv;
            }
            let mut y = alpha * acc;
            if let Some(bias) = bias {
                let bi = match bias.data.len() {
                    1 => 0,
                    len if len == n => col,
                    _ => row * n + col,
                };
                y += beta * bias.data[bi];
            }
            data[row * n + col] = y;
        }
    }
    Tensor::new(vec![m, n], data)
}

fn global_avg_pool(input: &Tensor) -> Result<Tensor> {
    if input.rank() < 2 {
        bail!("GlobalAveragePool expects rank >= 2, got {}", input.rank());
    }
    let num_planes = input.shape[0] * input.shape[1];
    let plane = input.shape[2..].iter().product::<usize>().max(1);
    let mut out_shape = input.shape.clone();
    for dim in out_shape.iter_mut().skip(2) {
        *dim = 1;
    }
    let mut data = vec![0.0f32; num_planes];
    for (p, slot) in data.iter_mut().enumerate() {
        let base = p * plane;
        let sum: f32 = input.data[base..base + plane].iter().sum();
        *slot = sum / plane as f32;
    }
    Tensor::new(out_shape, data)
}

/// Splits a flat row-major tensor at `axis` into (outer, axis_dim, inner).
fn axis_split(shape: &[usize], axis: usize) -> (usize, usize, usize) {
    let outer = shape[..axis].iter().product::<usize>();
    let axis_dim = shape[axis];
    let inner = shape[axis + 1..].iter().product::<usize>();
    (outer, axis_dim, inner)
}

fn cumsum(input: &Tensor, axis: usize) -> Result<Tensor> {
    if axis >= input.rank() {
        bail!("CumSum axis {axis} out of range for rank {}", input.rank());
    }
    let (outer, axis_dim, inner) = axis_split(&input.shape, axis);
    let mut data = input.data.clone();
    for o in 0..outer {
        for i in 0..inner {
            let mut acc = 0.0f32;
            for k in 0..axis_dim {
                let idx = (o * axis_dim + k) * inner + i;
                acc += data[idx];
                data[idx] = acc;
            }
        }
    }
    Tensor::new(input.shape.clone(), data)
}

fn reduce_sum(input: &Tensor, axis: usize, keepdims: bool) -> Result<Tensor> {
    if axis >= input.rank() {
        bail!(
            "ReduceSum axis {axis} out of range for rank {}",
            input.rank()
        );
    }
    let (outer, axis_dim, inner) = axis_split(&input.shape, axis);
    let mut data = vec![0.0f32; outer * inner];
    for o in 0..outer {
        for i in 0..inner {
            let mut acc = 0.0f32;
            for k in 0..axis_dim {
                acc += input.data[(o * axis_dim + k) * inner + i];
            }
            data[o * inner + i] = acc;
        }
    }
    let mut shape = input.shape.clone();
    if keepdims {
        shape[axis] = 1;
    } else {
        shape.remove(axis);
    }
    Tensor::new(shape, data)
}

fn space_to_depth(input: &Tensor, b: usize) -> Result<Tensor> {
    let s = &input.shape;
    if s.len() != 4 {
        bail!("SpaceToDepth expects rank-4 input, got {s:?}");
    }
    let (n, c, h, w) = (s[0], s[1], s[2], s[3]);
    let (oc, oh, ow) = (c * b * b, h / b, w / b);
    let mut data = vec![0.0f32; n * oc * oh * ow];
    // One output plane (nn, occ) per rayon task.
    data.par_chunks_mut(oh * ow)
        .enumerate()
        .for_each(|(p, dst)| {
            let nn = p / oc;
            let occ = p % oc;
            let bh = occ / (b * c);
            let rem = occ % (b * c);
            let bw = rem / c;
            let cc = rem % c;
            for ohh in 0..oh {
                for oww in 0..ow {
                    let ih = ohh * b + bh;
                    let iw = oww * b + bw;
                    let src = ((nn * c + cc) * h + ih) * w + iw;
                    dst[ohh * ow + oww] = input.data[src];
                }
            }
        });
    Tensor::new(vec![n, oc, oh, ow], data)
}

fn depth_to_space(input: &Tensor, b: usize) -> Result<Tensor> {
    let s = &input.shape;
    if s.len() != 4 {
        bail!("DepthToSpace expects rank-4 input, got {s:?}");
    }
    let (n, c, h, w) = (s[0], s[1], s[2], s[3]);
    let (oc, oh, ow) = (c / (b * b), h * b, w * b);
    let mut data = vec![0.0f32; n * oc * oh * ow];
    // One output plane (nn, occ) per rayon task.
    data.par_chunks_mut(oh * ow)
        .enumerate()
        .for_each(|(p, dst)| {
            let nn = p / oc;
            let occ = p % oc;
            for ohh in 0..oh {
                let hh = ohh / b;
                let b0 = ohh % b;
                for oww in 0..ow {
                    let ww = oww / b;
                    let b1 = oww % b;
                    let ic = b0 * b * oc + b1 * oc + occ;
                    let src = ((nn * c + ic) * h + hh) * w + ww;
                    dst[ohh * ow + oww] = input.data[src];
                }
            }
        });
    Tensor::new(vec![n, oc, oh, ow], data)
}

fn winograd_input_transform(input: &Tensor) -> Result<Tensor> {
    require_rank(input, 4, "WinogradInputTransform input")?;
    if input.shape[0] != 1 {
        bail!("WinogradInputTransform only supports batch 1");
    }
    let cin = input.shape[1];
    let h = input.shape[2];
    let w = input.shape[3];
    let ntw = w.div_ceil(2);
    let nth = h.div_ceil(2);
    let p = ntw * nth;
    let stride = cin * p;
    let mut out = vec![0.0f32; 16 * stride];

    for ci in 0..cin {
        for ty in 0..nth {
            for tx in 0..ntw {
                let tile = ty * ntw + tx;
                let mut d = [0.0f32; 16];
                for py in 0..4 {
                    let ih = (2 * ty + py) as isize - 1;
                    for px in 0..4 {
                        let iw = (2 * tx + px) as isize - 1;
                        if ih >= 0 && ih < h as isize && iw >= 0 && iw < w as isize {
                            d[py * 4 + px] = input.data[ci * h * w + ih as usize * w + iw as usize];
                        }
                    }
                }
                let v = input_transform_f23(&d);
                let base = ci * p + tile;
                for e in 0..16 {
                    out[e * stride + base] = v[e];
                }
            }
        }
    }

    Tensor::new(vec![16, cin, p], out)
}

fn winograd_batched_gemm(u: &Tensor, v: &Tensor) -> Result<Tensor> {
    if u.shape.len() != 3 || v.shape.len() != 3 {
        bail!("WinogradBatchedGemm expects rank-3 U and V");
    }
    let cout = u.shape[1];
    let cin = u.shape[2];
    if u.shape[0] != 16 || v.shape[0] != 16 || v.shape[1] != cin {
        bail!(
            "WinogradBatchedGemm shape mismatch: U={:?} V={:?}",
            u.shape,
            v.shape
        );
    }
    let p = v.shape[2];
    let mut out = vec![0.0f32; 16 * cout * p];
    for e in 0..16 {
        let u_base = e * cout * cin;
        let v_base = e * cin * p;
        let m_base = e * cout * p;
        for co in 0..cout {
            for tile in 0..p {
                let mut acc = 0.0f32;
                for ci in 0..cin {
                    acc += u.data[u_base + co * cin + ci] * v.data[v_base + ci * p + tile];
                }
                out[m_base + co * p + tile] = acc;
            }
        }
    }
    Tensor::new(vec![16, cout, p], out)
}

fn winograd_output_transform(
    inputs: &[&Tensor],
    use_bias: bool,
    h: usize,
    w: usize,
) -> Result<Tensor> {
    let m = inputs[0];
    if m.shape.len() != 3 || m.shape[0] != 16 {
        bail!("WinogradOutputTransform expects M shape [16,Cout,P]");
    }
    if use_bias && inputs.len() != 2 {
        bail!("WinogradOutputTransform expected bias input");
    }
    let cout = m.shape[1];
    let p = m.shape[2];
    let bias = inputs.get(1).copied();
    if let Some(bias) = bias {
        if bias.shape != [cout] {
            bail!("WinogradOutputTransform bias shape mismatch");
        }
    }

    let ntw = w.div_ceil(2);
    let nth = h.div_ceil(2);
    if p != ntw * nth {
        bail!("WinogradOutputTransform P mismatch: M P={p}, output H/W={h}x{w}");
    }
    let mut out = vec![0.0f32; cout * h * w];
    let stride = cout * p;
    for co in 0..cout {
        for tile in 0..p {
            let ty = tile / ntw;
            let tx = tile - ty * ntw;
            let mut mm = [0.0f32; 16];
            let base = co * p + tile;
            for e in 0..16 {
                mm[e] = m.data[e * stride + base];
            }
            let y = output_transform_f23(&mm);
            let b = bias.map(|tensor| tensor.data[co]).unwrap_or(0.0);
            let oh0 = 2 * ty;
            let ow0 = 2 * tx;
            let cbase = co * h * w;
            if oh0 < h && ow0 < w {
                out[cbase + oh0 * w + ow0] = y[0] + b;
            }
            if oh0 < h && ow0 + 1 < w {
                out[cbase + oh0 * w + ow0 + 1] = y[1] + b;
            }
            if oh0 + 1 < h && ow0 < w {
                out[cbase + (oh0 + 1) * w + ow0] = y[2] + b;
            }
            if oh0 + 1 < h && ow0 + 1 < w {
                out[cbase + (oh0 + 1) * w + ow0 + 1] = y[3] + b;
            }
        }
    }
    Tensor::new(vec![1, cout, h, w], out)
}

fn unary(kind: &UnaryKind, inputs: &[&Tensor]) -> Result<Tensor> {
    let input = exactly_one(inputs)?;
    let data = match kind {
        UnaryKind::Identity => input.data.clone(),
        UnaryKind::Sigmoid => input
            .data
            .iter()
            .map(|x| 1.0 / (1.0 + (-x).exp()))
            .collect(),
        UnaryKind::Relu => input.data.iter().map(|x| x.max(0.0)).collect(),
        UnaryKind::HardSwish => input
            .data
            .iter()
            .map(|x| x * (x / 6.0 + 0.5).clamp(0.0, 1.0))
            .collect(),
        UnaryKind::HardSigmoid { alpha, beta } => input
            .data
            .iter()
            .map(|x| (alpha * x + beta).clamp(0.0, 1.0))
            .collect(),
        UnaryKind::Sqrt => input.data.iter().map(|x| x.sqrt()).collect(),
        UnaryKind::Pow { exponent } => input.data.iter().map(|x| x.powf(*exponent)).collect(),
    };
    Tensor::new(input.shape.clone(), data)
}

fn elementwise(kind: &ElementwiseKind, inputs: &[&Tensor]) -> Result<Tensor> {
    if inputs.len() != 2 {
        bail!("elementwise op expects 2 inputs, got {}", inputs.len());
    }
    let (a, b) = (inputs[0], inputs[1]);
    let shape = broadcast_shape(&a.shape, &b.shape)?;
    let len = shape.iter().product::<usize>();
    let op = |lhs: f32, rhs: f32| match kind {
        ElementwiseKind::Add => lhs + rhs,
        ElementwiseKind::Mul => lhs * rhs,
        ElementwiseKind::Sub => lhs - rhs,
        ElementwiseKind::Div => lhs / rhs,
        ElementwiseKind::Max => lhs.max(rhs),
    };

    // Fast path: both operands already match the output shape (no broadcast).
    if a.shape == shape && b.shape == shape {
        let mut data = vec![0.0f32; len];
        data.par_iter_mut()
            .zip(&a.data)
            .zip(&b.data)
            .for_each(|((out, &lhs), &rhs)| *out = op(lhs, rhs));
        return Tensor::new(shape, data);
    }
    // Fast path: one operand is a scalar broadcast over the whole output.
    if b.data.len() == 1 && a.shape == shape {
        let rhs = b.data[0];
        let mut data = vec![0.0f32; len];
        data.par_iter_mut()
            .zip(&a.data)
            .for_each(|(out, &lhs)| *out = op(lhs, rhs));
        return Tensor::new(shape, data);
    }
    if a.data.len() == 1 && b.shape == shape {
        let lhs = a.data[0];
        let mut data = vec![0.0f32; len];
        data.par_iter_mut()
            .zip(&b.data)
            .for_each(|(out, &rhs)| *out = op(lhs, rhs));
        return Tensor::new(shape, data);
    }

    // General broadcast: process one output row (last axis) per rayon task. The
    // row's base input offsets come from decomposing the row index over the
    // leading dims with broadcast-aware strides; the inner loop walks the last
    // axis. No per-element allocation, parallel across rows.
    let rank = shape.len();
    if rank == 0 {
        // Scalar output (defensive: the equal-shape fast path covers scalars).
        return Tensor::new(shape, vec![op(a.data[0], b.data[0])]);
    }
    let sa = broadcast_strides(&shape, &a.shape);
    let sb = broadcast_strides(&shape, &b.shape);
    let row = *shape.last().unwrap_or(&len);
    let (la, lb) = (sa[rank - 1], sb[rank - 1]);
    let mut data = vec![0.0f32; len];
    if row == 0 {
        return Tensor::new(shape, data);
    }
    data.par_chunks_mut(row).enumerate().for_each(|(r, dst)| {
        let (mut ia, mut ib, mut rem) = (0usize, 0usize, r);
        for axis in (0..rank - 1).rev() {
            let c = rem % shape[axis];
            rem /= shape[axis];
            ia += c * sa[axis];
            ib += c * sb[axis];
        }
        for (k, out) in dst.iter_mut().enumerate() {
            *out = op(a.data[ia + k * la], b.data[ib + k * lb]);
        }
    });
    Tensor::new(shape, data)
}

/// Per-output-axis stride into a contiguous `in_shape` tensor, 0 where the input
/// broadcasts (size-1 or missing leading axes). Lets a broadcast op advance the
/// input's flat index without recomputing coordinates per element.
fn broadcast_strides(out_shape: &[usize], in_shape: &[usize]) -> Vec<usize> {
    let in_strides = strides(in_shape);
    let offset = out_shape.len() - in_shape.len();
    (0..out_shape.len())
        .map(|axis| {
            if axis < offset {
                0
            } else if in_shape[axis - offset] == 1 {
                0
            } else {
                in_strides[axis - offset]
            }
        })
        .collect()
}

fn concat(inputs: &[&Tensor], axis: usize) -> Result<Tensor> {
    if inputs.is_empty() {
        bail!("Concat needs at least one input");
    }
    let rank = inputs[0].rank();
    if axis >= rank {
        bail!("Concat axis {axis} out of range for rank {rank}");
    }
    let mut shape = inputs[0].shape.clone();
    shape[axis] = 0;
    for input in inputs {
        if input.rank() != rank {
            bail!("Concat rank mismatch");
        }
        for dim in 0..rank {
            if dim == axis {
                shape[axis] += input.shape[axis];
            } else if input.shape[dim] != shape[dim] {
                bail!("Concat dimension mismatch at dim {dim}");
            }
        }
    }

    // Copy contiguous `axis_size * inner` blocks per outer index — no per-element
    // indexing.
    let (outer, out_axis, inner) = axis_split(&shape, axis);
    let mut data = vec![0.0f32; outer * out_axis * inner];
    let mut axis_offset = 0usize;
    for input in inputs {
        let ai = input.shape[axis];
        let block = ai * inner;
        for o in 0..outer {
            let src = o * block;
            let dst = (o * out_axis + axis_offset) * inner;
            data[dst..dst + block].copy_from_slice(&input.data[src..src + block]);
        }
        axis_offset += ai;
    }
    Tensor::new(shape, data)
}

fn split(inputs: &[&Tensor], axis: usize, sizes: &[i64]) -> Result<Vec<Tensor>> {
    let input = exactly_one(inputs)?;
    if axis >= input.rank() {
        bail!("Split axis {axis} out of range for rank {}", input.rank());
    }
    let sizes = sizes
        .iter()
        .map(|size| usize::try_from(*size).context("Split size must be non-negative"))
        .collect::<Result<Vec<_>>>()?;
    if sizes.iter().sum::<usize>() != input.shape[axis] {
        bail!("Split sizes do not sum to axis dimension");
    }

    let (outer, in_axis, inner) = axis_split(&input.shape, axis);
    let mut outputs = Vec::new();
    let mut axis_offset = 0usize;
    for size in sizes {
        let mut shape = input.shape.clone();
        shape[axis] = size;
        let block = size * inner;
        let mut data = vec![0.0f32; outer * block];
        for o in 0..outer {
            let src = (o * in_axis + axis_offset) * inner;
            let dst = o * block;
            data[dst..dst + block].copy_from_slice(&input.data[src..src + block]);
        }
        axis_offset += size;
        outputs.push(Tensor::new(shape, data)?);
    }
    Ok(outputs)
}

fn slice(
    input: &Tensor,
    axes: &[usize],
    starts: &[i64],
    ends: &[i64],
    steps: &[i64],
) -> Result<Tensor> {
    if axes.len() != starts.len() || axes.len() != ends.len() || axes.len() != steps.len() {
        bail!("Slice parameter length mismatch");
    }
    let mut ranges = input
        .shape
        .iter()
        .map(|dim| (0usize, *dim, 1usize))
        .collect::<Vec<_>>();
    for (((axis, start), end), step) in axes.iter().zip(starts).zip(ends).zip(steps) {
        if *axis >= input.rank() {
            bail!("Slice axis {axis} out of range for rank {}", input.rank());
        }
        if *step <= 0 {
            bail!("Slice only supports positive steps");
        }
        let dim = input.shape[*axis];
        let start = normalize_index(*start, dim);
        let end = normalize_index(*end, dim);
        ranges[*axis] = (start, end, usize::try_from(*step).unwrap());
    }
    let shape = ranges
        .iter()
        .map(|(start, end, step)| end.saturating_sub(*start).div_ceil(*step))
        .collect::<Vec<_>>();
    // Walk the output with a coordinate odometer; per output axis the input flat
    // index advances by `step * in_stride`, with a base offset from the starts.
    let in_strides = strides(&input.shape);
    let rank = input.rank();
    let base = ranges
        .iter()
        .enumerate()
        .map(|(axis, (start, _, _))| start * in_strides[axis])
        .sum::<usize>();
    let advance = (0..rank)
        .map(|axis| ranges[axis].2 * in_strides[axis])
        .collect::<Vec<_>>();
    let len = shape.iter().product::<usize>();
    if rank == 0 {
        return Tensor::new(shape, vec![input.data[base]]);
    }
    let row = *shape.last().unwrap_or(&len);
    let last_adv = advance[rank - 1];
    let mut data = vec![0.0f32; len];
    if row == 0 {
        return Tensor::new(shape, data);
    }
    data.par_chunks_mut(row).enumerate().for_each(|(r, dst)| {
        let (mut src, mut rem) = (base, r);
        for axis in (0..rank - 1).rev() {
            let c = rem % shape[axis];
            rem /= shape[axis];
            src += c * advance[axis];
        }
        for (k, out) in dst.iter_mut().enumerate() {
            *out = input.data[src + k * last_adv];
        }
    });
    Tensor::new(shape, data)
}

fn reshape(input: &Tensor, target: &[i64]) -> Result<Tensor> {
    let input_elems = input.data.len();
    let mut shape = Vec::with_capacity(target.len());
    let mut inferred = None;
    for (index, dim) in target.iter().enumerate() {
        if *dim == 0 {
            shape.push(
                *input
                    .shape
                    .get(index)
                    .context("Reshape 0 dim out of input range")?,
            );
        } else if *dim == -1 {
            if inferred.is_some() {
                bail!("Reshape has multiple inferred dimensions");
            }
            inferred = Some(index);
            shape.push(1);
        } else {
            shape.push(usize::try_from(*dim).context("Reshape dim must be non-negative")?);
        }
    }
    if let Some(index) = inferred {
        let known = shape.iter().product::<usize>();
        if input_elems % known != 0 {
            bail!("Reshape inferred dimension is not integral");
        }
        shape[index] = input_elems / known;
    }
    Tensor::new(shape, input.data.clone())
}

fn transpose(input: &Tensor, perm: &[usize]) -> Result<Tensor> {
    if perm.len() != input.rank() {
        bail!("Transpose perm rank mismatch");
    }
    let shape = perm
        .iter()
        .map(|axis| input.shape[*axis])
        .collect::<Vec<_>>();
    // Stride into the input for each *output* axis. Process one output row (last
    // axis) per rayon task: compute the row's input base by decomposing the row
    // index over the leading output dims, then walk the last axis.
    let in_strides = strides(&input.shape);
    let out_in_stride = perm
        .iter()
        .map(|axis| in_strides[*axis])
        .collect::<Vec<_>>();
    let rank = shape.len();
    let len = shape.iter().product::<usize>();
    if rank == 0 {
        return Tensor::new(shape, vec![input.data[0]]);
    }
    let row = *shape.last().unwrap_or(&len);
    let last_stride = out_in_stride[rank - 1];
    let mut data = vec![0.0f32; len];
    if row == 0 {
        return Tensor::new(shape, data);
    }
    data.par_chunks_mut(row).enumerate().for_each(|(r, dst)| {
        let (mut src, mut rem) = (0usize, r);
        for axis in (0..rank - 1).rev() {
            let c = rem % shape[axis];
            rem /= shape[axis];
            src += c * out_in_stride[axis];
        }
        for (k, out) in dst.iter_mut().enumerate() {
            *out = input.data[src + k * last_stride];
        }
    });
    Tensor::new(shape, data)
}

fn conv2d(plan: &Conv2dPlan, inputs: &[&Tensor]) -> Result<Tensor> {
    if inputs.len() < 2 || inputs.len() > 3 {
        bail!("Conv2d expects input, weight, and optional bias");
    }
    let x = inputs[0];
    let w = inputs[1];
    let bias = inputs.get(2).copied();
    require_rank(x, 4, "Conv input")?;
    require_rank(w, 4, "Conv weight")?;
    let n = x.shape[0];
    let cin = x.shape[1];
    let hin = x.shape[2];
    let win = x.shape[3];
    let cout = w.shape[0];
    let cin_per_group = w.shape[1];
    let kh = w.shape[2];
    let kw = w.shape[3];
    let group = usize::try_from(plan.group).context("Conv group must be non-negative")?;
    if cin != cin_per_group * group || cout % group != 0 {
        bail!("Conv grouped channel mismatch");
    }
    if let Some(bias) = bias {
        if bias.shape != [cout] {
            bail!("Conv bias shape mismatch");
        }
    }
    let hout = conv_out_dim(
        hin,
        plan.pads[0],
        plan.pads[2],
        plan.dilations[0],
        kh,
        plan.strides[0],
    )?;
    let wout = conv_out_dim(
        win,
        plan.pads[1],
        plan.pads[3],
        plan.dilations[1],
        kw,
        plan.strides[1],
    )?;
    let cout_per_group = cout / group;
    let (sh, sw) = (plan.strides[0] as isize, plan.strides[1] as isize);
    let (dh, dw) = (plan.dilations[0] as isize, plan.dilations[1] as isize);
    let (ph, pw) = (plan.pads[0] as isize, plan.pads[1] as isize);
    let plane = hout * wout;
    let x_batch = cin * hin * win;

    // One output row (b, co, oh) per rayon task for good core utilization even
    // when cout is small. Each (ci, ky, kx) tap contributes a scaled, shifted
    // input row to the output row via a contiguous accumulate (`out[ow] += wv *
    // x[ow*sw + off]`) over the precomputed in-bounds `ow` range — vectorizable,
    // no per-element bounds checks or allocation.
    let mut out = vec![0.0f32; n * cout * plane];
    out.par_chunks_mut(wout).enumerate().for_each(|(row, dst)| {
        let oh = row % hout;
        let co = (row / hout) % cout;
        let b = row / (hout * cout);
        let g = co / cout_per_group;
        if let Some(bias) = bias {
            dst.fill(bias.data[co]);
        }
        let w_co = co * cin_per_group * kh * kw;
        for ci_g in 0..cin_per_group {
            let ci = g * cin_per_group + ci_g;
            let x_c = b * x_batch + ci * hin * win;
            let w_c = w_co + ci_g * kh * kw;
            for ky in 0..kh {
                let ih = oh as isize * sh + ky as isize * dh - ph;
                if ih < 0 || ih >= hin as isize {
                    continue;
                }
                let x_row = &x.data[x_c + ih as usize * win..x_c + (ih as usize + 1) * win];
                for kx in 0..kw {
                    let wv = w.data[w_c + ky * kw + kx];
                    // iw = ow*sw + off, valid when 0 <= iw < win.
                    let off = kx as isize * dw - pw;
                    let lo = if off >= 0 {
                        0
                    } else {
                        (((-off) as usize) + sw as usize - 1) / sw as usize
                    };
                    let hi = if (win as isize) > off {
                        (((win as isize - off) as usize) + sw as usize - 1) / sw as usize
                    } else {
                        0
                    }
                    .min(wout);
                    if sw == 1 {
                        let base = off; // iw = ow + off
                        for ow in lo..hi {
                            dst[ow] += wv * x_row[(ow as isize + base) as usize];
                        }
                    } else {
                        for ow in lo..hi {
                            dst[ow] += wv * x_row[(ow as isize * sw + off) as usize];
                        }
                    }
                }
            }
        }
    });
    Tensor::new(vec![n, cout, hout, wout], out)
}

fn maxpool2d(plan: &Pool2dPlan, input: &Tensor) -> Result<Tensor> {
    require_rank(input, 4, "MaxPool input")?;
    let n = input.shape[0];
    let c = input.shape[1];
    let hin = input.shape[2];
    let win = input.shape[3];
    let kh =
        usize::try_from(plan.kernel_shape[0]).context("MaxPool kernel must be non-negative")?;
    let kw =
        usize::try_from(plan.kernel_shape[1]).context("MaxPool kernel must be non-negative")?;
    let hout = conv_out_dim(
        hin,
        plan.pads[0],
        plan.pads[2],
        plan.dilations[0],
        kh,
        plan.strides[0],
    )?;
    let wout = conv_out_dim(
        win,
        plan.pads[1],
        plan.pads[3],
        plan.dilations[1],
        kw,
        plan.strides[1],
    )?;
    let mut output = Tensor::zeros(vec![n, c, hout, wout]);
    for b in 0..n {
        for ch in 0..c {
            for oh in 0..hout {
                for ow in 0..wout {
                    let mut max = f32::NEG_INFINITY;
                    for ky in 0..kh {
                        let ih = oh as isize * plan.strides[0] as isize
                            + ky as isize * plan.dilations[0] as isize
                            - plan.pads[0] as isize;
                        if ih < 0 || ih >= hin as isize {
                            continue;
                        }
                        for kx in 0..kw {
                            let iw = ow as isize * plan.strides[1] as isize
                                + kx as isize * plan.dilations[1] as isize
                                - plan.pads[1] as isize;
                            if iw < 0 || iw >= win as isize {
                                continue;
                            }
                            max = max.max(
                                input.data[ravel(&[b, ch, ih as usize, iw as usize], &input.shape)],
                            );
                        }
                    }
                    let index = ravel(&[b, ch, oh, ow], &output.shape);
                    output.data[index] = max;
                }
            }
        }
    }
    Ok(output)
}

fn resize_nearest(input: &Tensor, scales: &[f32]) -> Result<Tensor> {
    require_rank(input, 4, "Resize input")?;
    if scales.len() != 4 {
        bail!("Resize nearest currently expects rank-4 scales");
    }
    let shape = input
        .shape
        .iter()
        .zip(scales)
        .map(|(dim, scale)| ((*dim as f32) * scale).round() as usize)
        .collect::<Vec<_>>();
    let mut output = Tensor::zeros(shape.clone());
    for out_index in 0..output.data.len() {
        let out = unravel(out_index, &shape);
        let in_coords = out
            .iter()
            .enumerate()
            .map(|(axis, coord)| ((*coord as f32) / scales[axis]).floor() as usize)
            .zip(&input.shape)
            .map(|(coord, dim)| coord.min(dim - 1))
            .collect::<Vec<_>>();
        output.data[out_index] = input.data[ravel(&in_coords, &input.shape)];
    }
    Ok(output)
}

fn resize_linear(input: &Tensor, sizes: &[i64], align_corners: bool) -> Result<Tensor> {
    require_rank(input, 4, "ResizeLinear input")?;
    if sizes.len() != 4 {
        bail!("ResizeLinear expects rank-4 sizes");
    }
    let out_shape = sizes
        .iter()
        .map(|&d| usize::try_from(d).context("ResizeLinear size must be non-negative"))
        .collect::<Result<Vec<_>>>()?;
    let [n, c, hin, win] = input.shape.as_slice() else {
        unreachable!()
    };
    let hout = out_shape[2];
    let wout = out_shape[3];
    let mut output = Tensor::zeros(out_shape.clone());
    for b in 0..*n {
        for ch in 0..*c {
            for oh in 0..hout {
                let y =
                    resize_src_coord(oh, *hin, hout, align_corners).clamp(0.0, (*hin - 1) as f32);
                let y0 = y.floor() as usize;
                let y1 = (y0 + 1).min(*hin - 1);
                let wy = y - y0 as f32;
                for ow in 0..wout {
                    let x = resize_src_coord(ow, *win, wout, align_corners)
                        .clamp(0.0, (*win - 1) as f32);
                    let x0 = x.floor() as usize;
                    let x1 = (x0 + 1).min(*win - 1);
                    let wx = x - x0 as f32;
                    let v00 = input.data[ravel(&[b, ch, y0, x0], &input.shape)];
                    let v01 = input.data[ravel(&[b, ch, y0, x1], &input.shape)];
                    let v10 = input.data[ravel(&[b, ch, y1, x0], &input.shape)];
                    let v11 = input.data[ravel(&[b, ch, y1, x1], &input.shape)];
                    let top = v00 * (1.0 - wx) + v01 * wx;
                    let bot = v10 * (1.0 - wx) + v11 * wx;
                    output.data[ravel(&[b, ch, oh, ow], &out_shape)] = top * (1.0 - wy) + bot * wy;
                }
            }
        }
    }
    Ok(output)
}

fn pad(input: &Tensor, pads: &[i64], mode: &PadMode, value: f32) -> Result<Tensor> {
    let rank = input.rank();
    if pads.len() != rank * 2 {
        bail!("Pad pads length must be 2*rank");
    }
    let out_shape = (0..rank)
        .map(|axis| usize::try_from(input.shape[axis] as i64 + pads[axis] + pads[axis + rank]))
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("Pad output dim must be non-negative")?;
    // Constant mode: fill with `value`, then copy the input block in by walking
    // the input with a coordinate odometer and advancing the output flat index by
    // the output strides (contiguous along the last axis). No per-element alloc.
    if matches!(mode, PadMode::Constant) {
        let out_strides = strides(&out_shape);
        let len = out_shape.iter().product::<usize>();
        let mut data = vec![value; len];
        let base = (0..rank)
            .map(|axis| pads[axis].max(0) as usize * out_strides[axis])
            .sum::<usize>();
        let mut coords = vec![0usize; rank];
        let mut dst = base;
        for &src in &input.data {
            data[dst] = src;
            for axis in (0..rank).rev() {
                coords[axis] += 1;
                dst += out_strides[axis];
                if coords[axis] < input.shape[axis] {
                    break;
                }
                coords[axis] = 0;
                dst -= out_strides[axis] * input.shape[axis];
            }
        }
        return Tensor::new(out_shape, data);
    }

    // Reflect mode (deskew; not on the sauvola CPU path): general per-element map.
    let mut output = Tensor::zeros(out_shape.clone());
    for out_index in 0..output.data.len() {
        let out_coords = unravel(out_index, &out_shape);
        let mut in_coords = vec![0usize; rank];
        for axis in 0..rank {
            let raw = out_coords[axis] as i64 - pads[axis];
            in_coords[axis] = reflect_index(raw, input.shape[axis])?;
        }
        output.data[out_index] = input.data[ravel(&in_coords, &input.shape)];
    }
    Ok(output)
}

fn grid_sample(input: &Tensor, grid: &Tensor, align_corners: bool) -> Result<Tensor> {
    require_rank(input, 4, "GridSample input")?;
    require_rank(grid, 4, "GridSample grid")?;
    if grid.shape[3] != 2 {
        bail!("GridSample grid last dim must be 2");
    }
    let [n, c, hin, win] = input.shape.as_slice() else {
        unreachable!()
    };
    let hout = grid.shape[1];
    let wout = grid.shape[2];
    let out_shape = vec![*n, *c, hout, wout];
    let mut output = Tensor::zeros(out_shape.clone());
    for b in 0..*n {
        for oh in 0..hout {
            for ow in 0..wout {
                let gx = grid.data[ravel(&[b, oh, ow, 0], &grid.shape)];
                let gy = grid.data[ravel(&[b, oh, ow, 1], &grid.shape)];
                let x = grid_unnormalize(gx, *win, align_corners);
                let y = grid_unnormalize(gy, *hin, align_corners);
                let x0 = x.floor() as isize;
                let y0 = y.floor() as isize;
                let wx = x - x0 as f32;
                let wy = y - y0 as f32;
                for ch in 0..*c {
                    let v00 = sample_zeros(input, b, ch, y0, x0);
                    let v01 = sample_zeros(input, b, ch, y0, x0 + 1);
                    let v10 = sample_zeros(input, b, ch, y0 + 1, x0);
                    let v11 = sample_zeros(input, b, ch, y0 + 1, x0 + 1);
                    let top = v00 * (1.0 - wx) + v01 * wx;
                    let bot = v10 * (1.0 - wx) + v11 * wx;
                    output.data[ravel(&[b, ch, oh, ow], &out_shape)] = top * (1.0 - wy) + bot * wy;
                }
            }
        }
    }
    Ok(output)
}

fn resize_src_coord(out: usize, input: usize, output: usize, align_corners: bool) -> f32 {
    if align_corners {
        if output <= 1 {
            0.0
        } else {
            out as f32 * (input - 1) as f32 / (output - 1) as f32
        }
    } else {
        (out as f32 + 0.5) * input as f32 / output as f32 - 0.5
    }
}

fn grid_unnormalize(v: f32, size: usize, align_corners: bool) -> f32 {
    if align_corners {
        (v + 1.0) * 0.5 * (size - 1) as f32
    } else {
        ((v + 1.0) * size as f32 - 1.0) * 0.5
    }
}

fn sample_zeros(input: &Tensor, b: usize, ch: usize, y: isize, x: isize) -> f32 {
    if y < 0 || x < 0 || y >= input.shape[2] as isize || x >= input.shape[3] as isize {
        0.0
    } else {
        input.data[ravel(&[b, ch, y as usize, x as usize], &input.shape)]
    }
}

fn reflect_index(mut index: i64, dim: usize) -> Result<usize> {
    if dim == 0 {
        bail!("cannot reflect-pad empty dim");
    }
    if dim == 1 {
        return Ok(0);
    }
    let dim = dim as i64;
    while index < 0 || index >= dim {
        if index < 0 {
            index = -index;
        } else {
            index = 2 * dim - 2 - index;
        }
    }
    Ok(index as usize)
}

fn matmul(lhs: &Tensor, rhs: &Tensor) -> Result<Tensor> {
    if lhs.rank() < 2 || rhs.rank() < 2 {
        bail!("MatMul expects rank >= 2");
    }
    let m = lhs.shape[lhs.rank() - 2];
    let k = lhs.shape[lhs.rank() - 1];
    let rhs_k = rhs.shape[rhs.rank() - 2];
    let n = rhs.shape[rhs.rank() - 1];
    if k != rhs_k {
        bail!("MatMul contracting dimensions mismatch");
    }
    let batch_shape = broadcast_shape(&lhs.shape[..lhs.rank() - 2], &rhs.shape[..rhs.rank() - 2])?;
    let mut shape = batch_shape.clone();
    shape.push(m);
    shape.push(n);
    let mut output = Tensor::zeros(shape.clone());

    for out_index in 0..output.data.len() {
        let out = unravel(out_index, &shape);
        let batch = &out[..batch_shape.len()];
        let row = out[batch_shape.len()];
        let col = out[batch_shape.len() + 1];
        let mut lhs_coords = broadcast_coords(batch, &batch_shape, &lhs.shape[..lhs.rank() - 2]);
        lhs_coords.push(row);
        lhs_coords.push(0);
        let mut rhs_coords = broadcast_coords(batch, &batch_shape, &rhs.shape[..rhs.rank() - 2]);
        rhs_coords.push(0);
        rhs_coords.push(col);
        let mut sum = 0.0;
        for kk in 0..k {
            lhs_coords[lhs.rank() - 1] = kk;
            rhs_coords[rhs.rank() - 2] = kk;
            sum +=
                lhs.data[ravel(&lhs_coords, &lhs.shape)] * rhs.data[ravel(&rhs_coords, &rhs.shape)];
        }
        output.data[out_index] = sum;
    }
    Ok(output)
}

fn softmax(input: &Tensor, axis: usize) -> Result<Tensor> {
    if axis >= input.rank() {
        bail!("Softmax axis out of range");
    }
    let outer = input.shape[..axis].iter().product::<usize>();
    let dim = input.shape[axis];
    let inner = input.shape[axis + 1..].iter().product::<usize>();
    let mut output = Tensor::zeros(input.shape.clone());
    for o in 0..outer {
        for i in 0..inner {
            let base = o * dim * inner + i;
            let mut max = f32::NEG_INFINITY;
            for d in 0..dim {
                max = max.max(input.data[base + d * inner]);
            }
            let mut sum = 0.0;
            for d in 0..dim {
                let value = (input.data[base + d * inner] - max).exp();
                output.data[base + d * inner] = value;
                sum += value;
            }
            for d in 0..dim {
                output.data[base + d * inner] /= sum;
            }
        }
    }
    Ok(output)
}

fn exactly_one<'a>(inputs: &[&'a Tensor]) -> Result<&'a Tensor> {
    if inputs.len() != 1 {
        bail!("op expects one input, got {}", inputs.len());
    }
    Ok(inputs[0])
}

fn require_rank(tensor: &Tensor, rank: usize, name: &str) -> Result<()> {
    if tensor.rank() != rank {
        bail!("{name} expects rank {rank}, got {}", tensor.rank());
    }
    Ok(())
}

fn conv_out_dim(
    input: usize,
    pad_begin: i64,
    pad_end: i64,
    dilation: i64,
    kernel: usize,
    stride: i64,
) -> Result<usize> {
    let value =
        (input as i64 + pad_begin + pad_end - dilation * (kernel as i64 - 1) - 1) / stride + 1;
    usize::try_from(value).context("conv/pool output dimension must be non-negative")
}

pub(crate) fn broadcast_shape(lhs: &[usize], rhs: &[usize]) -> Result<Vec<usize>> {
    let rank = lhs.len().max(rhs.len());
    let mut output = Vec::with_capacity(rank);
    for offset in 0..rank {
        let lhs_dim = lhs
            .get(lhs.len().wrapping_sub(1 + offset))
            .copied()
            .unwrap_or(1);
        let rhs_dim = rhs
            .get(rhs.len().wrapping_sub(1 + offset))
            .copied()
            .unwrap_or(1);
        let dim = if lhs_dim == rhs_dim {
            lhs_dim
        } else if lhs_dim == 1 {
            rhs_dim
        } else if rhs_dim == 1 {
            lhs_dim
        } else {
            bail!("cannot broadcast dimensions {lhs_dim} and {rhs_dim}");
        };
        output.push(dim);
    }
    output.reverse();
    Ok(output)
}

fn broadcast_index(out_coords: &[usize], out_shape: &[usize], in_shape: &[usize]) -> usize {
    let coords = broadcast_coords(out_coords, out_shape, in_shape);
    ravel(&coords, in_shape)
}

fn broadcast_coords(out_coords: &[usize], out_shape: &[usize], in_shape: &[usize]) -> Vec<usize> {
    let offset = out_shape.len() - in_shape.len();
    (0..in_shape.len())
        .map(|axis| {
            if in_shape[axis] == 1 {
                0
            } else {
                out_coords[offset + axis]
            }
        })
        .collect()
}

fn normalize_index(index: i64, dim: usize) -> usize {
    if index < 0 {
        (index + dim as i64).clamp(0, dim as i64) as usize
    } else {
        index.clamp(0, dim as i64) as usize
    }
}

fn ravel(coords: &[usize], shape: &[usize]) -> usize {
    coords
        .iter()
        .zip(strides(shape))
        .map(|(coord, stride)| coord * stride)
        .sum()
}

fn unravel(mut index: usize, shape: &[usize]) -> Vec<usize> {
    let strides = strides(shape);
    let mut coords = vec![0; shape.len()];
    for axis in 0..shape.len() {
        coords[axis] = index / strides[axis];
        index %= strides[axis];
    }
    coords
}

fn strides(shape: &[usize]) -> Vec<usize> {
    let mut strides = vec![1; shape.len()];
    if shape.len() >= 2 {
        for index in (0..shape.len() - 1).rev() {
            strides[index] = strides[index + 1] * shape[index + 1];
        }
    }
    strides
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(shape: &[usize], data: &[f32]) -> Tensor {
        Tensor::new(shape.to_vec(), data.to_vec()).unwrap()
    }

    #[test]
    fn elementwise_broadcasts_channel_bias() {
        let x = t(&[1, 2, 2, 2], &[1., 2., 3., 4., 10., 20., 30., 40.]);
        let bias = t(&[1, 2, 1, 1], &[100., 200.]);
        let y = elementwise(&ElementwiseKind::Add, &[&x, &bias]).unwrap();
        assert_eq!(y.shape, vec![1, 2, 2, 2]);
        assert_eq!(y.data, vec![101., 102., 103., 104., 210., 220., 230., 240.]);
    }

    #[test]
    fn concat_split_and_slice_work_on_channel_axis() {
        let a = t(&[1, 1, 2, 2], &[1., 2., 3., 4.]);
        let b = t(&[1, 2, 2, 2], &[5., 6., 7., 8., 9., 10., 11., 12.]);
        let joined = concat(&[&a, &b], 1).unwrap();
        assert_eq!(joined.shape, vec![1, 3, 2, 2]);
        let parts = split(&[&joined], 1, &[1, 2]).unwrap();
        assert_eq!(parts, vec![a, b]);
        let sliced = slice(&joined, &[1], &[1], &[3], &[1]).unwrap();
        assert_eq!(sliced, parts[1]);
    }

    #[test]
    fn reshape_and_transpose_preserve_values() {
        let x = t(&[1, 2, 3], &[1., 2., 3., 4., 5., 6.]);
        let r = reshape(&x, &[1, 3, 2]).unwrap();
        assert_eq!(r.shape, vec![1, 3, 2]);
        let tr = transpose(&r, &[0, 2, 1]).unwrap();
        assert_eq!(tr.shape, vec![1, 2, 3]);
        assert_eq!(tr.data, vec![1., 3., 5., 2., 4., 6.]);
    }

    #[test]
    fn conv2d_supports_groups_and_bias() {
        let plan = Conv2dPlan {
            group: 2,
            pads: [0, 0, 0, 0],
            strides: [1, 1],
            dilations: [1, 1],
            kernel_shape: [1, 1],
        };
        let x = t(&[1, 2, 1, 2], &[1., 2., 10., 20.]);
        let w = t(&[2, 1, 1, 1], &[3., 4.]);
        let b = t(&[2], &[1., 2.]);
        let y = conv2d(&plan, &[&x, &w, &b]).unwrap();
        assert_eq!(y.shape, vec![1, 2, 1, 2]);
        assert_eq!(y.data, vec![4., 7., 42., 82.]);
    }

    #[test]
    fn maxpool_and_resize_nearest_are_nchw() {
        let x = t(&[1, 1, 2, 2], &[1., 2., 3., 4.]);
        let pool = Pool2dPlan {
            pads: [0, 0, 0, 0],
            strides: [1, 1],
            dilations: [1, 1],
            kernel_shape: [2, 2],
        };
        assert_eq!(maxpool2d(&pool, &x).unwrap().data, vec![4.]);
        let resized = resize_nearest(&x, &[1., 1., 2., 2.]).unwrap();
        assert_eq!(resized.shape, vec![1, 1, 4, 4]);
        assert_eq!(
            resized.data,
            vec![
                1., 1., 2., 2., 1., 1., 2., 2., 3., 3., 4., 4., 3., 3., 4., 4.
            ]
        );
    }

    #[test]
    fn matmul_and_softmax_cover_attention_shapes() {
        let lhs = t(&[1, 1, 2, 3], &[1., 2., 3., 4., 5., 6.]);
        let rhs = t(&[1, 1, 3, 2], &[1., 0., 0., 1., 1., 1.]);
        let y = matmul(&lhs, &rhs).unwrap();
        assert_eq!(y.shape, vec![1, 1, 2, 2]);
        assert_eq!(y.data, vec![4., 5., 10., 11.]);
        let probs = softmax(&t(&[1, 1, 1, 2], &[0., 0.]), 3).unwrap();
        assert_eq!(probs.data, vec![0.5, 0.5]);
    }
}
