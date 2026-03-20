use crate::streamline::{Jbig2Mode, Jbig2Settings};
use crate::{EncodingError, Result};
use jbig2enc_rust::{
    Jbig2Config, Jbig2Context, Jbig2EncodeResult, encode_single_image_lossless,
    encode_single_image_with_config,
    jbig2halftone::encode_halftone_pdf_split_auto,
    jbig2structs::GenericRegionParams,
    jbig2sym::binary_pixels_to_bitimage,
};

/// Normalize to one byte per pixel for `jbig2enc_rust::binary_pixels_to_bitimage`:
/// **0 = white (background), non-zero = black (foreground)**.
///
/// Accepts:
/// - channels=0: packed logical buffer (same rules as grayscale below)
/// - channels=1: grayscale 0–255 or already 0/1
fn normalize_binary_data(
    input: &[u8],
    width: u32,
    height: u32,
    channels: u8,
) -> Result<Vec<u8>> {
    let expected_len = (width as usize)
        .checked_mul(height as usize)
        .ok_or_else(|| EncodingError::InvalidInput("Dimensions too large".to_string()))?;

    if input.len() < expected_len {
        return Err(EncodingError::InputBufferTooSmall);
    }

    let slice = &input[..expected_len];

    /// Map buffers to JBIG2 convention: **0 = background (white), non-zero = foreground (ink)**.
    ///
    /// `jbig2enc_rust::binary_pixels_to_bitimage` treats **non-zero** as **set** (ink).
    ///
    /// **Default adaptive binarization** (`improved_binarize` / `sauvola_via_integral`): dark text is
    /// **0**, paper is **255** (`px > thr → 255` marks bright pixels). Same convention as
    /// `encoders/ccitt4` (`is_black = pixel <= 128`).
    ///
    /// **Heavy ONNX** path is normalized to that convention in `apply_heavy_duty_binarization_raw`
    /// (invert after the model) so all encoders agree.
    fn from_grayscale_like(slice: &[u8]) -> Vec<u8> {
        let already_binary = slice.iter().all(|&b| b <= 1);
        if already_binary {
            // 0=background, 1=foreground (ink)
            return slice.to_vec();
        }
        // Match CCITT: paper (high) -> 0 jbig2 white, ink (low) -> 1 jbig2 black
        slice
            .iter()
            .map(|&b| if b > 128 { 0 } else { 1 })
            .collect()
    }

    match channels {
        0 | 1 => Ok(from_grayscale_like(slice)),
        _ => Err(EncodingError::UnsupportedChannels {
            format: "JBIG2",
            channels,
        }),
    }
}

fn jbig2_debug_log_line(msg: &str) {
    if std::env::var("LEGE_JBIG2_DEBUG").ok().as_deref() != Some("1") {
        return;
    }
    let line = format!(
        "[{}] {}\n",
        chrono_simple_now(),
        msg
    );
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(jbig2_debug_log_path())
        .and_then(|mut f| std::io::Write::write_all(&mut f, line.as_bytes()));
}

fn jbig2_debug_log_path() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("LEGE_JBIG2_DEBUG_LOG") {
        if !p.is_empty() {
            return std::path::PathBuf::from(p);
        }
    }
    // Default: next to the running binary (reliable for GUI / `cd` anywhere; cwd is not).
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            return dir.join("jbig2-debug.log");
        }
    }
    std::env::current_dir()
        .unwrap_or_else(|_| std::path::PathBuf::from("."))
        .join("jbig2-debug.log")
}

fn chrono_simple_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}.{:03}", d.as_secs(), d.subsec_millis())
}

