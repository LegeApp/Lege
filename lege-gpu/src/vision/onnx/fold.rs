//! Shape-plumbing constant folding.
//!
//! Dynamic-resolution models (paddle-deskew, sauvola) compute output tensor
//! sizes — and, for sauvola, the window pixel-count maps — from the input shape
//! with a subgraph of shape ops. ORT's basic optimizer folds that subgraph away
//! only when H/W are static, which is why the static prepared artifacts exist.
//! Instead of baking a fixed size, we fold the subgraph here during preparation
//! once the concrete input dims are injected: `Shape(x)` is just `known_shapes[x]`,
//! and the int64 arithmetic on those vectors collapses to constants. Those
//! constants then feed the `Resize`/`Reshape`/`ConstantOfShape` consumers,
//! letting the existing shape inference + lowering proceed at the real size.
//!
//! A node folds only when every data input is already constant (`Shape` always
//! folds, using its input's known shape). Ops that also appear on real feature
//! maps — `Concat`/`Slice`/`Cast`/`Split`/`Unsqueeze` — therefore stay as compute
//! ops on the data path and only fold on the shape path.
//!
//! Folded values are bools-as-int64 (0/1) for `Equal`/`Not`. A folded float
//! constant that a *kept* op consumes as data (e.g. the `ConstantOfShape` ones
//! tensor feeding `CumSum` for sauvola's integral-image counts) is promoted to a
//! GPU initializer by the caller.

use std::collections::{BTreeMap, HashMap};

use anyhow::{Context, Result, bail};

use crate::vision::onnx_pb::NodeProto;

use super::attrs::{
    attr_i64, attr_i64s, attr_tensor, normalize_axis, normalize_index, tensor_const,
};
use super::types::TensorConst;

/// A value produced by folding a shape-plumbing node.
pub(crate) struct Folded {
    pub(crate) name: String,
    pub(crate) value: TensorConst,
    pub(crate) shape: Vec<i64>,
}

/// Ops eligible for shape-plumbing folding. `Shape` is always foldable; the rest
/// fold only when all their data inputs are already constant.
fn is_foldable_op(op: &str) -> bool {
    matches!(
        op,
        "Shape"
            | "Cast"
            | "Slice"
            | "Concat"
            | "Split"
            | "Gather"
            | "Unsqueeze"
            | "Squeeze"
            | "Neg"
            | "Not"
            | "Equal"
            | "ConstantOfShape"
            | "Reshape"
            | "Add"
            | "Sub"
            | "Mul"
            | "Div"
    )
}

/// Attempts to fold `node` into constant outputs.
///
/// - `Ok(None)`  — not a fold candidate (or a foldable op whose data inputs are
///   not all constant); the caller keeps it as a compute op.
/// - `Ok(Some(_))` — folded; the caller records the constants and drops the node.
/// - `Err(_)` — the node should have folded but the values were unsupported.
pub(crate) fn try_fold(
    node: &NodeProto,
    known_shapes: &BTreeMap<String, Vec<i64>>,
    tensor_consts: &HashMap<String, TensorConst>,
) -> Result<Option<Vec<Folded>>> {
    let op = node.get_op_type();
    if !is_foldable_op(op) {
        return Ok(None);
    }
    validate_fold_arity(node)?;
    if op == "Shape" {
        return Ok(Some(fold_shape(node, known_shapes)?));
    }

    let inputs = node
        .get_input()
        .iter()
        .filter(|name| !name.is_empty())
        .map(String::as_str)
        .collect::<Vec<_>>();
    // Only fold when this is genuinely shape plumbing: every input constant.
    if !inputs.iter().all(|name| tensor_consts.contains_key(*name)) {
        return Ok(None);
    }
    let outputs = node
        .get_output()
        .iter()
        .filter(|name| !name.is_empty())
        .cloned()
        .collect::<Vec<_>>();

    let folded = match op {
        "Cast" => vec![fold_cast(
            node,
            &inputs,
            known_shapes,
            tensor_consts,
            &outputs,
        )?],
        "Slice" => vec![fold_slice(&inputs, tensor_consts, &outputs)?],
        "Concat" => vec![fold_concat(&inputs, tensor_consts, &outputs)?],
        "Split" => fold_split(node, &inputs, known_shapes, tensor_consts, &outputs)?,
        "Gather" => vec![fold_gather(
            node,
            &inputs,
            known_shapes,
            tensor_consts,
            &outputs,
        )?],
        "Unsqueeze" => vec![fold_unsqueeze(
            node,
            &inputs,
            known_shapes,
            tensor_consts,
            &outputs,
        )?],
        "Squeeze" => vec![fold_squeeze(
            node,
            &inputs,
            known_shapes,
            tensor_consts,
            &outputs,
        )?],
        "Neg" => vec![fold_neg(&inputs, known_shapes, tensor_consts, &outputs)?],
        "Not" => vec![fold_not(&inputs, known_shapes, tensor_consts, &outputs)?],
        "Equal" => vec![fold_equal(&inputs, tensor_consts, &outputs)?],
        "ConstantOfShape" => vec![fold_constant_of_shape(
            node,
            &inputs,
            tensor_consts,
            &outputs,
        )?],
        "Reshape" => vec![fold_reshape(
            &inputs,
            known_shapes,
            tensor_consts,
            &outputs,
        )?],
        "Add" | "Sub" | "Mul" | "Div" => {
            vec![fold_binary(
                op,
                &inputs,
                known_shapes,
                tensor_consts,
                &outputs,
            )?]
        }
        _ => return Ok(None),
    };
    Ok(Some(folded))
}

