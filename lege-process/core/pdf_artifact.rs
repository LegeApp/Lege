//! Adapter: `accumulator::Page` → `lege_pdf_write::PdfPageArtifact`.
//!
//! Keeps the pipeline's page DTO unchanged while feeding the new writer. All
//! hOCR parsing and the top-left → bottom-left Y flip happen here (in Lege), so
//! the writer only emits. Shared JBIG2 globals are keyed by a content hash so
//! the writer's registry writes each blob once.

use std::sync::Arc;

use anyhow::{Result, anyhow};

use lege_pdf_write::artifact::{
    ColorModel, PdfImageElement, PdfImageResource, PdfPageArtifact, PreparedTextLayer, TextFont,
    TextRun,
};
use lege_pdf_write::font::EmbeddedFont;
use lege_pdf_write::resources::SharedResourceId;
use lege_pdf_write::types::{Affine, PdfRect};

use crate::accumulator::{ContentType, Page};
use crate::hocr;
use crate::unicode_font::UnicodeFontData;

/// A shared-resource blob the writer must register before `add_page`.
pub type SharedBlob = (SharedResourceId, Arc<[u8]>);

/// Convert a processed page into a writer artifact plus any shared globals to
/// register. `embedded_font_available` selects the OCR font resource.
pub fn page_to_artifact(
    page: &Page,
    embedded_font_available: bool,
) -> Result<(PdfPageArtifact, Vec<SharedBlob>)> {
    let mut elements = Vec::with_capacity(page.elements.len());
    let mut globals = Vec::new();

    for el in &page.elements {
        // Top-left (pixel/point) origin → PDF bottom-left origin.
        let pdf_y = (page.height - el.y - el.height) as f64;
        let transform =
            Affine::scale_translate(el.width as f64, el.height as f64, el.x as f64, pdf_y);
        let image = content_to_resource(&el.content, &mut globals)?;
        elements.push(PdfImageElement { transform, image });
    }

    let text_layer = build_text_layer(page, embedded_font_available);

    let artifact = PdfPageArtifact {
        index: page.index as u32,
        media_box: PdfRect::from_size(page.width as f64, page.height as f64),
        elements: elements.into_boxed_slice(),
        text_layer,
    };
    Ok((artifact, globals))
}

/// Build `EmbeddedFont` from Lege's glyphless font data.
pub fn embedded_font_from(u: &UnicodeFontData) -> EmbeddedFont {
    let m = &u.metrics;
    EmbeddedFont {
        data: u.data.clone(),
        post_script_name: u.post_script_name.clone(),
        ascent: m.ascent as i32,
        descent: m.descent as i32,
        cap_height: m.cap_height as i32,
        italic_angle: m.italic_angle,
        bbox: [
            m.bbox.x_min as i32,
            m.bbox.y_min as i32,
            m.bbox.x_max as i32,
            m.bbox.y_max as i32,
        ],
    }
}