pub fn encode(
    input: &[u8],
    width: u32,
    height: u32,
    channels: u8,
    settings: &Jbig2Settings,
) -> Result<Jbig2EncodeResult> {
    // Normalize input data to 0/1 format required by jbig2enc_rust
    let normalized = normalize_binary_data(input, width, height, channels)?;

    let sample = normalized.len().min(4096);
    let nz: usize = normalized[..sample].iter().filter(|&&b| b != 0).count();
    let in_sample = input.len().min(4096);
    let (in_min, in_max) = if input.len() >= in_sample && in_sample > 0 {
        let s = &input[..in_sample];
        (*s.iter().min().unwrap_or(&0), *s.iter().max().unwrap_or(&0))
    } else {
        (0u8, 0u8)
    };
    jbig2_debug_log_line(&format!(
        "encode start: {}x{} ch={} mode={:?} norm_len={} norm_nz_in_first{}={} raw_in_min={} raw_in_max={}",
        width,
        height,
        channels,
        settings.mode,
        normalized.len(),
        sample,
        nz,
        in_min,
        in_max
    ));

    // Debug: Log input characteristics
    log::debug!(
        "JBIG2 encode: {}x{}, {} bytes, mode={:?}, halftone_segments={}",
        width,
        height,
        normalized.len(),
        settings.mode,
        settings.use_jbig2_halftone_segments,
    );

    // Halftone page encoding (jbig2halftone.rs): independent pipeline from symbol/generic encoders.
    if settings.use_jbig2_halftone_segments {
        if !settings.pdf_fragment_mode {
            return Err(EncodingError::EncoderError(
                "JBIG2 halftone segments require pdf_fragment_mode (PDF embedding)".to_string(),
            ));
        }
        let bitimage = binary_pixels_to_bitimage(normalized.as_slice(), width as usize, height as usize)
            .map_err(|m| EncodingError::EncoderError(m))?;
        let mut jcfg = Jbig2Config::default();
        jcfg.want_full_headers = false;
        let dpi = jcfg.dpi;
        let region = GenericRegionParams::new(width, height, dpi);
        let (global_data, page_data) = encode_halftone_pdf_split_auto(
            &bitimage,
            &jcfg,
            &region,
            1,
            Some(1),
        )
        .map_err(|e| EncodingError::EncoderError(e.to_string()))?;
        jbig2_debug_log_line(&format!(
            "encode (jbig2halftone): page_len={} global_len={}",
            page_data.len(),
            global_data.len()
        ));
        return Ok(Jbig2EncodeResult {
            global_data: Some(global_data),
            page_data,
        });
    }

    let out = match settings.mode {
        Jbig2Mode::Generic => encode_single_image_lossless(
            normalized.as_slice(),
            width,
            height,
            settings.pdf_fragment_mode,
        )
        .map_err(|e: jbig2enc_rust::Jbig2Error| EncodingError::EncoderError(e.to_string())),
        Jbig2Mode::Symbol => encode_single_image_with_config(
            normalized.as_slice(),
            width,
            height,
            Jbig2Context::with_config(Jbig2Config::text(), settings.pdf_fragment_mode),
        )
        .map_err(|e: jbig2enc_rust::Jbig2Error| {
            log::error!("JBIG2 encoding failed: {:?}", e);
            EncodingError::EncoderError(e.to_string())
        }),
        Jbig2Mode::SymUnify => encode_single_image_with_config(
            normalized.as_slice(),
            width,
            height,
            Jbig2Context::with_config(Jbig2Config::text_symbol_unify(), settings.pdf_fragment_mode),
        )
        .map_err(|e: jbig2enc_rust::Jbig2Error| {
            log::error!("JBIG2 encoding failed: {:?}", e);
            EncodingError::EncoderError(e.to_string())
        }),
    }?;

    let head: String = out
        .page_data
        .iter()
        .take(12)
        .map(|b| format!("{:02x}", b))
        .collect::<Vec<_>>()
        .join(" ");
    jbig2_debug_log_line(&format!(
        "encode done: page_len={} global_len={} page_head12={}",
        out.page_data.len(),
        out.global_data.as_ref().map(|g| g.len()).unwrap_or(0),
        head
    ));

    Ok(out)
}
