//! Shape inference for the supported YOLO op subset.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use anyhow::{Context, Result, bail};

use crate::vision::onnx_pb::NodeProto;

use super::attrs::{
    attr_i64, attr_i64s, const_f32, const_i64, input_shape, node_context, normalize_axis,
    normalize_index,
};
use super::fold::try_fold;
use super::types::TensorConst;

#[derive(Debug, Default)]
pub(crate) struct ShapeReport {
    pub(crate) inferred_outputs: usize,
    pub(crate) annotation_fallbacks: usize,
    pub(crate) missing_shapes: Vec<String>,
}

/// Result of the combined shape-inference + constant-fold pass. `folded` holds
/// the indices of shape-plumbing nodes that collapsed to constants and must be
/// dropped from the executable/lowered graph; `folded_values` names the
/// constants they produced (so float ones consumed by kept ops can be promoted
/// to GPU initializers).
pub(crate) struct ShapeInference {
    pub(crate) report: ShapeReport,
    pub(crate) folded: BTreeSet<usize>,
    pub(crate) folded_values: Vec<String>,
}

pub(crate) fn infer_all_shapes(
    nodes: &[NodeProto],
    annotated_shapes: &BTreeMap<String, Vec<i64>>,
    tensor_consts: &mut HashMap<String, TensorConst>,
    known_shapes: &mut BTreeMap<String, Vec<i64>>,
) -> Result<ShapeInference> {
    let mut report = ShapeReport::default();
    let mut folded = BTreeSet::new();
    let mut folded_values = Vec::new();
    for (index, node) in nodes.iter().enumerate() {
        // Shape-plumbing nodes fold to constants once their inputs are known,
        // feeding downstream Resize/Reshape and dropping out of the graph.
        if let Some(values) = try_fold(node, known_shapes, tensor_consts).with_context(|| {
            format!(
                "constant folding failed at node {index} {}",
                node_context(node)
            )
        })? {
            for value in values {
                known_shapes.insert(value.name.clone(), value.shape);
                folded_values.push(value.name.clone());
                tensor_consts.insert(value.name, value.value);
            }
            folded.insert(index);
            continue;
        }

        let inferred = infer_node_shapes(node, known_shapes, tensor_consts).with_context(|| {
            format!(
                "shape inference failed at node {index} {}",
                node_context(node)
            )
        })?;
        for output in node.get_output() {
            if output.is_empty() {
                continue;
            }
            if let Some(shape) = inferred.get(output) {
                known_shapes.insert(output.to_owned(), shape.clone());
                report.inferred_outputs += 1;
            } else if let Some(shape) = annotated_shapes.get(output) {
                known_shapes.insert(output.to_owned(), shape.clone());
                report.annotation_fallbacks += 1;
            } else {
                report
                    .missing_shapes
                    .push(format!("{output} from {}", node_context(node)));
            }
        }
    }
    if report.missing_shapes.is_empty() {
        Ok(ShapeInference {
            report,
            folded,
            folded_values,
        })
    } else {
        bail!(
            "missing {} output shape(s); first: {}",
            report.missing_shapes.len(),
            report.missing_shapes[0]
        )
    }
}

