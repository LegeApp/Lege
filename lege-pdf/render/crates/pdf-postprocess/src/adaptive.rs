use std::sync::Mutex;
use std::time::{Duration, Instant};

use pdf_render_api::{HostPage, PostprocessCapabilities};

use crate::{
    CpuPostprocess, PostprocessBackend, PostprocessError, PostprocessGraph, PostprocessOutput,
};

#[cfg(feature = "gpu")]
use crate::gpu::{GpuExecution, WgpuPostprocess};

#[cfg(feature = "gpu")]
trait GpuExecutor: Send + Sync + std::fmt::Debug {
    fn capabilities(&self) -> PostprocessCapabilities;
    fn supports(&self, graph: &PostprocessGraph) -> bool;
    fn adapter_name(&self) -> &str;
    fn execute_measured(
        &self,
        source: &HostPage,
        graph: &PostprocessGraph,
    ) -> Result<GpuExecution, PostprocessError>;
}

#[cfg(feature = "gpu")]
impl GpuExecutor for WgpuPostprocess {
    fn capabilities(&self) -> PostprocessCapabilities {
        PostprocessBackend::capabilities(self)
    }

    fn supports(&self, graph: &PostprocessGraph) -> bool {
        PostprocessBackend::supports(self, graph)
    }

    fn adapter_name(&self) -> &str {
        &self.adapter_info().name
    }

    fn execute_measured(
        &self,
        source: &HostPage,
        graph: &PostprocessGraph,
    ) -> Result<GpuExecution, PostprocessError> {
        self.execute_measured(source, graph)
    }
}

/// Requested executor policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PostprocessPreference {
    /// Always use the normative CPU implementation.
    Cpu,
    /// Require GPU execution. Initialization or execution failure is returned
    /// to the caller; no silent fallback occurs.
    Gpu,
    /// Use a discrete/integrated GPU when available, otherwise execute the
    /// entire graph on CPU. A failed GPU graph is rerun from its original
    /// `HostPage`, never resumed midway.
    Auto,
}

