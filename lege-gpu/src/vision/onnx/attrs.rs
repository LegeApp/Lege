//! ONNX proto helper functions and formatting utilities.
//! Pure functions over proto types — no graph state.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::vision::onnx_pb::{
    AttributeProto, ModelProto, NodeProto, TensorProto, TensorShapeProto_Dimension,
    TensorShapeProto_Dimension_oneof_value, TypeProto_oneof_value, ValueInfoProto,
};

use super::types::TensorConst;

// ── ONNX attribute readers ────────────────────────────────────────────────────

pub(crate) fn attr_i64(node: &NodeProto, name: &str) -> Option<i64> {
    node.get_attribute()
        .iter()
        .find(|attr| attr.get_name() == name)
        .map(AttributeProto::get_i)
}

pub(crate) fn attr_i64s(node: &NodeProto, name: &str) -> Option<Vec<i64>> {
    node.get_attribute()
        .iter()
        .find(|attr| attr.get_name() == name)
        .map(|attr| attr.get_ints().to_vec())
}

pub(crate) fn attr_f32(node: &NodeProto, name: &str) -> Option<f32> {
    node.get_attribute()
        .iter()
        .find(|attr| attr.get_name() == name)
        .map(AttributeProto::get_f)
}

pub(crate) fn attr_tensor<'a>(node: &'a NodeProto, name: &str) -> Option<&'a TensorProto> {
    node.get_attribute()
        .iter()
        .find(|attr| attr.get_name() == name && attr.has_t())
        .map(AttributeProto::get_t)
}

pub(crate) fn attr_string(node: &NodeProto, name: &str) -> Option<String> {
    node.get_attribute()
        .iter()
        .find(|attr| attr.get_name() == name)
        .and_then(|attr| String::from_utf8(attr.get_s().to_vec()).ok())
}

pub(crate) fn attr_i64_array<const N: usize>(
    node: &NodeProto,
    name: &str,
    default: [i64; N],
) -> Result<[i64; N]> {
    let Some(values) = attr_i64s(node, name) else {
        return Ok(default);
    };
    values.try_into().map_err(|values: Vec<i64>| {
        anyhow::anyhow!(
            "attribute `{name}` expected {N} values, found {}",
            values.len()
        )
    })
}

// ── Constant tensor accessors ─────────────────────────────────────────────────

pub(crate) fn const_i64<'a>(
    tensor_consts: &'a HashMap<String, TensorConst>,
    name: &str,
) -> Option<&'a [i64]> {
    match tensor_consts.get(name) {
        Some(TensorConst::Int64(values)) => Some(values),
        _ => None,
    }
}

pub(crate) fn const_f32<'a>(
    tensor_consts: &'a HashMap<String, TensorConst>,
    name: &str,
) -> Option<&'a [f32]> {
    match tensor_consts.get(name) {
        Some(TensorConst::Float32(values)) => Some(values),
        _ => None,
    }
}

// ── Proto tensor helpers ──────────────────────────────────────────────────────

pub(crate) fn tensor_const(tensor: &TensorProto) -> Option<TensorConst> {
    match tensor.get_data_type() {
        1 => Some(TensorConst::Float32(tensor_f32(tensor))),
        6 => Some(TensorConst::Int64(tensor_i32(tensor))),
        7 => Some(TensorConst::Int64(tensor_i64(tensor))),
        // FLOAT16: up-convert to f32 on load so the rest of the bridge (and all
        // wgpu kernels) keep operating in f32. This is the loader half of the
        // "fp16 weights on disk, f32 compute" path — it halves the on-disk model
        // size with no kernel changes. The separate, larger runtime-fp16-compute
        // effort is documented in docs/fp16-speed-route.md.
        10 => Some(TensorConst::Float32(tensor_f16_as_f32(tensor))),
        _ => None,
    }
}

