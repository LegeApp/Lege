//! Phase 2: sheet composition and preview.
//!
//! Every test here builds its own `Sheet` and `Placement` values by hand
//! rather than going through `layout::impose`, so composition is exercised
//! independently of the imposition maths.
//!
//! The fixture is a 200x200 point page with its **top-left** quadrant filled
//! black, which is deliberately asymmetric in both axes: a composition that
//! flips y, mirrors x, or turns the page the wrong way puts the black square
//! somewhere the assertions catch.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use lege_pdf_print::compose::{
    Band, ComposeOptions, SheetRaster, compose_sheet, compose_sheet_banded, sheet_pixel_size,
};
use lege_pdf_print::preview::{PreviewOptions, render_preview_png};
use lege_pdf_print::{
    Margins, Matrix, Orientation, PaperSize, Placement, PrintError, Rect, Sheet, Side,
};
use lege_pdf_read::RenderSession;

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

const PAGE_POINTS: f64 = 200.0;

/// Assemble a classic-xref PDF from object bodies, numbered from 1.
fn build_pdf(objects: &[String]) -> Vec<u8> {
    let mut out = Vec::from(&b"%PDF-1.7\n"[..]);
    let mut offsets = Vec::with_capacity(objects.len());
    for (index, body) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", index + 1).as_bytes());
    }
    let startxref = out.len();
    let size = objects.len() + 1;
    out.extend_from_slice(format!("xref\n0 {size}\n0000000000 65535 f \n").as_bytes());
    for offset in &offsets {
        out.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!("trailer\n<< /Size {size} /Root 1 0 R >>\nstartxref\n{startxref}\n%%EOF\n")
            .as_bytes(),
    );
    out
}

/// A one-page PDF whose top-left quadrant is black and whose other three
/// quadrants are unpainted (so, white).
fn quadrant_pdf() -> Vec<u8> {
    // PDF user space is y up, so the top-left quadrant of a 200x200 page is
    // x in [0, 100], y in [100, 200].
    let content = b"0 0 0 rg\n0 100 100 100 re\nf\n";
    build_pdf(&[
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] \
          /Resources << >> /Contents 4 0 R >>"
            .to_string(),
        format!(
            "<< /Length {} >>\nstream\n{}endstream",
            content.len(),
            String::from_utf8_lossy(content)
        ),
    ])
}

fn session() -> RenderSession {
    let bytes: Arc<[u8]> = Arc::from(quadrant_pdf().into_boxed_slice());
    RenderSession::open(bytes, None).expect("the fixture PDF parses")
}

fn sheet(bounds: Rect, imageable: Rect, placements: Vec<Placement>) -> Sheet {
    Sheet {
        index: 0,
        bounds,
        imageable,
        side: Side::Front,
        placements,
    }
}

fn placement(transform: Matrix, clip: Rect) -> Placement {
    Placement {
        source_page: 0,
        transform,
        clip,
    }
}

/// One point per pixel, so every assertion below reads in page points.
fn options() -> ComposeOptions {
    ComposeOptions {
        dpi: 72.0,
        ..ComposeOptions::default()
    }
}

#[track_caller]
fn assert_black(raster: &SheetRaster, x: u32, y: u32) {
    let pixel = raster.pixel(x, y).expect("pixel is on the sheet");
    assert!(
        pixel.iter().all(|&byte| byte < 32),
        "expected black at ({x}, {y}), found {pixel:?}"
    );
}

#[track_caller]
fn assert_white(raster: &SheetRaster, x: u32, y: u32) {
    let pixel = raster.pixel(x, y).expect("pixel is on the sheet");
    assert!(
        pixel.iter().all(|&byte| byte > 223),
        "expected white at ({x}, {y}), found {pixel:?}"
    );
}

// ---------------------------------------------------------------------------
// Geometry
// ---------------------------------------------------------------------------

#[test]
fn a_sheets_pixel_size_follows_its_bounds_and_dpi() {
    let one_up = sheet(
        Rect::new(0.0, 0.0, 200.0, 400.0),
        Rect::new(0.0, 0.0, 200.0, 400.0),
        Vec::new(),
    );
    assert_eq!(sheet_pixel_size(&one_up, &options()).unwrap(), (200, 400));
    let at_144 = ComposeOptions {
        dpi: 144.0,
        ..options()
    };
    assert_eq!(sheet_pixel_size(&one_up, &at_144).unwrap(), (400, 800));
}