fn infer_node_shapes(
    node: &NodeProto,
    known_shapes: &BTreeMap<String, Vec<i64>>,
    tensor_consts: &HashMap<String, TensorConst>,
) -> Result<BTreeMap<String, Vec<i64>>> {
    let mut outputs = BTreeMap::new();
    if node.get_output().iter().all(|output| output.is_empty()) {
        bail!("{} has no non-empty outputs", node_context(node));
    }
    match node.get_op_type() {
        "Identity" | "Sigmoid" | "Softmax" | "Relu" | "HardSwish" | "HardSigmoid" | "PRelu"
        | "Sqrt" | "Erf" | "Pow" | "CumSum" => {
            let shape = input_shape(node, known_shapes, 0)?;
            for output in node.get_output() {
                outputs.insert(output.to_owned(), shape.clone());
            }
        }
        "Unsqueeze" => {
            let mut shape = input_shape(node, known_shapes, 0)?;
            let axes = if node.get_input().len() > 1 {
                const_i64(tensor_consts, &node.get_input()[1])
                    .context("Unsqueeze axes must be constant")?
                    .to_vec()
            } else {
                attr_i64s(node, "axes").context("Unsqueeze axes must be present")?
            };
            let new_rank = shape.len() + axes.len();
            let mut axes = axes
                .into_iter()
                .map(|axis| normalize_axis_for_insert(axis, new_rank))
                .collect::<Result<Vec<_>>>()?;
            axes.sort_unstable();
            if axes.windows(2).any(|pair| pair[0] == pair[1]) {
                bail!("Unsqueeze axes must be unique");
            }
            for axis in axes {
                shape.insert(axis, 1);
            }
            outputs.insert(node.get_output()[0].to_owned(), shape);
        }
        "Squeeze" => {
            let input = input_shape(node, known_shapes, 0)?;
            let axes = if node.get_input().len() > 1 {
                const_i64(tensor_consts, &node.get_input()[1]).map(|values| values.to_vec())
            } else {
                attr_i64s(node, "axes")
            };
            let shape = if let Some(axes) = axes {
                let axes = axes
                    .into_iter()
                    .map(|axis| normalize_axis(axis, input.len()))
                    .collect::<Result<Vec<_>>>()?;
                if axes.iter().any(|&axis| input[axis] != 1) {
                    bail!("Squeeze can only remove dimensions of size 1");
                }
                input
                    .into_iter()
                    .enumerate()
                    .filter_map(|(axis, dim)| (!axes.contains(&axis)).then_some(dim))
                    .collect()
            } else {
                input.into_iter().filter(|dim| *dim != 1).collect()
            };
            outputs.insert(node.get_output()[0].to_owned(), shape);
        }
        "Gemm" => {
            let a = input_shape(node, known_shapes, 0)?;
            let b = input_shape(node, known_shapes, 1)?;
            if a.len() != 2 || b.len() != 2 {
                bail!("Gemm expects rank-2 A and B tensors");
            }
            let trans_a = attr_i64(node, "transA").unwrap_or(0) != 0;
            let trans_b = attr_i64(node, "transB").unwrap_or(0) != 0;
            let m = if trans_a { a[1] } else { a[0] };
            let a_k = if trans_a { a[0] } else { a[1] };
            let b_k = if trans_b { b[1] } else { b[0] };
            if a_k != b_k {
                bail!("Gemm contracting dims mismatch: {a_k} vs {b_k}");
            }
            let n = if trans_b { b[0] } else { b[1] };
            outputs.insert(node.get_output()[0].to_owned(), vec![m, n]);
        }
        "GlobalAveragePool" => {
            // [N, C, d0, d1, ...] -> [N, C, 1, 1, ...]: spatial dims collapse to 1.
            let mut shape = input_shape(node, known_shapes, 0)?;
            if shape.len() < 2 {
                bail!("GlobalAveragePool expects rank >= 2");
            }
            for dim in shape.iter_mut().skip(2) {
                *dim = 1;
            }
            outputs.insert(node.get_output()[0].to_owned(), shape);
        }
        "Add" | "Mul" | "Sub" | "Div" | "Max" => {
            let lhs = input_shape(node, known_shapes, 0)?;
            let rhs = input_shape(node, known_shapes, 1)?;
            outputs.insert(
                node.get_output()[0].to_owned(),
                broadcast_shape(&lhs, &rhs)?,
            );
        }
        "ReduceSum" | "ReduceMean" => {
            let mut shape = input_shape(node, known_shapes, 0)?;
            // ReduceSum (opset 13+) takes axes as input[1]; ReduceMean (opset 11)
            // and older ReduceSum take an `axes` attribute.
            let axes = if node.get_op_type() == "ReduceSum" && node.get_input().len() > 1 {
                const_i64(tensor_consts, &node.get_input()[1])
                    .context("ReduceSum axes must be constant")?
                    .to_vec()
            } else {
                attr_i64s(node, "axes").context("Reduce axes attribute is required")?
            };
            let keepdims = attr_i64(node, "keepdims").unwrap_or(1) != 0;
            let mut axes = axes
                .iter()
                .map(|axis| normalize_axis(*axis, shape.len()))
                .collect::<Result<Vec<_>>>()?;
            axes.sort_unstable();
            axes.dedup();
            if keepdims {
                for axis in axes {
                    shape[axis] = 1;
                }
            } else {
                for axis in axes.into_iter().rev() {
                    shape.remove(axis);
                }
            }
            outputs.insert(node.get_output()[0].to_owned(), shape);
        }
        "SpaceToDepth" => {
            let inp = input_shape(node, known_shapes, 0)?;
            if inp.len() != 4 {
                bail!("SpaceToDepth expects rank-4 input");
            }
            let b = attr_i64(node, "blocksize").context("SpaceToDepth blocksize is required")?;
            if b <= 0 || inp[2] % b != 0 || inp[3] % b != 0 {
                bail!("SpaceToDepth blocksize must be positive and divide H and W");
            }
            let channels = inp[1]
                .checked_mul(b)
                .and_then(|value| value.checked_mul(b))
                .context("SpaceToDepth channel dimension overflow")?;
            outputs.insert(
                node.get_output()[0].to_owned(),
                vec![inp[0], channels, inp[2] / b, inp[3] / b],
            );
        }
        "DepthToSpace" => {
            let inp = input_shape(node, known_shapes, 0)?;
            if inp.len() != 4 {
                bail!("DepthToSpace expects rank-4 input");
            }
            let b = attr_i64(node, "blocksize").context("DepthToSpace blocksize is required")?;
            if b <= 0 {
                bail!("DepthToSpace blocksize must be positive");
            }
            let block_area = b
                .checked_mul(b)
                .context("DepthToSpace blocksize overflow")?;
            if inp[1] % block_area != 0 {
                bail!("DepthToSpace block area must divide channels");
            }
            outputs.insert(
                node.get_output()[0].to_owned(),
                vec![
                    inp[0],
                    inp[1] / block_area,
                    inp[2]
                        .checked_mul(b)
                        .context("DepthToSpace height overflow")?,
                    inp[3]
                        .checked_mul(b)
                        .context("DepthToSpace width overflow")?,
                ],
            );
        }
        "Conv" => {
            let inp = input_shape(node, known_shapes, 0)?;
            let weight = input_shape(node, known_shapes, 1)?;
            outputs.insert(
                node.get_output()[0].to_owned(),
                infer_conv(node, &inp, &weight)?,
            );
        }
        "MaxPool" | "AveragePool" => {
            let inp = input_shape(node, known_shapes, 0)?;
            outputs.insert(node.get_output()[0].to_owned(), infer_pool(node, &inp)?);
        }
        "Pad" => {
            let input = input_shape(node, known_shapes, 0)?;
            let pads = const_i64(tensor_consts, constant_input_name(node, 1, "Pad")?)
                .context("Pad pads must be constant")?;
            if pads.len() != input.len() * 2 {
                bail!("Pad pads length does not match rank");
            }
            let output = (0..input.len())
                .map(|axis| {
                    input[axis]
                        .checked_add(pads[axis])
                        .and_then(|dim| dim.checked_add(pads[axis + input.len()]))
                        .filter(|dim| *dim >= 0)
                        .context("Pad output dimension is negative or overflows")
                })
                .collect::<Result<Vec<_>>>()?;
            outputs.insert(node.get_output()[0].to_owned(), output);
        }
        "Concat" => {
            let axis = normalize_axis(
                attr_i64(node, "axis").unwrap_or(0),
                input_shape(node, known_shapes, 0)?.len(),
            )?;
            let mut output = input_shape(node, known_shapes, 0)?;
            output[axis] = 0;
            for index in 0..node.get_input().len() {
                let shape = input_shape(node, known_shapes, index)?;
                if shape.len() != output.len() {
                    bail!("Concat inputs must have equal rank");
                }
                for (dim_index, dim) in shape.iter().enumerate() {
                    if dim_index == axis {
                        output[dim_index] = output[dim_index]
                            .checked_add(*dim)
                            .context("Concat axis dimension overflow")?;
                    } else if output[dim_index] != *dim {
                        bail!("Concat input dimension mismatch on axis {dim_index}");
                    }
                }
            }
            outputs.insert(node.get_output()[0].to_owned(), output);
        }
        "Split" => {
            let inp = input_shape(node, known_shapes, 0)?;
            let axis = normalize_axis(attr_i64(node, "axis").unwrap_or(0), inp.len())?;
            let splits = if node.get_input().len() > 1 {
                const_i64(tensor_consts, &node.get_input()[1]).map(|values| values.to_vec())
            } else {
                attr_i64s(node, "split")
            };
            let sizes = if let Some(splits) = splits {
                splits
            } else {
                let output_count = node.get_output().len() as i64;
                if output_count == 0 {
                    bail!("Split must have at least one output");
                }
                if inp[axis] % output_count != 0 {
                    bail!(
                        "Split without sizes cannot divide axis {} evenly",
                        inp[axis]
                    );
                }
                vec![inp[axis] / output_count; output_count as usize]
            };
            if sizes.len() != node.get_output().len() {
                bail!("Split size count does not match output count");
            }
            if sizes.iter().any(|size| *size < 0) {
                bail!("Split sizes must be non-negative");
            }
            let total = sizes.iter().try_fold(0i64, |total, &size| {
                total.checked_add(size).context("Split sizes overflow")
            })?;
            if total != inp[axis] {
                bail!(
                    "Split sizes sum to {total}, expected input axis size {}",
                    inp[axis]
                );
            }
            for (output, size) in node.get_output().iter().zip(sizes) {
                let mut shape = inp.clone();
                shape[axis] = size;
                outputs.insert(output.to_owned(), shape);
            }
        }
        "Slice" => {
            let inp = input_shape(node, known_shapes, 0)?;
            let starts = const_i64(tensor_consts, constant_input_name(node, 1, "Slice starts")?)
                .context("Slice starts must be constant")?;
            let ends = const_i64(tensor_consts, constant_input_name(node, 2, "Slice ends")?)
                .context("Slice ends must be constant")?;
            if starts.len() != ends.len() {
                bail!("Slice starts and ends lengths differ");
            }
            let axes = if node.get_input().len() > 3 {
                const_i64(tensor_consts, &node.get_input()[3])
                    .map(|values| values.to_vec())
                    .unwrap_or_else(|| (0..starts.len() as i64).collect())
            } else {
                (0..starts.len() as i64).collect()
            };
            let steps = if node.get_input().len() > 4 {
                const_i64(tensor_consts, &node.get_input()[4])
                    .map(|values| values.to_vec())
                    .unwrap_or_else(|| vec![1; starts.len()])
            } else {
                vec![1; starts.len()]
            };
            if axes.len() != starts.len() || steps.len() != starts.len() {
                bail!("Slice axes/steps lengths must match starts");
            }
            let mut output = inp.clone();
            for (((start, end), axis), step) in starts.iter().zip(ends).zip(axes).zip(steps) {
                if step <= 0 {
                    bail!("Slice only supports positive steps");
                }
                let axis = normalize_axis(axis, inp.len())?;
                let dim = inp[axis];
                let start = normalize_index(*start, dim);
                let end = normalize_index(*end, dim);
                let length = (end - start).max(0);
                output[axis] = length / step + i64::from(length % step != 0);
            }
            outputs.insert(node.get_output()[0].to_owned(), output);
        }
        "Reshape" => {
            let inp = input_shape(node, known_shapes, 0)?;
            let target = const_i64(tensor_consts, constant_input_name(node, 1, "Reshape")?)
                .context("Reshape target must be constant")?;
            outputs.insert(
                node.get_output()[0].to_owned(),
                infer_reshape(&inp, target)?,
            );
        }
        "Transpose" => {
            let inp = input_shape(node, known_shapes, 0)?;
            let perm =
                attr_i64s(node, "perm").unwrap_or_else(|| (0..inp.len() as i64).rev().collect());
            if perm.len() != inp.len() {
                bail!("Transpose perm rank mismatch");
            }
            let normalized = perm
                .into_iter()
                .map(|axis| normalize_axis(axis, inp.len()))
                .collect::<Result<Vec<_>>>()?;
            let unique = normalized.iter().copied().collect::<BTreeSet<_>>();
            if unique.len() != inp.len() {
                bail!("Transpose perm must contain each axis exactly once");
            }
            outputs.insert(
                node.get_output()[0].to_owned(),
                normalized.into_iter().map(|axis| inp[axis]).collect(),
            );
        }
        "MatMul" => {
            let lhs = input_shape(node, known_shapes, 0)?;
            let rhs = input_shape(node, known_shapes, 1)?;
            outputs.insert(node.get_output()[0].to_owned(), infer_matmul(&lhs, &rhs)?);
        }
        "Resize" => {
            let inp = input_shape(node, known_shapes, 0)?;
            let output = if node.get_input().len() > 3 && !node.get_input()[3].is_empty() {
                const_i64(tensor_consts, &node.get_input()[3]).and_then(|values| {
                    (values.len() == inp.len() && values.iter().all(|dim| *dim > 0))
                        .then(|| values.to_vec())
                })
            } else if node.get_input().len() > 2 && !node.get_input()[2].is_empty() {
                const_f32(tensor_consts, &node.get_input()[2]).and_then(|scales| {
                    if scales.len() != inp.len()
                        || scales
                            .iter()
                            .any(|scale| !scale.is_finite() || *scale <= 0.0)
                    {
                        return None;
                    }
                    inp.iter()
                        .zip(scales)
                        .map(|(dim, scale)| {
                            let scaled = (*dim as f64) * f64::from(*scale);
                            if !scaled.is_finite() || scaled < 1.0 || scaled > i64::MAX as f64 {
                                return None;
                            }
                            // ONNX defines scaled output dimensions with floor.
                            Some(scaled.floor() as i64)
                        })
                        .collect::<Option<Vec<_>>>()
                })
            } else {
                None
            };
            outputs.insert(
                node.get_output()[0].to_owned(),
                output.context("Resize must have constant sizes or scales")?,
            );
        }
        "GridSample" => {
            let input = input_shape(node, known_shapes, 0)?;
            let grid = input_shape(node, known_shapes, 1)?;
            if input.len() != 4 || grid.len() != 4 {
                bail!("GridSample expects input rank 4 and grid rank 4");
            }
            if grid[0] != input[0] || grid[3] != 2 {
                bail!("GridSample grid must be [N,H,W,2] with matching batch");
            }
            outputs.insert(
                node.get_output()[0].to_owned(),
                vec![input[0], input[1], grid[1], grid[2]],
            );
        }
        op => bail!("shape inference is not implemented for op {op}"),
    }
    Ok(outputs)
}

