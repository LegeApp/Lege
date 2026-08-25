//! PreparedGraph: the fully lowered, shape-inferred execution-ready graph.

use std::collections::{BTreeMap, HashMap};

use anyhow::{Context, Result, bail};

use crate::vision::onnx_pb::ModelProto;

use super::attrs::{shape_i64_to_usize, static_value_shape, tensor_const};
use super::lower::lower_all_ops;
use super::shape::infer_all_shapes;
use super::types::{PlannedOp, TensorConst};
use super::winograd_rewrite::rewrite_winograd_f23;

#[derive(Debug)]
pub(crate) struct PreparedGraph {
    pub(crate) inputs: Vec<String>,
    pub(crate) outputs: Vec<String>,
    pub(crate) known_shapes: BTreeMap<String, Vec<i64>>,
    pub(crate) planned_ops: Vec<PlannedOp>,
    pub(crate) constants: HashMap<String, crate::vision::reference::Tensor>,
}

impl PreparedGraph {
    #[cfg(feature = "layout-detection")]
    pub(crate) fn from_model(model: &ModelProto) -> Result<Self> {
        Self::from_model_with_input_dims(model, None)
    }

    /// Prepares a graph, optionally stamping the primary input's shape with
    /// concrete dims. Dynamic-resolution models (paddle-deskew, sauvola) declare
    /// symbolic H/W; injecting the page's real dims here lets the shape-plumbing
    /// subgraph fold to constants so the graph compiles natively at that size —
    /// no resize, no per-size prepared artifact.
    pub(crate) fn from_model_with_input_dims(
        model: &ModelProto,
        input_dims: Option<&[i64]>,
    ) -> Result<Self> {
        let graph = if model.has_graph() {
            model.get_graph()
        } else {
            bail!("model has no graph")
        };

        let mut annotated_shapes = BTreeMap::<String, Vec<i64>>::new();

        for value in graph
            .get_input()
            .iter()
            .chain(graph.get_output())
            .chain(graph.get_value_info())
        {
            if let Some(shape) = static_value_shape(value) {
                annotated_shapes.insert(value.get_name().to_owned(), shape);
            }
        }

        // Stamp the concrete input shape over any symbolic dims. The primary
        // input is the first graph input that is not an initializer.
        if let Some(dims) = input_dims {
            let initializer_names = graph
                .get_initializer()
                .iter()
                .map(|tensor| tensor.get_name())
                .collect::<std::collections::HashSet<_>>();
            let input_name = graph
                .get_input()
                .iter()
                .map(|value| value.get_name())
                .find(|name| !initializer_names.contains(name))
                .context("model has no non-initializer input to stamp dims onto")?
                .to_owned();
            annotated_shapes.insert(input_name, dims.to_vec());
        }

        let mut tensor_consts = HashMap::<String, TensorConst>::new();
        let mut constants = HashMap::<String, crate::vision::reference::Tensor>::new();
        let mut known_shapes = BTreeMap::<String, Vec<i64>>::new();
        for tensor in graph.get_initializer() {
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
        }

        for input in graph.get_input() {
            if let Some(shape) = annotated_shapes.get(input.get_name()) {
                known_shapes.insert(input.get_name().to_owned(), shape.clone());
            }
        }

        // Run shape inference + shape-plumbing folding first so the folded nodes
        // can be dropped from the executable graph and lowering below.
        let inference = infer_all_shapes(
            graph.get_node(),
            &annotated_shapes,
            &mut tensor_consts,
            &mut known_shapes,
        )?;
        let kept_nodes = graph
            .get_node()
            .iter()
            .enumerate()
            .filter(|(index, _)| !inference.folded.contains(index))
            .map(|(_, node)| node)
            .collect::<Vec<_>>();

        // Promote folded float constants that a kept (compute) op consumes as
        // data into GPU initializers — e.g. sauvola's ConstantOfShape ones
        // tensor feeding CumSum. Int64 folds are shape params, stripped at
        // lowering, so they need no buffer.
        let kept_inputs = kept_nodes
            .iter()
            .flat_map(|node| node.get_input().iter())
            .filter(|name| !name.is_empty())
            .map(String::as_str)
            .collect::<std::collections::HashSet<_>>();
        for name in &inference.folded_values {
            if constants.contains_key(name) || !kept_inputs.contains(name.as_str()) {
                continue;
            }
            if let Some(TensorConst::Float32(values)) = tensor_consts.get(name) {
                let shape = known_shapes
                    .get(name)
                    .with_context(|| format!("folded constant `{name}` has no shape"))?;
                constants.insert(
                    name.clone(),
                    crate::vision::reference::Tensor::new(
                        shape_i64_to_usize(shape)?,
                        values.clone(),
                    )?,
                );
            }
        }

        let planned_ops = lower_all_ops(&kept_nodes, &known_shapes, &tensor_consts)?;
        let disable_winograd = std::env::var("LEGE_DISABLE_WINOGRAD")
            .map(|value| value != "0" && !value.eq_ignore_ascii_case("false"))
            .unwrap_or(false);
        let planned_ops = if disable_winograd {
            #[cfg(feature = "debug-logging")]
            eprintln!("  winograd.rewrite: disabled by LEGE_DISABLE_WINOGRAD");
            planned_ops
        } else {
            rewrite_winograd_f23(planned_ops, &mut known_shapes, &mut constants)?
        };

        Ok(Self {
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
            known_shapes,
            planned_ops,
            constants,
        })
    }

