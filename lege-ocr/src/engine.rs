use anyhow::Result;
use image::GrayImage;

use crate::types::{OcrLineResult, OcrResult, OcrWord};

/// Abstraction over OCR backends.
pub trait OcrEngine: Send + Sync {
    /// OCR raw image bytes (1-byte-per-pixel grayscale or binary, or 3-byte RGB).
    ///
    /// `is_binary` is true when data is 1bpp binarized (0 = ink, 255 = background).
    /// Returns hOCR markup + plain text, or `None` if the engine fails or the image
    /// is empty/uniform.
    ///
    /// This is the entry point for the **fast** OCR path (page/region/tile mode).
    fn run_image(
        &self,
        data: &[u8],
        width: usize,
        height: usize,
        is_binary: bool,
        lang: &str,
    ) -> Option<OcrResult>;

    /// OCR a single-line grayscale image.
    ///
    /// The image should be a tight crop of one text line, in grayscale. Bboxes in the
    /// returned words are in crop-local pixel coordinates.
    fn ocr_line(&self, image: &GrayImage, lang: &str) -> Result<OcrLineResult>;

    /// OCR a multi-line text-block grayscale image (used as region-level fallback).
    ///
    /// Returns one `OcrLineResult` per line found by the engine.
    fn ocr_region(&self, image: &GrayImage, lang: &str) -> Result<Vec<OcrLineResult>>;

    /// OCR a full page or unknown-layout region. Backends may use automatic
    /// layout analysis instead of assuming one text block.
    fn ocr_page(&self, image: &GrayImage, lang: &str) -> Result<Vec<OcrLineResult>> {
        self.ocr_region(image, lang)
    }

    /// Whether the shared line OCR path should run a second binary-image pass
    /// and choose between grayscale/binary outputs.
    fn use_binary_line_retry(&self) -> bool {
        false
    }

    fn name(&self) -> &'static str;
}

/// Validate a tightly packed grayscale/binary or RGB image and return its
/// bytes-per-pixel. Keeping this check shared prevents platform backends from
/// evaluating unchecked attacker-controlled dimension products.
///
/// Gated to the backends that call it: with no OCR backend selected nothing
/// reaches this, and `test` keeps it available to the unit tests below.
#[cfg(any(
    target_os = "windows",
    feature = "paddle-ocr",
    all(
        any(target_os = "linux", target_os = "macos"),
        feature = "tesseract-ocr"
    ),
    test,
))]
pub(crate) fn raw_image_bpp(
    data: &[u8],
    width: usize,
    height: usize,
    is_binary: bool,
) -> Option<usize> {
    if width == 0 || height == 0 || width > 65535 || height > 65535 {
        return None;
    }
    let pixels = width.checked_mul(height)?;
    if data.len() == pixels {
        Some(1)
    } else if !is_binary && pixels.checked_mul(3) == Some(data.len()) {
        Some(3)
    } else {
        None
    }
}

// ── Platform engine constructors ─────────────────────────────────────────────

/// Return the platform default engine.
///
/// Precedence: Windows → WinRT OCR. Linux/macOS → PP-OCR (`paddle-ocr`) when
/// available (GPU, pure-Rust, cross-compiles cleanly), else Tesseract
/// (`tesseract-ocr`), else the unavailable stub.
#[allow(unreachable_code)]
pub fn default_engine() -> Box<dyn OcrEngine> {
    #[cfg(target_os = "windows")]
    return Box::new(WinOcrEngine);

    // Prefer PP-OCR on Linux/macOS: it replaces the native Tesseract dependency
    // that blocks the macOS cross-compile. Android uses it for the same reason
    // and has no alternative — there is no system OCR service to fall back to,
    // and PP-OCR's models are embedded, so nothing needs installing.
    #[cfg(all(
        any(
            target_os = "linux",
            target_os = "macos",
            all(target_os = "android", feature = "android")
        ),
        feature = "paddle-ocr"
    ))]
    {
        match crate::engine_paddle::PaddleOcrEngine::shared_embedded() {
            Ok(engine) => return Box::new(engine),
            Err(err) => {
                log::warn!("PP-OCR engine unavailable, falling back: {err:#}");
            }
        }
    }

    #[cfg(all(
        any(target_os = "linux", target_os = "macos"),
        feature = "tesseract-ocr"
    ))]
    return Box::new(TesseractEngine);

    #[cfg(not(any(
        target_os = "windows",
        all(
            any(target_os = "linux", target_os = "macos"),
            feature = "tesseract-ocr"
        )
    )))]
    return Box::new(UnavailableOcrEngine);
}

#[cfg(not(any(
    target_os = "windows",
    all(
        any(target_os = "linux", target_os = "macos"),
        feature = "tesseract-ocr"
    )
)))]
pub struct UnavailableOcrEngine;

#[cfg(not(any(
    target_os = "windows",
    all(
        any(target_os = "linux", target_os = "macos"),
        feature = "tesseract-ocr"
    )
)))]
impl OcrEngine for UnavailableOcrEngine {
    fn name(&self) -> &'static str {
        "unavailable"
    }

    fn run_image(
        &self,
        _data: &[u8],
        _width: usize,
        _height: usize,
        _is_binary: bool,
        _lang: &str,
    ) -> Option<OcrResult> {
        None
    }

    fn ocr_line(&self, _image: &GrayImage, _lang: &str) -> Result<OcrLineResult> {
        Err(anyhow::anyhow!(
            "OCR support was not compiled into this build"
        ))
    }

    fn ocr_region(&self, _image: &GrayImage, _lang: &str) -> Result<Vec<OcrLineResult>> {
        Err(anyhow::anyhow!(
            "OCR support was not compiled into this build"
        ))
    }
}