fn validate_fold_arity(node: &NodeProto) -> Result<()> {
    let op = node.get_op_type();
    let required_inputs = match op {
        "Shape" | "Cast" | "Concat" | "Split" | "Unsqueeze" | "Squeeze" | "Neg" | "Not"
        | "ConstantOfShape" => 1,
        "Gather" | "Reshape" | "Equal" | "Add" | "Sub" | "Mul" | "Div" => 2,
        "Slice" => 3,
        _ => return Ok(()),
    };
    if node.get_input().len() < required_inputs
        || node.get_input()[..required_inputs]
            .iter()
            .any(String::is_empty)
    {
        bail!("shape-fold {op} is missing a required input");
    }
    if node.get_output().iter().all(String::is_empty) {
        bail!("shape-fold {op} has no non-empty output");
    }
    Ok(())
}

fn fold_shape(node: &NodeProto, known_shapes: &BTreeMap<String, Vec<i64>>) -> Result<Vec<Folded>> {
    let input = &node.get_input()[0];
    let shape = known_shapes
        .get(input)
        .with_context(|| format!("Shape input `{input}` has no known shape"))?;
    let rank = shape.len() as i64;
    let norm = |value: i64| (if value < 0 { value + rank } else { value }).clamp(0, rank);
    let start = norm(attr_i64(node, "start").unwrap_or(0));
    let end = norm(attr_i64(node, "end").unwrap_or(rank));
    let values = if start <= end {
        shape[start as usize..end as usize].to_vec()
    } else {
        Vec::new()
    };
    Ok(vec![Folded {
        name: node.get_output()[0].clone(),
        shape: vec![values.len() as i64],
        value: TensorConst::Int64(values),
    }])
}

fn fold_cast(
    node: &NodeProto,
    inputs: &[&str],
    known_shapes: &BTreeMap<String, Vec<i64>>,
    tensor_consts: &HashMap<String, TensorConst>,
    outputs: &[String],
) -> Result<Folded> {
    let to = attr_i64(node, "to").context("Cast missing `to` attribute")?;
    let source = &tensor_consts[inputs[0]];
    let value = match to {
        1 => TensorConst::Float32(as_f32(source)),
        6 | 7 | 9 => TensorConst::Int64(as_i64(source)),
        other => bail!("shape-fold Cast to dtype {other} is not supported"),
    };
    Ok(Folded {
        name: outputs[0].clone(),
        shape: shape_of(inputs[0], known_shapes, &value),
        value,
    })
}