    /// Runs the whole graph on the CPU reference executor with a real input,
    /// returning the model output tensors. Intermediates are freed at their last
    /// use to bound peak RAM. This is the production CPU path for models that run
    /// better on CPU than GPU (heavy sauvola): whole-image, no resize, no VRAM
    /// ceiling, and global statistics (instance norm) stay correct.
    pub(crate) fn run_cpu(
        &self,
        input: &crate::vision::reference::Tensor,
    ) -> Result<HashMap<String, crate::vision::reference::Tensor>> {
        use crate::vision::reference::{Tensor, run_op};

        let input_name = self.inputs.first().context("graph has no input")?.clone();

        // Last planned-op index that reads each value, so intermediates can be
        // dropped once consumed. Graph outputs live to the end.
        let mut last_use = HashMap::<&str, usize>::new();
        for (index, op) in self.planned_ops.iter().enumerate() {
            for name in &op.inputs {
                last_use.insert(name.as_str(), index);
            }
        }

        // Computed values only; constants are borrowed from `self.constants`
        // at lookup so the (potentially large) weight set is never cloned.
        let mut computed = HashMap::<String, Tensor>::new();
        computed.insert(input_name, input.clone());

        for (index, op) in self.planned_ops.iter().enumerate() {
            // Borrow inputs by reference (no clone) and drop the borrow before
            // mutating the tensor map.
            let outputs = {
                let inputs = op
                    .inputs
                    .iter()
                    .map(|name| {
                        computed
                            .get(name)
                            .or_else(|| self.constants.get(name))
                            .with_context(|| format!("missing CPU tensor `{name}` for {}", op.name))
                    })
                    .collect::<Result<Vec<&Tensor>>>()?;
                run_op(&op.kind, &inputs)
                    .with_context(|| format!("CPU reference failed at {}", op.name))?
            };
            for (name, tensor) in op.outputs.iter().zip(outputs) {
                computed.insert(name.clone(), tensor);
            }
            // Free computed inputs whose last use was this op and that are not
            // graph outputs.
            for name in &op.inputs {
                if last_use.get(name.as_str()) == Some(&index)
                    && !self.outputs.iter().any(|o| o == name)
                {
                    computed.remove(name);
                }
            }
        }

        Ok(self
            .outputs
            .iter()
            .filter_map(|name| computed.remove(name).map(|t| (name.clone(), t)))
            .collect())
    }
}
