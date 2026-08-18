//! Stdio JSONL persistent agent service.

use std::borrow::Cow;
use std::io::{BufRead, Write};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::Value;

use crate::bounds::Bounds;
use crate::cache::SnapshotCache;
use crate::commands::{content, images, inspect, render, search, text};
use crate::pages::parse_bbox;

#[derive(Debug, Deserialize)]
struct Request {
    id: Value,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Debug, serde::Serialize)]
struct Response {
    id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<ErrorBody>,
}

#[derive(Debug, serde::Serialize)]
struct ErrorBody {
    code: i32,
    message: String,
}

#[derive(Debug)]
pub struct ServeArgs {
    pub max_open: usize,
    pub idle_timeout: u64,
    pub bounds: Bounds,
}

pub fn run(args: ServeArgs) -> Result<i32> {
    let mut cache = SnapshotCache::new(args.max_open, args.idle_timeout);
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let mut lines = stdin.lock().lines();

    while let Some(line) = lines.next() {
        let line = line.context("reading stdin")?;
        if line.trim().is_empty() {
            continue;
        }
        let req: Request = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(err) => {
                write_response(
                    &mut stdout,
                    Response {
                        id: Value::Null,
                        result: None,
                        error: Some(ErrorBody {
                            code: -32700,
                            message: format!("parse error: {err}"),
                        }),
                    },
                )?;
                continue;
            }
        };

        if req.method == "close" || req.method == "shutdown" {
            write_response(
                &mut stdout,
                Response {
                    id: req.id,
                    result: Some(serde_json::json!({"ok": true})),
                    error: None,
                },
            )?;
            break;
        }

        let response = match call_method(&mut cache, &req.method, &req.params, args.bounds) {
            Ok(result) => Response {
                id: req.id,
                result: Some(result),
                error: None,
            },
            Err(err) => Response {
                id: req.id,
                result: None,
                error: Some(ErrorBody {
                    code: -32000,
                    message: err.to_string(),
                }),
            },
        };
        write_response(&mut stdout, response)?;
    }
    Ok(0)
}

fn write_response(out: &mut impl Write, response: Response) -> Result<()> {
    let mut line = serde_json::to_string(&response)?;
    line.push('\n');
    out.write_all(line.as_bytes())?;
    out.flush()?;
    Ok(())
}

