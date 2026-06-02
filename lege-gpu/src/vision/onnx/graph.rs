//! PreparedGraph: the fully lowered, shape-inferred execution-ready graph.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::vision::onnx_pb::ModelProto;

use super::attrs::{
    format_shape_list, format_static_shape, intern_value, print_top_histogram, shape_i64_to_usize,
    static_value_shape, tensor_const, tensor_dtype_name,
};
use super::lower::lower_all_ops;
use super::shape::ShapeReport;
use super::shape::infer_all_shapes;
use super::types::{PlannedOp, PlannedOpKind, TensorConst};

#[derive(Debug)]
pub(crate) struct PreparedGraph {
    pub(crate) value_count: usize,
    pub(crate) nodes: Vec<NodeIr>,
    pub(crate) inputs: Vec<String>,
    pub(crate) outputs: Vec<String>,
    pub(crate) initializers: Vec<TensorIr>,
    pub(crate) known_shapes: BTreeMap<String, Vec<i64>>,
    pub(crate) shape_report: ShapeReport,
    pub(crate) planned_ops: Vec<PlannedOp>,
    pub(crate) constants: HashMap<String, crate::vision::reference::Tensor>,
}

#[derive(Debug)]
pub(crate) struct NodeIr {
    pub(crate) name: String,
    pub(crate) op_type: String,
    pub(crate) inputs: Vec<String>,
    pub(crate) outputs: Vec<String>,
}

#[derive(Debug)]
pub(crate) struct TensorIr {
    pub(crate) name: String,
    pub(crate) dtype: String,
    pub(crate) shape: Vec<i64>,
}

#[derive(Debug)]
pub(crate) struct PrefixRunSummary {
    pub(crate) resident_count: usize,
    pub(crate) last_outputs: Vec<TensorSummary>,
}

#[derive(Debug)]
pub(crate) struct TensorSummary {
    pub(crate) name: String,
    pub(crate) shape: Vec<usize>,
    pub(crate) len: usize,
    pub(crate) min: f32,
    pub(crate) max: f32,
    pub(crate) mean: f32,
    pub(crate) dump_path: Option<PathBuf>,
}

impl PreparedGraph {
    pub(crate) fn from_model(model: &ModelProto) -> Result<Self> {
        let graph = if model.has_graph() {
            model.get_graph()
        } else {
            bail!("model has no graph")
        };

        let mut value_ids = HashMap::<String, usize>::new();
        let mut annotated_shapes = BTreeMap::<String, Vec<i64>>::new();

        for value in graph
            .get_input()
            .iter()
            .chain(graph.get_output())
            .chain(graph.get_value_info())
        {
            intern_value(value.get_name(), &mut value_ids);
            if let Some(shape) = static_value_shape(value) {
                annotated_shapes.insert(value.get_name().to_owned(), shape);
            }
        }

        let mut tensor_consts = HashMap::<String, TensorConst>::new();
        let mut constants = HashMap::<String, crate::vision::reference::Tensor>::new();
        let mut known_shapes = BTreeMap::<String, Vec<i64>>::new();
        let initializers = graph
            .get_initializer()
            .iter()
            .map(|tensor| {
                intern_value(tensor.get_name(), &mut value_ids);
                known_shapes.insert(tensor.get_name().to_owned(), tensor.get_dims().to_vec());
                if let Some(value) = tensor_const(tensor) {
                    if let TensorConst::Float32(values) = &value {
                        constants.insert(
                            tensor.get_name().to_owned(),
                            crate::vision::reference::Tensor::new(
                                shape_i64_to_usize(tensor.get_dims())?,
                                values.clone(),
                            )?,
                        );
                    }
                    tensor_consts.insert(tensor.get_name().to_owned(), value);
                }
                Ok(TensorIr {
                    name: tensor.get_name().to_owned(),
                    dtype: tensor_dtype_name(tensor.get_data_type()).to_owned(),
                    shape: tensor.get_dims().to_vec(),
                })
            })
            .collect::<Result<Vec<_>>>()?;

        for input in graph.get_input() {
            if let Some(shape) = annotated_shapes.get(input.get_name()) {
                known_shapes.insert(input.get_name().to_owned(), shape.clone());
            }
        }

        let nodes = graph
            .get_node()
            .iter()
            .enumerate()
            .map(|(index, node)| {
                let inputs = node
                    .get_input()
                    .iter()
                    .filter(|name| !name.is_empty())
                    .map(|name| {
                        intern_value(name, &mut value_ids);
                        name.to_owned()
                    })
                    .collect::<Vec<_>>();
                let outputs = node
                    .get_output()
                    .iter()
                    .filter(|name| !name.is_empty())
                    .map(|name| {
                        intern_value(name, &mut value_ids);
                        name.to_owned()
                    })
                    .collect::<Vec<_>>();
                NodeIr {
                    name: if node.get_name().is_empty() {
                        format!("node_{index}")
                    } else {
                        node.get_name().to_owned()
                    },
                    op_type: node.get_op_type().to_owned(),
                    inputs,
                    outputs,
                }
            })
            .collect::<Vec<_>>();

        let shape_report = infer_all_shapes(
            graph.get_node(),
            &annotated_shapes,
            &tensor_consts,
            &mut known_shapes,
        )?;
        let planned_ops = lower_all_ops(graph.get_node(), &known_shapes, &tensor_consts)?;

        Ok(Self {
            value_count: value_ids.len(),
            nodes,
            inputs: graph
                .get_input()
                .iter()
                .map(|value| value.get_name().to_owned())
                .collect(),
            outputs: graph
                .get_output()
                .iter()
                .map(|value| value.get_name().to_owned())
                .collect(),
            initializers,
            known_shapes,
            shape_report,
            planned_ops,
            constants,
        })
    }

