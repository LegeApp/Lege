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
    nodes: &[&NodeProto],
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
    validate_lowering_arity(node)?;
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
    if input_shapes
        .iter()
        .chain(&output_shapes)
        .flatten()
        .any(|dim| *dim < 0)
    {
        bail!("lowering requires fully resolved non-negative shapes");
    }

    let kind = match node.get_op_type() {
        "Conv" => {
            if input_shapes[0].len() != 4
                || input_shapes[1].len() != 4
                || output_shapes[0].len() != 4
            {
                bail!("Conv2d plan requires rank-4 input, weight, and output");
            }
            let strides = attr_i64_array::<2>(node, "strides", [1, 1])?;
            let dilations = attr_i64_array::<2>(node, "dilations", [1, 1])?;
            let kernel_shape = attr_i64_array::<2>(
                node,
                "kernel_shape",
                [input_shapes[1][2], input_shapes[1][3]],
            )?;
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
            if plan.group <= 0 {
                bail!("Conv group must be positive");
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
        "Erf" => PlannedOpKind::Unary(UnaryKind::Erf),
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
                    .collect::<Result<Vec<_>>>()?,
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
                if output_shapes
                    .iter()
                    .any(|shape| shape.len() != input_shapes[0].len())
                {
                    bail!("Split output ranks must match the input rank");
                }
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
            if attr_i64(node, "ceil_mode").unwrap_or(0) != 0 {
                bail!("MaxPool only supports ceil_mode=0");
            }
            PlannedOpKind::MaxPool2d(pool_plan(node, &input_shapes[0])?)
        }
        "AveragePool" => {
            if attr_i64(node, "ceil_mode").unwrap_or(0) != 0 {
                bail!("AveragePool only supports ceil_mode=0");
            }
            // count_include_pad=0 (default): divide by the count of valid (in-bounds)
            // window elements. We only implement that mode.
            if attr_i64(node, "count_include_pad").unwrap_or(0) != 0 {
                bail!("AveragePool only supports count_include_pad=0");
            }
            PlannedOpKind::AvgPool2d(pool_plan(node, &input_shapes[0])?)
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
                if attr_string(node, "nearest_mode")
                    .as_deref()
                    .unwrap_or("round_prefer_floor")
                    != "floor"
                {
                    bail!("Resize nearest lowering only supports nearest_mode=floor");
                }
                if input_shapes[0].len() != 4 || output_shapes[0].len() != 4 {
                    bail!("Resize nearest requires rank-4 input and output");
                }
                let scales_name = node
                    .get_input()
                    .get(2)
                    .filter(|name| !name.is_empty())
                    .context("Resize nearest scales input is missing")?;
                let scales = const_f32(tensor_consts, scales_name)
                    .context("Resize scales must be constant for lowering")?
                    .to_vec();
                if scales.len() != 4 || scales[0] != 1.0 || scales[1] != 1.0 {
                    bail!("Resize nearest only supports spatial scaling of rank-4 tensors");
                }
                PlannedOpKind::ResizeNearest { scales }
            } else if mode == "linear" {
                if coord != "half_pixel" && coord != "align_corners" {
                    bail!(
                        "Resize linear lowering only supports half_pixel or align_corners coordinate mode"
                    );
                }
                if output_shapes[0].len() != 4
                    || output_shapes[0][0] != input_shapes[0][0]
                    || output_shapes[0][1] != input_shapes[0][1]
                {
                    bail!("Resize linear only supports spatial scaling of rank-4 tensors");
                }
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
                mean: false,
            }
        }
        "ReduceMean" => {
            // opset-11 form: axes as an attribute; single axis supported.
            let axes = attr_i64s(node, "axes")
                .context("ReduceMean axes attribute is required for lowering")?;
            if axes.len() != 1 {
                bail!("ReduceMean only supports a single reduction axis");
            }
            PlannedOpKind::ReduceSum {
                axis: normalize_axis(axes[0], input_shapes[0].len())?,
                keepdims: attr_i64(node, "keepdims").unwrap_or(1) != 0,
                mean: true,
            }
        }
        "SpaceToDepth" => PlannedOpKind::SpaceToDepth {
            blocksize: usize::try_from(
                attr_i64(node, "blocksize")
                    .filter(|value| *value > 0)
                    .context("SpaceToDepth blocksize must be positive")?,
            )
            .context("SpaceToDepth blocksize exceeds usize")?,
        },
        "DepthToSpace" => {
            if attr_string(node, "mode").as_deref().unwrap_or("DCR") != "DCR" {
                bail!("DepthToSpace only supports DCR mode");
            }
            PlannedOpKind::DepthToSpace {
                blocksize: usize::try_from(
                    attr_i64(node, "blocksize")
                        .filter(|value| *value > 0)
                        .context("DepthToSpace blocksize must be positive")?,
                )
                .context("DepthToSpace blocksize exceeds usize")?,
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

/// Builds the pooling plan shared by MaxPool and AveragePool, resolving
/// `auto_pad` against the concrete input shape exactly as Conv does. Paddle
/// exports lean on `SAME_UPPER` for the stride-1 2x2 windows in PP-OCRv6's stem,
/// where dropping the padding would silently shrink the feature map by a pixel
/// and only surface later as a Concat mismatch.
fn pool_plan(node: &NodeProto, input_shape: &[i64]) -> Result<Pool2dPlan> {
    let kernel_shape = attr_i64_array::<2>(node, "kernel_shape", [1, 1])?;
    let strides = attr_i64_array::<2>(node, "strides", [1, 1])?;
    let dilations = attr_i64_array::<2>(node, "dilations", [1, 1])?;
    Ok(Pool2dPlan {
        pads: super::shape::resolve_auto_pad(
            attr_string(node, "auto_pad").as_deref().unwrap_or("NOTSET"),
            input_shape,
            kernel_shape,
            strides,
            dilations,
            attr_i64_array::<4>(node, "pads", [0, 0, 0, 0])?,
        )?,
        strides,
        dilations,
        kernel_shape,
    })
}

fn validate_lowering_arity(node: &NodeProto) -> Result<()> {
    let op = node.get_op_type();
    let required_positions: &[usize] = match op {
        "Conv" | "Add" | "Mul" | "Sub" | "Div" | "Max" | "Pow" | "PRelu" | "MatMul" | "Gemm"
        | "GridSample" | "CumSum" | "ReduceSum" | "Reshape" | "Pad" => &[0, 1],
        "Slice" => &[0, 1, 2],
        "Identity" | "Sigmoid" | "Relu" | "Sqrt" | "Erf" | "HardSwish" | "HardSigmoid"
        | "GlobalAveragePool" | "Unsqueeze" | "Squeeze" | "Concat" | "Split" | "Transpose"
        | "MaxPool" | "AveragePool" | "Resize" | "Softmax" | "ReduceMean" | "SpaceToDepth"
        | "DepthToSpace" => &[0],
        _ => &[],
    };
    for &index in required_positions {
        if node.get_input().get(index).is_none_or(String::is_empty) {
            bail!("{op} is missing required input {index}");
        }
    }
    if node.get_output().iter().all(String::is_empty) {
        bail!("{op} has no non-empty outputs");
    }
    Ok(())
}

fn normalize_axis_for_insert(axis: i64, rank: usize) -> Result<usize> {
    let rank = rank as i64;
    let axis = if axis < 0 { axis + rank } else { axis };
    if axis < 0 || axis >= rank {
        bail!("insertion axis {axis} is out of range for rank {rank}");
    }
    Ok(axis as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_nodes_never_panic_during_lowering() {
        let mut known_shapes = BTreeMap::from([
            ("a".to_owned(), vec![1]),
            ("b".to_owned(), vec![1]),
            ("c".to_owned(), vec![1]),
        ]);
        for index in 0..4 {
            known_shapes.insert(format!("out_{index}"), vec![1]);
        }
        let constants = HashMap::from([
            ("a".to_owned(), TensorConst::Int64(vec![i64::MIN])),
            ("b".to_owned(), TensorConst::Int64(Vec::new())),
            ("c".to_owned(), TensorConst::Float32(vec![f32::NAN])),
        ]);
        let ops = [
            "Conv",
            "Add",
            "Pow",
            "PRelu",
            "Unsqueeze",
            "Squeeze",
            "Concat",
            "Split",
            "Slice",
            "Reshape",
            "Transpose",
            "MaxPool",
            "AveragePool",
            "Pad",
            "Resize",
            "GridSample",
            "MatMul",
            "Gemm",
            "Softmax",
            "CumSum",
            "ReduceSum",
            "ReduceMean",
            "SpaceToDepth",
            "DepthToSpace",
        ];
        let names = ["a", "b", "c", ""];
        let mut state = 0x243f_6a88_85a3_08d3u64;

        for case in 0..2_000 {
            state = state
                .wrapping_mul(2_862_933_555_777_941_757)
                .wrapping_add(3_037_000_493);
            let op = ops[(state as usize) % ops.len()];
            let mut node = NodeProto {
                op_type: Some(op.to_owned()),
                ..NodeProto::new()
            };
            for index in 0..((state >> 8) as usize % 6) {
                node.input
                    .push(names[((state >> (index * 7 % 48)) as usize) % names.len()].to_owned());
            }
            for index in 0..((state >> 16) as usize % 4) {
                node.output.push(if (state >> (index + 40)) & 1 == 0 {
                    String::new()
                } else {
                    format!("out_{index}")
                });
            }

            let result = std::panic::catch_unwind(|| lower_node(&node, &known_shapes, &constants));
            assert!(
                result.is_ok(),
                "lowering panicked for generated case {case}: op={op}, inputs={:?}, outputs={:?}",
                node.get_input(),
                node.get_output()
            );
        }
    }
}