/// Decode an IEEE-754 half-precision (binary16) value into f32.
fn f16_bits_to_f32(bits: u16) -> f32 {
    let sign = (bits >> 15) & 0x1;
    let exp = (bits >> 10) & 0x1f;
    let mant = bits & 0x3ff;
    let sign_f = if sign == 1 { -1.0_f32 } else { 1.0_f32 };
    match exp {
        // Subnormal / zero: value = sign * 2^-14 * (mant / 1024).
        0 => sign_f * 2.0_f32.powi(-14) * (mant as f32 / 1024.0),
        // Inf / NaN.
        0x1f => {
            if mant == 0 {
                sign_f * f32::INFINITY
            } else {
                f32::NAN
            }
        }
        // Normal: value = sign * 2^(exp-15) * (1 + mant/1024).
        _ => sign_f * 2.0_f32.powi(exp as i32 - 15) * (1.0 + mant as f32 / 1024.0),
    }
}

/// Loads a FLOAT16 tensor, widening to f32. ONNX stores fp16 either packed in
/// `int32_data` (one half per int32, low 16 bits) or as little-endian `raw_data`.
pub(crate) fn tensor_f16_as_f32(tensor: &TensorProto) -> Vec<f32> {
    if !tensor.get_int32_data().is_empty() {
        return tensor
            .get_int32_data()
            .iter()
            .map(|&v| f16_bits_to_f32(v as u16))
            .collect();
    }
    tensor
        .get_raw_data()
        .chunks_exact(2)
        .map(|bytes| {
            f16_bits_to_f32(u16::from_le_bytes(
                bytes.try_into().expect("chunks_exact yields 2 bytes"),
            ))
        })
        .collect()
}

/// Loads an INT32 tensor, widening to i64. ONNX stores int32 in `int32_data`
/// or little-endian `raw_data`; we treat both as shape/axis constants.
pub(crate) fn tensor_i32(tensor: &TensorProto) -> Vec<i64> {
    if !tensor.get_int32_data().is_empty() {
        return tensor.get_int32_data().iter().map(|&v| v as i64).collect();
    }
    tensor
        .get_raw_data()
        .chunks_exact(4)
        .map(|bytes| {
            i32::from_le_bytes(bytes.try_into().expect("chunks_exact yields 4 bytes")) as i64
        })
        .collect()
}

pub(crate) fn tensor_i64(tensor: &TensorProto) -> Vec<i64> {
    if !tensor.get_int64_data().is_empty() {
        return tensor.get_int64_data().to_vec();
    }
    tensor
        .get_raw_data()
        .chunks_exact(8)
        .map(|bytes| i64::from_le_bytes(bytes.try_into().expect("chunks_exact yields 8 bytes")))
        .collect()
}

pub(crate) fn tensor_f32(tensor: &TensorProto) -> Vec<f32> {
    if !tensor.get_float_data().is_empty() {
        return tensor.get_float_data().to_vec();
    }
    tensor
        .get_raw_data()
        .chunks_exact(4)
        .map(|bytes| f32::from_le_bytes(bytes.try_into().expect("chunks_exact yields 4 bytes")))
        .collect()
}

pub(crate) fn tensor_dtype_name(dtype: i32) -> &'static str {
    match dtype {
        1 => "FLOAT",
        2 => "UINT8",
        3 => "INT8",
        4 => "UINT16",
        5 => "INT16",
        6 => "INT32",
        7 => "INT64",
        9 => "BOOL",
        10 => "FLOAT16",
        11 => "DOUBLE",
        12 => "UINT32",
        13 => "UINT64",
        16 => "BFLOAT16",
        _ => "UNKNOWN",
    }
}

// ── Shape/axis normalization ──────────────────────────────────────────────────

pub(crate) fn normalize_axis(axis: i64, rank: usize) -> Result<usize> {
    let rank = rank as i64;
    let axis = if axis < 0 { axis + rank } else { axis };
    if axis < 0 || axis >= rank {
        bail!("axis {axis} is out of range for rank {rank}");
    }
    Ok(axis as usize)
}

pub(crate) fn normalize_index(index: i64, dim: i64) -> i64 {
    if index < 0 {
        (index + dim).clamp(0, dim)
    } else {
        index.clamp(0, dim)
    }
}

// ── Proto value helpers ───────────────────────────────────────────────────────

