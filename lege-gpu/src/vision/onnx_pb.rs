//! The ONNX model subset Lege's runtime reads, decoded from the protobuf wire
//! format by hand.
//!
//! This replaces generated `rust-protobuf` bindings. `proto/onnx.proto` stays
//! in the tree as the reference document for the field numbers below; the
//! wire-format mechanics live in [`wire`].
//!
//! # Why hand-written
//!
//! The generated bindings covered all 22 messages of the schema. The graph
//! loader reads eleven of them and about thirty-five fields — the set spelled
//! out in this file — and never serialises anything. The generated stack cost a
//! `protobuf-codegen` / `protobuf-parse` build step on every clean build, and
//! `Message::parse_from_bytes` copied every `raw_data` blob out of the input,
//! so loading `yolo-layout.onnx` (37.8 MB) held roughly twice the model in
//! memory at peak.
//!
//! # Ownership, and why there is no lifetime parameter
//!
//! A borrowed `ModelProto<'a>` would be the obvious way to get zero-copy
//! `raw_data`, but [`ModelProto`] is *stored* — `api.rs` keeps one in four
//! different structs and passes it by value — so a lifetime would make those
//! self-referential, which cannot be written without `unsafe`.
//!
//! Instead the model file is read once into a single `Arc<[u8]>` and every
//! `raw_data` is a [`Blob`]: that `Arc` plus a range. Cloning is a refcount
//! bump, the weights are never copied, and the parsed model is a plain owned
//! value with no lifetime and no `unsafe` anywhere. Small fields (names, dims,
//! attribute scalars) are copied out, which is a few kilobytes across a whole
//! model.

use std::ops::Range;
use std::sync::{Arc, LazyLock};

pub mod wire;

use wire::{
    Reader, WIRE_LEN, WIRE_VARINT, WireError, read_repeated_f32, read_repeated_i32,
    read_repeated_i64,
};

/// A slice of the model file, held without copying it.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Blob {
    buffer: Option<Arc<[u8]>>,
    range: Range<usize>,
}

impl Blob {
    fn new(buffer: &Arc<[u8]>, range: Range<usize>) -> Self {
        Self {
            buffer: Some(Arc::clone(buffer)),
            range,
        }
    }

    /// The bytes, borrowed from the shared model buffer.
    pub fn as_slice(&self) -> &[u8] {
        match &self.buffer {
            // The range came from the reader that produced it, so it is always
            // within bounds; `get` keeps a corrupted one from panicking.
            Some(buffer) => buffer.get(self.range.clone()).unwrap_or(&[]),
            None => &[],
        }
    }
}

impl std::fmt::Debug for Blob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print megabytes of weights.
        f.debug_struct("Blob")
            .field("len", &self.range.len())
            .finish()
    }
}

// ── Messages ─────────────────────────────────────────────────────────────────
//
// Field numbers are from `proto/onnx.proto`. Only the fields the runtime reads
// are decoded; everything else is skipped, so a newer export still loads.

/// `onnx.ModelProto`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ModelProto {
    pub ir_version: i64,
    pub producer_name: String,
    pub producer_version: String,
    pub domain: String,
    pub model_version: i64,
    pub opset_import: Vec<OperatorSetIdProto>,
    pub graph: Option<Box<GraphProto>>,
}

/// `onnx.GraphProto`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GraphProto {
    pub node: Vec<NodeProto>,
    pub name: String,
    pub initializer: Vec<TensorProto>,
    pub input: Vec<ValueInfoProto>,
    pub output: Vec<ValueInfoProto>,
    pub value_info: Vec<ValueInfoProto>,
}

/// `onnx.NodeProto`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NodeProto {
    pub input: Vec<String>,
    pub output: Vec<String>,
    pub name: Option<String>,
    pub op_type: Option<String>,
    pub domain: Option<String>,
    pub attribute: Vec<AttributeProto>,
}

/// `onnx.AttributeProto`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AttributeProto {
    pub name: Option<String>,
    pub f: Option<f32>,
    pub i: Option<i64>,
    pub s: Option<Vec<u8>>,
    pub t: Option<Box<TensorProto>>,
    pub floats: Vec<f32>,
    pub ints: Vec<i64>,
    pub type_: i32,
}

