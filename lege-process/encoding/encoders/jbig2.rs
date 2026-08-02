use crate::encoding::streamline::{Jbig2Mode, Jbig2Settings};
use crate::encoding::{EncodingError, Result};
use jbig2enc_rust::{
    Jbig2Config, Jbig2Context, Jbig2EncodeResult, encode_single_image_lossless,
    encode_single_image_with_config, jbig2halftone::encode_halftone_pdf_split_auto,
    jbig2structs::GenericRegionParams, jbig2sym::binary_pixels_to_bitimage,
};

/// Normalize to one byte per pixel for `jbig2enc_rust::binary_pixels_to_bitimage`:
/// **0 = white (background), non-zero = black (foreground)**.
///
/// Accepts:
/// - channels=0: logical buffer where 0 is background and non-zero is ink
/// - channels=1: grayscale/bilevel luma where dark values are ink
fn normalize_binary_data(input: &[u8], width: u32, height: u32, channels: u8) -> Result<Vec<u8>> {
    if width == 0 || height == 0 {
        return Err(EncodingError::InvalidDimensions {
            format: "JBIG2",
            width,
            height,
        });
    }
    let expected_len = (width as usize)
        .checked_mul(height as usize)
        .ok_or_else(|| EncodingError::InvalidInput("Dimensions too large".to_string()))?;

    if input.len() < expected_len {
        return Err(EncodingError::InputBufferTooSmall);
    }

    let slice = &input[..expected_len];

    match channels {
        // Logical data is already in jbig2enc-rust's convention.
        0 => Ok(slice.iter().map(|&value| u8::from(value != 0)).collect()),
        // Pipeline bilevel pages use the image convention (0=black, 255=white).
        // Never infer the convention from the values: an all-black page is a
        // valid all-zero grayscale buffer and must not turn into an all-white
        // JBIG2 page.
        1 => Ok(slice.iter().map(|&value| u8::from(value <= 128)).collect()),
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
    let line = format!("[{}] {}\n", chrono_simple_now(), msg);
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
        "encode start: {}x{} ch={} mode={:?} backend={} norm_len={} norm_nz_in_first{}={} raw_in_min={} raw_in_max={}",
        width,
        height,
        channels,
        settings.mode,
        crate::encoding::active_jbig2_backend_info(),
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
        let bitimage =
            binary_pixels_to_bitimage(normalized.as_slice(), width as usize, height as usize)
                .map_err(|m| EncodingError::EncoderError(m))?;
        let mut jcfg = Jbig2Config::default();
        jcfg.want_full_headers = false;
        let dpi = jcfg.dpi;
        let region = GenericRegionParams::new(width, height, dpi);
        let (global_data, page_data) =
            encode_halftone_pdf_split_auto(&bitimage, &jcfg, &region, 1, Some(1))
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

#[cfg(test)]
mod tests {
    use super::normalize_binary_data;
    use crate::encoding::EncodingError;

    #[test]
    fn all_black_grayscale_is_not_misread_as_logical_background() {
        let normalized = normalize_binary_data(&[0, 0, 0, 0], 2, 2, 1).expect("normalize");
        assert_eq!(normalized, vec![1, 1, 1, 1]);
    }

    #[test]
    fn logical_binary_input_keeps_zero_as_background() {
        let normalized = normalize_binary_data(&[0, 1, 255, 0], 2, 2, 0).expect("normalize");
        assert_eq!(normalized, vec![0, 1, 1, 0]);
    }

    #[test]
    fn zero_sized_page_is_rejected() {
        assert!(matches!(
            normalize_binary_data(&[], 0, 1, 1),
            Err(EncodingError::InvalidDimensions { .. })
        ));
    }
}