fn constant_input_name<'a>(node: &'a NodeProto, index: usize, label: &str) -> Result<&'a str> {
    node.get_input()
        .get(index)
        .filter(|name| !name.is_empty())
        .map(String::as_str)
        .with_context(|| format!("{label} input {index} is missing"))
}

fn normalize_axis_for_insert(axis: i64, rank: usize) -> Result<usize> {
    let rank = rank as i64;
    let axis = if axis < 0 { axis + rank } else { axis };
    if axis < 0 || axis >= rank {
        bail!("insertion axis {axis} is out of range for rank {rank}");
    }
    Ok(axis as usize)
}

/// Reads a node's `auto_pad` attribute, defaulting to `NOTSET` (use `pads`).
fn auto_pad_attr(node: &NodeProto) -> String {
    node.get_attribute()
        .iter()
        .find(|attr| attr.get_name() == "auto_pad")
        .map(|attr| String::from_utf8_lossy(attr.get_s()).to_string())
        .unwrap_or_else(|| "NOTSET".to_owned())
}

/// Resolves an `auto_pad` value into explicit ONNX-order pads
/// `[h_begin, w_begin, h_end, w_end]`. For `NOTSET`, returns `explicit`.
pub(crate) fn resolve_auto_pad(
    auto_pad: &str,
    input: &[i64],
    kernel: [i64; 2],
    strides: [i64; 2],
    dilations: [i64; 2],
    explicit: [i64; 4],
) -> Result<[i64; 4]> {
    if input.len() != 4
        || kernel.iter().any(|value| *value <= 0)
        || strides.iter().any(|value| *value <= 0)
        || dilations.iter().any(|value| *value <= 0)
    {
        bail!("Conv/Pool dimensions, kernel, strides, and dilations must be positive");
    }
    match auto_pad {
        "NOTSET" => Ok(explicit),
        "VALID" => Ok([0, 0, 0, 0]),
        "SAME_UPPER" | "SAME_LOWER" => {
            let mut pads = [0i64; 4];
            for (axis, &spatial) in input[2..4].iter().enumerate() {
                let out = spatial
                    .checked_add(strides[axis] - 1)
                    .context("auto_pad output dimension overflow")?
                    / strides[axis];
                let effective_kernel = (kernel[axis] - 1)
                    .checked_mul(dilations[axis])
                    .and_then(|value| value.checked_add(1))
                    .context("auto_pad effective kernel overflow")?;
                let needed = (out - 1)
                    .checked_mul(strides[axis])
                    .and_then(|value| value.checked_add(effective_kernel))
                    .and_then(|value| value.checked_sub(spatial))
                    .context("auto_pad padding overflow")?;
                let total = needed.max(0);
                let (begin, end) = if auto_pad == "SAME_UPPER" {
                    (total / 2, total - total / 2)
                } else {
                    (total - total / 2, total / 2)
                };
                pads[axis] = begin;
                pads[axis + 2] = end;
            }
            Ok(pads)
        }
        other => bail!("auto_pad `{other}` is not supported"),
    }
}