/// `onnx.TensorProto`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TensorProto {
    pub dims: Vec<i64>,
    pub data_type: i32,
    pub float_data: Vec<f32>,
    pub int32_data: Vec<i32>,
    pub int64_data: Vec<i64>,
    pub name: Option<String>,
    /// The bulk payload, borrowed from the model buffer rather than copied.
    pub raw_data: Blob,
}

/// `onnx.ValueInfoProto`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ValueInfoProto {
    pub name: Option<String>,
    pub type_: Option<Box<TypeProto>>,
}

/// `onnx.TypeProto`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TypeProto {
    pub value: Option<type_proto::Value>,
    pub denotation: Option<String>,
}

/// `onnx.TypeProto`'s nested types.
pub mod type_proto {
    use super::TensorShapeProto;

    /// The `TypeProto` oneof. Only the tensor arm is decoded; the sequence,
    /// map, optional and sparse arms are skipped, which reads as "no type" —
    /// exactly how the loader already treated them.
    #[derive(Clone, Debug, PartialEq)]
    pub enum Value {
        TensorType(Tensor),
    }

    /// `onnx.TypeProto.Tensor`.
    #[derive(Clone, Debug, Default, PartialEq)]
    pub struct Tensor {
        pub elem_type: i32,
        pub shape: Option<Box<TensorShapeProto>>,
    }
}

/// `onnx.TensorShapeProto`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TensorShapeProto {
    pub dim: Vec<tensor_shape_proto::Dimension>,
}

/// `onnx.TensorShapeProto`'s nested types.
pub mod tensor_shape_proto {
    /// `onnx.TensorShapeProto.Dimension`.
    #[derive(Clone, Debug, Default, PartialEq)]
    pub struct Dimension {
        pub value: Option<dimension::Value>,
        pub denotation: Option<String>,
    }

    /// `Dimension`'s oneof.
    pub mod dimension {
        /// A dimension is either a literal extent or a symbolic name.
        #[derive(Clone, Debug, PartialEq)]
        pub enum Value {
            DimValue(i64),
            DimParam(String),
        }
    }
}

/// `onnx.OperatorSetIdProto`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct OperatorSetIdProto {
    pub domain: String,
    pub version: i64,
}

pub use tensor_shape_proto::Dimension as TensorShapeProto_Dimension;

// ── Parsing ──────────────────────────────────────────────────────────────────

impl ModelProto {
    /// Decode a model from a buffer already shared as an `Arc`.
    ///
    /// The `Arc` is what every `raw_data` [`Blob`] holds onto, so the caller's
    /// buffer must be the one the model was read from.
    pub fn parse_from_arc(buffer: &Arc<[u8]>) -> Result<Self, WireError> {
        let mut reader = Reader::new(buffer);
        Self::parse(&mut reader, buffer)
    }

    fn parse(reader: &mut Reader<'_>, buffer: &Arc<[u8]>) -> Result<Self, WireError> {
        let mut out = Self::new();
        while let Some((field, wire)) = reader.tag()? {
            match (field, wire) {
                (1, WIRE_VARINT) => out.ir_version = reader.varint()? as i64,
                (2, WIRE_LEN) => out.producer_name = reader.string()?,
                (3, WIRE_LEN) => out.producer_version = reader.string()?,
                (4, WIRE_LEN) => out.domain = reader.string()?,
                (5, WIRE_VARINT) => out.model_version = reader.varint()? as i64,
                (7, WIRE_LEN) => {
                    let mut nested = reader.nested()?;
                    out.graph = Some(Box::new(GraphProto::parse(&mut nested, buffer)?));
                }
                (8, WIRE_LEN) => {
                    let mut nested = reader.nested()?;
                    out.opset_import
                        .push(OperatorSetIdProto::parse(&mut nested)?);
                }
                _ => reader.skip(field, wire)?,
            }
        }
        Ok(out)
    }
}

