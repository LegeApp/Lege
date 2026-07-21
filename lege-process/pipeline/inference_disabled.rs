use crate::engine::Detection;
use crate::pipeline::config::PipelineConfig;
use anyhow::{Result, bail};
use image::RgbImage;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};

pub struct InferenceJob {
    pub page_index: usize,
    pub image: Arc<RgbImage>,
    pub response_tx: oneshot::Sender<Result<Vec<Detection>>>,
}

pub struct InferenceJobAsync {
    pub page_index: usize,
    pub image: Arc<RgbImage>,
    pub result_tx: mpsc::Sender<(usize, Result<Vec<Detection>>)>,
}

pub struct InferenceActor;

#[derive(Clone)]
pub struct InferenceHandle;

impl InferenceHandle {
    pub fn new(_config: &PipelineConfig) -> Result<Self> {
        bail!("layout detection was not compiled into this Lege build")
    }

    pub async fn detect(
        &self,
        _page_index: usize,
        _image: Arc<RgbImage>,
    ) -> Result<Vec<Detection>> {
        bail!("layout detection was not compiled into this Lege build")
    }

    pub async fn submit(
        &self,
        _page_index: usize,
        _image: Arc<RgbImage>,
    ) -> Result<oneshot::Receiver<Result<Vec<Detection>>>> {
        bail!("layout detection was not compiled into this Lege build")
    }

    pub fn has_capacity(&self) -> bool {
        false
    }
}

pub fn is_layout_software_adapter_error(_error: &(dyn std::error::Error + 'static)) -> bool {
    false
}

pub fn is_gpu_device_error(_error: &(dyn std::error::Error + 'static)) -> bool {
    false
}