fn infer_conv(node: &NodeProto, input: &[i64], weight: &[i64]) -> Result<Vec<i64>> {
    if input.len() != 4 || weight.len() != 4 {
        bail!("Conv expects rank-4 input and weight");
    }
    let explicit = attr_i64s(node, "pads").unwrap_or_else(|| vec![0, 0, 0, 0]);
    let strides = attr_i64s(node, "strides").unwrap_or_else(|| vec![1, 1]);
    let dilations = attr_i64s(node, "dilations").unwrap_or_else(|| vec![1, 1]);
    if explicit.len() != 4 || strides.len() != 2 || dilations.len() != 2 {
        bail!("Conv attrs have unexpected lengths");
    }
    let pads = resolve_auto_pad(
        &auto_pad_attr(node),
        input,
        [weight[2], weight[3]],
        [strides[0], strides[1]],
        [dilations[0], dilations[1]],
        [explicit[0], explicit[1], explicit[2], explicit[3]],
    )?
    .to_vec();
    let h = conv_out_dim(
        input[2],
        pads[0],
        pads[2],
        dilations[0],
        weight[2],
        strides[0],
    )?;
    let w = conv_out_dim(
        input[3],
        pads[1],
        pads[3],
        dilations[1],
        weight[3],
        strides[1],
    )?;
    Ok(vec![input[0], weight[0], h, w])
}