/// Folds `Slice` on a rank-1 constant (the shape-plumbing case).
fn fold_slice(
    inputs: &[&str],
    tensor_consts: &HashMap<String, TensorConst>,
    outputs: &[String],
) -> Result<Folded> {
    let data = as_i64(&tensor_consts[inputs[0]]);
    let dim = data.len() as i64;
    let starts = as_i64(&tensor_consts[inputs[1]]);
    let ends = as_i64(&tensor_consts[inputs[2]]);
    let axes = inputs.get(3).map(|name| as_i64(&tensor_consts[*name]));
    let steps = inputs.get(4).map(|name| as_i64(&tensor_consts[*name]));
    if starts.len() != 1 || ends.len() != 1 {
        bail!("shape-fold Slice only supports a single rank-1 slice range");
    }
    if let Some(axes) = &axes {
        let axis = *axes.first().unwrap_or(&0);
        if axis != 0 && axis != -1 {
            bail!("shape-fold Slice only supports axis 0 on rank-1 data");
        }
    }
    let step = steps.and_then(|s| s.first().copied()).unwrap_or(1);
    if step <= 0 {
        bail!("shape-fold Slice only supports positive steps");
    }
    let start = normalize_index(starts[0], dim);
    let end = normalize_index(ends[0], dim);
    let mut values = Vec::new();
    let mut index = start;
    while index < end {
        values.push(data[index as usize]);
        let Some(next) = index.checked_add(step) else {
            break;
        };
        index = next;
    }
    Ok(Folded {
        name: outputs[0].clone(),
        shape: vec![values.len() as i64],
        value: TensorConst::Int64(values),
    })
}

/// Folds `Concat` of rank-1 (or scalar) constants. Integer inputs stay integer;
/// any float input promotes the result to float.
fn fold_concat(
    inputs: &[&str],
    tensor_consts: &HashMap<String, TensorConst>,
    outputs: &[String],
) -> Result<Folded> {
    let any_float = inputs
        .iter()
        .any(|name| matches!(tensor_consts[*name], TensorConst::Float32(_)));
    let value = if any_float {
        let mut values = Vec::new();
        for name in inputs {
            values.extend(as_f32(&tensor_consts[*name]));
        }
        TensorConst::Float32(values)
    } else {
        let mut values = Vec::new();
        for name in inputs {
            values.extend(as_i64(&tensor_consts[*name]));
        }
        TensorConst::Int64(values)
    };
    let len = const_len(&value) as i64;
    Ok(Folded {
        name: outputs[0].clone(),
        shape: vec![len],
        value,
    })
}

/// Folds `Split` of a rank-1 constant into its pieces.
fn fold_split(
    node: &NodeProto,
    inputs: &[&str],
    known_shapes: &BTreeMap<String, Vec<i64>>,
    tensor_consts: &HashMap<String, TensorConst>,
    outputs: &[String],
) -> Result<Vec<Folded>> {
    let data = as_i64(&tensor_consts[inputs[0]]);
    let rank = known_shapes.get(inputs[0]).map(Vec::len).unwrap_or(1);
    if normalize_axis(attr_i64(node, "axis").unwrap_or(0), rank)? != 0 || rank > 1 {
        bail!("shape-fold Split only supports axis 0 on rank-1 data");
    }
    let sizes = if inputs.len() > 1 {
        as_i64(&tensor_consts[inputs[1]])
    } else if let Some(split) = attr_i64s(node, "split") {
        split
    } else {
        let count = outputs.len() as i64;
        if count == 0 || data.len() as i64 % count != 0 {
            bail!("shape-fold Split cannot divide rank-1 data evenly");
        }
        vec![data.len() as i64 / count; outputs.len()]
    };
    if sizes.len() != outputs.len() {
        bail!("shape-fold Split size count does not match outputs");
    }
    if sizes.iter().any(|size| *size < 0) {
        bail!("shape-fold Split sizes must be non-negative");
    }
    let mut folded = Vec::with_capacity(outputs.len());
    let mut offset = 0usize;
    for (out, size) in outputs.iter().zip(sizes) {
        let size = usize::try_from(size).context("shape-fold Split size exceeds usize")?;
        let end = offset
            .checked_add(size)
            .filter(|end| *end <= data.len())
            .context("shape-fold Split sizes exceed input length")?;
        folded.push(Folded {
            name: out.clone(),
            shape: vec![size as i64],
            value: TensorConst::Int64(data[offset..end].to_vec()),
        });
        offset = end;
    }
    if offset != data.len() {
        bail!("shape-fold Split sizes do not cover the input");
    }
    Ok(folded)
}

