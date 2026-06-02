//! Shape inference for the supported YOLO op subset.

use std::collections::{BTreeMap, HashMap};

use anyhow::{Context, Result, bail};

use crate::vision::onnx_pb::NodeProto;

use super::attrs::{
    attr_i64, attr_i64s, const_f32, const_i64, input_shape, node_context, normalize_axis,
    normalize_index,
};
use super::types::TensorConst;

#[derive(Debug, Default)]
pub(crate) struct ShapeReport {
    pub(crate) inferred_outputs: usize,
    pub(crate) annotation_fallbacks: usize,
    pub(crate) missing_shapes: Vec<String>,
}

pub(crate) fn infer_all_shapes(
    nodes: &[NodeProto],
    annotated_shapes: &BTreeMap<String, Vec<i64>>,
    tensor_consts: &HashMap<String, TensorConst>,
    known_shapes: &mut BTreeMap<String, Vec<i64>>,
) -> Result<ShapeReport> {
    let mut report = ShapeReport::default();
    for (index, node) in nodes.iter().enumerate() {
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
        Ok(report)
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
    match node.get_op_type() {
        "Identity" | "Sigmoid" | "Softmax" | "Relu" | "HardSwish" | "HardSigmoid" | "PRelu"
        | "Sqrt" | "Pow" | "CumSum" => {
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
                .collect::<Vec<_>>();
            axes.sort_unstable();
            for axis in axes {
                shape.insert(axis.min(shape.len()), 1);
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
            let trans_a = attr_i64(node, "transA").unwrap_or(0) != 0;
            let trans_b = attr_i64(node, "transB").unwrap_or(0) != 0;
            let m = if trans_a { a[1] } else { a[0] };
            let n = if trans_b { b[0] } else { b[1] };
            outputs.insert(node.get_output()[0].to_owned(), vec![m, n]);
        }
        "GlobalAveragePool" => {
            // [N, C, d0, d1, ...] -> [N, C, 1, 1, ...]: spatial dims collapse to 1.
            let mut shape = input_shape(node, known_shapes, 0)?;
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
        "ReduceSum" => {
            let mut shape = input_shape(node, known_shapes, 0)?;
            let axes = const_i64(tensor_consts, &node.get_input()[1])
                .context("ReduceSum axes must be constant")?;
            let keepdims = attr_i64(node, "keepdims").unwrap_or(1) != 0;
            let mut axes = axes
                .iter()
                .map(|axis| normalize_axis(*axis, shape.len()))
                .collect::<Result<Vec<_>>>()?;
            axes.sort_unstable();
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
            outputs.insert(
                node.get_output()[0].to_owned(),
                vec![inp[0], inp[1] * b * b, inp[2] / b, inp[3] / b],
            );
        }
        "DepthToSpace" => {
            let inp = input_shape(node, known_shapes, 0)?;
            if inp.len() != 4 {
                bail!("DepthToSpace expects rank-4 input");
            }
            let b = attr_i64(node, "blocksize").context("DepthToSpace blocksize is required")?;
            outputs.insert(
                node.get_output()[0].to_owned(),
                vec![inp[0], inp[1] / (b * b), inp[2] * b, inp[3] * b],
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
        "MaxPool" => {
            let inp = input_shape(node, known_shapes, 0)?;
            outputs.insert(node.get_output()[0].to_owned(), infer_pool(node, &inp)?);
        }
        "Pad" => {
            let input = input_shape(node, known_shapes, 0)?;
            let pads = const_i64(tensor_consts, &node.get_input()[1])
                .context("Pad pads must be constant")?;
            if pads.len() != input.len() * 2 {
                bail!("Pad pads length does not match rank");
            }
            let output = (0..input.len())
                .map(|axis| input[axis] + pads[axis] + pads[axis + input.len()])
                .collect::<Vec<_>>();
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
                for (dim_index, dim) in shape.iter().enumerate() {
                    if dim_index == axis {
                        output[dim_index] += dim;
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
            for (output, size) in node.get_output().iter().zip(sizes) {
                let mut shape = inp.clone();
                shape[axis] = size;
                outputs.insert(output.to_owned(), shape);
            }
        }
        "Slice" => {
            let inp = input_shape(node, known_shapes, 0)?;
            let starts = const_i64(tensor_consts, &node.get_input()[1])
                .context("Slice starts must be constant")?;
            let ends = const_i64(tensor_consts, &node.get_input()[2])
                .context("Slice ends must be constant")?;
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
            let mut output = inp.clone();
            for (((start, end), axis), step) in starts.iter().zip(ends).zip(axes).zip(steps) {
                if step <= 0 {
                    bail!("Slice only supports positive steps");
                }
                let axis = normalize_axis(axis, inp.len())?;
                let dim = inp[axis];
                let start = normalize_index(*start, dim);
                let end = normalize_index(*end, dim);
                output[axis] = ((end - start).max(0) + step - 1) / step;
            }
            outputs.insert(node.get_output()[0].to_owned(), output);
        }
        "Reshape" => {
            let inp = input_shape(node, known_shapes, 0)?;
            let target = const_i64(tensor_consts, &node.get_input()[1])
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
            outputs.insert(
                node.get_output()[0].to_owned(),
                perm.into_iter()
                    .map(|axis| normalize_axis(axis, inp.len()).map(|axis| inp[axis]))
                    .collect::<Result<Vec<_>>>()?,
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
                const_i64(tensor_consts, &node.get_input()[3]).map(|values| values.to_vec())
            } else if node.get_input().len() > 2 && !node.get_input()[2].is_empty() {
                const_f32(tensor_consts, &node.get_input()[2]).map(|scales| {
                    inp.iter()
                        .zip(scales)
                        .map(|(dim, scale)| ((*dim as f32) * scale).round() as i64)
                        .collect()
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
            outputs.insert(
                node.get_output()[0].to_owned(),
                vec![input[0], input[1], grid[1], grid[2]],
            );
        }
        op => bail!("shape inference is not implemented for op {op}"),
    }
    Ok(outputs)
}

fn normalize_axis_for_insert(axis: i64, rank: usize) -> usize {
    let rank = rank as i64;
    let axis = if axis < 0 { axis + rank } else { axis };
    axis.clamp(0, rank) as usize
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
    match auto_pad {
        "NOTSET" => Ok(explicit),
        "VALID" => Ok([0, 0, 0, 0]),
        "SAME_UPPER" | "SAME_LOWER" => {
            let mut pads = [0i64; 4];
            for (axis, &spatial) in input[2..4].iter().enumerate() {
                let out = (spatial + strides[axis] - 1) / strides[axis];
                let needed = (out - 1) * strides[axis] + ((kernel[axis] - 1) * dilations[axis] + 1)
                    - spatial;
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
        other => bail!("Conv auto_pad `{other}` is not supported"),
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
    let auto_pad = node
        .get_attribute()
        .iter()
        .find(|a| a.get_name() == "auto_pad")
        .map(|a| String::from_utf8_lossy(a.get_s()).to_string())
        .unwrap_or_else(|| "NOTSET".to_owned());
    let pads = resolve_auto_pad(
        &auto_pad,
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
    );
    let w = conv_out_dim(
        input[3],
        pads[1],
        pads[3],
        dilations[1],
        weight[3],
        strides[1],
    );
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
    let h = conv_out_dim(
        input[2],
        pads[0],
        pads[2],
        dilations[0],
        kernel[0],
        strides[0],
    );
    let w = conv_out_dim(
        input[3],
        pads[1],
        pads[3],
        dilations[1],
        kernel[1],
        strides[1],
    );
    Ok(vec![input[0], input[1], h, w])
}

pub(crate) fn conv_out_dim(
    input: i64,
    pad_begin: i64,
    pad_end: i64,
    dilation: i64,
    kernel: i64,
    stride: i64,
) -> i64 {
    (input + pad_begin + pad_end - dilation * (kernel - 1) - 1) / stride + 1
}

fn infer_reshape(input: &[i64], target: &[i64]) -> Result<Vec<i64>> {
    let input_elems = input.iter().product::<i64>();
    let mut output = target.to_vec();
    let mut infer_index = None;
    for (index, dim) in output.iter_mut().enumerate() {
        if *dim == 0 {
            *dim = input[index];
        } else if *dim == -1 {
            if infer_index.is_some() {
                bail!("Reshape has more than one inferred dim");
            }
            infer_index = Some(index);
        }
    }
    if let Some(index) = infer_index {
        let known = output.iter().filter(|dim| **dim != -1).product::<i64>();
        output[index] = input_elems / known;
    }
    if output.iter().product::<i64>() != input_elems {
        bail!("Reshape changes element count");
    }
    Ok(output)
}

fn infer_matmul(lhs: &[i64], rhs: &[i64]) -> Result<Vec<i64>> {
    if lhs.len() < 2 || rhs.len() < 2 {
        bail!("MatMul expects rank >= 2");
    }
    let m = lhs[lhs.len() - 2];
    let k = lhs[lhs.len() - 1];
    let rhs_k = rhs[rhs.len() - 2];
    let n = rhs[rhs.len() - 1];
    if k != rhs_k {
        bail!("MatMul contracting dims mismatch: {k} vs {rhs_k}");
    }
    let batch = broadcast_shape(&lhs[..lhs.len() - 2], &rhs[..rhs.len() - 2])?;
    let mut output = batch;
    output.push(m);
    output.push(n);
    Ok(output)
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