fn infer_pool(node: &NodeProto, input: &[i64]) -> Result<Vec<i64>> {
    if input.len() != 4 {
        bail!("MaxPool expects rank-4 input");
    }
    let kernel = attr_i64s(node, "kernel_shape").context("MaxPool kernel_shape is required")?;
    let pads = attr_i64s(node, "pads").unwrap_or_else(|| vec![0, 0, 0, 0]);
    let strides = attr_i64s(node, "strides").unwrap_or_else(|| vec![1, 1]);
    let dilations = attr_i64s(node, "dilations").unwrap_or_else(|| vec![1, 1]);
    if kernel.len() != 2 || pads.len() != 4 || strides.len() != 2 || dilations.len() != 2 {
        bail!("Pool attrs have unexpected lengths");
    }
    if input.iter().any(|dim| *dim <= 0)
        || kernel.iter().any(|value| *value <= 0)
        || strides.iter().any(|value| *value <= 0)
        || dilations.iter().any(|value| *value <= 0)
    {
        bail!("Pool dimensions, kernel, strides, and dilations must be positive");
    }
    let pads = resolve_auto_pad(
        &auto_pad_attr(node),
        input,
        [kernel[0], kernel[1]],
        [strides[0], strides[1]],
        [dilations[0], dilations[1]],
        [pads[0], pads[1], pads[2], pads[3]],
    )?;
    let h = conv_out_dim(
        input[2],
        pads[0],
        pads[2],
        dilations[0],
        kernel[0],
        strides[0],
    )?;
    let w = conv_out_dim(
        input[3],
        pads[1],
        pads[3],
        dilations[1],
        kernel[1],
        strides[1],
    )?;
    Ok(vec![input[0], input[1], h, w])
}

