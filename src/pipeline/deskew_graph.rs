// deskew_graph.rs - Deskew model path utilities
use anyhow::{Result, anyhow};
use std::sync::Arc;

use crate::info_log;
use crate::pipeline::config::{PipelineConfig, runtime_asset_path_if_exists};

pub(crate) fn locate_deskew_model_path(
    hint: Option<&std::path::PathBuf>,
    fallback_filename: &str,
) -> Option<std::path::PathBuf> {
    if let Some(path) = hint {
        if path.exists() {
            return Some(path.clone());
        }
    }

    if let Ok(exe_path) = std::env::current_exe() {
        if let Some(parent) = exe_path.parent() {
            let candidate = parent.join(fallback_filename);
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }

    if let Ok(cwd) = std::env::current_dir() {
        let candidate = cwd.join(fallback_filename);
        if candidate.exists() {
            return Some(candidate);
        }
    }

    runtime_asset_path_if_exists(fallback_filename)
}

pub fn prepare_shared_deskew_engine(
    config: &PipelineConfig,
) -> Result<Option<Arc<crate::deskew::DeskewEngine>>> {
    if !config.enable_deskew() {
        return Ok(None);
    }

    // Updated default model filenames (previously doc-orientation.onnx / uvdoc-unwarp.onnx)
    // Align with new runtime distribution naming: paddle-rotate.onnx (rotation cls) and paddle-deskew.onnx (unwarp/correction)
    let rotation_path =
        locate_deskew_model_path(config.deskew_rotation_model(), "paddle-rotate.onnx");
    let unwarp_path = locate_deskew_model_path(config.deskew_unwarp_model(), "paddle-deskew.onnx");

    if rotation_path.is_none() && unwarp_path.is_none() {
        return Err(anyhow!(
            "Deskew enabled but no models were found. Place 'paddle-rotate.onnx' and/or 'paddle-deskew.onnx' beside the executable or specify explicit paths."
        ));
    }

    let rotation_owned = rotation_path
        .as_ref()
        .map(|p| p.to_string_lossy().to_string());
    let unwarp_owned = unwarp_path
        .as_ref()
        .map(|p| p.to_string_lossy().to_string());

    let engine = crate::deskew::DeskewEngine::new(
        rotation_owned.as_deref(),
        unwarp_owned.as_deref(),
        config.deskew_config().clone(),
    )?;

    info_log!(
        "Deskew enabled (rotation: {}, unwarp: {})",
        rotation_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "not provided".to_string()),
        unwarp_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "not provided".to_string())
    );

    Ok(Some(Arc::new(engine)))
}