// ═════════════════════════════════════════════════════════════════════════════
// Tesseract engine (Linux / macOS)
// ═════════════════════════════════════════════════════════════════════════════

#[cfg(all(
    any(target_os = "linux", target_os = "macos"),
    feature = "tesseract-ocr"
))]
pub struct TesseractEngine;

#[cfg(all(
    any(target_os = "linux", target_os = "macos"),
    feature = "tesseract-ocr"
))]
impl OcrEngine for TesseractEngine {
    fn name(&self) -> &'static str {
        "tesseract"
    }

    fn use_binary_line_retry(&self) -> bool {
        true
    }

    fn run_image(
        &self,
        data: &[u8],
        width: usize,
        height: usize,
        is_binary: bool,
        lang: &str,
    ) -> Option<OcrResult> {
        use tesseract::PageSegMode;

        let bpp = raw_image_bpp(data, width, height, is_binary)?;
        if data.iter().all(|&b| b == data[0]) {
            return Some(OcrResult {
                hocr: String::new(),
                plain_text: String::new(),
            });
        }

        const MAX_PIXELS: usize = 2_000_000;
        let pixels = width.checked_mul(height)?;
        let (final_data, final_w, final_h, _scale_x, _scale_y): (Vec<u8>, usize, usize, f32, f32) =
            if pixels > MAX_PIXELS {
                let scale = (MAX_PIXELS as f64 / pixels as f64).sqrt();
                let nw = ((width as f64 * scale).round() as usize).max(1);
                let nh = ((height as f64 * scale).round() as usize).max(1);
                let resized = resize_cpu(data, width, height, nw, nh, bpp);
                (
                    resized,
                    nw,
                    nh,
                    width as f32 / nw as f32,
                    height as f32 / nh as f32,
                )
            } else {
                (data.to_vec(), width, height, 1.0, 1.0)
            };

        let tess = build_tesseract(lang, PageSegMode::PsmAuto).ok()?;
        let bpl = final_w * bpp;
        let tess = tess
            .set_frame(
                &final_data,
                final_w as i32,
                final_h as i32,
                bpp as i32,
                bpl as i32,
            )
            .ok()?
            .set_source_resolution(300);
        let mut tess = tess.recognize().ok()?;
        let hocr = tess.get_hocr_text(0).ok()?;
        let plain = tess.get_text().unwrap_or_default();

        let final_hocr = if pixels > MAX_PIXELS {
            scale_hocr_coords(&hocr, final_w, final_h, width, height)
        } else {
            hocr
        };

        Some(OcrResult {
            hocr: final_hocr,
            plain_text: plain.trim().to_string(),
        })
    }

    fn ocr_line(&self, image: &GrayImage, lang: &str) -> Result<OcrLineResult> {
        use tesseract::PageSegMode;

        let w = image.width() as usize;
        let h = image.height() as usize;
        let data = image.as_raw();
        let bbox = [0u32, 0, image.width(), image.height()];

        if w == 0 || h == 0 {
            return Ok(OcrLineResult {
                text: String::new(),
                confidence: None,
                words: Vec::new(),
                bbox_highres: bbox,
            });
        }
        if data.iter().all(|&b| b == data[0]) {
            return Ok(OcrLineResult {
                text: String::new(),
                confidence: None,
                words: Vec::new(),
                bbox_highres: bbox,
            });
        }

        let tess = build_tesseract(lang, PageSegMode::PsmSingleLine)?;
        let (tess, _final_w, _final_h, scale_x, scale_y) =
            set_image_with_downscale(tess, data, w, h, 1)?;
        let mut tess = tess.recognize()?;
        let hocr = tess.get_hocr_text(0)?;
        let plain = tess.get_text().unwrap_or_default();
        let words = parse_hocr_words(&hocr, scale_x, scale_y);
        let confidence = mean_word_confidence(&words);

        Ok(OcrLineResult {
            text: plain.trim().to_string(),
            confidence,
            words,
            bbox_highres: bbox,
        })
    }

    fn ocr_region(&self, image: &GrayImage, lang: &str) -> Result<Vec<OcrLineResult>> {
        ocr_tesseract_multiline(image, lang, tesseract::PageSegMode::PsmSingleBlock)
    }

    fn ocr_page(&self, image: &GrayImage, lang: &str) -> Result<Vec<OcrLineResult>> {
        ocr_tesseract_multiline(image, lang, tesseract::PageSegMode::PsmAuto)
    }
}

#[cfg(all(
    any(target_os = "linux", target_os = "macos"),
    feature = "tesseract-ocr"
))]
fn ocr_tesseract_multiline(
    image: &GrayImage,
    lang: &str,
    page_seg_mode: tesseract::PageSegMode,
) -> Result<Vec<OcrLineResult>> {
    let w = image.width() as usize;
    let h = image.height() as usize;
    let data = image.as_raw();

    if w == 0 || h == 0 {
        return Ok(Vec::new());
    }
    if data.iter().all(|&b| b == data[0]) {
        return Ok(Vec::new());
    }

    let tess = build_tesseract(lang, page_seg_mode)?;
    let (tess, _final_w, _final_h, scale_x, scale_y) =
        set_image_with_downscale(tess, data, w, h, 1)?;
    let mut tess = tess.recognize()?;
    let hocr = tess.get_hocr_text(0)?;
    Ok(parse_hocr_lines(
        &hocr,
        image.width(),
        image.height(),
        scale_x,
        scale_y,
    ))
}

