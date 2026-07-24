use pdf_document::{PageIndex, ParseContext};
use pdf_page_ir::Rect;
use pdf_text::{TextPage, TextPageOptions};

use crate::{ReadError, RenderSession};

/// A word and its exact bounding box in source-render pixel space.
#[derive(Debug, Clone, PartialEq)]
pub struct NativeTextWord {
    pub text: String,
    /// `[left, top, right, bottom]` in a top-left-origin pixel space.
    pub bbox: [f32; 4],
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
    Ok(TextPage::build(&semantic, &TextPageOptions::default()))
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
