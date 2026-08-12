use pdf_document::{PageIndex, ParseContext};
use pdf_page_ir::Rect;
use pdf_text::{TextPage, TextPageOptions};
use std::sync::Arc;

use crate::{ReadError, RenderSession};

/// A word and its exact bounding box in source-render pixel space.
#[derive(Debug, Clone, PartialEq)]
pub struct NativeTextWord {
    pub text: String,
    /// `[left, top, right, bottom]` in a top-left-origin pixel space.
    pub bbox: [f32; 4],
}

/// OCR-relevant page classification derived from one semantic compilation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageContentKind {
    NativeText,
    ScannedImage,
    Hybrid,
    RenderRequired,
}

/// Native text and image evidence used to decide whether OCR is necessary.
#[derive(Debug, Clone, PartialEq)]
pub struct PageTextEvidence {
    pub kind: PageContentKind,
    pub text: String,
    pub words: Vec<NativeTextWord>,
    pub character_count: usize,
    pub replacement_character_count: usize,
    pub image_count: usize,
    pub largest_image_pixels: u64,
    pub trustworthy: bool,
    /// Original DCT payload for a simple one-image scan page. Callers can
    /// decode it directly and avoid a full-page render.
    pub direct_scan: Option<DirectScanImage>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectScanImage {
    pub jpeg: Arc<[u8]>,
    pub width: u32,
    pub height: u32,
}

/// Compile a page once and return its OCR routing evidence.
pub fn page_text_evidence(
    session: &RenderSession,
    page: u32,
    source_width: u32,
    source_height: u32,
) -> Result<PageTextEvidence, ReadError> {
    let geometry = session.page_geometry(page)?;
    let semantic = build_semantic_page(session, page)?;
    let text_page = TextPage::build(&semantic, &TextPageOptions::default());
    let text = text_page.all_text();
    let words = text_page
        .words()
        .into_iter()
        .filter(|word| !word.text.trim().is_empty())
        .map(|word| NativeTextWord {
            text: word.text,
            bbox: rect_to_pixels(
                word.bbox,
                geometry.crop_box,
                geometry.rotate,
                source_width,
                source_height,
            ),
        })
        .collect::<Vec<_>>();
    let character_count = text
        .chars()
        .filter(|character| !character.is_whitespace())
        .count();
    let replacement_character_count = text
        .chars()
        .filter(|character| *character == '\u{fffd}')
        .count();
    let valid_word_count = words
        .iter()
        .filter(|word| {
            word.bbox[2] > word.bbox[0]
                && word.bbox[3] > word.bbox[1]
                && word.bbox[0] >= 0.0
                && word.bbox[1] >= 0.0
                && word.bbox[2] <= source_width as f32 + 1.0
                && word.bbox[3] <= source_height as f32 + 1.0
        })
        .count();
    let trustworthy = character_count >= 8
        && replacement_character_count.saturating_mul(100) <= character_count
        && valid_word_count >= 2;
    let image_count = semantic.images.len();
    let largest_image_pixels = semantic
        .images
        .iter()
        .map(|image| u64::from(image.width) * u64::from(image.height))
        .max()
        .unwrap_or(0);
    let direct_scan = direct_scan_image(&semantic);
    let kind = match (trustworthy, image_count > 0, character_count > 0) {
        (true, true, _) => PageContentKind::Hybrid,
        (true, false, _) => PageContentKind::NativeText,
        (false, true, false) => PageContentKind::ScannedImage,
        _ => PageContentKind::RenderRequired,
    };
    Ok(PageTextEvidence {
        kind,
        text,
        words,
        character_count,
        replacement_character_count,
        image_count,
        largest_image_pixels,
        trustworthy,
        direct_scan,
    })
}

fn direct_scan_image(page: &pdf_content::SemanticPage) -> Option<DirectScanImage> {
    use pdf_content::SemanticOp;
    if page.bounds.rotate != 0 {
        return None;
    }
    let draws = page
        .ops
        .iter()
        .filter_map(|operation| match operation {
            SemanticOp::DrawImage(image) => Some(*image),
            _ => None,
        })
        .collect::<Vec<_>>();
    if draws.len() != 1
        || page.ops.iter().any(|operation| {
            !matches!(
                operation,
                SemanticOp::Save
                    | SemanticOp::Restore
                    | SemanticOp::Concat(_)
                    | SemanticOp::DrawImage(_)
            )
        })
    {
        return None;
    }
    let matrices = page
        .ops
        .iter()
        .filter_map(|operation| match operation {
            SemanticOp::Concat(matrix) => Some(*matrix),
            _ => None,
        })
        .collect::<Vec<_>>();
    let matrix = *matrices.first()?;
    if matrices.len() != 1 || matrix.b.abs() > 1e-6 || matrix.c.abs() > 1e-6 {
        return None;
    }
    let crop = page.bounds.crop.normalized();
    let covered_width = matrix.a.abs();
    let covered_height = matrix.d.abs();
    if (covered_width - crop.width()).abs() > crop.width().abs() * 0.02
        || (covered_height - crop.height()).abs() > crop.height().abs() * 0.02
    {
        return None;
    }
    let image = page.images.get(draws[0].index())?;
    if image.is_mask
        || image.smask.is_some()
        || image.mask.is_some()
        || image.bits_per_component != 8
        || image.codec != Some(pdf_page_ir::ImageCodecKind::Dct)
        || !matches!(
            image.color_space,
            Some(pdf_page_ir::ImageColorSpace::Gray | pdf_page_ir::ImageColorSpace::Rgb)
        )
    {
        return None;
    }
    Some(DirectScanImage {
        jpeg: Arc::clone(image.codec_data.as_ref()?),
        width: image.width,
        height: image.height,
    })
}

pub fn has_text_layer(session: &RenderSession, page: u32) -> Result<bool, ReadError> {
    Ok(build_text_page(session, page)?.has_text())
}

pub fn page_text(session: &RenderSession, page: u32) -> Result<String, ReadError> {
    Ok(build_text_page(session, page)?.all_text())
}

pub fn positioned_words(
    session: &RenderSession,
    page: u32,
    source_width: u32,
    source_height: u32,
) -> Result<Vec<NativeTextWord>, ReadError> {
    let geometry = session.page_geometry(page)?;
    let text_page = build_text_page(session, page)?;
    Ok(text_page
        .words()
        .into_iter()
        .filter(|word| !word.text.trim().is_empty())
        .map(|word| NativeTextWord {
            text: word.text,
            bbox: rect_to_pixels(
                word.bbox,
                geometry.crop_box,
                geometry.rotate,
                source_width,
                source_height,
            ),
        })
        .collect())
}

fn build_text_page(session: &RenderSession, page: u32) -> Result<TextPage, ReadError> {
    let semantic = build_semantic_page(session, page)?;
    Ok(TextPage::build(&semantic, &TextPageOptions::default()))
}

fn build_semantic_page(
    session: &RenderSession,
    page: u32,
) -> Result<pdf_content::SemanticPage, ReadError> {
    if page >= session.page_count() {
        return Err(ReadError::PageOutOfRange {
            page,
            page_count: session.page_count(),
        });
    }
    let mut context = ParseContext::new();
    let semantic = session
        .compiler
        .compile_semantic(&session.snapshot, PageIndex(page), &mut context)
        .map_err(|error| ReadError::Compile {
            page,
            message: error.to_string(),
        })?;
    Ok(semantic)
}

fn rect_to_pixels(
    rect: Rect,
    crop_box: [f64; 4],
    rotate: u16,
    output_width: u32,
    output_height: u32,
) -> [f32; 4] {
    let [x0, y0, x1, y1] = crop_box;
    let crop_width = (x1 - x0).abs().max(f64::EPSILON);
    let crop_height = (y1 - y0).abs().max(f64::EPSILON);
    let rotation = rotate % 360;
    let (display_width, display_height) = if rotation % 180 == 0 {
        (crop_width, crop_height)
    } else {
        (crop_height, crop_width)
    };
    let scale_x = output_width as f64 / display_width;
    let scale_y = output_height as f64 / display_height;

    let transform = |x: f64, y: f64| -> (f64, f64) {
        let (device_x, device_y) = match rotation {
            90 => (y - y0, x - x0),
            180 => (x1 - x, y - y0),
            270 => (y1 - y, x1 - x),
            _ => (x - x0, y1 - y),
        };
        (device_x * scale_x, device_y * scale_y)
    };

    let corners = [
        transform(rect.x0, rect.y0),
        transform(rect.x0, rect.y1),
        transform(rect.x1, rect.y0),
        transform(rect.x1, rect.y1),
    ];
    let left = corners
        .iter()
        .map(|point| point.0)
        .fold(f64::INFINITY, f64::min);
    let top = corners
        .iter()
        .map(|point| point.1)
        .fold(f64::INFINITY, f64::min);
    let right = corners
        .iter()
        .map(|point| point.0)
        .fold(f64::NEG_INFINITY, f64::max);
    let bottom = corners
        .iter()
        .map(|point| point.1)
        .fold(f64::NEG_INFINITY, f64::max);
    [left as f32, top as f32, right as f32, bottom as f32]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_unrotated_pdf_coordinates_to_top_left_pixels() {
        let mapped = rect_to_pixels(
            Rect {
                x0: 10.0,
                y0: 20.0,
                x1: 30.0,
                y1: 40.0,
            },
            [0.0, 0.0, 100.0, 200.0],
            0,
            200,
            400,
        );
        assert_eq!(mapped, [20.0, 320.0, 60.0, 360.0]);
    }

    #[test]
    fn maps_rotated_pdf_coordinates_and_swaps_display_axes() {
        let mapped = rect_to_pixels(
            Rect {
                x0: 10.0,
                y0: 20.0,
                x1: 30.0,
                y1: 40.0,
            },
            [0.0, 0.0, 100.0, 200.0],
            90,
            400,
            200,
        );
        assert_eq!(mapped, [40.0, 20.0, 80.0, 60.0]);
    }
}