impl GraphProto {
    fn parse(reader: &mut Reader<'_>, buffer: &Arc<[u8]>) -> Result<Self, WireError> {
        let mut out = Self::new();
        while let Some((field, wire)) = reader.tag()? {
            match (field, wire) {
                (1, WIRE_LEN) => {
                    let mut nested = reader.nested()?;
                    out.node.push(NodeProto::parse(&mut nested, buffer)?);
                }
                (2, WIRE_LEN) => out.name = reader.string()?,
                (5, WIRE_LEN) => {
                    let mut nested = reader.nested()?;
                    out.initializer
                        .push(TensorProto::parse(&mut nested, buffer)?);
                }
                (11, WIRE_LEN) => {
                    let mut nested = reader.nested()?;
                    out.input.push(ValueInfoProto::parse(&mut nested)?);
                }
                (12, WIRE_LEN) => {
                    let mut nested = reader.nested()?;
                    out.output.push(ValueInfoProto::parse(&mut nested)?);
                }
                (13, WIRE_LEN) => {
                    let mut nested = reader.nested()?;
                    out.value_info.push(ValueInfoProto::parse(&mut nested)?);
                }
                _ => reader.skip(field, wire)?,
            }
        }
        Ok(out)
    }
}

impl NodeProto {
    fn parse(reader: &mut Reader<'_>, buffer: &Arc<[u8]>) -> Result<Self, WireError> {
        let mut out = Self::new();
        while let Some((field, wire)) = reader.tag()? {
            match (field, wire) {
                (1, WIRE_LEN) => out.input.push(reader.string()?),
                (2, WIRE_LEN) => out.output.push(reader.string()?),
                (3, WIRE_LEN) => out.name = Some(reader.string()?),
                (4, WIRE_LEN) => out.op_type = Some(reader.string()?),
                (5, WIRE_LEN) => {
                    let mut nested = reader.nested()?;
                    out.attribute
                        .push(AttributeProto::parse(&mut nested, buffer)?);
                }
                (7, WIRE_LEN) => out.domain = Some(reader.string()?),
                _ => reader.skip(field, wire)?,
            }
        }
        Ok(out)
    }
}

impl AttributeProto {
    fn parse(reader: &mut Reader<'_>, buffer: &Arc<[u8]>) -> Result<Self, WireError> {
        let mut out = Self::new();
        while let Some((field, wire)) = reader.tag()? {
            match (field, wire) {
                (1, WIRE_LEN) => out.name = Some(reader.string()?),
                (2, wire::WIRE_FIXED32) => out.f = Some(f32::from_bits(reader.fixed32()?)),
                (3, WIRE_VARINT) => out.i = Some(reader.varint()? as i64),
                (4, WIRE_LEN) => out.s = Some(reader.bytes()?.to_vec()),
                (5, WIRE_LEN) => {
                    let mut nested = reader.nested()?;
                    out.t = Some(Box::new(TensorProto::parse(&mut nested, buffer)?));
                }
                (7, _) => read_repeated_f32(reader, field, wire, &mut out.floats)?,
                (8, _) => read_repeated_i64(reader, field, wire, &mut out.ints)?,
                (20, WIRE_VARINT) => out.type_ = reader.varint()? as i32,
                _ => reader.skip(field, wire)?,
            }
        }
        Ok(out)
    }
}

impl TensorProto {
    fn parse(reader: &mut Reader<'_>, buffer: &Arc<[u8]>) -> Result<Self, WireError> {
        let mut out = Self::new();
        while let Some((field, wire)) = reader.tag()? {
            match (field, wire) {
                (1, _) => read_repeated_i64(reader, field, wire, &mut out.dims)?,
                (2, WIRE_VARINT) => out.data_type = reader.varint()? as i32,
                (4, _) => read_repeated_f32(reader, field, wire, &mut out.float_data)?,
                (5, _) => read_repeated_i32(reader, field, wire, &mut out.int32_data)?,
                (7, _) => read_repeated_i64(reader, field, wire, &mut out.int64_data)?,
                (8, WIRE_LEN) => out.name = Some(reader.string()?),
                (9, WIRE_LEN) => {
                    // The whole point of the rewrite: record where the weights
                    // are instead of copying them out.
                    let (_, range) = reader.bytes_range()?;
                    out.raw_data = Blob::new(buffer, range);
                }
                _ => reader.skip(field, wire)?,
            }
        }
        Ok(out)
    }
}

