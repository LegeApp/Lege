//! ONNX model loading and compatibility reporting.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, bail};

use crate::vision::onnx_pb::{ModelProto, type_proto::Value as TypeValue};

use super::attrs::{DimReport, dim_report, format_shape, producer, tensor_dtype_name};
use super::graph::PreparedGraph;

/// Tensor layout of a model's image input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Layout {
    Nchw,
    Nhwc,
}

/// A supported model input contract. `dims` holds the expected shape in the
/// declared `layout`; `None` entries accept any value (dynamic dims). The batch
/// dim is `None` for models that declare it symbolically.
pub(crate) struct ModelTarget {
    pub(crate) input_name: &'static str,
    pub(crate) dtype: &'static str,
    // Consumed once NHWC kernels land (Phase 3 / sauvola).
    #[allow(dead_code)]
    pub(crate) layout: Layout,
    pub(crate) dims: [Option<i64>; 4],
}

/// Registry of model inputs the bridge accepts: the PP-DocLayout layout model
/// plus the paddle/sauvola models.
pub(crate) const TARGETS: &[ModelTarget] = &[
    // PP-DocLayout-M (PicoDet/GFL), static 640×640; raw boxes+scores heads
    // (NMS stripped, Conv+BN fused). See doclayout-m/onnx-work/provenance.json.
    ModelTarget {
        input_name: "pp_image",
        dtype: "FLOAT",
        layout: Layout::Nchw,
        dims: [Some(1), Some(3), Some(640), Some(640)],
    },
    // paddle-rotate (MobileNetV3 classifier); batch fixed to 1 in prep.
    ModelTarget {
        input_name: "x",
        dtype: "FLOAT",
        layout: Layout::Nchw,
        dims: [None, Some(3), Some(224), Some(224)],
    },
    // paddle-deskew (native-resolution warp); H/W dynamic.
    ModelTarget {
        input_name: "image",
        dtype: "FLOAT",
        layout: Layout::Nchw,
        dims: [None, Some(3), None, None],
    },
    // sauvola binarization; NHWC grayscale, batch + H/W dynamic (stamped at prep).
    ModelTarget {
        input_name: "img01_inp",
        dtype: "FLOAT",
        layout: Layout::Nhwc,
        dims: [None, None, None, Some(1)],
    },
    // PP-OCRv5 mobile text detection (DBNet); ConvTranspose rewritten to
    // Conv1x1+DepthToSpace + BN folded in prep. Batch + H/W stamped at prep time.
    ModelTarget {
        input_name: "pp_det_image",
        dtype: "FLOAT",
        layout: Layout::Nchw,
        dims: [None, Some(3), None, None],
    },
    // PP-OCRv5 mobile text recognition (SVTR); fixed height 48, width stamped
    // per bucket at prep time; batch stamped at prep time.
    ModelTarget {
        input_name: "pp_rec_image",
        dtype: "FLOAT",
        layout: Layout::Nchw,
        dims: [None, Some(3), Some(48), None],
    },
];

/// Finds the registered target matching a model input by name.
pub(crate) fn match_target(input_name: &str) -> Option<&'static ModelTarget> {
    TARGETS.iter().find(|t| t.input_name == input_name)
}

#[derive(Debug)]
pub(crate) struct ValueReport {
    pub(crate) name: String,
    pub(crate) dtype: String,
    pub(crate) shape: Vec<DimReport>,
}

#[derive(Debug)]
pub(crate) struct ModelReport {
    pub(crate) ir_version: i64,
    pub(crate) producer: String,
    pub(crate) opsets: Vec<String>,
    pub(crate) inputs: Vec<ValueReport>,
    pub(crate) outputs: Vec<ValueReport>,
    pub(crate) node_count: usize,
    pub(crate) initializer_count: usize,
    pub(crate) initializer_dtypes: BTreeMap<String, usize>,
    pub(crate) op_histogram: BTreeMap<String, usize>,
    pub(crate) rejection_reasons: Vec<String>,
}