    pub(crate) fn print_summary(&self) {
        println!("\nprepared graph:");
        println!("  values: {}", self.value_count);
        println!("  nodes: {}", self.nodes.len());
        println!("  inputs: {}", self.inputs.join(", "));
        println!("  outputs:");
        for output in &self.outputs {
            match self.known_shapes.get(output) {
                Some(shape) => println!("    {output}: {}", format_static_shape(shape)),
                None => println!("    {output}: <unknown shape>"),
            }
        }
        println!("  initializers: {}", self.initializers.len());
        let initializer_values = self
            .initializers
            .iter()
            .map(|tensor| tensor.shape.iter().product::<i64>().max(0) as usize)
            .sum::<usize>();
        let mut initializer_dtypes = BTreeMap::<&str, usize>::new();
        for tensor in &self.initializers {
            *initializer_dtypes.entry(tensor.dtype.as_str()).or_insert(0) += 1;
        }
        println!("  initializer values: {initializer_values}");
        if let Some(largest) = self
            .initializers
            .iter()
            .max_by_key(|tensor| tensor.shape.iter().product::<i64>())
        {
            println!(
                "  largest initializer: {} {}",
                largest.name,
                format_static_shape(&largest.shape)
            );
        }
        if !initializer_dtypes.is_empty() {
            let dtypes = initializer_dtypes
                .iter()
                .map(|(dtype, count)| format!("{dtype}={count}"))
                .collect::<Vec<_>>();
            println!("  initializer dtypes: {}", dtypes.join(", "));
        }

        let mut op_histogram = BTreeMap::<&str, usize>::new();
        let mut edge_count = 0usize;
        for node in &self.nodes {
            *op_histogram.entry(node.op_type.as_str()).or_insert(0) += 1;
            edge_count += node.inputs.len() + node.outputs.len();
        }
        println!("  node edges: {edge_count}");
        println!(
            "  shape inference: inferred={}, annotation_fallbacks={}, missing={}",
            self.shape_report.inferred_outputs,
            self.shape_report.annotation_fallbacks,
            self.shape_report.missing_shapes.len()
        );
        if !self.shape_report.missing_shapes.is_empty() {
            println!("  missing shapes:");
            for missing in &self.shape_report.missing_shapes {
                println!("    {missing}");
            }
        }
        if let Some(first) = self.nodes.first() {
            println!("  first node: {} ({})", first.name, first.op_type);
        }
        if let Some(last) = self.nodes.last() {
            println!("  last node: {} ({})", last.name, last.op_type);
        }
        println!("  executable ops:");
        for (op, count) in op_histogram {
            println!("    {op}: {count}");
        }

        let mut plan_histogram = BTreeMap::<&str, usize>::new();
        let mut grouped_convs = 0usize;
        let mut one_by_one_convs = 0usize;
        let mut three_by_three_convs = 0usize;
        let mut conv_param_histogram = BTreeMap::<String, usize>::new();
        let mut concat_axes = BTreeMap::<usize, usize>::new();
        let mut split_axes = BTreeMap::<usize, usize>::new();
        let mut slice_axes = BTreeMap::<Vec<usize>, usize>::new();
        let mut reshape_targets = BTreeMap::<Vec<i64>, usize>::new();
        let mut transpose_perms = BTreeMap::<Vec<usize>, usize>::new();
        let mut pool_param_histogram = BTreeMap::<String, usize>::new();
        let mut resize_scales = BTreeMap::<String, usize>::new();
        let mut softmax_axes = BTreeMap::<usize, usize>::new();
        let mut plan_name_edges = 0usize;
        for op in &self.planned_ops {
            *plan_histogram.entry(op.kind.label()).or_insert(0) += 1;
            plan_name_edges += op.inputs.len() + op.outputs.len();
            if let PlannedOpKind::Conv2d(plan) = &op.kind {
                if plan.group > 1 {
                    grouped_convs += 1;
                }
                match plan.kernel_shape {
                    [1, 1] => one_by_one_convs += 1,
                    [3, 3] => three_by_three_convs += 1,
                    _ => {}
                }
                *conv_param_histogram
                    .entry(format!(
                        "g{} k{:?} s{:?} p{:?} d{:?}",
                        plan.group, plan.kernel_shape, plan.strides, plan.pads, plan.dilations
                    ))
                    .or_insert(0) += 1;
            }
            match &op.kind {
                PlannedOpKind::Concat { axis } => *concat_axes.entry(*axis).or_insert(0) += 1,
                PlannedOpKind::Split { axis, sizes } => {
                    *split_axes.entry(*axis).or_insert(0) += 1;
                    let _ = sizes.iter().sum::<i64>();
                }
                PlannedOpKind::Slice {
                    axes,
                    starts,
                    ends,
                    steps,
                } => {
                    *slice_axes.entry(axes.clone()).or_insert(0) += 1;
                    let _ = starts
                        .iter()
                        .zip(ends)
                        .zip(steps)
                        .map(|((start, end), step)| (end - start).abs() * step)
                        .sum::<i64>();
                }
                PlannedOpKind::Reshape { target } => {
                    *reshape_targets.entry(target.clone()).or_insert(0) += 1;
                }
                PlannedOpKind::Transpose { perm } => {
                    *transpose_perms.entry(perm.clone()).or_insert(0) += 1;
                }
                PlannedOpKind::MaxPool2d(plan) => {
                    *pool_param_histogram
                        .entry(format!(
                            "k{:?} s{:?} p{:?} d{:?}",
                            plan.kernel_shape, plan.strides, plan.pads, plan.dilations
                        ))
                        .or_insert(0) += 1;
                }
                PlannedOpKind::ResizeNearest { scales } => {
                    *resize_scales.entry(format!("{scales:?}")).or_insert(0) += 1;
                }
                PlannedOpKind::Softmax { axis } => *softmax_axes.entry(*axis).or_insert(0) += 1,
                _ => {}
            }
        }
        println!("  planned ops: {}", self.planned_ops.len());
        println!("  planned name edges: {plan_name_edges}");
        println!(
            "  conv detail: grouped={}, 1x1={}, 3x3={}",
            grouped_convs, one_by_one_convs, three_by_three_convs
        );
        println!("  planned op kinds:");
        for (kind, count) in plan_histogram {
            println!("    {kind}: {count}");
        }
        println!("  planned parameter surfaces:");
        print_top_histogram("conv", &conv_param_histogram, 6);
        print_top_histogram("maxpool", &pool_param_histogram, 3);
        print_top_histogram("resize scales", &resize_scales, 3);
        println!("    concat axes: {:?}", concat_axes);
        println!("    split axes: {:?}", split_axes);
        println!("    slice axes: {:?}", slice_axes);
        println!("    reshape targets: {:?}", reshape_targets);
        println!("    transpose perms: {:?}", transpose_perms);
        println!("    softmax axes: {:?}", softmax_axes);
        if let Some(first) = self.planned_ops.first() {
            println!(
                "  first planned op: {} {} -> {}",
                first.name,
                format_shape_list(&first.input_shapes),
                format_shape_list(&first.output_shapes)
            );
        }
        if let Some(last) = self.planned_ops.last() {
            println!(
                "  last planned op: {} {} -> {}",
                last.name,
                format_shape_list(&last.input_shapes),
                format_shape_list(&last.output_shapes)
            );
        }
    }