impl ValueInfoProto {
    fn parse(reader: &mut Reader<'_>) -> Result<Self, WireError> {
        let mut out = Self::new();
        while let Some((field, wire)) = reader.tag()? {
            match (field, wire) {
                (1, WIRE_LEN) => out.name = Some(reader.string()?),
                (2, WIRE_LEN) => {
                    let mut nested = reader.nested()?;
                    out.type_ = Some(Box::new(TypeProto::parse(&mut nested)?));
                }
                _ => reader.skip(field, wire)?,
            }
        }
        Ok(out)
    }
}

impl TypeProto {
    fn parse(reader: &mut Reader<'_>) -> Result<Self, WireError> {
        let mut out = Self::new();
        while let Some((field, wire)) = reader.tag()? {
            match (field, wire) {
                (1, WIRE_LEN) => {
                    let mut nested = reader.nested()?;
                    out.value = Some(type_proto::Value::TensorType(type_proto::Tensor::parse(
                        &mut nested,
                    )?));
                }
                (6, WIRE_LEN) => out.denotation = Some(reader.string()?),
                _ => reader.skip(field, wire)?,
            }
        }
        Ok(out)
    }
}

impl type_proto::Tensor {
    fn parse(reader: &mut Reader<'_>) -> Result<Self, WireError> {
        let mut out = Self::default();
        while let Some((field, wire)) = reader.tag()? {
            match (field, wire) {
                (1, WIRE_VARINT) => out.elem_type = reader.varint()? as i32,
                (2, WIRE_LEN) => {
                    let mut nested = reader.nested()?;
                    out.shape = Some(Box::new(TensorShapeProto::parse(&mut nested)?));
                }
                _ => reader.skip(field, wire)?,
            }
        }
        Ok(out)
    }
}

impl TensorShapeProto {
    fn parse(reader: &mut Reader<'_>) -> Result<Self, WireError> {
        let mut out = Self::new();
        while let Some((field, wire)) = reader.tag()? {
            match (field, wire) {
                (1, WIRE_LEN) => {
                    let mut nested = reader.nested()?;
                    out.dim
                        .push(TensorShapeProto_Dimension::parse(&mut nested)?);
                }
                _ => reader.skip(field, wire)?,
            }
        }
        Ok(out)
    }
}

impl TensorShapeProto_Dimension {
    fn parse(reader: &mut Reader<'_>) -> Result<Self, WireError> {
        let mut out = Self::default();
        while let Some((field, wire)) = reader.tag()? {
            match (field, wire) {
                (1, WIRE_VARINT) => {
                    out.value = Some(tensor_shape_proto::dimension::Value::DimValue(
                        reader.varint()? as i64,
                    ));
                }
                (2, WIRE_LEN) => {
                    out.value = Some(tensor_shape_proto::dimension::Value::DimParam(
                        reader.string()?,
                    ));
                }
                (3, WIRE_LEN) => out.denotation = Some(reader.string()?),
                _ => reader.skip(field, wire)?,
            }
        }
        Ok(out)
    }
}

impl OperatorSetIdProto {
    fn parse(reader: &mut Reader<'_>) -> Result<Self, WireError> {
        let mut out = Self::new();
        while let Some((field, wire)) = reader.tag()? {
            match (field, wire) {
                (1, WIRE_LEN) => out.domain = reader.string()?,
                (2, WIRE_VARINT) => out.version = reader.varint()? as i64,
                _ => reader.skip(field, wire)?,
            }
        }
        Ok(out)
    }
}

// ── Accessor surface ─────────────────────────────────────────────────────────
//
// The `get_*` / `new` shape the nine consumer modules were already written
// against, preserved verbatim so the rewrite did not touch them.

/// Shared empty values, for the accessors that hand out a reference to an
/// absent submessage.
static DEFAULT_TENSOR: LazyLock<TensorProto> = LazyLock::new(TensorProto::new);
static DEFAULT_GRAPH: LazyLock<GraphProto> = LazyLock::new(GraphProto::new);
static DEFAULT_TYPE: LazyLock<TypeProto> = LazyLock::new(TypeProto::new);
static DEFAULT_SHAPE: LazyLock<TensorShapeProto> = LazyLock::new(TensorShapeProto::new);

macro_rules! impl_new {
    ($($ty:ty),+ $(,)?) => {
        $(impl $ty {
            /// An all-default message.
            pub fn new() -> Self {
                Self::default()
            }
        })+
    };
}

