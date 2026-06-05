//! GPU op dispatch layer. Mirrors `reference::run_op` but executes on the GPU.

pub(crate) mod activations;
pub(crate) mod common;
pub(crate) mod compile;
pub(crate) mod concat;
pub(crate) mod conv;
pub(crate) mod deskew;
pub(crate) mod elemwise;
pub(crate) mod gemm;
pub(crate) mod globalavgpool;
pub(crate) mod matmul;
pub(crate) mod maxpool;
pub(crate) mod resize;
pub(crate) mod sauvola;
pub(crate) mod sigmoid;
pub(crate) mod slice;
pub(crate) mod softmax;
pub(crate) mod split;
pub(crate) mod transpose;
pub(crate) mod winograd;

use anyhow::{Result, bail};

use crate::vision::onnx::types::{PlannedOpKind, UnaryKind};
use crate::vision::reference::Tensor;
use crate::vision::runtime::device::GpuContext;

pub(crate) async fn run_op(
    ctx: &GpuContext,
    kind: &PlannedOpKind,
    inputs: &[Tensor],
) -> Result<Vec<Tensor>> {
    match kind {
        PlannedOpKind::Elementwise(k) => Ok(vec![elemwise::run_elementwise(ctx, k, inputs).await?]),
        PlannedOpKind::Unary(UnaryKind::Sigmoid) => {
            if inputs.len() != 1 {
                bail!("Sigmoid expects 1 input");
            }
            Ok(vec![sigmoid::run_sigmoid(ctx, &inputs[0]).await?])
        }
        PlannedOpKind::Unary(UnaryKind::Identity) => {
            if inputs.len() != 1 {
                bail!("Identity expects 1 input");
            }
            Ok(inputs.to_vec())
        }
        PlannedOpKind::Unary(UnaryKind::Relu) => {
            if inputs.len() != 1 {
                bail!("Relu expects 1 input");
            }
            Ok(vec![activations::run_relu(ctx, &inputs[0]).await?])
        }
        PlannedOpKind::Unary(UnaryKind::HardSwish) => {
            if inputs.len() != 1 {
                bail!("HardSwish expects 1 input");
            }
            Ok(vec![activations::run_hardswish(ctx, &inputs[0]).await?])
        }
        PlannedOpKind::Unary(UnaryKind::HardSigmoid { alpha, beta }) => {
            if inputs.len() != 1 {
                bail!("HardSigmoid expects 1 input");
            }
            Ok(vec![
                activations::run_hardsigmoid(ctx, &inputs[0], *alpha, *beta).await?,
            ])
        }
        PlannedOpKind::Unary(UnaryKind::Sqrt) => {
            if inputs.len() != 1 {
                bail!("Sqrt expects 1 input");
            }
            Ok(vec![activations::run_sqrt(ctx, &inputs[0]).await?])
        }
        PlannedOpKind::Unary(UnaryKind::Pow { exponent }) => {
            if inputs.len() != 1 {
                bail!("Pow expects 1 input (exponent folded at lowering)");
            }
            Ok(vec![
                activations::run_pow(ctx, &inputs[0], *exponent).await?,
            ])
        }
        PlannedOpKind::GlobalAveragePool => {
            if inputs.len() != 1 {
                bail!("GlobalAveragePool expects 1 input");
            }
            Ok(vec![
                globalavgpool::run_global_avg_pool(ctx, &inputs[0]).await?,
            ])
        }
        PlannedOpKind::CumSum { axis } => {
            if inputs.len() != 1 {
                bail!("CumSum expects 1 input (axis folded at lowering)");
            }
            Ok(vec![sauvola::run_cumsum(ctx, &inputs[0], *axis).await?])
        }
        PlannedOpKind::ReduceSum { axis, keepdims } => {
            if inputs.len() != 1 {
                bail!("ReduceSum expects 1 input (axes folded at lowering)");
            }
            Ok(vec![
                sauvola::run_reduce_sum(ctx, &inputs[0], *axis, *keepdims).await?,
            ])
        }
        PlannedOpKind::SpaceToDepth { blocksize } => {
            if inputs.len() != 1 {
                bail!("SpaceToDepth expects 1 input");
            }
            Ok(vec![
                sauvola::run_space_to_depth(ctx, &inputs[0], *blocksize).await?,
            ])
        }
        PlannedOpKind::DepthToSpace { blocksize } => {
            if inputs.len() != 1 {
                bail!("DepthToSpace expects 1 input");
            }
            Ok(vec![
                sauvola::run_depth_to_space(ctx, &inputs[0], *blocksize).await?,
            ])
        }
        PlannedOpKind::PRelu => {
            if inputs.len() != 2 {
                bail!("PRelu expects 2 inputs");
            }
            Ok(vec![deskew::run_prelu(ctx, &inputs[0], &inputs[1]).await?])
        }
        PlannedOpKind::Unsqueeze { axes } => {
            if inputs.len() != 1 {
                bail!("Unsqueeze expects 1 input");
            }
            Ok(vec![deskew::run_unsqueeze(&inputs[0], axes)?])
        }
        PlannedOpKind::Squeeze { axes } => {
            if inputs.len() != 1 {
                bail!("Squeeze expects 1 input");
            }
            Ok(vec![deskew::run_squeeze(&inputs[0], axes)?])
        }
        PlannedOpKind::Pad { pads, mode, value } => {
            if inputs.len() != 1 {
                bail!("Pad expects 1 input");
            }
            Ok(vec![
                deskew::run_pad(ctx, pads, mode, *value, &inputs[0]).await?,
            ])
        }
        PlannedOpKind::ResizeLinear {
            sizes,
            align_corners,
        } => {
            if inputs.len() != 1 {
                bail!("ResizeLinear expects 1 input");
            }
            Ok(vec![
                deskew::run_resize_linear(ctx, sizes, *align_corners, &inputs[0]).await?,
            ])
        }
        PlannedOpKind::GridSample { align_corners } => {
            if inputs.len() != 2 {
                bail!("GridSample expects 2 inputs");
            }
            Ok(vec![
                deskew::run_grid_sample(ctx, *align_corners, &inputs[0], &inputs[1]).await?,
            ])
        }
        PlannedOpKind::Transpose { perm } => {
            if inputs.len() != 1 {
                bail!("Transpose expects 1 input");
            }
            Ok(vec![transpose::run_transpose(ctx, perm, &inputs[0]).await?])
        }
        PlannedOpKind::Concat { axis } => Ok(vec![concat::run_concat(ctx, *axis, inputs).await?]),
        PlannedOpKind::Conv2d(plan) => {
            let w_shape = &inputs[1].shape;
            // Optimized conv kernels assume batch 1; route N>1 to the naive path.
            if inputs[0].shape[0] > 1 {
                Ok(vec![conv::run_conv2d(ctx, plan, inputs).await?])
            } else if conv::is_conv1x1_gemm(plan, w_shape) {
                if plan.strides == [1, 1] {
                    Ok(vec![conv::run_conv1x1_gemm_s1(ctx, plan, inputs).await?])
                } else {
                    Ok(vec![conv::run_conv1x1(ctx, plan, inputs).await?])
                }
            } else if conv::is_conv3x3_tiled(plan, w_shape) {
                if plan.group == 1 && plan.strides == [1, 1] && plan.dilations == [1, 1] {
                    Ok(vec![
                        conv::run_conv3x3_co8_sp2x2_s1d1(ctx, plan, inputs).await?,
                    ])
                } else {
                    Ok(vec![conv::run_conv3x3_tiled(ctx, plan, inputs).await?])
                }
            } else {
                Ok(vec![conv::run_conv2d(ctx, plan, inputs).await?])
            }
        }
        PlannedOpKind::Split { axis, sizes } => {
            if inputs.len() != 1 {
                bail!("Split expects 1 input");
            }
            split::run_split(ctx, *axis, sizes, &inputs[0]).await
        }
        PlannedOpKind::MaxPool2d(plan) => {
            if inputs.len() != 1 {
                bail!("MaxPool2d expects 1 input");
            }
            Ok(vec![maxpool::run_maxpool2d(ctx, plan, &inputs[0]).await?])
        }
        PlannedOpKind::ResizeNearest { scales } => {
            if inputs.len() != 1 {
                bail!("ResizeNearest expects 1 input");
            }
            Ok(vec![
                resize::run_resize_nearest(ctx, scales, &inputs[0]).await?,
            ])
        }
        PlannedOpKind::Reshape { target } => {
            if inputs.len() != 1 {
                bail!("Reshape expects 1 input");
            }
            // Resolve ONNX reshape semantics: 0 copies the input dim, -1 is inferred.
            let input = &inputs[0];
            let mut shape = Vec::with_capacity(target.len());
            let mut inferred = None;
            for (axis, &dim) in target.iter().enumerate() {
                if dim == 0 {
                    shape.push(
                        *input
                            .shape
                            .get(axis)
                            .ok_or_else(|| anyhow::anyhow!("Reshape 0 dim out of input range"))?,
                    );
                } else if dim == -1 {
                    if inferred.is_some() {
                        bail!("Reshape has multiple inferred dimensions");
                    }
                    inferred = Some(axis);
                    shape.push(1);
                } else {
                    shape.push(
                        usize::try_from(dim)
                            .map_err(|_| anyhow::anyhow!("negative reshape dim"))?,
                    );
                }
            }
            if let Some(axis) = inferred {
                let known = shape.iter().product::<usize>();
                if known == 0 || input.data.len() % known != 0 {
                    bail!("Reshape inferred dimension is not integral");
                }
                shape[axis] = input.data.len() / known;
            }
            Ok(vec![Tensor::new(shape, input.data.clone())?])
        }
        PlannedOpKind::Slice {
            axes,
            starts,
            ends,
            steps,
        } => {
            if inputs.len() != 1 {
                bail!("Slice expects 1 input");
            }
            Ok(vec![
                slice::run_slice(ctx, axes, starts, ends, steps, &inputs[0]).await?,
            ])
        }
        PlannedOpKind::MatMul => {
            if inputs.len() != 2 {
                bail!("MatMul expects 2 inputs");
            }
            Ok(vec![matmul::run_matmul(ctx, &inputs[0], &inputs[1]).await?])
        }
        PlannedOpKind::Gemm {
            alpha,
            beta,
            trans_a,
            trans_b,
        } => {
            if inputs.len() < 2 || inputs.len() > 3 {
                bail!("Gemm expects 2 or 3 inputs");
            }
            Ok(vec![
                gemm::run_gemm(
                    ctx,
                    &inputs[0],
                    &inputs[1],
                    inputs.get(2),
                    *alpha,
                    *beta,
                    *trans_a,
                    *trans_b,
                )
                .await?,
            ])
        }
        PlannedOpKind::Softmax { axis } => {
            if inputs.len() != 1 {
                bail!("Softmax expects 1 input");
            }
            Ok(vec![softmax::run_softmax(ctx, *axis, &inputs[0]).await?])
        }
    }
}