impl PostprocessPreference {
    pub fn from_env() -> Result<Self, PostprocessError> {
        match std::env::var("LEGE_POSTPROCESS_BACKEND") {
            Ok(value) => match value.trim().to_ascii_lowercase().as_str() {
                "cpu" => Ok(Self::Cpu),
                "gpu" => Ok(Self::Gpu),
                "auto" => Ok(Self::Auto),
                _ => Err(PostprocessError::InvalidParams(
                    "LEGE_POSTPROCESS_BACKEND must be cpu, gpu, or auto",
                )),
            },
            // Experimental builds remain CPU-default until the documented
            // stability and performance gates are met.
            Err(std::env::VarError::NotPresent) => Ok(Self::Cpu),
            Err(std::env::VarError::NotUnicode(_)) => Err(PostprocessError::InvalidParams(
                "LEGE_POSTPROCESS_BACKEND is not valid Unicode",
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionBackend {
    Cpu,
    Gpu,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FallbackReason {
    GpuFeatureDisabled,
    GpuUnavailable(String),
    SoftwareAdapter(String),
    GraphUnsupported,
    ExecutionFailed(String),
}

#[derive(Debug, Clone)]
pub struct ExecutionStats {
    pub requested: PostprocessPreference,
    pub used: ExecutionBackend,
    pub fallback: Option<FallbackReason>,
    pub elapsed: Duration,
    pub uploaded_bytes: u64,
    pub readback_bytes: u64,
    pub adapter: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PostprocessReport {
    pub output: PostprocessOutput,
    pub stats: ExecutionStats,
}

/// Policy wrapper that owns the CPU reference executor and, when enabled, a
/// pooled GPU executor.
pub struct AdaptivePostprocess {
    preference: PostprocessPreference,
    #[cfg(feature = "gpu")]
    gpu: Option<Box<dyn GpuExecutor>>,
    initialization_fallback: Option<FallbackReason>,
    last_stats: Mutex<Option<ExecutionStats>>,
}

impl std::fmt::Debug for AdaptivePostprocess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdaptivePostprocess")
            .field("preference", &self.preference)
            .field("initialization_fallback", &self.initialization_fallback)
            .finish_non_exhaustive()
    }
}

impl Default for AdaptivePostprocess {
    fn default() -> Self {
        Self {
            preference: PostprocessPreference::Cpu,
            #[cfg(feature = "gpu")]
            gpu: None,
            initialization_fallback: None,
            last_stats: Mutex::new(None),
        }
    }
}

impl AdaptivePostprocess {
    pub fn from_env() -> Result<Self, PostprocessError> {
        Self::new(PostprocessPreference::from_env()?)
    }

    pub fn new(preference: PostprocessPreference) -> Result<Self, PostprocessError> {
        #[cfg(feature = "gpu")]
        {
            let (gpu, initialization_fallback) = match preference {
                PostprocessPreference::Cpu => (None, None),
                PostprocessPreference::Gpu => (
                    Some(Box::new(WgpuPostprocess::new()?) as Box<dyn GpuExecutor>),
                    None,
                ),
                PostprocessPreference::Auto => match WgpuPostprocess::new() {
                    Ok(gpu) if gpu.is_hardware_gpu() => {
                        (Some(Box::new(gpu) as Box<dyn GpuExecutor>), None)
                    }
                    Ok(gpu) => (
                        None,
                        Some(FallbackReason::SoftwareAdapter(
                            gpu.adapter_info().name.to_string(),
                        )),
                    ),
                    Err(error) => (
                        None,
                        Some(FallbackReason::GpuUnavailable(error.to_string())),
                    ),
                },
            };
            return Ok(Self {
                preference,
                gpu,
                initialization_fallback,
                last_stats: Mutex::new(None),
            });
        }

        #[cfg(not(feature = "gpu"))]
        {
            if preference == PostprocessPreference::Gpu {
                return Err(PostprocessError::Unsupported(
                    "GPU postprocessing requires the pdf-postprocess `gpu` feature",
                ));
            }
            Ok(Self {
                preference,
                initialization_fallback: (preference == PostprocessPreference::Auto)
                    .then_some(FallbackReason::GpuFeatureDisabled),
                last_stats: Mutex::new(None),
            })
        }
    }

    pub fn preference(&self) -> PostprocessPreference {
        self.preference
    }

    pub fn last_stats(&self) -> Option<ExecutionStats> {
        self.last_stats
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub fn execute_with_report(
        &self,
        source: &HostPage,
        graph: &PostprocessGraph,
    ) -> Result<PostprocessReport, PostprocessError> {
        let report = self.execute_inner(source, graph)?;
        *self
            .last_stats
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(report.stats.clone());
        Ok(report)
    }

    fn execute_cpu(
        &self,
        source: &HostPage,
        graph: &PostprocessGraph,
        fallback: Option<FallbackReason>,
        started: Instant,
    ) -> Result<PostprocessReport, PostprocessError> {
        let output = CpuPostprocess.execute(source, graph)?;
        Ok(PostprocessReport {
            output,
            stats: ExecutionStats {
                requested: self.preference,
                used: ExecutionBackend::Cpu,
                fallback,
                elapsed: started.elapsed(),
                uploaded_bytes: 0,
                readback_bytes: 0,
                adapter: None,
            },
        })
    }

    fn execute_inner(
        &self,
        source: &HostPage,
        graph: &PostprocessGraph,
    ) -> Result<PostprocessReport, PostprocessError> {
        let started = Instant::now();
        match self.preference {
            PostprocessPreference::Cpu => self.execute_cpu(source, graph, None, started),
            PostprocessPreference::Gpu => {
                #[cfg(feature = "gpu")]
                {
                    let gpu = self.gpu.as_ref().ok_or_else(|| {
                        PostprocessError::Failed(
                            "forced GPU executor was not initialized".to_owned(),
                        )
                    })?;
                    let execution = gpu.execute_measured(source, graph)?;
                    return Ok(PostprocessReport {
                        output: execution.output,
                        stats: ExecutionStats {
                            requested: self.preference,
                            used: ExecutionBackend::Gpu,
                            fallback: None,
                            elapsed: started.elapsed(),
                            uploaded_bytes: execution.uploaded_bytes,
                            readback_bytes: execution.readback_bytes,
                            adapter: Some(gpu.adapter_name().to_owned()),
                        },
                    });
                }
                #[cfg(not(feature = "gpu"))]
                Err(PostprocessError::Unsupported(
                    "GPU postprocessing requires the pdf-postprocess `gpu` feature",
                ))
            }
            PostprocessPreference::Auto => {
                #[cfg(feature = "gpu")]
                if let Some(gpu) = &self.gpu {
                    if !gpu.supports(graph) {
                        return self.execute_cpu(
                            source,
                            graph,
                            Some(FallbackReason::GraphUnsupported),
                            started,
                        );
                    }
                    match gpu.execute_measured(source, graph) {
                        Ok(execution) => {
                            return Ok(PostprocessReport {
                                output: execution.output,
                                stats: ExecutionStats {
                                    requested: self.preference,
                                    used: ExecutionBackend::Gpu,
                                    fallback: None,
                                    elapsed: started.elapsed(),
                                    uploaded_bytes: execution.uploaded_bytes,
                                    readback_bytes: execution.readback_bytes,
                                    adapter: Some(gpu.adapter_name().to_owned()),
                                },
                            });
                        }
                        Err(error) => {
                            return self.execute_cpu(
                                source,
                                graph,
                                Some(FallbackReason::ExecutionFailed(error.to_string())),
                                started,
                            );
                        }
                    }
                }
                self.execute_cpu(source, graph, self.initialization_fallback.clone(), started)
            }
        }
    }
}

impl PostprocessBackend for AdaptivePostprocess {
    fn capabilities(&self) -> PostprocessCapabilities {
        #[cfg(feature = "gpu")]
        if let Some(gpu) = &self.gpu {
            return gpu.capabilities();
        }
        CpuPostprocess.capabilities()
    }

    fn supports(&self, graph: &PostprocessGraph) -> bool {
        match self.preference {
            PostprocessPreference::Cpu | PostprocessPreference::Auto => {
                CpuPostprocess.supports(graph)
            }
            PostprocessPreference::Gpu => {
                #[cfg(feature = "gpu")]
                {
                    self.gpu
                        .as_ref()
                        .is_some_and(|backend| backend.supports(graph))
                }
                #[cfg(not(feature = "gpu"))]
                {
                    false
                }
            }
        }
    }

    fn execute(
        &self,
        source: &HostPage,
        graph: &PostprocessGraph,
    ) -> Result<PostprocessOutput, PostprocessError> {
        Ok(self.execute_with_report(source, graph)?.output)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    #[cfg(feature = "gpu")]
    use std::sync::Arc;

    #[cfg(feature = "gpu")]
    use pdf_render_api::OutputFormat;

    #[test]
    fn experimental_default_remains_cpu() {
        let backend = AdaptivePostprocess::default();
        assert_eq!(backend.preference(), PostprocessPreference::Cpu);
        assert_eq!(backend.capabilities(), PostprocessCapabilities::HOST_ALL);
    }

    #[cfg(feature = "gpu")]
    #[derive(Debug)]
    struct FailingGpu;

    #[cfg(feature = "gpu")]
    impl GpuExecutor for FailingGpu {
        fn capabilities(&self) -> PostprocessCapabilities {
            PostprocessCapabilities {
                operations: pdf_render_api::PostprocessOperations::all(),
                resident_execution: true,
            }
        }

        fn supports(&self, _graph: &PostprocessGraph) -> bool {
            true
        }

        fn adapter_name(&self) -> &str {
            "test GPU"
        }

        fn execute_measured(
            &self,
            _source: &HostPage,
            _graph: &PostprocessGraph,
        ) -> Result<GpuExecution, PostprocessError> {
            Err(PostprocessError::Failed("injected device loss".to_owned()))
        }
    }

    #[cfg(feature = "gpu")]
    #[test]
    fn auto_restarts_the_whole_graph_on_cpu_after_gpu_failure() {
        let backend = AdaptivePostprocess {
            preference: PostprocessPreference::Auto,
            gpu: Some(Box::new(FailingGpu)),
            initialization_fallback: None,
            last_stats: Mutex::new(None),
        };
        let source = HostPage {
            width: 3,
            height: 1,
            stride: 3,
            format: OutputFormat::Gray8,
            pixels: Arc::from([0, 127, 255]),
        };
        let graph = PostprocessGraph {
            ops: vec![crate::PostprocessOp::Dither(crate::DitherSpec::None)],
        };
        let report = backend.execute_with_report(&source, &graph).unwrap();
        let PostprocessOutput::Page(pdf_render_api::RenderedPage::Host(page)) = report.output
        else {
            panic!("CPU fallback must return a host page");
        };
        assert_eq!(&*page.pixels, &[0, 0, 255]);
        assert_eq!(report.stats.used, ExecutionBackend::Cpu);
        assert!(matches!(
            report.stats.fallback,
            Some(FallbackReason::ExecutionFailed(ref message))
                if message.contains("injected device loss")
        ));
    }

    #[cfg(not(feature = "gpu"))]
    #[test]
    fn forced_gpu_is_truthful_without_feature() {
        let error = AdaptivePostprocess::new(PostprocessPreference::Gpu)
            .expect_err("forced GPU must fail when its feature is disabled");
        assert!(matches!(error, PostprocessError::Unsupported(_)));
    }
}