impl_new!(
    ModelProto,
    GraphProto,
    NodeProto,
    AttributeProto,
    TensorProto,
    ValueInfoProto,
    TypeProto,
    TensorShapeProto,
    OperatorSetIdProto,
);

impl AttributeProto {
    pub fn has_t(&self) -> bool {
        self.t.is_some()
    }
    pub fn get_name(&self) -> &str {
        self.name.as_deref().unwrap_or_default()
    }
    pub fn get_i(&self) -> i64 {
        self.i.unwrap_or_default()
    }
    pub fn get_f(&self) -> f32 {
        self.f.unwrap_or_default()
    }
    pub fn get_s(&self) -> &[u8] {
        self.s.as_deref().unwrap_or_default()
    }
    pub fn get_t(&self) -> &TensorProto {
        self.t.as_deref().unwrap_or(&DEFAULT_TENSOR)
    }
    pub fn get_ints(&self) -> &[i64] {
        &self.ints
    }
}

impl NodeProto {
    pub fn get_attribute(&self) -> &[AttributeProto] {
        &self.attribute
    }
    pub fn get_input(&self) -> &[String] {
        &self.input
    }
    pub fn get_output(&self) -> &[String] {
        &self.output
    }
    pub fn get_name(&self) -> &str {
        self.name.as_deref().unwrap_or_default()
    }
    pub fn get_op_type(&self) -> &str {
        self.op_type.as_deref().unwrap_or_default()
    }
}

impl TensorProto {
    pub fn get_data_type(&self) -> i32 {
        self.data_type
    }
    pub fn get_dims(&self) -> &[i64] {
        &self.dims
    }
    pub fn get_float_data(&self) -> &[f32] {
        &self.float_data
    }
    pub fn get_int32_data(&self) -> &[i32] {
        &self.int32_data
    }
    pub fn get_int64_data(&self) -> &[i64] {
        &self.int64_data
    }
    pub fn get_name(&self) -> &str {
        self.name.as_deref().unwrap_or_default()
    }
    pub fn get_raw_data(&self) -> &[u8] {
        self.raw_data.as_slice()
    }
}

impl ValueInfoProto {
    pub fn get_field_type(&self) -> &TypeProto {
        self.type_.as_deref().unwrap_or(&DEFAULT_TYPE)
    }
    pub fn get_name(&self) -> &str {
        self.name.as_deref().unwrap_or_default()
    }
}

impl type_proto::Tensor {
    pub fn get_elem_type(&self) -> i32 {
        self.elem_type
    }
    pub fn get_shape(&self) -> &TensorShapeProto {
        self.shape.as_deref().unwrap_or(&DEFAULT_SHAPE)
    }
}

impl TensorShapeProto {
    pub fn get_dim(&self) -> &[TensorShapeProto_Dimension] {
        &self.dim
    }
}

impl ModelProto {
    pub fn has_graph(&self) -> bool {
        self.graph.is_some()
    }
    pub fn get_graph(&self) -> &GraphProto {
        self.graph.as_deref().unwrap_or(&DEFAULT_GRAPH)
    }
    // These four are only exercised by the wire-parser regression tests below
    // (the graph loader itself only needs `has_graph`/`get_graph`), but they
    // cover real parsing risk for a hand-rolled decoder, so they stay.
    #[cfg(test)]
    pub fn get_ir_version(&self) -> i64 {
        self.ir_version
    }
    #[cfg(test)]
    pub fn get_opset_import(&self) -> &[OperatorSetIdProto] {
        &self.opset_import
    }
    #[cfg(test)]
    pub fn get_producer_name(&self) -> &str {
        &self.producer_name
    }
    #[cfg(test)]
    pub fn get_producer_version(&self) -> &str {
        &self.producer_version
    }
}

impl GraphProto {
    pub fn get_initializer(&self) -> &[TensorProto] {
        &self.initializer
    }
    pub fn get_input(&self) -> &[ValueInfoProto] {
        &self.input
    }
    pub fn get_node(&self) -> &[NodeProto] {
        &self.node
    }
    pub fn get_output(&self) -> &[ValueInfoProto] {
        &self.output
    }
    pub fn get_value_info(&self) -> &[ValueInfoProto] {
        &self.value_info
    }
}