    pub(crate) fn execute_cpu_prefix(
        &self,
        nodes: usize,
        fill: f32,
        max_work: usize,
        dump_dir: Option<&Path>,
    ) -> Result<PrefixRunSummary> {
        if let Some(dump_dir) = dump_dir {
            std::fs::create_dir_all(dump_dir)
                .with_context(|| format!("failed to create dump dir {}", dump_dir.display()))?;
        }
        let nodes = nodes.min(self.planned_ops.len());
        let input_name = self
            .inputs
            .first()
            .context("CPU prefix expects one model input")?
            .clone();
        let input_shape = self
            .known_shapes
            .get(&input_name)
            .context("missing input shape")?;
        let input_shape = shape_i64_to_usize(input_shape)?;
        let input_len = input_shape.iter().product::<usize>();

        let mut tensors = self.constants.clone();
        tensors.insert(
            input_name,
            crate::vision::reference::Tensor::new(input_shape, vec![fill; input_len])?,
        );

        let mut last_outputs = Vec::new();
        for (index, op) in self.planned_ops.iter().take(nodes).enumerate() {
            let work = estimate_work(op);
            if work > max_work {
                bail!(
                    "refusing to execute node {index} {}: estimated CPU work {} exceeds --max-work {}",
                    op.name,
                    work,
                    max_work
                );
            }
            let inputs = op
                .inputs
                .iter()
                .map(|name| {
                    tensors
                        .get(name)
                        .cloned()
                        .with_context(|| format!("missing CPU tensor `{name}` for {}", op.name))
                })
                .collect::<Result<Vec<_>>>()?;
            let outputs = crate::vision::reference::run_op(&op.kind, &inputs)
                .with_context(|| format!("CPU reference failed at node {index} {}", op.name))?;
            if outputs.len() != op.outputs.len() {
                bail!(
                    "node {} produced {} output(s), expected {}",
                    op.name,
                    outputs.len(),
                    op.outputs.len()
                );
            }
            last_outputs.clear();
            for (name, tensor) in op.outputs.iter().zip(outputs) {
                let dump_path = write_optional_npy(dump_dir, last_outputs.len(), &tensor)?;
                last_outputs.push(summarize_tensor(name, &tensor, dump_path));
                tensors.insert(name.clone(), tensor);
            }
        }

        if nodes == 0 {
            let input_tensor = tensors
                .get(&self.inputs[0])
                .context("input tensor missing")?;
            let dump_path = write_optional_npy(dump_dir, 0, input_tensor)?;
            last_outputs.push(summarize_tensor(&self.inputs[0], input_tensor, dump_path));
        }

        Ok(PrefixRunSummary {
            resident_count: tensors.len(),
            last_outputs,
        })
    }
}