/// Folds `Gather` of a rank-1 constant along axis 0.
fn fold_gather(
    node: &NodeProto,
    inputs: &[&str],
    known_shapes: &BTreeMap<String, Vec<i64>>,
    tensor_consts: &HashMap<String, TensorConst>,
    outputs: &[String],
) -> Result<Folded> {
    let rank = known_shapes.get(inputs[0]).map(Vec::len).unwrap_or(1);
    if normalize_axis(attr_i64(node, "axis").unwrap_or(0), rank.max(1))? != 0 || rank > 1 {
        bail!("shape-fold Gather only supports axis 0 on rank-1 data");
    }
    let data = as_i64(&tensor_consts[inputs[0]]);
    let dim = data.len() as i64;
    if dim == 0 && !as_i64(&tensor_consts[inputs[1]]).is_empty() {
        bail!("shape-fold Gather cannot index an empty tensor");
    }
    let indices = as_i64(&tensor_consts[inputs[1]]);
    let gathered = indices
        .iter()
        .map(|index| {
            let resolved = if *index < 0 {
                index.saturating_add(dim)
            } else {
                *index
            };
            if resolved < 0 || resolved >= dim {
                bail!("shape-fold Gather index {index} is out of bounds for {dim}");
            }
            Ok(data[resolved as usize])
        })
        .collect::<Result<Vec<_>>>()?;
    // A scalar index (rank-0 indices) yields a scalar result.
    let indices_rank = known_shapes.get(inputs[1]).map(Vec::len).unwrap_or(0);
    let shape = if indices_rank == 0 {
        Vec::new()
    } else {
        vec![gathered.len() as i64]
    };
    Ok(Folded {
        name: outputs[0].clone(),
        shape,
        value: TensorConst::Int64(gathered),
    })
}

fn fold_unsqueeze(
    node: &NodeProto,
    inputs: &[&str],
    known_shapes: &BTreeMap<String, Vec<i64>>,
    tensor_consts: &HashMap<String, TensorConst>,
    outputs: &[String],
) -> Result<Folded> {
    let value = tensor_consts[inputs[0]].clone();
    let mut shape = known_shapes
        .get(inputs[0])
        .cloned()
        .unwrap_or_else(|| vec![const_len(&value) as i64]);
    let axes = fold_axes(node, inputs, tensor_consts, 1)?;
    let new_rank = shape
        .len()
        .checked_add(axes.len())
        .context("shape-fold Unsqueeze rank overflow")?;
    let mut axes = axes
        .into_iter()
        .map(|axis| {
            let axis = if axis < 0 {
                axis + new_rank as i64
            } else {
                axis
            };
            if axis < 0 || axis >= new_rank as i64 {
                bail!("shape-fold Unsqueeze axis {axis} is out of range");
            }
            Ok(axis as usize)
        })
        .collect::<Result<Vec<_>>>()?;
    axes.sort_unstable();
    if axes.windows(2).any(|pair| pair[0] == pair[1]) {
        bail!("shape-fold Unsqueeze axes must be unique");
    }
    for axis in axes {
        shape.insert(axis, 1);
    }
    Ok(Folded {
        name: outputs[0].clone(),
        shape,
        value,
    })
}

fn fold_squeeze(
    node: &NodeProto,
    inputs: &[&str],
    known_shapes: &BTreeMap<String, Vec<i64>>,
    tensor_consts: &HashMap<String, TensorConst>,
    outputs: &[String],
) -> Result<Folded> {
    let value = tensor_consts[inputs[0]].clone();
    let shape = known_shapes
        .get(inputs[0])
        .cloned()
        .unwrap_or_else(|| vec![const_len(&value) as i64]);
    let shape = if inputs.len() > 1 || node.get_attribute().iter().any(|a| a.get_name() == "axes") {
        let axes = fold_axes(node, inputs, tensor_consts, 1)?
            .into_iter()
            .map(|axis| normalize_axis(axis, shape.len()))
            .collect::<Result<Vec<_>>>()?;
        if axes.iter().any(|axis| shape[*axis] != 1) {
            bail!("shape-fold Squeeze can only remove dimensions of size 1");
        }
        shape
            .into_iter()
            .enumerate()
            .filter_map(|(axis, dim)| (!axes.contains(&axis)).then_some(dim))
            .collect()
    } else {
        shape.into_iter().filter(|dim| *dim != 1).collect()
    };
    Ok(Folded {
        name: outputs[0].clone(),
        shape,
        value,
    })
}

