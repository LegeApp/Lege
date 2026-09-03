//! Adapter: `accumulator::Page` → `lege_pdf_write::PdfPageArtifact`.
//!
//! Keeps the pipeline's page DTO unchanged while feeding the new writer. All
//! hOCR parsing and the top-left → bottom-left Y flip happen here (in Lege), so
//! the writer only emits. Shared JBIG2 globals are keyed by a content hash so
//! the writer's registry writes each blob once.

use std::sync::Arc;

use anyhow::{Result, anyhow};

use lege_pdf_write::artifact::{
    ColorModel, GlyphItem, GlyphLine, PdfImageElement, PdfImageResource, PdfPageArtifact,
    PreparedGlyphLayer, PreparedTextLayer, TextFont, TextRun,
};
use lege_pdf_write::font::EmbeddedFont;
use lege_pdf_write::resources::SharedResourceId;
use lege_pdf_write::types::{Affine, PdfRect};

use crate::accumulator::{ContentElement, ContentType, Page};
use crate::encoding::glyphfont::{
    EM_PIXELS, FIRST_SHAPE_GID, PageGlyphRuns, SPACE_ADVANCE, SPACE_GID, UNITS_PER_PIXEL,
};
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
    let mut glyph_lines: Vec<GlyphLine> = Vec::new();

    for el in &page.elements {
        if let ContentType::GlyphText {
            runs,
            pixel_width,
            pixel_height,
        } = &el.content
        {
            glyph_lines.extend(glyph_runs_to_lines(
                page,
                el,
                runs,
                *pixel_width,
                *pixel_height,
            )?);
            continue;
        }
        // Top-left (pixel/point) origin → PDF bottom-left origin.
        let pdf_y = (page.height - el.y - el.height) as f64;
        let transform =
            Affine::scale_translate(el.width as f64, el.height as f64, el.x as f64, pdf_y);
        let image = content_to_resource(&el.content, &mut globals)?;
        elements.push(PdfImageElement { transform, image });
    }

    // Glyph text carries its own ToUnicode mapping (from the same OCR words),
    // so an invisible text layer would only duplicate it.
    let text_layer = if glyph_lines.is_empty() {
        build_text_layer(page, embedded_font_available)
    } else {
        None
    };
    let glyph_layer = (!glyph_lines.is_empty()).then(|| PreparedGlyphLayer {
        lines: glyph_lines.into_boxed_slice(),
    });

    let artifact = PdfPageArtifact {
        index: page.index as u32,
        media_box: PdfRect::from_size(page.width as f64, page.height as f64),
        elements: elements.into_boxed_slice(),
        text_layer,
        glyph_layer,
    };
    Ok((artifact, globals))
}

/// Turn a page's glyph placements (pixel space, y down) into writer lines in
/// PDF user space. The font is selected at size 1, so each line's text matrix
/// scales one em (`EM_PIXELS` source pixels) to points and puts its origin
/// at the line's first glyph on the baseline. Inter-glyph gaps become `TJ`
/// adjustments, word-sized gaps get the space glyph, and any per-glyph
/// baseline deviation left after the dictionary's position variants becomes
/// a text rise. The matrix is never tilted: poppler-based extractors
/// scramble the reading order of lines whose text matrix is rotated even
/// slightly, so a skewed scan line is served by the variants instead.
/// Glyph placements this close (source pixels) to the pen are drawn at the
/// pen without a `TJ` adjustment.
const GLYPH_ADJUST_SNAP_PX: i32 = 1;
/// A gap between a glyph and the pen of at least this many source pixels is
/// a word space: the blank space glyph is drawn in it so text extraction
/// sees the word break. Letter gaps beyond the advance's typical gap stay
/// within a few pixels; word spaces on a 2400-pixel page run 15 pixels and
/// more even in small type.
const WORD_SPACE_MIN_PX: i32 = 8;