#[cfg(all(
    any(target_os = "linux", target_os = "macos"),
    feature = "tesseract-ocr"
))]
fn build_tesseract(lang: &str, psm: tesseract::PageSegMode) -> Result<tesseract::Tesseract> {
    use tesseract::{OcrEngineMode, Tesseract};

    let normalized = lang.trim().to_ascii_lowercase();
    let tessdata = find_tessdata_dir(&normalized);
    let tess = match tessdata {
        Some(ref dir) => Tesseract::new_with_oem(
            Some(dir.as_str()),
            Some(&normalized),
            OcrEngineMode::LstmOnly,
        )
        .or_else(|_| Tesseract::new_with_oem(None, Some(&normalized), OcrEngineMode::LstmOnly))?,
        None => Tesseract::new_with_oem(None, Some(&normalized), OcrEngineMode::LstmOnly)?,
    };
    #[cfg(windows)]
    let null_dev = "NUL";
    #[cfg(not(windows))]
    let null_dev = "/dev/null";
    let mut tess = tess.set_variable("debug_file", null_dev)?;
    tess.set_page_seg_mode(psm);
    Ok(tess)
}

#[cfg(all(
    any(target_os = "linux", target_os = "macos"),
    feature = "tesseract-ocr"
))]
fn set_image_with_downscale(
    tess: tesseract::Tesseract,
    data: &[u8],
    w: usize,
    h: usize,
    bpp: usize,
) -> Result<(tesseract::Tesseract, usize, usize, f32, f32)> {
    const MAX_PIXELS: usize = 2_000_000;
    let pixels = w
        .checked_mul(h)
        .ok_or_else(|| anyhow::anyhow!("OCR image dimensions overflow"))?;
    let expected = pixels
        .checked_mul(bpp)
        .ok_or_else(|| anyhow::anyhow!("OCR image byte length overflow"))?;
    if data.len() != expected {
        anyhow::bail!(
            "invalid OCR image buffer: expected {expected} bytes, got {}",
            data.len()
        );
    }
    if pixels > MAX_PIXELS {
        let scale = (MAX_PIXELS as f64 / pixels as f64).sqrt();
        let nw = ((w as f64 * scale).round() as usize).max(1);
        let nh = ((h as f64 * scale).round() as usize).max(1);
        let resized = resize_cpu(data, w, h, nw, nh, bpp);
        let bpl = nw * bpp;
        let tess = tess
            .set_frame(&resized, nw as i32, nh as i32, bpp as i32, bpl as i32)?
            .set_source_resolution(300);
        let sx = w as f32 / nw as f32;
        let sy = h as f32 / nh as f32;
        Ok((tess, nw, nh, sx, sy))
    } else {
        let bpl = w * bpp;
        let tess = tess
            .set_frame(data, w as i32, h as i32, bpp as i32, bpl as i32)?
            .set_source_resolution(300);
        Ok((tess, w, h, 1.0, 1.0))
    }
}

/// Gated to the backends that call it; see `raw_image_bpp`.
#[cfg(any(
    target_os = "windows",
    feature = "paddle-ocr",
    all(
        any(target_os = "linux", target_os = "macos"),
        feature = "tesseract-ocr"
    ),
))]
pub(crate) fn resize_cpu(
    data: &[u8],
    sw: usize,
    sh: usize,
    dw: usize,
    dh: usize,
    bytes_per_pixel: usize,
) -> Vec<u8> {
    let mut out = vec![255u8; dw * dh * bytes_per_pixel];
    let x_ratio = sw as f32 / dw as f32;
    let y_ratio = sh as f32 / dh as f32;
    for dy in 0..dh {
        for dx in 0..dw {
            let sx = (dx as f32 * x_ratio) as usize;
            let sy = (dy as f32 * y_ratio) as usize;
            let src = (sy.min(sh - 1) * sw + sx.min(sw - 1)) * bytes_per_pixel;
            let dst = (dy * dw + dx) * bytes_per_pixel;
            out[dst..dst + bytes_per_pixel].copy_from_slice(&data[src..src + bytes_per_pixel]);
        }
    }
    out
}

#[cfg(all(
    any(target_os = "linux", target_os = "macos"),
    feature = "tesseract-ocr"
))]
fn find_tessdata_dir(lang: &str) -> Option<String> {
    use std::path::PathBuf;
    let filename = format!("{}.traineddata", lang);
    let dirs = [
        std::env::var("TESSDATA_PREFIX").ok().map(PathBuf::from),
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("tessdata"))),
        Some(PathBuf::from("/usr/share/tesseract-ocr/tessdata")),
        Some(PathBuf::from("/usr/share/tessdata")),
        Some(PathBuf::from("/usr/local/share/tessdata")),
        Some(PathBuf::from("/usr/share/lege/tessdata")),
    ];
    for dir_opt in &dirs {
        if let Some(dir) = dir_opt {
            if dir.join(&filename).is_file() {
                return Some(dir.to_string_lossy().into_owned());
            }
        }
    }
    None
}