fn content_to_resource(
    content: &ContentType,
    globals: &mut Vec<SharedBlob>,
) -> Result<PdfImageResource> {
    match content {
        ContentType::EncodedImage {
            data,
            pixel_width,
            pixel_height,
            format,
        } => match format.as_str() {
            "jpeg" => Ok(PdfImageResource::Jpeg {
                data: data.clone(),
                width: *pixel_width,
                height: *pixel_height,
                color: ColorModel::Rgb,
            }),
            "jpeg-gray" => Ok(PdfImageResource::Jpeg {
                data: data.clone(),
                width: *pixel_width,
                height: *pixel_height,
                color: ColorModel::Gray,
            }),
            "jp2" => Ok(PdfImageResource::Jpx {
                data: data.clone(),
                width: *pixel_width,
                height: *pixel_height,
                color: ColorModel::Rgb,
            }),
            "jp2-gray" => Ok(PdfImageResource::Jpx {
                data: data.clone(),
                width: *pixel_width,
                height: *pixel_height,
                color: ColorModel::Gray,
            }),
            "jbig2" => Ok(PdfImageResource::Jbig2 {
                data: data.clone(),
                width: *pixel_width,
                height: *pixel_height,
                globals: None,
                image_mask: false,
                image_mask_paints_one: false,
            }),
            "ccitt" | "ccitt4" => Ok(PdfImageResource::CcittGroup4 {
                data: data.clone(),
                width: *pixel_width,
                height: *pixel_height,
                black_is_one: true,
            }),
            "indexed8" => {
                if data.len() < 768 {
                    return Err(anyhow!("indexed8 data too short: need 768-byte palette"));
                }
                Ok(PdfImageResource::Indexed8 {
                    palette: Arc::from(&data[0..768]),
                    indices: Arc::from(&data[768..]),
                    width: *pixel_width,
                    height: *pixel_height,
                })
            }
            other => Err(anyhow!("unsupported image format for PDF: {other}")),
        },
        ContentType::Jbig2ImageWithGlobals {
            page_data,
            global_data,
            pixel_width,
            pixel_height,
        } => Ok(PdfImageResource::Jbig2 {
            data: page_data.clone(),
            width: *pixel_width,
            height: *pixel_height,
            globals: register_globals(global_data, globals),
            image_mask: false,
            image_mask_paints_one: false,
        }),
        ContentType::Jbig2Mask {
            page_data,
            global_data,
            pixel_width,
            pixel_height,
            paint_one,
        } => Ok(PdfImageResource::Jbig2 {
            data: page_data.clone(),
            width: *pixel_width,
            height: *pixel_height,
            globals: register_globals(global_data, globals),
            image_mask: true,
            image_mask_paints_one: *paint_one,
        }),
    }
}

/// Register a globals blob (if non-empty) keyed by a content hash, returning its
/// id. Duplicate blobs collapse to one id, so the writer emits each once.
fn register_globals(
    global_data: &Arc<[u8]>,
    out: &mut Vec<SharedBlob>,
) -> Option<SharedResourceId> {
    if global_data.is_empty() {
        return None;
    }
    let id = SharedResourceId(fnv1a_64(global_data));
    if !out.iter().any(|(existing, _)| *existing == id) {
        out.push((id, global_data.clone()));
    }
    Some(id)
}

fn build_text_layer(page: &Page, embedded_font_available: bool) -> Option<PreparedTextLayer> {
    let hocr = page.hocr_text.as_ref()?;
    if hocr.trim().is_empty() {
        return None;
    }
    let mut lines = hocr::parse_hocr(hocr).ok()?;
    hocr::dedup_adjacent_repeats(&mut lines);
    if lines.is_empty() {
        return None;
    }

    let page_h = page.height;
    let mut runs = Vec::new();
    for line in lines {
        if !line.words.is_empty() {
            let last = line.words.len().saturating_sub(1);
            for (i, word) in line.words.iter().enumerate() {
                let mut text = word.text.clone();
                if i < last {
                    text.push(' ');
                }
                runs.push(TextRun {
                    text,
                    x: word.x as f64,
                    y: (page_h - (word.y + word.height)) as f64,
                    size: word.height.max(1.0) as f64,
                });
            }
        } else if let Some(raw) = line.raw_text.as_deref() {
            if raw.is_empty() {
                continue;
            }
            runs.push(TextRun {
                text: raw.to_string(),
                x: line.x as f64,
                y: (page_h - line.baseline) as f64,
                size: line.height.max(1.0) as f64,
            });
        }
    }

    if runs.is_empty() {
        return None;
    }

    let font = if embedded_font_available {
        TextFont::Embedded
    } else {
        TextFont::HelveticaFallback
    };
    Some(PreparedTextLayer {
        runs: runs.into_boxed_slice(),
        font,
    })
}

/// 64-bit FNV-1a, used only for shared-resource identity (not security).
fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}