#[test]
fn an_empty_sheet_is_all_white() {
    let blank = sheet(
        Rect::new(0.0, 0.0, PAGE_POINTS, PAGE_POINTS),
        Rect::new(0.0, 0.0, PAGE_POINTS, PAGE_POINTS),
        Vec::new(),
    );
    let raster = compose_sheet(&session(), &blank, &options()).unwrap();
    assert_eq!(
        (raster.width, raster.height, raster.channels),
        (200, 200, 3)
    );
    assert!(raster.pixels.iter().all(|&byte| byte == 0xFF));
}

#[test]
fn an_identity_placement_keeps_the_black_quadrant_top_left() {
    let full = Rect::new(0.0, 0.0, PAGE_POINTS, PAGE_POINTS);
    let one_up = sheet(full, full, vec![placement(Matrix::IDENTITY, full)]);
    let raster = compose_sheet(&session(), &one_up, &options()).unwrap();

    assert_black(&raster, 50, 50);
    assert_white(&raster, 150, 50);
    assert_white(&raster, 50, 150);
    assert_white(&raster, 150, 150);
}

#[test]
fn two_up_puts_each_page_in_its_own_half() {
    let bounds = Rect::new(0.0, 0.0, 400.0, 200.0);
    let left_cell = Rect::new(0.0, 0.0, 200.0, 200.0);
    let right_cell = Rect::new(200.0, 0.0, 400.0, 200.0);
    let two_up = sheet(
        bounds,
        bounds,
        vec![
            placement(Matrix::IDENTITY, left_cell),
            placement(Matrix::translate(200.0, 0.0), right_cell),
        ],
    );
    let raster = compose_sheet(&session(), &two_up, &options()).unwrap();
    assert_eq!((raster.width, raster.height), (400, 200));

    // Each cell carries the page's own top-left quadrant.
    assert_black(&raster, 50, 50);
    assert_black(&raster, 250, 50);
    // ... and nothing else.
    assert_white(&raster, 150, 50);
    assert_white(&raster, 350, 50);
    assert_white(&raster, 50, 150);
    assert_white(&raster, 250, 150);
}

#[test]
fn scaling_down_shrinks_the_page_into_its_cell() {
    let bounds = Rect::new(0.0, 0.0, PAGE_POINTS, PAGE_POINTS);
    // Half scale into the sheet's bottom-left quarter.
    let cell = Rect::new(0.0, 0.0, 100.0, 100.0);
    let scaled = sheet(
        bounds,
        bounds,
        vec![placement(Matrix::scale(0.5, 0.5), cell)],
    );
    let raster = compose_sheet(&session(), &scaled, &options()).unwrap();

    // The page now occupies sheet points x in [0, 100], y in [0, 100], which
    // is pixel rows 100..200. Its black quadrant is that block's top-left.
    assert_black(&raster, 25, 125);
    assert_white(&raster, 75, 125);
    assert_white(&raster, 25, 175);
    // Nothing escaped the cell.
    assert_white(&raster, 150, 50);
}

#[test]
fn a_quarter_turn_rotates_the_black_quadrant() {
    let full = Rect::new(0.0, 0.0, PAGE_POINTS, PAGE_POINTS);
    // 90 degrees counter-clockwise about the origin, then back onto the
    // sheet: (x, y) -> (200 - y, x).
    let transform = Matrix::rotate_degrees(90).then(Matrix::translate(PAGE_POINTS, 0.0));
    let turned = sheet(full, full, vec![placement(transform, full)]);
    let raster = compose_sheet(&session(), &turned, &options()).unwrap();

    // The black quadrant lands on sheet points x in [0, 100], y in [0, 100]:
    // the *bottom* left in pixel rows.
    assert_black(&raster, 50, 150);
    assert_white(&raster, 50, 50);
    assert_white(&raster, 150, 150);
    assert_white(&raster, 150, 50);
}