// ═════════════════════════════════════════════════════════════════════════════
// Windows OCR engine (WinRT)
// ═════════════════════════════════════════════════════════════════════════════

#[cfg(all(
    any(target_os = "linux", target_os = "macos"),
    feature = "tesseract-ocr"
))]
fn scale_hocr_coords(hocr: &str, sw: usize, sh: usize, ow: usize, oh: usize) -> String {
    let sx = ow as f32 / sw as f32;
    let sy = oh as f32 / sh as f32;
    let mut out = String::with_capacity(hocr.len());
    let mut chars = hocr.chars();
    while let Some(ch) = chars.next() {
        if ch == 'b' {
            let lookahead: String = std::iter::once(ch)
                .chain(chars.as_str().chars().take(4))
                .collect();
            if lookahead == "bbox " {
                out.push_str("bbox ");
                for _ in 0..4 {
                    chars.next();
                }
                let remaining = chars.as_str();
                if let Some(end) = remaining.find('"') {
                    let coords: Vec<&str> = remaining[..end].trim().split_whitespace().collect();
                    if coords.len() >= 4 {
                        if let (Ok(x1), Ok(y1), Ok(x2), Ok(y2)) = (
                            coords[0].parse::<f32>(),
                            coords[1].parse::<f32>(),
                            coords[2].parse::<f32>(),
                            coords[3].parse::<f32>(),
                        ) {
                            out.push_str(&format!(
                                "{} {} {} {}",
                                (x1 * sx) as i32,
                                (y1 * sy) as i32,
                                (x2 * sx) as i32,
                                (y2 * sy) as i32
                            ));
                        } else {
                            out.push_str(&remaining[..end]);
                        }
                    } else {
                        out.push_str(&remaining[..end]);
                    }
                    for _ in 0..end {
                        chars.next();
                    }
                    continue;
                }
            }
        }
        out.push(ch);
    }
    out
}

// ═════════════════════════════════════════════════════════════════════════════
// Windows OCR engine (WinRT)
// ═════════════════════════════════════════════════════════════════════════════

#[cfg(target_os = "windows")]
pub struct WinOcrEngine;

#[cfg(target_os = "windows")]
impl OcrEngine for WinOcrEngine {
    fn name(&self) -> &'static str {
        "winocr"
    }

    fn run_image(
        &self,
        data: &[u8],
        width: usize,
        height: usize,
        is_binary: bool,
        lang: &str,
    ) -> Option<OcrResult> {
        let _com = initialize_com().ok()?;
        run_winocr_impl(data, width, height, is_binary, lang)
    }

    fn ocr_line(&self, image: &GrayImage, lang: &str) -> Result<OcrLineResult> {
        let w = image.width() as usize;
        let h = image.height() as usize;
        let data = image.as_raw();
        let bbox = [0u32, 0, image.width(), image.height()];

        if w == 0 || h == 0 {
            return Ok(OcrLineResult {
                text: String::new(),
                confidence: None,
                words: Vec::new(),
                bbox_highres: bbox,
            });
        }

        match run_winocr_lines_inner(data, w, h, lang) {
            Some(lines) => {
                let mut text = String::new();
                let mut words = Vec::new();
                for line in lines {
                    if !text.is_empty() {
                        text.push(' ');
                    }
                    text.push_str(line.text.trim());
                    for mut word in line.words {
                        word.bbox_crop_local = [
                            word.bbox_crop_local[0] + line.bbox_highres[0],
                            word.bbox_crop_local[1] + line.bbox_highres[1],
                            word.bbox_crop_local[2] + line.bbox_highres[0],
                            word.bbox_crop_local[3] + line.bbox_highres[1],
                        ];
                        words.push(word);
                    }
                }
                Ok(OcrLineResult {
                    text,
                    confidence: None,
                    words,
                    bbox_highres: bbox,
                })
            }
            None => Err(anyhow::anyhow!("Windows OCR failed to recognize a line")),
        }
    }

    fn ocr_region(&self, image: &GrayImage, lang: &str) -> Result<Vec<OcrLineResult>> {
        run_winocr_lines_inner(
            image.as_raw(),
            image.width() as usize,
            image.height() as usize,
            lang,
        )
        .ok_or_else(|| anyhow::anyhow!("Windows OCR failed to recognize a region"))
    }
}