pub(crate) fn call_method(
    cache: &mut SnapshotCache,
    method: &str,
    params: &Value,
    bounds: Bounds,
) -> Result<Value> {
    match method {
        "ping" => Ok(serde_json::json!({
            "pong": true,
            "cache_open": cache.len(),
            "schema": crate::schema::SCHEMA_ID,
        })),
        "pdf_inspect" => {
            let path = param_path(params)?;
            let password = param_str(params, "password");
            let pages = param_page_selection(params, "pages")?;
            let (identity, snapshot) = cache.get_or_open(&path, password)?;
            inspect::inspect_value(
                &identity,
                snapshot.as_ref(),
                password,
                pages.as_deref(),
                bounds,
            )
        }
        "pdf_text" => {
            let path = param_path(params)?;
            let password = param_str(params, "password");
            let pages = param_page_selection(params, "pages")?;
            let pages = pages.as_deref().unwrap_or("1");
            let layout = match param_str(params, "layout").unwrap_or("plain") {
                "plain" => text::TextLayout::Plain,
                "blocks" => text::TextLayout::Blocks,
                "words" => text::TextLayout::Words,
                "chars" => text::TextLayout::Chars,
                other => bail!("unknown layout {other:?}"),
            };
            let bbox = param_str(params, "bbox").map(parse_bbox).transpose()?;
            let ocr = match param_str(params, "ocr").unwrap_or("never") {
                "never" => text::OcrMode::Never,
                "auto" => text::OcrMode::Auto,
                "always" => text::OcrMode::Always,
                other => bail!("unknown OCR mode {other:?}"),
            };
            let ocr_language = param_str(params, "ocr_language").unwrap_or("eng");
            let (identity, snapshot) = cache.get_or_open(&path, password)?;
            let _ = identity;
            // For multi-page, return array of per-page results.
            let page_count = snapshot.page_count();
            let (indices, _) =
                crate::pages::parse_page_range(Some(pages), page_count, bounds.max_pages)?;
            let mut out = Vec::new();
            for pz in indices {
                let v = text::text_value(
                    snapshot.as_ref(),
                    pz.one_based(),
                    layout,
                    bbox,
                    pdf_text::TextPageOptions::default(),
                    false,
                    bounds,
                    ocr,
                    ocr_language,
                )?;
                out.push(serde_json::json!({
                    "page": pz.one_based(),
                    "page_index": pz.0,
                    "data": v,
                }));
            }
            Ok(serde_json::json!({ "pages": out }))
        }
        "pdf_images" => {
            let path = param_path(params)?;
            let password = param_str(params, "password");
            let page = param_u32(params, "page").unwrap_or(1);
            let mode = match param_str(params, "mode").unwrap_or("inventory") {
                "inventory" => images::ImageMode::Inventory,
                "source" => images::ImageMode::Source,
                "decoded" => images::ImageMode::Decoded,
                "rendered" => images::ImageMode::Rendered,
                other => bail!("unknown image mode {other:?}"),
            };
            let extract = param_str(params, "extract").map(PathBuf::from);
            let (_identity, snapshot) = cache.get_or_open(&path, password)?;
            images::images_value(
                snapshot.as_ref(),
                page,
                mode,
                extract.as_deref(),
                false,
                bounds,
            )
        }
        "pdf_content" => {
            let path = param_path(params)?;
            let password = param_str(params, "password");
            let page = param_u32(params, "page").unwrap_or(1);
            let ops = param_bool(params, "ops").unwrap_or(true);
            let resources = param_bool(params, "resources").unwrap_or(false);
            let objects = param_bool(params, "objects").unwrap_or(false);
            let (_identity, snapshot) = cache.get_or_open(&path, password)?;
            content::content_value(
                snapshot.as_ref(),
                page,
                ops,
                resources,
                objects,
                false,
                bounds,
            )
        }
        "pdf_render" => {
            let path = param_path(params)?;
            let password = param_str(params, "password");
            let page = param_u32(params, "page").unwrap_or(1);
            let output = param_str(params, "output")
                .map(|s| s.to_owned())
                .unwrap_or_else(|| "page-{page}.png".into());
            let dpi = param_f64(params, "dpi");
            let scale = param_f64(params, "scale");
            let format = match param_str(params, "format").unwrap_or("png") {
                "png" => render::ImageFormat::Png,
                "ppm" => render::ImageFormat::Ppm,
                other => bail!("unknown format {other:?}"),
            };
            let crop = param_str(params, "crop").map(parse_bbox).transpose()?;
            let thumbnail = param_bool(params, "thumbnail").unwrap_or(false);
            let (identity, snapshot) = cache.get_or_open(&path, password)?;
            let stem = identity
                .path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("page");
            render::render_value(
                snapshot.as_ref(),
                page,
                &output,
                stem,
                dpi,
                scale,
                format,
                crop,
                thumbnail,
                false,
            )
        }
        "pdf_search" => {
            let path = param_path(params)?;
            let password = param_str(params, "password");
            let query = param_str(params, "query").context("params.query required")?;
            let pages = param_page_selection(params, "pages")?;
            let context = param_u32(params, "context").unwrap_or(32) as usize;
            let case_insensitive = param_bool(params, "case_insensitive").unwrap_or(true);
            let ocr = match param_str(params, "ocr").unwrap_or("never") {
                "never" => text::OcrMode::Never,
                "auto" => text::OcrMode::Auto,
                "always" => text::OcrMode::Always,
                other => bail!("unknown OCR mode {other:?}"),
            };
            let ocr_language = param_str(params, "ocr_language").unwrap_or("eng");
            let (_identity, snapshot) = cache.get_or_open(&path, password)?;
            let page_count = snapshot.page_count();
            let (indices, _) =
                crate::pages::parse_page_range(pages.as_deref(), page_count, bounds.max_pages)?;
            let mut out = Vec::new();
            for pz in indices {
                let v = search::search_value(
                    snapshot.as_ref(),
                    pz.one_based(),
                    query,
                    context,
                    case_insensitive,
                    false,
                    bounds,
                    ocr,
                    ocr_language,
                )?;
                out.push(serde_json::json!({
                    "page": pz.one_based(),
                    "page_index": pz.0,
                    "data": v,
                }));
            }
            Ok(serde_json::json!({ "pages": out }))
        }
        other => bail!("unknown method {other:?}"),
    }
}

fn param_path(params: &Value) -> Result<PathBuf> {
    let path = params
        .get("path")
        .and_then(|v| v.as_str())
        .context("params.path required")?;
    Ok(PathBuf::from(path))
}

fn param_str<'a>(params: &'a Value, key: &str) -> Option<&'a str> {
    params.get(key).and_then(|v| v.as_str())
}

fn param_page_selection<'a>(params: &'a Value, key: &str) -> Result<Option<Cow<'a, str>>> {
    let Some(value) = params.get(key) else {
        return Ok(None);
    };
    if let Some(selection) = value.as_str() {
        return Ok(Some(Cow::Borrowed(selection)));
    }
    if let Some(page) = value.as_u64() {
        if page == 0 || page > u64::from(u32::MAX) {
            bail!(
                "params.{key} page number must be between 1 and {}",
                u32::MAX
            );
        }
        return Ok(Some(Cow::Owned(page.to_string())));
    }
    bail!("params.{key} must be a page-range string or one-based page number")
}

fn param_u32(params: &Value, key: &str) -> Option<u32> {
    params
        .get(key)
        .and_then(|v| v.as_u64())
        .and_then(|n| u32::try_from(n).ok())
}

fn param_f64(params: &Value, key: &str) -> Option<f64> {
    params.get(key).and_then(|v| v.as_f64())
}

fn param_bool(params: &Value, key: &str) -> Option<bool> {
    params.get(key).and_then(|v| v.as_bool())
}