impl OperatorSetIdProto {
    // Regression-tested at the wire level (opset version parsing); no
    // production caller reads `domain` today, so only `get_version` is kept.
    #[cfg(test)]
    pub fn get_version(&self) -> i64 {
        self.version
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encode a `(field, wire)` tag.
    fn tag(field: u32, wire: u8) -> Vec<u8> {
        let mut key = (u64::from(field) << 3) | u64::from(wire);
        let mut out = Vec::new();
        loop {
            let byte = (key & 0x7F) as u8;
            key >>= 7;
            if key == 0 {
                out.push(byte);
                break;
            }
            out.push(byte | 0x80);
        }
        out
    }

    fn varint(mut value: u64) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let byte = (value & 0x7F) as u8;
            value >>= 7;
            if value == 0 {
                out.push(byte);
                break;
            }
            out.push(byte | 0x80);
        }
        out
    }

    fn len_field(field: u32, payload: &[u8]) -> Vec<u8> {
        let mut out = tag(field, WIRE_LEN);
        out.extend(varint(payload.len() as u64));
        out.extend_from_slice(payload);
        out
    }

    fn varint_field(field: u32, value: u64) -> Vec<u8> {
        let mut out = tag(field, WIRE_VARINT);
        out.extend(varint(value));
        out
    }

    #[test]
    fn parses_a_minimal_model_end_to_end() {
        // TensorProto initializer: dims [2,3], data_type 1, raw_data 24 bytes.
        let mut tensor = Vec::new();
        tensor.extend(varint_field(1, 2)); // dims (unpacked)
        tensor.extend(varint_field(1, 3));
        tensor.extend(varint_field(2, 1)); // data_type FLOAT
        tensor.extend(len_field(8, b"W")); // name
        let weights: Vec<u8> = (0..24u8).collect();
        tensor.extend(len_field(9, &weights)); // raw_data

        // NodeProto: op_type Conv, inputs [x, W], output [y].
        let mut node = Vec::new();
        node.extend(len_field(1, b"x"));
        node.extend(len_field(1, b"W"));
        node.extend(len_field(2, b"y"));
        node.extend(len_field(3, b"conv0"));
        node.extend(len_field(4, b"Conv"));
        // attribute: name=kernel_shape, ints=[3,3] packed
        let mut attr = Vec::new();
        attr.extend(len_field(1, b"kernel_shape"));
        let mut packed = Vec::new();
        packed.extend(varint(3));
        packed.extend(varint(3));
        attr.extend(len_field(8, &packed));
        node.extend(len_field(5, &attr));

        // ValueInfoProto input: name x, tensor type FLOAT, shape [1, "N"].
        let mut dim_static = varint_field(1, 1);
        let mut dim_symbol = len_field(2, b"N");
        let mut shape = Vec::new();
        shape.extend(len_field(1, &dim_static));
        shape.extend(len_field(1, &dim_symbol));
        dim_static.clear();
        dim_symbol.clear();
        let mut tensor_type = varint_field(1, 1); // elem_type FLOAT
        tensor_type.extend(len_field(2, &shape));
        let type_proto_bytes = len_field(1, &tensor_type);
        let mut value_info = len_field(1, b"x");
        value_info.extend(len_field(2, &type_proto_bytes));

        let mut graph = Vec::new();
        graph.extend(len_field(1, &node));
        graph.extend(len_field(2, b"g"));
        graph.extend(len_field(5, &tensor));
        graph.extend(len_field(11, &value_info));

        let mut opset = varint_field(2, 17);
        opset.splice(0..0, len_field(1, b""));

        let mut model = Vec::new();
        model.extend(varint_field(1, 9)); // ir_version
        model.extend(len_field(2, b"lege-test"));
        model.extend(len_field(3, b"1.0"));
        model.extend(len_field(6, b"ignored doc_string")); // unknown to us
        model.extend(len_field(7, &graph));
        model.extend(len_field(8, &opset));

        let buffer: Arc<[u8]> = Arc::from(model.into_boxed_slice());
        let parsed = ModelProto::parse_from_arc(&buffer).expect("parse");

        assert_eq!(parsed.get_ir_version(), 9);
        assert_eq!(parsed.get_producer_name(), "lege-test");
        assert_eq!(parsed.get_producer_version(), "1.0");
        assert!(parsed.has_graph());
        assert_eq!(parsed.get_opset_import().len(), 1);
        assert_eq!(parsed.get_opset_import()[0].get_version(), 17);

        let graph = parsed.get_graph();
        assert_eq!(graph.get_node().len(), 1);
        let node = &graph.get_node()[0];
        assert_eq!(node.get_op_type(), "Conv");
        assert_eq!(node.get_name(), "conv0");
        assert_eq!(node.get_input(), &["x".to_string(), "W".to_string()]);
        assert_eq!(node.get_output(), &["y".to_string()]);
        assert_eq!(node.get_attribute().len(), 1);
        assert_eq!(node.get_attribute()[0].get_name(), "kernel_shape");
        assert_eq!(node.get_attribute()[0].get_ints(), &[3, 3]);

        let init = &graph.get_initializer()[0];
        assert_eq!(init.get_name(), "W");
        assert_eq!(init.get_dims(), &[2, 3]);
        assert_eq!(init.get_data_type(), 1);
        assert_eq!(init.get_raw_data(), &(0..24u8).collect::<Vec<_>>()[..]);

        let value = &graph.get_input()[0];
        assert_eq!(value.get_name(), "x");
        // Production reads the oneof directly; mirror that here.
        let Some(type_proto::Value::TensorType(tensor_type)) = &value.get_field_type().value else {
            panic!("input should carry a tensor type");
        };
        assert_eq!(tensor_type.get_elem_type(), 1);
        let dims = tensor_type.get_shape().get_dim();
        assert_eq!(dims.len(), 2);
        assert!(matches!(
            dims[0].value,
            Some(tensor_shape_proto::dimension::Value::DimValue(1))
        ));
        assert!(matches!(
            &dims[1].value,
            Some(tensor_shape_proto::dimension::Value::DimParam(name)) if name == "N"
        ));
    }

    #[test]
    fn raw_data_borrows_the_shared_buffer_rather_than_copying() {
        let weights: Vec<u8> = (0..64u8).collect();
        let mut tensor = len_field(8, b"W");
        tensor.extend(len_field(9, &weights));
        let graph = len_field(5, &tensor);
        let model = len_field(7, &graph);

        let buffer: Arc<[u8]> = Arc::from(model.into_boxed_slice());
        let parsed = ModelProto::parse_from_arc(&buffer).expect("parse");
        let raw = parsed.get_graph().get_initializer()[0].get_raw_data();

        assert_eq!(raw, &weights[..]);
        // The blob is a window onto `buffer`, not a copy of it.
        let base = buffer.as_ptr() as usize;
        let got = raw.as_ptr() as usize;
        assert!(
            got >= base && got + raw.len() <= base + buffer.len(),
            "raw_data must point into the shared model buffer"
        );
    }

    #[test]
    fn accessors_on_absent_submessages_yield_empty_defaults() {
        let model = ModelProto::new();
        assert!(!model.has_graph());
        assert_eq!(model.get_graph().get_node().len(), 0);
        assert_eq!(model.get_ir_version(), 0);

        let value = ValueInfoProto::new();
        assert_eq!(value.get_name(), "");
        assert!(
            value.get_field_type().value.is_none(),
            "an absent type must read as no-oneof, not panic"
        );

        let attr = AttributeProto::new();
        assert!(!attr.has_t());
        assert_eq!(attr.get_t().get_dims().len(), 0);
        assert_eq!(attr.get_i(), 0);
        assert_eq!(attr.get_s(), b"");
    }

    #[test]
    fn truncated_model_errors_instead_of_panicking() {
        let weights: Vec<u8> = (0..64u8).collect();
        let mut tensor = len_field(9, &weights);
        tensor.extend(len_field(8, b"W"));
        let graph = len_field(5, &tensor);
        let model = len_field(7, &graph);

        for cut in 1..model.len() {
            let buffer: Arc<[u8]> = Arc::from(model[..cut].to_vec().into_boxed_slice());
            // Either a clean parse of a prefix or a typed error — never a panic.
            let _ = ModelProto::parse_from_arc(&buffer);
        }
    }
}