fn fold_neg(
    inputs: &[&str],
    known_shapes: &BTreeMap<String, Vec<i64>>,
    tensor_consts: &HashMap<String, TensorConst>,
    outputs: &[String],
) -> Result<Folded> {
    let value = match &tensor_consts[inputs[0]] {
        TensorConst::Int64(values) => TensorConst::Int64(
            values
                .iter()
                .map(|value| value.checked_neg().context("shape-fold Neg overflow"))
                .collect::<Result<Vec<_>>>()?,
        ),
        TensorConst::Float32(values) => TensorConst::Float32(values.iter().map(|v| -v).collect()),
    };
    Ok(Folded {
        name: outputs[0].clone(),
        shape: shape_of(inputs[0], known_shapes, &value),
        value,
    })
}

fn fold_not(
    inputs: &[&str],
    known_shapes: &BTreeMap<String, Vec<i64>>,
    tensor_consts: &HashMap<String, TensorConst>,
    outputs: &[String],
) -> Result<Folded> {
    // Bools are carried as int64 0/1.
    let value = TensorConst::Int64(
        as_i64(&tensor_consts[inputs[0]])
            .iter()
            .map(|v| (*v == 0) as i64)
            .collect(),
    );
    Ok(Folded {
        name: outputs[0].clone(),
        shape: shape_of(inputs[0], known_shapes, &value),
        value,
    })
}

fn fold_equal(
    inputs: &[&str],
    tensor_consts: &HashMap<String, TensorConst>,
    outputs: &[String],
) -> Result<Folded> {
    let lhs = as_i64(&tensor_consts[inputs[0]]);
    let rhs = as_i64(&tensor_consts[inputs[1]]);
    if lhs.is_empty() != rhs.is_empty() {
        bail!("shape-fold Equal cannot broadcast an empty and non-empty tensor");
    }
    let len = lhs.len().max(rhs.len());
    let at = |values: &[i64], index: usize| {
        if values.len() == 1 {
            values[0]
        } else {
            values[index]
        }
    };
    if lhs.len() != rhs.len() && lhs.len() != 1 && rhs.len() != 1 {
        bail!("shape-fold Equal requires equal lengths or a scalar operand");
    }
    let values = (0..len)
        .map(|index| (at(&lhs, index) == at(&rhs, index)) as i64)
        .collect::<Vec<_>>();
    Ok(Folded {
        name: outputs[0].clone(),
        shape: vec![len as i64],
        value: TensorConst::Int64(values),
    })
}

/// Folds `ConstantOfShape` to a filled constant. Sauvola uses this to build the
/// all-ones tensors whose integral images become per-window pixel counts; the
/// caller promotes the (float) result to a GPU initializer for the `CumSum`.
fn fold_constant_of_shape(
    node: &NodeProto,
    inputs: &[&str],
    tensor_consts: &HashMap<String, TensorConst>,
    outputs: &[String],
) -> Result<Folded> {
    let shape = as_i64(&tensor_consts[inputs[0]]);
    if shape.iter().any(|dim| *dim < 0) {
        bail!("ConstantOfShape shape has negative dims: {shape:?}");
    }
    let count = shape.iter().try_fold(1i64, |count, dim| {
        count
            .checked_mul(*dim)
            .context("ConstantOfShape element count overflow")
    })?;
    let count = usize::try_from(count).context("ConstantOfShape element count exceeds usize")?;
    let fill_f32 = |fill: f32| -> Result<Vec<f32>> {
        let mut values = Vec::new();
        values
            .try_reserve_exact(count)
            .context("ConstantOfShape allocation failed")?;
        values.resize(count, fill);
        Ok(values)
    };
    let fill_i64 = |fill: i64| -> Result<Vec<i64>> {
        let mut values = Vec::new();
        values
            .try_reserve_exact(count)
            .context("ConstantOfShape allocation failed")?;
        values.resize(count, fill);
        Ok(values)
    };
    // `value` is a 1-element tensor attribute; default is float 0.
    let value = match attr_tensor(node, "value").and_then(tensor_const) {
        Some(TensorConst::Float32(values)) => {
            TensorConst::Float32(fill_f32(values.first().copied().unwrap_or(0.0))?)
        }
        Some(TensorConst::Int64(values)) => {
            TensorConst::Int64(fill_i64(values.first().copied().unwrap_or(0))?)
        }
        None => TensorConst::Float32(fill_f32(0.0)?),
    };
    Ok(Folded {
        name: outputs[0].clone(),
        shape,
        value,
    })
}