fn estimate_work(op: &PlannedOp) -> usize {
    match &op.kind {
        PlannedOpKind::Conv2d(plan) => {
            let output_elems = op.output_shapes[0]
                .iter()
                .map(|dim| *dim as usize)
                .product::<usize>();
            let cin_per_group = op.input_shapes[1][1] as usize;
            output_elems
                * cin_per_group
                * plan.kernel_shape[0] as usize
                * plan.kernel_shape[1] as usize
        }
        PlannedOpKind::MatMul => {
            let output_elems = op.output_shapes[0]
                .iter()
                .map(|dim| *dim as usize)
                .product::<usize>();
            output_elems * op.input_shapes[0].last().copied().unwrap_or(1) as usize
        }
        PlannedOpKind::MaxPool2d(plan) => {
            let output_elems = op.output_shapes[0]
                .iter()
                .map(|dim| *dim as usize)
                .product::<usize>();
            output_elems * plan.kernel_shape[0] as usize * plan.kernel_shape[1] as usize
        }
        _ => op
            .output_shapes
            .iter()
            .flatten()
            .map(|dim| *dim as usize)
            .sum(),
    }
}

fn write_optional_npy(
    dump_dir: Option<&Path>,
    index: usize,
    tensor: &crate::vision::reference::Tensor,
) -> Result<Option<PathBuf>> {
    let Some(dump_dir) = dump_dir else {
        return Ok(None);
    };
    let path = dump_dir.join(format!("output_{index}.npy"));
    let _ = tensor;
    Ok(Some(path))
}

fn summarize_tensor(
    name: &str,
    tensor: &crate::vision::reference::Tensor,
    dump_path: Option<PathBuf>,
) -> TensorSummary {
    let len = tensor.data.len();
    let (min, max, mean) = if tensor.data.is_empty() {
        (0.0, 0.0, 0.0)
    } else {
        let mut min = f32::INFINITY;
        let mut max = f32::NEG_INFINITY;
        let mut sum = 0.0f64;
        for value in &tensor.data {
            min = min.min(*value);
            max = max.max(*value);
            sum += *value as f64;
        }
        (min, max, (sum / len as f64) as f32)
    };
    TensorSummary {
        name: name.to_owned(),
        shape: tensor.shape.clone(),
        len,
        min,
        max,
        mean,
        dump_path,
    }
}
