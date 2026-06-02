//! Lowers ONNX NodeProto sequences into typed PlannedOp sequences.

use std::collections::{BTreeMap, HashMap};

use anyhow::{Context, Result, bail};

use crate::vision::onnx_pb::NodeProto;

use super::attrs::{
    attr_f32, attr_i64, attr_i64_array, attr_i64s, attr_string, const_f32, const_i64, node_context,
    normalize_axis,
};
use super::types::{
    Conv2dPlan, ElementwiseKind, PadMode, PlannedOp, PlannedOpKind, Pool2dPlan, TensorConst,
    UnaryKind,
};

pub(crate) fn lower_all_ops(
    nodes: &[NodeProto],
    known_shapes: &BTreeMap<String, Vec<i64>>,
    tensor_consts: &HashMap<String, TensorConst>,
) -> Result<Vec<PlannedOp>> {
    nodes
        .iter()
        .enumerate()
        .map(|(index, node)| {
            lower_node(node, known_shapes, tensor_consts)
                .with_context(|| format!("lowering failed at node {index} {}", node_context(node)))
        })
        .collect()
}

fn lower_node(
    node: &NodeProto,
    known_shapes: &BTreeMap<String, Vec<i64>>,
    tensor_consts: &HashMap<String, TensorConst>,
) -> Result<PlannedOp> {
    let inputs = node
        .get_input()
        .iter()
        .filter(|name| !name.is_empty())
        .cloned()
        .collect::<Vec<_>>();
    let outputs = node
        .get_output()
        .iter()
        .filter(|name| !name.is_empty())
        .cloned()
        .collect::<Vec<_>>();
    let input_shapes = inputs
        .iter()
        .map(|name| {
            known_shapes
                .get(name)
                .cloned()
                .with_context(|| format!("missing planned input shape for `{name}`"))
        })
        .collect::<Result<Vec<_>>>()?;
    let output_shapes = outputs
        .iter()
        .map(|name| {
            known_shapes
                .get(name)
                .cloned()
                .with_context(|| format!("missing planned output shape for `{name}`"))
        })
        .collect::<Result<Vec<_>>>()?;

    let kind = match node.get_op_type() {
        "Conv" => {
            let strides = attr_i64_array::<2>(node, "strides", [1, 1])?;
            let dilations = attr_i64_array::<2>(node, "dilations", [1, 1])?;
            let kernel_shape = attr_i64_array::<2>(
                node,
                "kernel_shape",
                [input_shapes[1][2], input_shapes[1][3]],
            )?;
            if input_shapes[0].len() != 4 {
                bail!("Conv2d plan requires rank-4 input");
            }
            // Resolve auto_pad (SAME_*/VALID) into explicit pads for the kernel.
            let pads = super::shape::resolve_auto_pad(
                attr_string(node, "auto_pad").as_deref().unwrap_or("NOTSET"),
                &input_shapes[0],
                kernel_shape,
                strides,
                dilations,
                attr_i64_array::<4>(node, "pads", [0, 0, 0, 0])?,
            )?;
            let plan = Conv2dPlan {
                group: attr_i64(node, "group").unwrap_or(1),
                pads,
                strides,
                dilations,
                kernel_shape,
            };
            if input_shapes[0].len() != 4 || input_shapes[1].len() != 4 {
                bail!("Conv2d plan requires rank-4 input and weight");
            }
            if input_shapes[0][1] % plan.group != 0 || output_shapes[0][1] % plan.group != 0 {
                bail!("Conv group does not divide input/output channels");
            }
            PlannedOpKind::Conv2d(plan)
        }
        "Add" => PlannedOpKind::Elementwise(ElementwiseKind::Add),
        "Mul" => PlannedOpKind::Elementwise(ElementwiseKind::Mul),
        "Sub" => PlannedOpKind::Elementwise(ElementwiseKind::Sub),
        "Div" => PlannedOpKind::Elementwise(ElementwiseKind::Div),
        "Max" => PlannedOpKind::Elementwise(ElementwiseKind::Max),
        "Identity" => PlannedOpKind::Unary(UnaryKind::Identity),
        "Sigmoid" => PlannedOpKind::Unary(UnaryKind::Sigmoid),
        "Relu" => PlannedOpKind::Unary(UnaryKind::Relu),
        "Sqrt" => PlannedOpKind::Unary(UnaryKind::Sqrt),
        "Pow" => {
            // The exponent is a constant scalar (e.g. 2.0) materialized as input[1].
            let exponent = const_f32(tensor_consts, &node.get_input()[1])
                .and_then(|values| values.first().copied())
                .context("Pow exponent must be a constant scalar for lowering")?;
            PlannedOpKind::Unary(UnaryKind::Pow { exponent })
        }
        "HardSwish" => PlannedOpKind::Unary(UnaryKind::HardSwish),
        "HardSigmoid" => PlannedOpKind::Unary(UnaryKind::HardSigmoid {
            alpha: attr_f32(node, "alpha").unwrap_or(0.2),
            beta: attr_f32(node, "beta").unwrap_or(0.5),
        }),
        "GlobalAveragePool" => PlannedOpKind::GlobalAveragePool,
        "PRelu" => PlannedOpKind::PRelu,
        "Unsqueeze" => {
            let axes = if node.get_input().len() > 1 {
                const_i64(tensor_consts, &node.get_input()[1])
                    .context("Unsqueeze axes must be constant for lowering")?
                    .to_vec()
            } else {
                attr_i64s(node, "axes").context("Unsqueeze axes must be present for lowering")?
            };
            let new_rank = output_shapes[0].len();
            PlannedOpKind::Unsqueeze {
                axes: axes
                    .into_iter()
                    .map(|axis| normalize_axis_for_insert(axis, new_rank))
                    .collect(),
            }
        }
        "Squeeze" => {
            let axes = if node.get_input().len() > 1 {
                const_i64(tensor_consts, &node.get_input()[1])
                    .context("Squeeze axes must be constant for lowering")?
                    .to_vec()
            } else {
                attr_i64s(node, "axes").context("Squeeze axes must be present for lowering")?
            };
            PlannedOpKind::Squeeze {
                axes: axes
                    .into_iter()
                    .map(|axis| normalize_axis(axis, input_shapes[0].len()))
                    .collect::<Result<Vec<_>>>()?,
            }
        }
        "Concat" => PlannedOpKind::Concat {
            axis: normalize_axis(attr_i64(node, "axis").unwrap_or(0), input_shapes[0].len())?,
        },
        "Split" => {
            let axis = normalize_axis(attr_i64(node, "axis").unwrap_or(0), input_shapes[0].len())?;
            let sizes = if node.get_input().len() > 1 {
                const_i64(tensor_consts, &node.get_input()[1])
                    .context("Split sizes must be constant for lowering")?
                    .to_vec()
            } else {
                output_shapes.iter().map(|shape| shape[axis]).collect()
            };
            PlannedOpKind::Split { axis, sizes }
        }
        "Slice" => {
            let starts = const_i64(tensor_consts, &node.get_input()[1])
                .context("Slice starts must be constant for lowering")?
                .to_vec();
            let ends = const_i64(tensor_consts, &node.get_input()[2])
                .context("Slice ends must be constant for lowering")?
                .to_vec();
            let axes = if node.get_input().len() > 3 {
                const_i64(tensor_consts, &node.get_input()[3])
                    .map(|values| values.to_vec())
                    .unwrap_or_else(|| (0..starts.len() as i64).collect())
            } else {
                (0..starts.len() as i64).collect()
            }
            .into_iter()
            .map(|axis| normalize_axis(axis, input_shapes[0].len()))
            .collect::<Result<Vec<_>>>()?;
            let steps = if node.get_input().len() > 4 {
                const_i64(tensor_consts, &node.get_input()[4])
                    .map(|values| values.to_vec())
                    .unwrap_or_else(|| vec![1; starts.len()])
            } else {
                vec![1; starts.len()]
            };
            PlannedOpKind::Slice {
                axes,
                starts,
                ends,
                steps,
            }
        }
        "Reshape" => PlannedOpKind::Reshape {
            target: const_i64(tensor_consts, &node.get_input()[1])
                .context("Reshape target must be constant for lowering")?
                .to_vec(),
        },
        "Transpose" => PlannedOpKind::Transpose {
            perm: attr_i64s(node, "perm")
                .unwrap_or_else(|| (0..input_shapes[0].len() as i64).rev().collect())
                .into_iter()
                .map(|axis| normalize_axis(axis, input_shapes[0].len()))
                .collect::<Result<Vec<_>>>()?,
        },
        "MaxPool" => {
            if attr_string(node, "auto_pad").as_deref().unwrap_or("NOTSET") != "NOTSET" {
                bail!("MaxPool only supports auto_pad=NOTSET");
            }
            if attr_i64(node, "ceil_mode").unwrap_or(0) != 0 {
                bail!("MaxPool only supports ceil_mode=0");
            }
            PlannedOpKind::MaxPool2d(Pool2dPlan {
                pads: attr_i64_array::<4>(node, "pads", [0, 0, 0, 0])?,
                strides: attr_i64_array::<2>(node, "strides", [1, 1])?,
                dilations: attr_i64_array::<2>(node, "dilations", [1, 1])?,
                kernel_shape: attr_i64_array::<2>(node, "kernel_shape", [1, 1])?,
            })
        }
        "Pad" => {
            let mode = match attr_string(node, "mode").as_deref().unwrap_or("constant") {
                "constant" => PadMode::Constant,
                "reflect" => PadMode::Reflect,
                mode => bail!("Pad mode `{mode}` is not supported"),
            };
            let value = if node.get_input().len() > 2 && !node.get_input()[2].is_empty() {
                const_f32(tensor_consts, &node.get_input()[2])
                    .and_then(|values| values.first().copied())
                    .unwrap_or(0.0)
            } else {
                0.0
            };
            PlannedOpKind::Pad {
                pads: const_i64(tensor_consts, &node.get_input()[1])
                    .context("Pad pads must be constant for lowering")?
                    .to_vec(),
                mode,
                value,
            }
        }
        "Resize" => {
            let mode = attr_string(node, "mode").unwrap_or_else(|| "nearest".to_owned());
            let coord = attr_string(node, "coordinate_transformation_mode")
                .unwrap_or_else(|| "asymmetric".to_owned());
            if mode == "nearest" {
                if coord != "asymmetric" {
                    bail!("Resize nearest lowering only supports asymmetric coordinate mode");
                }
                PlannedOpKind::ResizeNearest {
                    scales: const_f32(tensor_consts, &node.get_input()[2])
                        .context("Resize scales must be constant for lowering")?
                        .to_vec(),
                }
            } else if mode == "linear" {
                PlannedOpKind::ResizeLinear {
                    sizes: output_shapes[0].clone(),
                    align_corners: coord == "align_corners",
                }
            } else {
                bail!("Resize mode `{mode}` is not supported");
            }
        }
        "GridSample" => {
            if attr_string(node, "mode").as_deref().unwrap_or("bilinear") != "bilinear" {
                bail!("GridSample only supports bilinear mode");
            }
            if attr_string(node, "padding_mode")
                .as_deref()
                .unwrap_or("zeros")
                != "zeros"
            {
                bail!("GridSample only supports zeros padding");
            }
            PlannedOpKind::GridSample {
                align_corners: attr_i64(node, "align_corners").unwrap_or(0) != 0,
            }
        }
        "MatMul" => PlannedOpKind::MatMul,
        "Gemm" => PlannedOpKind::Gemm {
            alpha: attr_f32(node, "alpha").unwrap_or(1.0),
            beta: attr_f32(node, "beta").unwrap_or(1.0),
            trans_a: attr_i64(node, "transA").unwrap_or(0) != 0,
            trans_b: attr_i64(node, "transB").unwrap_or(0) != 0,
        },
        "Softmax" => PlannedOpKind::Softmax {
            axis: normalize_axis(attr_i64(node, "axis").unwrap_or(-1), input_shapes[0].len())?,
        },
        "CumSum" => {
            if attr_i64(node, "exclusive").unwrap_or(0) != 0 {
                bail!("CumSum only supports exclusive=0");
            }
            if attr_i64(node, "reverse").unwrap_or(0) != 0 {
                bail!("CumSum only supports reverse=0");
            }
            let axis = const_i64(tensor_consts, &node.get_input()[1])
                .and_then(|values| values.first().copied())
                .context("CumSum axis must be a constant scalar for lowering")?;
            PlannedOpKind::CumSum {
                axis: normalize_axis(axis, input_shapes[0].len())?,
            }
        }
        "ReduceSum" => {
            // axes provided as input[1] (opset 13+); single axis supported.
            let axes = const_i64(tensor_consts, &node.get_input()[1])
                .context("ReduceSum axes must be constant for lowering")?;
            if axes.len() != 1 {
                bail!("ReduceSum only supports a single reduction axis");
            }
            PlannedOpKind::ReduceSum {
                axis: normalize_axis(axes[0], input_shapes[0].len())?,
                keepdims: attr_i64(node, "keepdims").unwrap_or(1) != 0,
            }
        }
        "SpaceToDepth" => PlannedOpKind::SpaceToDepth {
            blocksize: attr_i64(node, "blocksize").context("SpaceToDepth blocksize is required")?
                as usize,
        },
        "DepthToSpace" => {
            if attr_string(node, "mode").as_deref().unwrap_or("DCR") != "DCR" {
                bail!("DepthToSpace only supports DCR mode");
            }
            PlannedOpKind::DepthToSpace {
                blocksize: attr_i64(node, "blocksize")
                    .context("DepthToSpace blocksize is required")?
                    as usize,
            }
        }
        op => bail!("lowering is not implemented for op {op}"),
    };

    // Strip constant-parameter inputs down to just the runtime data tensor.
    let (inputs, input_shapes) = match &kind {
        PlannedOpKind::Split { .. }
        | PlannedOpKind::Reshape { .. }
        | PlannedOpKind::Slice { .. }
        | PlannedOpKind::ResizeNearest { .. }
        | PlannedOpKind::ResizeLinear { .. }
        | PlannedOpKind::Pad { .. }
        | PlannedOpKind::Unsqueeze { .. }
        | PlannedOpKind::Squeeze { .. }
        | PlannedOpKind::Unary(UnaryKind::Pow { .. })
        | PlannedOpKind::CumSum { .. }
        | PlannedOpKind::ReduceSum { .. } => {
            let mut inp = inputs;
            let mut sh = input_shapes;
            inp.truncate(1);
            sh.truncate(1);
            (inp, sh)
        }
        _ => (inputs, input_shapes),
    };

    Ok(PlannedOp {
        name: if node.get_name().is_empty() {
            outputs
                .first()
                .cloned()
                .unwrap_or_else(|| "<unnamed>".to_owned())
        } else {
            node.get_name().to_owned()
        },
        inputs,
        outputs,
        input_shapes,
        output_shapes,
        kind,
    })
}

fn normalize_axis_for_insert(axis: i64, rank: usize) -> usize {
    let rank = rank as i64;
    let axis = if axis < 0 { axis + rank } else { axis };
    axis.clamp(0, rank) as usize
}
