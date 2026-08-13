//! Generic ONNX session for prepared `scene_image` graphs.
//!
//! OCR adapters stay in [`super::api`]. This type is the narrow public
//! surface raw-autotune needs: load a prepared FLOAT NCHW model and run it
//! either on the resident wgpu executor or the deterministic CPU reference
//! executor. Adapter selection follows the shared Lege compute policy and can
//! be constrained with `WGPU_ADAPTER_NAME` when testing a particular device.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, bail};

use crate::vision::onnx::ModelReport;
use crate::vision::onnx::{PreparedGraph, load_model, load_model_from_bytes};
use crate::vision::reference::Tensor;
use crate::vision::runtime::compiled::CompiledGraph;

pub struct OnnxSession {
    graph: Arc<PreparedGraph>,
}

impl OnnxSession {
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self> {
        Self::from_path_with_input_dims(path, None)
    }

    pub fn from_path_with_input_dims(
        path: impl AsRef<Path>,
        input_dims: Option<&[i64]>,
    ) -> Result<Self> {
        let path = path.as_ref();
        let model =
            load_model(path).with_context(|| format!("failed to load {}", path.display()))?;
        Self::from_model(model, input_dims)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        Self::from_bytes_with_input_dims(bytes, None)
    }

    pub fn from_bytes_with_input_dims(bytes: &[u8], input_dims: Option<&[i64]>) -> Result<Self> {
        let model = load_model_from_bytes(bytes).context("failed to load ONNX bytes")?;
        Self::from_model(model, input_dims)
    }

    fn from_model(
        model: crate::vision::onnx_pb::ModelProto,
        input_dims: Option<&[i64]>,
    ) -> Result<Self> {
        let report = ModelReport::from_model(&model).context("failed to inspect model")?;
        if !report.rejection_reasons.is_empty() {
            bail!(
                "model is not compatible with lege-gpu: {}",
                report.rejection_reasons.join("; ")
            );
        }
        let graph = PreparedGraph::from_model_with_input_dims(&model, input_dims)
            .context("failed to prepare graph")?;
        Ok(Self {
            graph: Arc::new(graph),
        })
    }

    pub fn input_name(&self) -> &str {
        self.graph.inputs.first().map(String::as_str).unwrap_or("")
    }

    pub fn output_names(&self) -> &[String] {
        &self.graph.outputs
    }

    pub fn input_shape(&self) -> Option<&[i64]> {
        self.graph
            .known_shapes
            .get(self.input_name())
            .map(Vec::as_slice)
    }

    pub fn run_cpu(&self, input: &Tensor) -> Result<HashMap<String, Tensor>> {
        self.graph.run_cpu(input)
    }

    /// Run one inference pass through Lege's resident compiled wgpu executor.
    ///
    /// The compiled graph is deliberately scoped to this call for now: generic
    /// sessions may be used concurrently, while `CompiledGraph` is
    /// single-flight because its resident/readback buffers are reused. Callers
    /// that need repeated high-throughput inference should own a dedicated
    /// compiled session in a later API rather than hiding a mutex here.
    pub fn run_gpu(&self, input: &Tensor) -> Result<HashMap<String, Tensor>> {
        pollster::block_on(async {
            let compiled = CompiledGraph::build(&self.graph)
                .await
                .context("failed to compile prepared graph for GPU inference")?;
            compiled.run(input).await.context("GPU inference failed")
        })
    }
}