impl ModelReport {
    pub(crate) fn from_model(model: &ModelProto) -> Result<Self> {
        let graph = if model.has_graph() {
            model.get_graph()
        } else {
            bail!("model has no graph")
        };

        let mut op_histogram = BTreeMap::new();
        for node in graph.get_node() {
            *op_histogram
                .entry(node.get_op_type().to_owned())
                .or_insert(0) += 1;
        }

        let mut initializer_dtypes = BTreeMap::new();
        for tensor in graph.get_initializer() {
            *initializer_dtypes
                .entry(tensor_dtype_name(tensor.get_data_type()).to_owned())
                .or_insert(0) += 1;
        }

        let inputs = graph
            .get_input()
            .iter()
            .map(|value| {
                let tensor = match &value.get_field_type().value {
                    Some(TypeValue::TensorType(tensor)) => tensor,
                    _ => bail!("input {} is not a tensor", value.get_name()),
                };
                Ok(ValueReport {
                    name: value.get_name().to_owned(),
                    dtype: tensor_dtype_name(tensor.get_elem_type()).to_owned(),
                    shape: tensor
                        .get_shape()
                        .get_dim()
                        .iter()
                        .map(dim_report)
                        .collect(),
                })
            })
            .collect::<Result<Vec<_>>>()?;

        let outputs = graph
            .get_output()
            .iter()
            .map(|value| {
                let tensor = match &value.get_field_type().value {
                    Some(TypeValue::TensorType(tensor)) => tensor,
                    _ => bail!("output {} is not a tensor", value.get_name()),
                };
                Ok(ValueReport {
                    name: value.get_name().to_owned(),
                    dtype: tensor_dtype_name(tensor.get_elem_type()).to_owned(),
                    shape: tensor
                        .get_shape()
                        .get_dim()
                        .iter()
                        .map(dim_report)
                        .collect(),
                })
            })
            .collect::<Result<Vec<_>>>()?;

        let mut rejection_reasons = Vec::new();
        let matched_target = validate_target_input(&inputs, &mut rejection_reasons);
        validate_op_set(matched_target, &op_histogram, &mut rejection_reasons);

        Ok(Self {
            ir_version: model.get_ir_version(),
            producer: producer(model),
            opsets: model
                .get_opset_import()
                .iter()
                .map(|opset| {
                    let domain = if opset.get_domain().is_empty() {
                        "ai.onnx"
                    } else {
                        opset.get_domain()
                    };
                    format!("{domain}:{}", opset.get_version())
                })
                .collect(),
            inputs,
            outputs,
            node_count: graph.get_node().len(),
            initializer_count: graph.get_initializer().len(),
            initializer_dtypes,
            op_histogram,
            rejection_reasons,
        })
    }

    pub(crate) fn print(&self) {
        println!("ir_version: {}", self.ir_version);
        println!("producer: {}", self.producer);
        println!("opsets: {}", self.opsets.join(", "));
        println!("nodes: {}", self.node_count);
        println!("initializers: {}", self.initializer_count);

        println!("\ninputs:");
        for input in &self.inputs {
            println!(
                "  {}: {} {}",
                input.name,
                input.dtype,
                format_shape(&input.shape)
            );
        }

        println!("\noutputs:");
        for output in &self.outputs {
            println!(
                "  {}: {} {}",
                output.name,
                output.dtype,
                format_shape(&output.shape)
            );
        }

        println!("\ninitializer dtypes:");
        for (dtype, count) in &self.initializer_dtypes {
            println!("  {dtype}: {count}");
        }

        println!("\nop histogram:");
        let mut ops = self.op_histogram.iter().collect::<Vec<_>>();
        ops.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
        for (op, count) in ops {
            println!("  {op}: {count}");
        }

        if self.rejection_reasons.is_empty() {
            println!("\ntarget: compatible with supported prepared-model checks");
        } else {
            println!("\ntarget: rejected");
            for reason in &self.rejection_reasons {
                println!("  - {reason}");
            }
        }
    }