/// Folds `Reshape` of a constant: the data is unchanged, only the logical shape
/// is reinterpreted (with ONNX 0 = keep-dim and -1 = inferred semantics).
fn fold_reshape(
    inputs: &[&str],
    known_shapes: &BTreeMap<String, Vec<i64>>,
    tensor_consts: &HashMap<String, TensorConst>,
    outputs: &[String],
) -> Result<Folded> {
    let value = tensor_consts[inputs[0]].clone();
    let input_shape = known_shapes
        .get(inputs[0])
        .cloned()
        .unwrap_or_else(|| vec![const_len(&value) as i64]);
    let target = super::shape::infer_reshape(&input_shape, &as_i64(&tensor_consts[inputs[1]]))?;
    Ok(Folded {
        name: outputs[0].clone(),
        shape: target,
        value,
    })
}

/// Folds elementwise `Add`/`Sub`/`Mul`/`Div` on integer or float constants with
/// scalar / equal-length broadcasting (the shape-arithmetic case). Integer `Div`
/// truncates toward zero, matching ONNX integer division.
fn fold_binary(
    op: &str,
    inputs: &[&str],
    known_shapes: &BTreeMap<String, Vec<i64>>,
    tensor_consts: &HashMap<String, TensorConst>,
    outputs: &[String],
) -> Result<Folded> {
    let lhs = &tensor_consts[inputs[0]];
    let rhs = &tensor_consts[inputs[1]];
    let any_float =
        matches!(lhs, TensorConst::Float32(_)) || matches!(rhs, TensorConst::Float32(_));
    let (la, ra) = (as_i64(lhs), as_i64(rhs));
    let len = la.len().max(ra.len());
    if la.is_empty() != ra.is_empty() {
        bail!("shape-fold {op} cannot broadcast an empty and non-empty tensor");
    }
    if la.len() != ra.len() && la.len() != 1 && ra.len() != 1 {
        bail!("shape-fold {op} requires equal lengths or a scalar operand");
    }
    let pick = |values: &[i64], index: usize| {
        if values.len() == 1 {
            values[0]
        } else {
            values[index]
        }
    };
    if op == "Div" && !any_float && (0..len).any(|i| pick(&ra, i) == 0) {
        bail!("shape-fold integer Div by zero");
    }
    let value = if any_float {
        let (lf, rf) = (as_f32(lhs), as_f32(rhs));
        let pickf = |values: &[f32], index: usize| {
            if values.len() == 1 {
                values[0]
            } else {
                values[index]
            }
        };
        let values = (0..len)
            .map(|i| {
                let (a, b) = (pickf(&lf, i), pickf(&rf, i));
                match op {
                    "Add" => a + b,
                    "Sub" => a - b,
                    "Mul" => a * b,
                    _ => a / b,
                }
            })
            .collect();
        TensorConst::Float32(values)
    } else {
        let values = (0..len)
            .map(|i| -> Result<i64> {
                let (a, b) = (pick(&la, i), pick(&ra, i));
                let value = match op {
                    "Add" => a.checked_add(b),
                    "Sub" => a.checked_sub(b),
                    "Mul" => a.checked_mul(b),
                    _ => a.checked_div(b),
                };
                value.with_context(|| format!("shape-fold integer {op} overflow"))
            })
            .collect::<Result<Vec<_>>>()?;
        TensorConst::Int64(values)
    };
    // Result shape follows the operand whose flat length matches the result.
    let shape = [inputs[0], inputs[1]]
        .iter()
        .find_map(|name| {
            known_shapes
                .get(*name)
                .filter(|shape| {
                    shape
                        .iter()
                        .try_fold(1i64, |count, dim| count.checked_mul(*dim))
                        .and_then(|count| usize::try_from(count.max(1)).ok())
                        == Some(len)
                })
                .cloned()
        })
        .unwrap_or_else(|| vec![len as i64]);
    Ok(Folded {
        name: outputs[0].clone(),
        shape,
        value,
    })
}