#[test]
fn a_mirror_moves_the_black_quadrant_across() {
    let full = Rect::new(0.0, 0.0, PAGE_POINTS, PAGE_POINTS);
    // x -> 200 - x, y unchanged.
    let transform = Matrix::new(-1.0, 0.0, 0.0, 1.0, PAGE_POINTS, 0.0);
    let mirrored = sheet(full, full, vec![placement(transform, full)]);
    let raster = compose_sheet(&session(), &mirrored, &options()).unwrap();

    assert_black(&raster, 150, 50);
    assert_white(&raster, 50, 50);
    assert_white(&raster, 150, 150);
}

#[test]
fn a_rotation_that_is_not_a_quarter_turn_still_composes() {
    let full = Rect::new(0.0, 0.0, PAGE_POINTS, PAGE_POINTS);
    let transform = Matrix::rotate_degrees(30).then(Matrix::translate(120.0, 10.0));
    let skewed = sheet(full, full, vec![placement(transform, full)]);
    let raster = compose_sheet(&session(), &skewed, &options()).unwrap();

    // The fallback resamples rather than failing, so some of the page must
    // have landed on the sheet.
    assert!(
        raster.pixels.iter().any(|&byte| byte < 32),
        "the rotated page painted nothing"
    );
}

// ---------------------------------------------------------------------------
// Clipping
// ---------------------------------------------------------------------------

#[test]
fn the_imageable_area_clips_the_margins_white() {
    let bounds = Rect::new(0.0, 0.0, PAGE_POINTS, PAGE_POINTS);
    let imageable = Rect::new(20.0, 20.0, 180.0, 180.0);
    // The placement itself asks for the whole sheet; only the imageable area
    // holds it back.
    let clipped = sheet(bounds, imageable, vec![placement(Matrix::IDENTITY, bounds)]);
    let raster = compose_sheet(&session(), &clipped, &options()).unwrap();

    for x in [0, 5, 19, 180, 199] {
        assert_white(&raster, x, 50);
    }
    for y in [0, 5, 19, 180, 199] {
        assert_white(&raster, 50, y);
    }
    // Inside the imageable area the quadrant is still black.
    assert_black(&raster, 50, 50);
    assert_black(&raster, 25, 25);
}

#[test]
fn a_placement_clip_confines_it_to_its_own_cell() {
    let bounds = Rect::new(0.0, 0.0, PAGE_POINTS, PAGE_POINTS);
    // The page is placed at full size but may only paint its right half.
    let clip = Rect::new(100.0, 0.0, PAGE_POINTS, PAGE_POINTS);
    let clipped = sheet(bounds, bounds, vec![placement(Matrix::IDENTITY, clip)]);
    let raster = compose_sheet(&session(), &clipped, &options()).unwrap();

    // The black quadrant is entirely in the left half, so it is all clipped.
    assert_white(&raster, 50, 50);
    assert!(raster.pixels.iter().all(|&byte| byte == 0xFF));
}

// ---------------------------------------------------------------------------
// Colour
// ---------------------------------------------------------------------------

#[test]
fn grayscale_emits_one_byte_per_pixel() {
    let full = Rect::new(0.0, 0.0, PAGE_POINTS, PAGE_POINTS);
    let one_up = sheet(full, full, vec![placement(Matrix::IDENTITY, full)]);
    let gray = ComposeOptions {
        grayscale: true,
        ..options()
    };
    let raster = compose_sheet(&session(), &one_up, &gray).unwrap();

    assert_eq!(raster.channels, 1);
    assert_eq!(raster.pixels.len(), 200 * 200);
    assert_eq!(raster.pixel(50, 50), Some(&[0u8][..]));
    assert_eq!(raster.pixel(150, 50), Some(&[0xFFu8][..]));
}

// ---------------------------------------------------------------------------
// Banding
// ---------------------------------------------------------------------------

fn collect_bands(sheet: &Sheet, options: &ComposeOptions) -> Vec<Band> {
    let mut bands = Vec::new();
    compose_sheet_banded(&session(), sheet, options, |band| {
        bands.push(band.clone());
        Ok(())
    })
    .unwrap();
    bands
}