/// Inner WinRT call returning one OCR result per detected line. All line bboxes
/// are relative to the supplied image and all word bboxes are relative to their
/// containing line.
#[cfg(target_os = "windows")]
fn run_winocr_lines_inner(
    data: &[u8],
    width: usize,
    height: usize,
    language: &str,
) -> Option<Vec<OcrLineResult>> {
    use windows::Graphics::Imaging::{BitmapBufferAccessMode, BitmapPixelFormat, SoftwareBitmap};
    use windows::Win32::System::WinRT::IMemoryBufferByteAccess;
    use windows_core::Interface;

    if raw_image_bpp(data, width, height, false)? != 1 {
        return None;
    }
    if data.iter().all(|&byte| byte == data[0]) {
        return Some(Vec::new());
    }

    // WinRT requires a four-byte BGRA staging image. Bound the grayscale input
    // first so a page-sized fallback does not transiently allocate four times
    // an arbitrarily large region.
    const MAX_PIXELS: usize = 1_500_000;
    let pixels = width.checked_mul(height)?;
    let resized;
    let (data, final_width, final_height) = if pixels > MAX_PIXELS {
        let scale = (MAX_PIXELS as f64 / pixels as f64).sqrt();
        let resized_width = ((width as f64 * scale).round() as usize).max(1);
        let resized_height = ((height as f64 * scale).round() as usize).max(1);
        resized = resize_cpu(data, width, height, resized_width, resized_height, 1);
        (resized.as_slice(), resized_width, resized_height)
    } else {
        (data, width, height)
    };

    let _com = initialize_com().ok()?;
    // Convert grayscale → BGRA
    let bgra: Vec<u8> = data.iter().flat_map(|&p| [p, p, p, 255u8]).collect();
    let w = final_width as i32;
    let h = final_height as i32;

    let bitmap = SoftwareBitmap::Create(BitmapPixelFormat::Bgra8, w, h).ok()?;
    {
        let buf = bitmap.LockBuffer(BitmapBufferAccessMode::Write).ok()?;
        let buf_ref = buf.CreateReference().ok()?;
        unsafe {
            let byte_access = Interface::cast::<IMemoryBufferByteAccess>(&buf_ref).ok()?;
            let mut ptr: *mut u8 = std::ptr::null_mut();
            let mut cap: u32 = 0;
            byte_access.GetBuffer(&mut ptr, &mut cap).ok()?;
            if ptr.is_null() || cap < bgra.len() as u32 {
                return None;
            }
            std::ptr::copy_nonoverlapping(bgra.as_ptr(), ptr, bgra.len());
        }
    }

    let engine = create_ocr_engine(language)?;
    let result = engine.RecognizeAsync(&bitmap).ok()?.join().ok()?;

    let lines = result.Lines().ok()?;
    let mut results = Vec::new();

    for line in lines.into_iter() {
        let wds = match line.Words() {
            Ok(w) => w,
            Err(_) => continue,
        };
        let mut line_words = Vec::new();
        let mut line_text = String::new();
        let mut lx1 = final_width as u32;
        let mut ly1 = final_height as u32;
        let mut lx2 = 0u32;
        let mut ly2 = 0u32;

        for word in wds.into_iter() {
            let text = match word.Text() {
                Ok(t) => t.to_string(),
                Err(_) => continue,
            };
            let rect = match word.BoundingRect() {
                Ok(r) if r.Width > 0.0 && r.Height > 0.0 => r,
                _ => continue,
            };
            if text.trim().is_empty() {
                continue;
            }
            let wx1 = rect.X.floor().max(0.0).min(final_width as f32) as u32;
            let wy1 = rect.Y.floor().max(0.0).min(final_height as f32) as u32;
            let wx2 = (rect.X + rect.Width)
                .ceil()
                .max(0.0)
                .min(final_width as f32) as u32;
            let wy2 = (rect.Y + rect.Height)
                .ceil()
                .max(0.0)
                .min(final_height as f32) as u32;
            if wx2 <= wx1 || wy2 <= wy1 {
                continue;
            }
            lx1 = lx1.min(wx1);
            ly1 = ly1.min(wy1);
            lx2 = lx2.max(wx2);
            ly2 = ly2.max(wy2);
            if !line_text.is_empty() {
                line_text.push(' ');
            }
            line_text.push_str(text.trim());
            line_words.push(OcrWord {
                text,
                bbox_crop_local: [wx1, wy1, wx2, wy2],
                confidence: None,
            });
        }

        if line_words.is_empty() || lx2 <= lx1 || ly2 <= ly1 {
            continue;
        }
        for word in &mut line_words {
            word.bbox_crop_local = [
                word.bbox_crop_local[0].saturating_sub(lx1),
                word.bbox_crop_local[1].saturating_sub(ly1),
                word.bbox_crop_local[2].saturating_sub(lx1),
                word.bbox_crop_local[3].saturating_sub(ly1),
            ];
        }
        results.push(OcrLineResult {
            text: line_text,
            confidence: None,
            words: line_words,
            bbox_highres: [lx1, ly1, lx2, ly2],
        });
    }

    if final_width != width || final_height != height {
        crate::coordinate::scale_lines(
            &mut results,
            width as f32 / final_width as f32,
            height as f32 / final_height as f32,
            width as u32,
            height as u32,
        );
    }
    Some(results)
}