pub(crate) fn conv_out_dim(
    input: i64,
    pad_begin: i64,
    pad_end: i64,
    dilation: i64,
    kernel: i64,
    stride: i64,
) -> Result<i64> {
    if input <= 0 || kernel <= 0 || dilation <= 0 || stride <= 0 {
        bail!("conv/pool dimensions, kernel, dilation, and stride must be positive");
    }
    let effective_kernel = dilation
        .checked_mul(kernel - 1)
        .and_then(|value| value.checked_add(1))
        .context("conv/pool effective kernel overflow")?;
    let numerator = input
        .checked_add(pad_begin)
        .and_then(|value| value.checked_add(pad_end))
        .and_then(|value| value.checked_sub(effective_kernel))
        .context("conv/pool output dimension overflow")?;
    if numerator < 0 {
        bail!("conv/pool effective kernel exceeds the padded input");
    }
    let output = numerator / stride + 1;
    if output <= 0 {
        bail!("conv/pool output dimension must be positive");
    }
    Ok(output)
}

pub(crate) fn infer_reshape(input: &[i64], target: &[i64]) -> Result<Vec<i64>> {
    let input_elems = checked_shape_elements(input, "Reshape input")?;
    let mut output = target.to_vec();
    let mut infer_index = None;
    for (index, dim) in output.iter_mut().enumerate() {
        if *dim == 0 {
            *dim = *input
                .get(index)
                .context("Reshape zero-copy dimension exceeds input rank")?;
        } else if *dim == -1 {
            if infer_index.is_some() {
                bail!("Reshape has more than one inferred dim");
            }
            infer_index = Some(index);
        } else if *dim < -1 {
            bail!("Reshape dimensions must be >= -1");
        }
    }
    if let Some(index) = infer_index {
        let known = checked_shape_elements(
            &output
                .iter()
                .copied()
                .filter(|dim| *dim != -1)
                .collect::<Vec<_>>(),
            "Reshape target",
        )?;
        if known == 0 || input_elems % known != 0 {
            bail!("Reshape inferred dimension is not integral");
        }
        output[index] = input_elems / known;
    }
    if checked_shape_elements(&output, "Reshape output")? != input_elems {
        bail!("Reshape changes element count");
    }
    Ok(output)
}