fn glyph_runs_to_lines(
    page: &Page,
    el: &ContentElement,
    runs: &PageGlyphRuns,
    pixel_width: u32,
    pixel_height: u32,
) -> Result<Vec<GlyphLine>> {
    if pixel_width == 0 || pixel_height == 0 {
        return Err(anyhow!("glyph text element has a zero-sized raster"));
    }
    let sx = el.width as f64 / pixel_width as f64;
    let sy = el.height as f64 / pixel_height as f64;
    let mut lines = Vec::with_capacity(runs.lines.len());
    for line in &runs.lines {
        let Some(first) = line.glyphs.first() else {
            continue;
        };
        let mut items = Vec::with_capacity(line.glyphs.len());
        let mut pen_px: Option<i32> = None;
        for g in &line.glyphs {
            let gid = u16::try_from(g.glyph + FIRST_SHAPE_GID).map_err(|_| {
                anyhow!(
                    "glyph id {} exceeds the 16-bit CID space",
                    g.glyph + FIRST_SHAPE_GID
                )
            })?;
            // A word-sized gap gets the space glyph, adjusted so its advance
            // plus the adjustment spans the gap exactly.
            if let Some(pen) = pen_px {
                let gap_units = (g.x - pen) * UNITS_PER_PIXEL;
                if g.x - pen >= WORD_SPACE_MIN_PX {
                    items.push(GlyphItem {
                        gid: SPACE_GID,
                        adjust: SPACE_ADVANCE as i32 - gap_units,
                        rise: 0,
                    });
                    pen_px = Some(g.x);
                }
            }
            // Where the pen is versus where the glyph goes. A gap of at most
            // one source pixel is left to the advance: the glyph lands up to
            // a pixel off, the pen is tracked from where it actually landed,
            // and the run stays one string.
            let (adjust, drawn_x) = match pen_px {
                None => (0, g.x),
                Some(pen) if (g.x - pen).abs() <= GLYPH_ADJUST_SNAP_PX => (0, pen),
                Some(pen) => (-(g.x - pen) * UNITS_PER_PIXEL, g.x),
            };
            items.push(GlyphItem {
                gid,
                adjust,
                rise: g.rise_px * UNITS_PER_PIXEL,
            });
            pen_px = Some(drawn_x + g.width as i32);
        }
        // Lines are level in the upright frame; on a page scanned sideways
        // or upside down the text matrix turns them back onto the raster.
        let frame = &runs.frame;
        let (ox, oy) = frame.unturn_point(first.x as f64, line.baseline_y as f64);
        let origin_x = el.x as f64 + ox * sx;
        let origin_y = (page.height - el.y) as f64 - oy * sy;
        let (ax, ay) = frame.unturn_direction(1.0, 0.0);
        let (bx, by) = frame.unturn_direction(0.0, -1.0);
        lines.push(GlyphLine {
            matrix: Affine::new(
                EM_PIXELS * ax * sx,
                -EM_PIXELS * ay * sy,
                EM_PIXELS * bx * sx,
                -EM_PIXELS * by * sy,
                origin_x,
                origin_y,
            ),
            items: items.into_boxed_slice(),
        });
    }
    Ok(lines)
}

/// Build `EmbeddedFont` from Lege's glyphless font data.
pub fn embedded_font_from(u: &UnicodeFontData) -> EmbeddedFont {
    let m = &u.metrics;
    EmbeddedFont::glyphless(
        u.data.clone(),
        u.post_script_name.clone(),
        m.ascent as i32,
        m.descent as i32,
        m.cap_height as i32,
        m.italic_angle,
        [
            m.bbox.x_min as i32,
            m.bbox.y_min as i32,
            m.bbox.x_max as i32,
            m.bbox.y_max as i32,
        ],
    )
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
        ContentType::GlyphText { .. } => Err(anyhow!(
            "glyph text is not an image resource (handled by page_to_artifact)"
        )),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoding::glyphfont::{GlyphLine as RunLine, PlacedGlyph};

    #[test]
    fn word_gaps_get_the_space_glyph() {
        let runs = PageGlyphRuns {
            lines: vec![RunLine {
                baseline_y: 100,
                glyphs: vec![
                    PlacedGlyph {
                        glyph: 0,
                        x: 10,
                        width: 20,
                        rise_px: 0,
                    },
                    // One pixel past the pen: drawn at the pen, no adjustment.
                    PlacedGlyph {
                        glyph: 1,
                        x: 31,
                        width: 20,
                        rise_px: 0,
                    },
                    // Thirty pixels past the pen: a word space.
                    PlacedGlyph {
                        glyph: 0,
                        x: 80,
                        width: 20,
                        rise_px: 0,
                    },
                ],
            }],
            glyph_count: 3,
            frame: Default::default(),
        };
        let page = Page {
            width: 1000.0,
            height: 1000.0,
            index: 0,
            hocr_text: Some("<p class='ocr_line'>ignored</p>".into()),
            binarized: None,
            elements: vec![ContentElement {
                x: 0.0,
                y: 0.0,
                width: 1000.0,
                height: 1000.0,
                content: ContentType::GlyphText {
                    runs: Arc::new(runs),
                    pixel_width: 1000,
                    pixel_height: 1000,
                },
            }],
        };
        let (art, _) = page_to_artifact(&page, false).unwrap();
        assert!(
            art.text_layer.is_none(),
            "glyph text replaces the OCR layer"
        );
        let layer = art.glyph_layer.unwrap();
        let line = &layer.lines[0];
        let gids: Vec<u16> = line.items.iter().map(|i| i.gid).collect();
        assert_eq!(gids, vec![2, 3, SPACE_GID, 2]);
        // The pen sits at 50 after the second glyph; the gap to 80 is 30 px.
        assert_eq!(line.items[1].adjust, 0);
        assert_eq!(
            line.items[2].adjust,
            SPACE_ADVANCE as i32 - 30 * UNITS_PER_PIXEL
        );
        assert_eq!(line.items[3].adjust, 0);
        assert_eq!(
            line.matrix,
            Affine::new(EM_PIXELS, 0.0, 0.0, EM_PIXELS, 10.0, 900.0)
        );
    }
}