pub(crate) fn op_is_gpu_implemented(kind: &PlannedOpKind) -> bool {
    matches!(
        kind,
        PlannedOpKind::Conv2d(_)
            | PlannedOpKind::Elementwise(_)
            | PlannedOpKind::Unary(_)
            | PlannedOpKind::Transpose { .. }
            | PlannedOpKind::Concat { .. }
            | PlannedOpKind::Split { .. }
            | PlannedOpKind::MaxPool2d(_)
            | PlannedOpKind::ResizeNearest { .. }
            | PlannedOpKind::Reshape { .. }
            | PlannedOpKind::Slice { .. }
            | PlannedOpKind::MatMul
            | PlannedOpKind::Gemm { .. }
            | PlannedOpKind::Softmax { .. }
            | PlannedOpKind::GlobalAveragePool
            | PlannedOpKind::CumSum { .. }
            | PlannedOpKind::ReduceSum { .. }
            | PlannedOpKind::SpaceToDepth { .. }
            | PlannedOpKind::DepthToSpace { .. }
            | PlannedOpKind::PRelu
            | PlannedOpKind::Unsqueeze { .. }
            | PlannedOpKind::Squeeze { .. }
            | PlannedOpKind::Pad { .. }
            | PlannedOpKind::ResizeLinear { .. }
            | PlannedOpKind::GridSample { .. }
    )
}