pub(crate) fn static_value_shape(value: &ValueInfoProto) -> Option<Vec<i64>> {
    let tensor = match &value.get_field_type().value {
        Some(TypeProto_oneof_value::tensor_type(tensor)) => tensor,
        _ => return None,
    };
    let mut shape = Vec::new();
    for dim in tensor.get_shape().get_dim() {
        match dim_report(dim) {
            DimReport::Static(value) => shape.push(value),
            DimReport::Symbol(_) | DimReport::Unknown => return None,
        }
    }
    Some(shape)
}

pub(crate) fn producer(model: &ModelProto) -> String {
    match (model.get_producer_name(), model.get_producer_version()) {
        ("", "") => "<unknown>".to_owned(),
        (name, "") => name.to_owned(),
        ("", version) => version.to_owned(),
        (name, version) => format!("{name} {version}"),
    }
}

pub(crate) fn dim_report(dim: &TensorShapeProto_Dimension) -> DimReport {
    match &dim.value {
        Some(TensorShapeProto_Dimension_oneof_value::dim_value(value)) => DimReport::Static(*value),
        Some(TensorShapeProto_Dimension_oneof_value::dim_param(value)) => {
            DimReport::Symbol(value.clone())
        }
        None => DimReport::Unknown,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DimReport {
    Static(i64),
    Symbol(String),
    Unknown,
}

// ── Graph helpers ─────────────────────────────────────────────────────────────

pub(crate) fn node_context(node: &NodeProto) -> String {
    if node.get_name().is_empty() {
        format!("<unnamed {}>", node.get_op_type())
    } else {
        format!("{} ({})", node.get_name(), node.get_op_type())
    }
}

pub(crate) fn intern_value(name: &str, value_ids: &mut HashMap<String, usize>) -> usize {
    if let Some(id) = value_ids.get(name) {
        *id
    } else {
        let id = value_ids.len();
        value_ids.insert(name.to_owned(), id);
        id
    }
}

// ── Format utilities ──────────────────────────────────────────────────────────

pub(crate) fn format_shape(shape: &[DimReport]) -> String {
    let dims = shape
        .iter()
        .map(|dim| match dim {
            DimReport::Static(value) => value.to_string(),
            DimReport::Symbol(value) => value.clone(),
            DimReport::Unknown => "?".to_owned(),
        })
        .collect::<Vec<_>>();
    format!("[{}]", dims.join(","))
}

pub(crate) fn format_static_shape(shape: &[i64]) -> String {
    format!(
        "[{}]",
        shape
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(",")
    )
}

pub(crate) fn format_shape_list(shapes: &[Vec<i64>]) -> String {
    let shapes = shapes
        .iter()
        .map(|s| format_static_shape(s))
        .collect::<Vec<_>>();
    format!("[{}]", shapes.join(", "))
}

pub(crate) fn shape_i64_to_usize(shape: &[i64]) -> Result<Vec<usize>> {
    shape
        .iter()
        .map(|dim| usize::try_from(*dim).context("shape dimensions must be non-negative"))
        .collect()
}

pub(crate) fn print_top_histogram(label: &str, histogram: &BTreeMap<String, usize>, limit: usize) {
    if histogram.is_empty() {
        return;
    }
    println!("    {label}:");
    let mut entries = histogram.iter().collect::<Vec<_>>();
    entries.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
    for (key, count) in entries.into_iter().take(limit) {
        println!("      {key}: {count}");
    }
}

pub(crate) fn input_shape(
    node: &NodeProto,
    known_shapes: &BTreeMap<String, Vec<i64>>,
    index: usize,
) -> Result<Vec<i64>> {
    let name = node
        .get_input()
        .get(index)
        .with_context(|| format!("missing input {index}"))?;
    known_shapes
        .get(name)
        .cloned()
        .with_context(|| format!("missing shape for input `{name}`"))
}

// ── Model loading ─────────────────────────────────────────────────────────────

pub(crate) fn load_model(path: &Path) -> Result<ModelProto> {
    use protobuf::Message;
    let bytes = std::fs::read(path)
        .with_context(|| format!("failed to read ONNX model {}", path.display()))?;
    ModelProto::parse_from_bytes(&bytes)
        .with_context(|| format!("failed to parse ONNX protobuf {}", path.display()))
}