    pub(crate) fn print_prepare_result(&self, model: &ModelProto) -> Result<()> {
        self.print();
        if self.rejection_reasons.is_empty() {
            let graph = PreparedGraph::from_model(model)?;
            graph.print_summary();
            println!("\nprepare: accepted");
            Ok(())
        } else {
            println!("\nprepare: rejected before graph lowering");
            bail!("model is not compatible with a supported prepared-model target")
        }
    }
}

fn validate_target_input(
    inputs: &[ValueReport],
    rejection_reasons: &mut Vec<String>,
) -> Option<&'static ModelTarget> {
    // Find the model's primary input by matching any registered target name.
    let matched = inputs
        .iter()
        .find_map(|input| match_target(&input.name).map(|target| (input, target)));
    let Some((input, target)) = matched else {
        let names = TARGETS
            .iter()
            .map(|t| format!("`{}`", t.input_name))
            .collect::<Vec<_>>()
            .join(", ");
        rejection_reasons.push(format!(
            "no input matches a supported model target (expected one of {names})"
        ));
        return None;
    };

    if input.dtype != target.dtype {
        rejection_reasons.push(format!(
            "`{}` must be {}, found {}",
            target.input_name, target.dtype, input.dtype
        ));
    }

    if input.shape.len() != target.dims.len() {
        rejection_reasons.push(format!(
            "`{}` must be rank {}, found {}",
            target.input_name,
            target.dims.len(),
            input.shape.len()
        ));
        return Some(target);
    }

    // Each fixed (Some) dim must match exactly; None accepts static or dynamic.
    for (axis, expected) in target.dims.iter().enumerate() {
        if let Some(want) = expected {
            if input.shape[axis] != DimReport::Static(*want) {
                rejection_reasons.push(format!(
                    "`{}` axis {axis} must be {want}, found {}",
                    target.input_name,
                    format_shape(&input.shape)
                ));
            }
        }
    }
    Some(target)
}

fn validate_op_set(
    _target: Option<&'static ModelTarget>,
    op_histogram: &BTreeMap<String, usize>,
    rejection_reasons: &mut Vec<String>,
) {
    let hard_reject = [
        "QuantizeLinear",
        "DequantizeLinear",
        "TopK",
        "GatherElements",
        "Range",
        "Expand",
        "Tile",
    ];
    for op in hard_reject {
        if let Some(count) = op_histogram.get(op) {
            rejection_reasons.push(format!(
                "`{op}` appears {count} time(s); unsupported in prepared bridge artifacts"
            ));
        }
    }

    let supported = BTreeSet::from([
        "AveragePool",
        "Add",
        "Concat",
        "Constant",
        "Conv",
        "CumSum",
        "DepthToSpace",
        "Div",
        "Flatten",
        "Gemm",
        "GridSample",
        "GlobalAveragePool",
        "HardSigmoid",
        "HardSwish",
        "Identity",
        "MatMul",
        "Max",
        "MaxPool",
        "Mul",
        "PRelu",
        "Pad",
        "Pow",
        "ReduceMean",
        "ReduceSum",
        "Relu",
        "Resize",
        "Reshape",
        "Sigmoid",
        "Slice",
        "Softmax",
        "SpaceToDepth",
        "Split",
        "Sqrt",
        "Sub",
        "Squeeze",
        "Transpose",
        "Unsqueeze",
    ]);
    for op in op_histogram.keys() {
        let op = op.as_str();
        if !supported.contains(op)
            && ![
                // Shape-plumbing ops the bridge folds to constants at prep time.
                "Cast",
                "ConstantOfShape",
                "Equal",
                "Gather",
                "Neg",
                "Not",
                "Shape",
                "Squeeze",
                "Unsqueeze",
                // Hard-rejected above with a clearer message; listed so they
                // don't also trip the generic "unsupported op" path.
                "Expand",
                "GatherElements",
                "Range",
                "Tile",
                "TopK",
            ]
            .contains(&op)
        {
            rejection_reasons.push(format!("unsupported op `{op}` for v1"));
        }
    }
}