/// Full WinRT OCR: converts any raw image to BGRA, runs OcrEngine, builds hOCR + plain text.
/// Mirrors the logic in the original src/ocr/winocr.rs `run_winocr_impl`.
#[cfg(target_os = "windows")]
fn run_winocr_impl(
    data: &[u8],
    width: usize,
    height: usize,
    is_binary: bool,
    language: &str,
) -> Option<OcrResult> {
    use windows::Graphics::Imaging::{BitmapBufferAccessMode, BitmapPixelFormat, SoftwareBitmap};
    use windows::Win32::System::WinRT::IMemoryBufferByteAccess;
    use windows_core::Interface;

    const MAX_PIXELS: usize = 1_500_000;
    let source_bpp = raw_image_bpp(data, width, height, is_binary)?;
    let pixels = width.checked_mul(height)?;
    let (processed, final_w, final_h): (Vec<u8>, usize, usize) = if pixels > MAX_PIXELS {
        let scale = (MAX_PIXELS as f64 / pixels as f64).sqrt();
        let nw = ((width as f64 * scale) as usize).max(1);
        let nh = ((height as f64 * scale) as usize).max(1);
        let resized = resize_cpu(data, width, height, nw, nh, source_bpp);
        (resized, nw, nh)
    } else {
        (data.to_vec(), width, height)
    };

    // Convert to BGRA
    let bgra: Vec<u8> = if is_binary {
        processed
            .iter()
            .flat_map(|&p| {
                let v = if p == 0 { 0u8 } else { 255u8 };
                [v, v, v, 255u8]
            })
            .collect()
    } else if processed.len() == final_w * final_h {
        // grayscale
        processed.iter().flat_map(|&p| [p, p, p, 255u8]).collect()
    } else {
        // RGB
        processed
            .chunks_exact(3)
            .flat_map(|c| [c[2], c[1], c[0], 255u8])
            .collect()
    };

    let bitmap =
        SoftwareBitmap::Create(BitmapPixelFormat::Bgra8, final_w as i32, final_h as i32).ok()?;
    {
        let buf = bitmap.LockBuffer(BitmapBufferAccessMode::Write).ok()?;
        let buf_ref = buf.CreateReference().ok()?;
        unsafe {
            let byte_access = Interface::cast::<IMemoryBufferByteAccess>(&buf_ref).ok()?;
            let mut ptr: *mut u8 = std::ptr::null_mut();
            let mut cap: u32 = 0;
            byte_access.GetBuffer(&mut ptr, &mut cap).ok()?;
            if ptr.is_null() || cap < bgra.len() as u32 {
                return None;
            }
            std::ptr::copy_nonoverlapping(bgra.as_ptr(), ptr, bgra.len());
        }
    }

    let engine = create_ocr_engine(language)?;
    let ocr_result = engine.RecognizeAsync(&bitmap).ok()?.join().ok()?;
    let lines = ocr_result.Lines().ok()?;
    let x_scale = width as f32 / final_w as f32;
    let y_scale = height as f32 / final_h as f32;

    let mut plain = String::new();
    let mut hocr = format!(
        r#"<html><head></head><body><div class="ocr_page" title="bbox 0 0 {width} {height}"><div class="ocr_carea" title="bbox 0 0 {width} {height}">"#
    );

    for line in lines.into_iter() {
        let words = match line.Words() {
            Ok(w) => w,
            Err(_) => continue,
        };
        if words.Size().unwrap_or(0) == 0 {
            continue;
        }

        hocr.push_str(r#"<p class="ocr_par">"#);
        let mut lx1 = i32::MAX;
        let mut ly1 = i32::MAX;
        let mut lx2 = i32::MIN;
        let mut ly2 = i32::MIN;

        for word in words.into_iter() {
            let text = match word.Text() {
                Ok(t) => t.to_string(),
                Err(_) => continue,
            };
            let rect = match word.BoundingRect() {
                Ok(r)
                    if r.Width > 0.0
                        && r.Height > 0.0
                        && r.X >= 0.0
                        && r.Y >= 0.0
                        && r.X + r.Width <= final_w as f32
                        && r.Y + r.Height <= final_h as f32 =>
                {
                    r
                }
                _ => continue,
            };
            if text.trim().is_empty() {
                continue;
            }
            plain.push_str(&text);
            plain.push(' ');

            let wx1 = (rect.X * x_scale) as i32;
            let wy1 = (rect.Y * y_scale) as i32;
            let wx2 = ((rect.X + rect.Width) * x_scale) as i32;
            let wy2 = ((rect.Y + rect.Height) * y_scale) as i32;
            lx1 = lx1.min(wx1);
            ly1 = ly1.min(wy1);
            lx2 = lx2.max(wx2);
            ly2 = ly2.max(wy2);

            hocr.push_str(&format!(
                r#"<span class="ocrx_word" title="bbox {wx1} {wy1} {wx2} {wy2}">{}</span> "#,
                crate::hocr::html_escape(&text)
            ));
        }
        if lx1 != i32::MAX {
            let line_text = line.Text().map(|t| t.to_string()).unwrap_or_default();
            if !line_text.trim().is_empty() {
                hocr.push_str(&format!(
                    r#"<span class="ocr_line" title="bbox {lx1} {ly1} {lx2} {ly2}">{}</span>"#,
                    crate::hocr::html_escape(&line_text)
                ));
            }
        }
        hocr.push_str("</p>\n");
        if !plain.ends_with('\n') {
            plain.push('\n');
        }
    }
    hocr.push_str("</div></div></body></html>");

    Some(OcrResult {
        hocr,
        plain_text: plain.trim().to_string(),
    })
}

#[cfg(target_os = "windows")]
struct ComGuard;

#[cfg(target_os = "windows")]
impl Drop for ComGuard {
    fn drop(&mut self) {
        unsafe {
            windows::Win32::System::Com::CoUninitialize();
        }
    }
}

#[cfg(target_os = "windows")]
fn initialize_com() -> windows::core::Result<ComGuard> {
    unsafe {
        windows::Win32::System::Com::CoInitializeEx(
            None,
            windows::Win32::System::Com::COINIT_APARTMENTTHREADED
                | windows::Win32::System::Com::COINIT_DISABLE_OLE1DDE,
        )
        .ok()?;
    }
    Ok(ComGuard)
}

#[cfg(target_os = "windows")]
fn create_ocr_engine(language: &str) -> Option<windows::Media::Ocr::OcrEngine> {
    use windows::Globalization::Language;
    use windows::Media::Ocr::OcrEngine;
    let normalized = match language.to_ascii_lowercase().as_str() {
        "eng" => "en",
        "deu" | "ger" => "de",
        "fra" | "fre" => "fr",
        "spa" => "es",
        "ita" => "it",
        "por" => "pt",
        "nld" | "dut" => "nl",
        "chi_sim" => "zh-Hans",
        "chi_tra" => "zh-Hant",
        _ => language,
    };
    Language::CreateLanguage(&windows_core::HSTRING::from(normalized))
        .ok()
        .and_then(|lang| OcrEngine::TryCreateFromLanguage(&lang).ok())
}

// ═════════════════════════════════════════════════════════════════════════════
// TrOCR stub (feature = "trocr")
// ═════════════════════════════════════════════════════════════════════════════

#[cfg(feature = "trocr")]
pub struct TrOcrEngine;

#[cfg(feature = "trocr")]
impl OcrEngine for TrOcrEngine {
    fn name(&self) -> &'static str {
        "trocr"
    }
    fn run_image(
        &self,
        _data: &[u8],
        _width: usize,
        _height: usize,
        _is_binary: bool,
        _lang: &str,
    ) -> Option<OcrResult> {
        None
    }
    fn ocr_line(&self, _image: &GrayImage, _lang: &str) -> Result<OcrLineResult> {
        anyhow::bail!("TrOCR is not yet implemented")
    }
    fn ocr_region(&self, _image: &GrayImage, _lang: &str) -> Result<Vec<OcrLineResult>> {
        anyhow::bail!("TrOCR is not yet implemented")
    }
}