#[test]
fn bands_tile_the_sheet_top_to_bottom() {
    let full = Rect::new(0.0, 0.0, PAGE_POINTS, PAGE_POINTS);
    let one_up = sheet(full, full, vec![placement(Matrix::IDENTITY, full)]);
    let banded = ComposeOptions {
        band_rows: 64,
        ..options()
    };
    let bands = collect_bands(&one_up, &banded);

    assert_eq!(bands.len(), 4);
    let mut expected_y = 0;
    for band in &bands {
        assert_eq!(band.y, expected_y);
        assert_eq!(band.width, 200);
        assert_eq!(band.channels, 3);
        assert_eq!(
            band.pixels.len(),
            band.width as usize * band.height as usize * 3
        );
        expected_y += band.height;
    }
    assert_eq!(expected_y, 200);
    // The last band is short: 200 is not a multiple of 64.
    assert_eq!(bands[3].height, 8);
}

#[test]
fn banding_produces_the_same_bytes_as_the_unbanded_path() {
    let bounds = Rect::new(0.0, 0.0, 400.0, 200.0);
    let two_up = sheet(
        bounds,
        Rect::new(10.0, 10.0, 390.0, 190.0),
        vec![
            placement(Matrix::IDENTITY, Rect::new(0.0, 0.0, 200.0, 200.0)),
            placement(
                Matrix::rotate_degrees(90).then(Matrix::translate(400.0, 0.0)),
                Rect::new(200.0, 0.0, 400.0, 200.0),
            ),
        ],
    );

    let one_shot = compose_sheet(
        &session(),
        &two_up,
        &ComposeOptions {
            band_rows: u32::MAX,
            ..options()
        },
    )
    .unwrap();
    let fine = compose_sheet(
        &session(),
        &two_up,
        &ComposeOptions {
            band_rows: 7,
            ..options()
        },
    )
    .unwrap();

    assert_eq!(one_shot.width, fine.width);
    assert_eq!(one_shot.height, fine.height);
    assert_eq!(one_shot.pixels, fine.pixels);

    // And the streamed bands concatenate to the same thing.
    let streamed: Vec<u8> = collect_bands(
        &two_up,
        &ComposeOptions {
            band_rows: 13,
            ..options()
        },
    )
    .into_iter()
    .flat_map(|band| band.pixels)
    .collect();
    assert_eq!(streamed, one_shot.pixels);
}

#[test]
fn a_band_callback_error_stops_the_composition() {
    let full = Rect::new(0.0, 0.0, PAGE_POINTS, PAGE_POINTS);
    let one_up = sheet(full, full, vec![placement(Matrix::IDENTITY, full)]);
    let mut seen = 0usize;
    let result = compose_sheet_banded(
        &session(),
        &one_up,
        &ComposeOptions {
            band_rows: 32,
            ..options()
        },
        |_| {
            seen += 1;
            if seen == 2 {
                Err(PrintError::Spool("the device went away".into()))
            } else {
                Ok(())
            }
        },
    );
    assert!(matches!(result, Err(PrintError::Spool(_))));
    assert_eq!(seen, 2);
}

// ---------------------------------------------------------------------------
// Budgets and validation
// ---------------------------------------------------------------------------

#[test]
fn an_oversized_sheet_is_refused_before_it_is_allocated() {
    let full = Rect::new(0.0, 0.0, PAGE_POINTS, PAGE_POINTS);
    let one_up = sheet(full, full, vec![placement(Matrix::IDENTITY, full)]);
    let stingy = ComposeOptions {
        max_pixels: 1_000,
        ..options()
    };
    match compose_sheet(&session(), &one_up, &stingy) {
        Err(PrintError::SheetTooLarge {
            width,
            height,
            max_pixels,
        }) => {
            assert_eq!((width, height, max_pixels), (200, 200, 1_000));
        }
        other => panic!("expected SheetTooLarge, got {other:?}"),
    }
}

#[test]
fn nonsense_composition_settings_are_rejected() {
    let full = Rect::new(0.0, 0.0, PAGE_POINTS, PAGE_POINTS);
    let one_up = sheet(full, full, Vec::new());
    for bad in [
        ComposeOptions {
            dpi: 0.0,
            ..options()
        },
        ComposeOptions {
            dpi: f64::NAN,
            ..options()
        },
        ComposeOptions {
            dpi: 20_000.0,
            ..options()
        },
        ComposeOptions {
            band_rows: 0,
            ..options()
        },
    ] {
        assert!(
            matches!(
                compose_sheet(&session(), &one_up, &bad),
                Err(PrintError::InvalidOptions(_))
            ),
            "{bad:?} should not compose"
        );
    }
}