/// Reads an axes list from input `idx` (opset 13+) or the `axes` attribute.
fn fold_axes(
    node: &NodeProto,
    inputs: &[&str],
    tensor_consts: &HashMap<String, TensorConst>,
    idx: usize,
) -> Result<Vec<i64>> {
    if let Some(name) = inputs.get(idx) {
        Ok(as_i64(&tensor_consts[*name]))
    } else {
        attr_i64s(node, "axes").context("Unsqueeze/Squeeze axes must be present for folding")
    }
}

fn shape_of(
    name: &str,
    known_shapes: &BTreeMap<String, Vec<i64>>,
    value: &TensorConst,
) -> Vec<i64> {
    known_shapes
        .get(name)
        .cloned()
        .unwrap_or_else(|| vec![const_len(value) as i64])
}

fn as_i64(value: &TensorConst) -> Vec<i64> {
    match value {
        TensorConst::Int64(values) => values.clone(),
        TensorConst::Float32(values) => values.iter().map(|v| *v as i64).collect(),
    }
}

fn as_f32(value: &TensorConst) -> Vec<f32> {
    match value {
        TensorConst::Float32(values) => values.clone(),
        TensorConst::Int64(values) => values.iter().map(|v| *v as f32).collect(),
    }
}

fn const_len(value: &TensorConst) -> usize {
    match value {
        TensorConst::Int64(values) => values.len(),
        TensorConst::Float32(values) => values.len(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap};

    use super::*;

    #[test]
    fn malformed_foldable_nodes_never_panic() {
        let constants = HashMap::from([
            (
                "a".to_owned(),
                TensorConst::Int64(vec![i64::MIN, i64::MAX, 0, -1, 1]),
            ),
            ("b".to_owned(), TensorConst::Int64(Vec::new())),
            ("c".to_owned(), TensorConst::Int64(vec![0])),
        ]);
        let known_shapes = BTreeMap::from([
            ("a".to_owned(), vec![5]),
            ("b".to_owned(), vec![0]),
            ("c".to_owned(), vec![1]),
        ]);
        let ops = [
            "Shape",
            "Cast",
            "Slice",
            "Concat",
            "Split",
            "Gather",
            "Unsqueeze",
            "Squeeze",
            "Neg",
            "Not",
            "Equal",
            "ConstantOfShape",
            "Reshape",
            "Add",
            "Sub",
            "Mul",
            "Div",
        ];
        let names = ["a", "b", "c", ""];
        let mut state = 0x92d6_8ca2_1f04_7b31u64;

        for case in 0..2_000 {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            let op = ops[(state as usize) % ops.len()];
            let input_count = ((state >> 8) as usize) % 6;
            let output_count = ((state >> 16) as usize) % 4;
            let mut node = NodeProto {
                op_type: Some(op.to_owned()),
                ..NodeProto::new()
            };
            for index in 0..input_count {
                let choice = ((state >> (index * 7 % 48)) as usize) % names.len();
                node.input.push(names[choice].to_owned());
            }
            for index in 0..output_count {
                node.output.push(if (state >> (index + 40)) & 1 == 0 {
                    String::new()
                } else {
                    format!("out_{index}")
                });
            }

            let outcome = std::panic::catch_unwind(|| try_fold(&node, &known_shapes, &constants));
            assert!(
                outcome.is_ok(),
                "shape folding panicked for generated case {case}: op={op}, inputs={:?}, outputs={:?}",
                node.get_input(),
                node.get_output()
            );
        }
    }
}