// ═════════════════════════════════════════════════════════════════════════════
// HOCR parsing utilities (shared across platforms)
// ═════════════════════════════════════════════════════════════════════════════

/// Parse `<span class="ocrx_word" title="bbox x1 y1 x2 y2">text</span>` entries
/// from a Tesseract HOCR string.
pub fn parse_hocr_words(hocr: &str, scale_x: f32, scale_y: f32) -> Vec<OcrWord> {
    let mut words = Vec::new();
    let mut rest = hocr;

    while let Some(pos) = rest.find("ocrx_word") {
        rest = &rest[pos + "ocrx_word".len()..];

        // Extract bbox from title attribute
        let bbox_opt = extract_bbox_from_title(rest);
        // Extract text between > and </span>
        let text_opt = extract_span_text(rest);

        if let (Some(raw_bbox), Some(text)) = (bbox_opt, text_opt)
            && !text.trim().is_empty()
            && let Some(bbox) = scale_hocr_bbox(raw_bbox, scale_x, scale_y, None)
        {
            words.push(OcrWord {
                text,
                bbox_crop_local: bbox,
                confidence: extract_word_confidence(rest),
            });
        }
    }
    words
}

/// Parse Tesseract HOCR into per-line `OcrLineResult` entries.
pub fn parse_hocr_lines(
    hocr: &str,
    image_w: u32,
    image_h: u32,
    scale_x: f32,
    scale_y: f32,
) -> Vec<OcrLineResult> {
    let mut lines = Vec::new();
    let mut rest = hocr;

    while let Some(pos) = rest.find("ocr_line") {
        rest = &rest[pos + "ocr_line".len()..];
        let bbox_opt = extract_bbox_from_title(rest);
        // Collect all words until </span> for this line
        let (mut words, line_text) = collect_words_in_line(rest, scale_x, scale_y);
        if !line_text.trim().is_empty() {
            let Some(bbox) = bbox_opt
                .and_then(|bbox| scale_hocr_bbox(bbox, scale_x, scale_y, Some((image_w, image_h))))
            else {
                continue;
            };
            let line_w = bbox[2].saturating_sub(bbox[0]);
            let line_h = bbox[3].saturating_sub(bbox[1]);
            for word in &mut words {
                word.bbox_crop_local = [
                    word.bbox_crop_local[0].saturating_sub(bbox[0]).min(line_w),
                    word.bbox_crop_local[1].saturating_sub(bbox[1]).min(line_h),
                    word.bbox_crop_local[2].saturating_sub(bbox[0]).min(line_w),
                    word.bbox_crop_local[3].saturating_sub(bbox[1]).min(line_h),
                ];
            }
            let confidence = mean_word_confidence(&words);
            lines.push(OcrLineResult {
                text: line_text.trim().to_string(),
                confidence,
                words,
                bbox_highres: bbox,
            });
        }
    }
    lines
}

fn collect_words_in_line(hocr: &str, scale_x: f32, scale_y: f32) -> (Vec<OcrWord>, String) {
    let mut words = Vec::new();
    let mut text = String::new();
    let mut rest = hocr;
    // Simple: collect ocrx_word entries until we hit the next ocr_line or end
    while let Some(pos) = rest.find("ocrx_word") {
        // Stop if there's a new line before this word
        if let Some(lp) = rest.find("ocr_line")
            && lp < pos
        {
            break;
        }
        rest = &rest[pos + "ocrx_word".len()..];
        if let (Some(raw_bbox), Some(word_text)) =
            (extract_bbox_from_title(rest), extract_span_text(rest))
            && !word_text.trim().is_empty()
            && let Some(bbox) = scale_hocr_bbox(raw_bbox, scale_x, scale_y, None)
        {
            text.push_str(&word_text);
            text.push(' ');
            words.push(OcrWord {
                text: word_text,
                bbox_crop_local: bbox,
                confidence: extract_word_confidence(rest),
            });
        }
    }
    (words, text)
}