// ---------------------------------------------------------------------------
// Preview
// ---------------------------------------------------------------------------

#[test]
fn the_preview_is_a_png_of_the_composed_sheet() {
    let full = Rect::new(0.0, 0.0, PAGE_POINTS, PAGE_POINTS);
    let one_up = sheet(full, full, vec![placement(Matrix::IDENTITY, full)]);
    let png_bytes = render_preview_png(
        &session(),
        &one_up,
        PreviewOptions {
            dpi: 72.0,
            grayscale: false,
        },
    )
    .unwrap();

    let decoder = png::Decoder::new(std::io::Cursor::new(&png_bytes));
    let mut reader = decoder.read_info().expect("the preview decodes");
    let info = reader.info();
    assert_eq!((info.width, info.height), (200, 200));
    assert_eq!(info.color_type, png::ColorType::Rgb);
    assert_eq!(info.bit_depth, png::BitDepth::Eight);

    let mut pixels = vec![
        0u8;
        reader
            .output_buffer_size()
            .expect("the buffer size is known")
    ];
    let frame = reader.next_frame(&mut pixels).expect("the frame decodes");
    let pixels = &pixels[..frame.buffer_size()];
    let at = |x: usize, y: usize| &pixels[(y * 200 + x) * 3..(y * 200 + x) * 3 + 3];
    assert!(at(50, 50).iter().all(|&byte| byte < 32));
    assert!(at(150, 50).iter().all(|&byte| byte > 223));
}

#[test]
fn a_grayscale_preview_is_an_eight_bit_gray_png() {
    let full = Rect::new(0.0, 0.0, PAGE_POINTS, PAGE_POINTS);
    let one_up = sheet(full, full, vec![placement(Matrix::IDENTITY, full)]);
    let png_bytes = render_preview_png(
        &session(),
        &one_up,
        PreviewOptions {
            dpi: 36.0,
            grayscale: true,
        },
    )
    .unwrap();

    let decoder = png::Decoder::new(std::io::Cursor::new(&png_bytes));
    let reader = decoder.read_info().expect("the preview decodes");
    let info = reader.info();
    assert_eq!((info.width, info.height), (100, 100));
    assert_eq!(info.color_type, png::ColorType::Grayscale);
}

// ---------------------------------------------------------------------------
// A realistic sheet
// ---------------------------------------------------------------------------

/// Two pages side by side on a 300-DPI A4 sheet: the shape a real job takes,
/// at a resolution where banding is the point.
#[test]
fn a_300_dpi_a4_sheet_bands_without_holding_the_whole_raster() {
    let bounds = PaperSize::A4.rect(Orientation::Landscape);
    let margins = Margins::millimetres(6.35);
    let imageable = bounds.inset(margins.left, margins.bottom);
    let half = bounds.width() / 2.0;
    // Fit the 200x200pt page into each half of the imageable area.
    let scale = (half - margins.left * 2.0).min(imageable.height()) / PAGE_POINTS;
    let two_up = sheet(
        bounds,
        imageable,
        vec![
            placement(
                Matrix::scale(scale, scale).then(Matrix::translate(margins.left, margins.bottom)),
                Rect::new(imageable.x0, imageable.y0, half, imageable.y1),
            ),
            placement(
                Matrix::scale(scale, scale).then(Matrix::translate(half, margins.bottom)),
                Rect::new(half, imageable.y0, imageable.x1, imageable.y1),
            ),
        ],
    );
    let settings = ComposeOptions {
        dpi: 300.0,
        band_rows: 128,
        ..ComposeOptions::default()
    };

    let (width, height) = sheet_pixel_size(&two_up, &settings).unwrap();
    assert_eq!((width, height), (3508, 2480));

    let mut rows = 0u32;
    let mut widest_band = 0usize;
    let mut painted = false;
    compose_sheet_banded(&session(), &two_up, &settings, |band| {
        assert_eq!(band.y, rows);
        rows += band.height;
        widest_band = widest_band.max(band.pixels.len());
        painted |= band.pixels.iter().any(|&byte| byte < 32);
        Ok(())
    })
    .unwrap();

    assert_eq!(rows, height);
    // The whole sheet would be 26 MB; a band is a small fraction of that.
    assert_eq!(widest_band, 3508 * 128 * 3);
    assert!(painted, "neither page painted anything");
}