fn checked_shape_elements(shape: &[i64], label: &str) -> Result<i64> {
    shape.iter().try_fold(1i64, |elements, &dim| {
        if dim < 0 {
            bail!("{label} contains a negative dimension");
        }
        elements
            .checked_mul(dim)
            .with_context(|| format!("{label} element count overflow"))
    })
}

fn infer_matmul(lhs: &[i64], rhs: &[i64]) -> Result<Vec<i64>> {
    let (lhs2, rhs2, drop_m, drop_n) = promote_matmul(lhs, rhs)?;
    let m = lhs2[lhs2.len() - 2];
    let k = lhs2[lhs2.len() - 1];
    let rhs_k = rhs2[rhs2.len() - 2];
    let n = rhs2[rhs2.len() - 1];
    if k != rhs_k {
        bail!("MatMul contracting dims mismatch: {k} vs {rhs_k}");
    }
    let batch = broadcast_shape(&lhs2[..lhs2.len() - 2], &rhs2[..rhs2.len() - 2])?;
    let mut output = batch;
    output.push(m);
    output.push(n);
    // Undo the 1-D promotion: ONNX removes the dim it appended/prepended.
    if drop_n {
        output.pop();
    }
    if drop_m {
        output.remove(output.len() - if drop_n { 1 } else { 2 });
    }
    Ok(output)
}