fn extract_bbox_from_title(s: &str) -> Option<[i32; 4]> {
    // Bound the search to this element's opening tag. Otherwise a line with no
    // bbox can accidentally inherit the first descendant word's coordinates.
    let tag = &s[..s.find('>')?];
    let title_pos = tag.find("title=")?;
    let after = &tag[title_pos + 6..];
    let bbox_pos = after.find("bbox ")?;
    let coords_str = &after[bbox_pos + 5..];
    // Read up to 4 integers
    let mut nums = [0i32; 4];
    let mut found = 0;
    let mut rest = coords_str.trim_start();
    while found < 4 {
        let end = rest
            .find(|c: char| !c.is_ascii_digit() && c != '-')
            .unwrap_or(rest.len());
        if end == 0 {
            break;
        }
        if let Ok(n) = rest[..end].parse::<i32>() {
            nums[found] = n;
            found += 1;
        }
        rest = rest[end..].trim_start();
    }
    if found == 4 { Some(nums) } else { None }
}

fn scale_hocr_bbox(
    [x1, y1, x2, y2]: [i32; 4],
    scale_x: f32,
    scale_y: f32,
    bounds: Option<(u32, u32)>,
) -> Option<[u32; 4]> {
    if !scale_x.is_finite() || !scale_y.is_finite() || scale_x < 0.0 || scale_y < 0.0 {
        return None;
    }
    let max_w = bounds.map_or(u32::MAX, |(width, _)| width);
    let max_h = bounds.map_or(u32::MAX, |(_, height)| height);
    let scale = |value: i32, factor: f32, max: u32| {
        (value as f64 * factor as f64)
            .round()
            .clamp(0.0, max as f64) as u32
    };
    let bbox = [
        scale(x1, scale_x, max_w),
        scale(y1, scale_y, max_h),
        scale(x2, scale_x, max_w),
        scale(y2, scale_y, max_h),
    ];
    (bbox[2] > bbox[0] && bbox[3] > bbox[1]).then_some(bbox)
}

fn extract_word_confidence(s: &str) -> Option<f32> {
    let tag = &s[..s.find('>')?];
    let pos = tag.find("x_wconf")?;
    let value = tag[pos + "x_wconf".len()..]
        .trim_start()
        .split(|c: char| !c.is_ascii_digit() && c != '.')
        .next()?
        .parse::<f32>()
        .ok()?;
    Some((value / 100.0).clamp(0.0, 1.0))
}

fn mean_word_confidence(words: &[OcrWord]) -> Option<f32> {
    let mut sum = 0.0;
    let mut count = 0usize;
    for confidence in words.iter().filter_map(|word| word.confidence) {
        sum += confidence;
        count += 1;
    }
    (count > 0).then_some(sum / count as f32)
}

fn extract_span_text(s: &str) -> Option<String> {
    let gt = s.find('>')?;
    let after = &s[gt + 1..];
    let close = after.find("</span>")?;
    let raw = &after[..close];
    Some(crate::hocr::html_unescape(raw).into_owned())
}

#[cfg(test)]
mod line_parser_tests {
    use super::{parse_hocr_lines, parse_hocr_words, raw_image_bpp};

    #[test]
    fn region_hocr_words_are_made_line_local() {
        let hocr = r#"
            <span class="ocr_line" title="bbox 10 20 100 40">
              <span class="ocrx_word" title="bbox 12 22 30 38">word</span>
            </span>
        "#;
        let lines = parse_hocr_lines(hocr, 100, 100, 1.0, 1.0);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].bbox_highres, [10, 20, 100, 40]);
        assert_eq!(lines[0].words[0].bbox_crop_local, [2, 2, 20, 18]);
    }

    #[test]
    fn line_without_bbox_does_not_inherit_word_bbox() {
        let hocr = r#"
            <span class="ocr_line">
              <span class="ocrx_word" title="bbox 12 22 30 38">word</span>
            </span>
        "#;
        assert!(parse_hocr_lines(hocr, 100, 100, 1.0, 1.0).is_empty());
    }

    #[test]
    fn parser_rejects_invalid_geometry_and_decodes_numeric_entities() {
        let hocr = r#"
            <span class="ocrx_word" title="bbox 30 20 10 40">bad</span>
            <span class="ocrx_word" title="bbox 1 2 10 20">A&#x2014;B</span>
        "#;
        let words = parse_hocr_words(hocr, 1.0, 1.0);
        assert_eq!(words.len(), 1);
        assert_eq!(words[0].text, "A—B");
    }

    #[test]
    fn raw_geometry_validation_uses_checked_products() {
        assert_eq!(raw_image_bpp(&[0; 12], 2, 2, false), Some(3));
        assert_eq!(raw_image_bpp(&[0; 11], 2, 2, false), None);
        assert_eq!(raw_image_bpp(&[], usize::MAX, 2, false), None);
    }

    #[cfg(feature = "trocr")]
    #[test]
    fn placeholder_trocr_engine_fails_without_panicking() {
        use super::{OcrEngine, TrOcrEngine};

        let engine = TrOcrEngine;
        assert!(engine.run_image(&[], 0, 0, false, "eng").is_none());
        assert_eq!(engine.name(), "trocr");
    }
}
