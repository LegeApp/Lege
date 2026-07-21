#![allow(
    clippy::all,
    dead_code,
    missing_docs,
    non_camel_case_types,
    non_snake_case,
    non_upper_case_globals,
    trivial_casts,
    unknown_lints,
    unused_attributes,
    unused_mut,
    unused_results
)]

//! Rust bindings generated from the vendored ONNX protobuf schema, with the
//! small compatibility surface used by Lege's graph loader.

include!(concat!(env!("OUT_DIR"), "/onnx/onnx.rs"));

pub use tensor_shape_proto::Dimension as TensorShapeProto_Dimension;

impl AttributeProto {
    pub fn has_t(&self) -> bool {
        self.t.is_some()
    }
    pub fn get_name(&self) -> &str {
        self.name()
    }
    pub fn get_i(&self) -> i64 {
        self.i()
    }
    pub fn get_f(&self) -> f32 {
        self.f()
    }
    pub fn get_s(&self) -> &[u8] {
        self.s()
    }
    pub fn get_t(&self) -> &TensorProto {
        self.t.get_or_default()
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
        self.name()
    }
    pub fn get_op_type(&self) -> &str {
        self.op_type()
    }
}

impl TensorProto {
    pub fn get_data_type(&self) -> i32 {
        self.data_type() as i32
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
        self.name()
    }
    pub fn get_raw_data(&self) -> &[u8] {
        self.raw_data()
    }
}

impl ValueInfoProto {
    pub fn get_field_type(&self) -> &TypeProto {
        self.type_.get_or_default()
    }
    pub fn get_name(&self) -> &str {
        self.name()
    }
}

impl TypeProto {
    pub fn get_tensor_type(&self) -> &type_proto::Tensor {
        self.tensor_type()
    }
}

impl type_proto::Tensor {
    pub fn get_elem_type(&self) -> i32 {
        self.elem_type() as i32
    }
    pub fn get_shape(&self) -> &TensorShapeProto {
        self.shape.get_or_default()
    }
}

impl TensorShapeProto {
    pub fn get_dim(&self) -> &[tensor_shape_proto::Dimension] {
        &self.dim
    }
}

impl ModelProto {
    pub fn has_graph(&self) -> bool {
        self.graph.is_some()
    }
    pub fn get_graph(&self) -> &GraphProto {
        self.graph.get_or_default()
    }
    pub fn get_ir_version(&self) -> i64 {
        self.ir_version()
    }
    pub fn get_opset_import(&self) -> &[OperatorSetIdProto] {
        &self.opset_import
    }
    pub fn get_producer_name(&self) -> &str {
        self.producer_name()
    }
    pub fn get_producer_version(&self) -> &str {
        self.producer_version()
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
    pub fn get_domain(&self) -> &str {
        self.domain()
    }
    pub fn get_version(&self) -> i64 {
        self.version()
    }
}