/// ONNX MatMul 1-D operand promotion. A 1-D `lhs` `[k]` becomes `[1,k]`
/// (its `m` dim is later removed); a 1-D `rhs` `[k]` becomes `[k,1]` (its `n`
/// dim is later removed). Returns the promoted shapes and which output dims to
/// drop. Both ranks are >= 2 after promotion.
pub(crate) fn promote_matmul(lhs: &[i64], rhs: &[i64]) -> Result<(Vec<i64>, Vec<i64>, bool, bool)> {
    if lhs.is_empty() || rhs.is_empty() {
        bail!("MatMul operands must be rank >= 1");
    }
    let drop_m = lhs.len() == 1;
    let drop_n = rhs.len() == 1;
    let lhs2 = if drop_m {
        vec![1, lhs[0]]
    } else {
        lhs.to_vec()
    };
    let rhs2 = if drop_n {
        vec![rhs[0], 1]
    } else {
        rhs.to_vec()
    };
    Ok((lhs2, rhs2, drop_m, drop_n))
}

pub(crate) fn broadcast_shape(lhs: &[i64], rhs: &[i64]) -> Result<Vec<i64>> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vision::onnx_pb::AttributeProto;

    fn node(op: &str, inputs: &[&str], outputs: &[&str]) -> NodeProto {
        NodeProto {
            op_type: Some(op.to_owned()),
            input: inputs.iter().map(|value| (*value).to_owned()).collect(),
            output: outputs.iter().map(|value| (*value).to_owned()).collect(),
            ..NodeProto::new()
        }
    }

    fn ints_attr(name: &str, values: &[i64]) -> AttributeProto {
        AttributeProto {
            name: Some(name.to_owned()),
            ints: values.to_vec(),
            ..AttributeProto::new()
        }
    }

    #[test]
    fn malformed_shape_helpers_return_errors_instead_of_panicking() {
        let malformed_reshapes = [
            (vec![2, 3], vec![0, 0, 0]),
            (vec![2, 3], vec![-2, 3]),
            (vec![2, 3], vec![-1, 0, 0]),
            (vec![i64::MAX, 2], vec![-1]),
        ];
        for (input, target) in malformed_reshapes {
            let outcome = std::panic::catch_unwind(|| infer_reshape(&input, &target));
            assert!(
                outcome.is_ok(),
                "reshape panicked for {input:?} -> {target:?}"
            );
            assert!(outcome.unwrap().is_err());
        }

        for stride in [0, -1] {
            let outcome = std::panic::catch_unwind(|| conv_out_dim(10, 0, 0, 1, 3, stride));
            assert!(outcome.is_ok());
            assert!(outcome.unwrap().is_err());
        }

        let overflow = std::panic::catch_unwind(|| {
            resolve_auto_pad(
                "SAME_UPPER",
                &[1, 1, i64::MAX, i64::MAX],
                [3, 3],
                [2, 2],
                [1, 1],
                [0; 4],
            )
        });
        assert!(overflow.is_ok());
        assert!(overflow.unwrap().is_err());
        assert_eq!(normalize_index(i64::MIN, i64::MAX), 0);
    }

    #[test]
    fn malformed_nodes_do_not_panic_during_shape_inference() {
        let mut known = BTreeMap::new();
        known.insert("x".to_owned(), vec![1, 1, 8, 8]);
        known.insert("rank3".to_owned(), vec![1, 8, 8]);
        known.insert("negative".to_owned(), vec![1, -1]);
        let constants = HashMap::new();

        let mut short_pool = node("MaxPool", &["x"], &["y"]);
        short_pool.attribute.push(ints_attr("kernel_shape", &[3]));
        let zero_output_split = node("Split", &["x"], &[]);
        let missing_slice_constants = node("Slice", &["x"], &["y"]);
        let mismatched_concat = node("Concat", &["x", "rank3"], &["y"]);
        let unresolved_input = node("Identity", &["negative"], &["y"]);

        for malformed in [
            short_pool,
            zero_output_split,
            missing_slice_constants,
            mismatched_concat,
            unresolved_input,
        ] {
            let outcome =
                std::panic::catch_unwind(|| infer_node_shapes(&malformed, &known, &constants));
            assert!(
                outcome.is_ok(),
                "{} unexpectedly panicked",
                malformed.get_op_type()
            );
            assert!(outcome.unwrap().is_err());
        }
    }
}
