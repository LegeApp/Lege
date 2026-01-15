use crate::{streamline::Jp2Settings, EncodingError, Result};

#[cfg(feature = "debug-logging")]
use crate::streamline::log_debug_message;
use openjp2::{
    openjpeg::opj_set_default_encoder_parameters, opj_cparameters_t, opj_image,
    opj_image_comptparm, Codec, Stream, CODEC_FORMAT, OPJ_CLRSPC_GRAY, OPJ_CLRSPC_SRGB,
};

pub fn encode(
    input: &[u8],
    width: u32,
    height: u32,
    channels: u8,
    settings: &Jp2Settings,
) -> Result<Vec<u8>> {
    #[cfg(feature = "debug-logging")]
    log_debug_message(&format!(
        "JP2 encoding start: {}x{}, {} channels, input size: {} bytes",
        width,
        height,
        channels,
        input.len()
    ));
    if channels != 1 && channels != 3 {
        return Err(EncodingError::InvalidInput(
            "JP2 requires 1 (grayscale) or 3 (RGB) channels".to_string(),
        ));
    }
    let expected_len = width as usize * height as usize * channels as usize;
    if input.len() < expected_len {
        return Err(EncodingError::InvalidInput(
            "Input buffer too small for JP2 dimensions".to_string(),
        ));
    }

    let num_comps = channels as u32;
    let color_space = if channels == 1 {
        OPJ_CLRSPC_GRAY
    } else {
        OPJ_CLRSPC_SRGB
    };

    let mut cmptparms = Vec::with_capacity(num_comps as usize);
    for _ in 0..num_comps {
        cmptparms.push(opj_image_comptparm {
            dx: 1,
            dy: 1,
            w: width,
            h: height,
            x0: 0,
            y0: 0,
            prec: 8,
            bpp: 8,
            sgnd: 0,
        });
    }

    let mut jp2_image = opj_image::create(&cmptparms, color_space);
    jp2_image.x0 = 0;
    jp2_image.y0 = 0;
    jp2_image.x1 = width;
    jp2_image.y1 = height;

    #[cfg(feature = "debug-logging")]
    log_debug_message(&format!(
        "JP2 image object created: {}x{}, {} components, color_space: {}",
        width,
        height,
        num_comps,
        if channels == 1 { "GRAY" } else { "RGB" }
    ));

    if let Some(chans) = jp2_image.comps_data_mut_iter() {
        // Convert interleaved RGB data to component planes for OpenJP2
        for (c, buf) in chans.enumerate() {
            for (i, pix) in buf.iter_mut().enumerate() {
                let pixel_idx = if channels == 1 {
                    // For grayscale, we have only 1 component and 1 byte per pixel
                    // Each pixel position i corresponds to input[i]
                    i
                } else {
                    // For RGB, convert from interleaved (RGBRGBRGB...) to component planes
                    // Each pixel has 3 bytes: R, G, B at positions i*3, i*3+1, i*3+2
                    i * channels as usize + c
                };
                if pixel_idx < input.len() {
                    *pix = input[pixel_idx] as i32;
                } else {
                    *pix = 0; // Fill with black if we run out of data
                }
            }
        }
        #[cfg(feature = "debug-logging")]
        log_debug_message(&format!(
            "JP2 pixel data copied to {} components",
            num_comps
        ));
    } else {
        return Err(EncodingError::EncoderError(
            "Failed to access JP2 image component buffers".to_string(),
        ));
    }

    let mut params = opj_cparameters_t::default();
    unsafe {
        opj_set_default_encoder_parameters(&mut params);
    }

    // Configure layers and rates. Prefer multi-layer when settings.rates is provided.
    // If PSNR targets are provided, use fixed-quality mode. Otherwise, use rate-based.
    // Default progression order to LRCP if not specified.

    // Default: single layer
    params.tcp_numlayers = 1;

    if let Some(psnrs) = &settings.psnrs {
        let n = psnrs.len().min(params.tcp_distoratio.len());
        for (i, &q) in psnrs.iter().take(n).enumerate() {
            params.tcp_distoratio[i] = q;
        }
        params.tcp_numlayers = n as i32;
        params.cp_fixed_quality = 1; // use PSNR targets
        params.cp_disto_alloc = 0;
        params.irreversible = 1; // PSNR mode implies lossy here
    } else if let Some(rates) = &settings.rates {
        let n = rates.len().min(params.tcp_rates.len());
        for (i, &r) in rates.iter().take(n).enumerate() {
            params.tcp_rates[i] = r;
        }
        params.tcp_numlayers = n as i32;
        params.cp_disto_alloc = 1;
        params.cp_fixed_quality = 0;
        params.irreversible = 1; // multi-layer implies lossy here
    } else if let Some(r) = settings.rate {
        params.tcp_rates[0] = r.max(0.0);
        if r > 0.0 {
            params.cp_disto_alloc = 1;
            params.cp_fixed_quality = 0;
            params.irreversible = 1; // lossy
        } else {
            // r == 0 => lossless
            params.cp_disto_alloc = 0;
            params.cp_fixed_quality = 0;
            params.irreversible = 0; // reversible 5/3
        }
    } else {
        // default lossless if nothing specified
        params.tcp_rates[0] = 0.0;
        params.cp_disto_alloc = 0;
        params.cp_fixed_quality = 0;
        params.irreversible = 0;
    }

    if let Some(nr) = settings.num_resolutions {
        params.numresolution = nr;
    }
    if let Some(po) = settings.prog_order {
        params.prog_order = po;
    } else {
        // Default to LRCP progression
        params.prog_order = openjp2::openjpeg::OPJ_LRCP;
    }
    // Handle irreversible setting after rate logic (can override)
    if let Some(irr) = settings.irreversible {
        params.irreversible = if irr { 1 } else { 0 };
        if !irr && settings.rate.is_none() {
            params.tcp_rates[0] = 0.0;
            params.cp_disto_alloc = 0;
        }
    }

    // Optional: codeblock and tiling configuration
    if let Some((cbw, cbh)) = settings.codeblock {
        params.cblockw_init = cbw.max(4).min(1024);
        params.cblockh_init = cbh.max(4).min(1024);
    }
    if let Some((tw, th)) = settings.tile_size {
        params.tile_size_on = 1;
        params.cp_tx0 = 0;
        params.cp_ty0 = 0;
        params.cp_tdx = tw.max(64);
        params.cp_tdy = th.max(64);
    }

    #[cfg(feature = "debug-logging")]
    log_debug_message(&format!(
        "JP2 params configured: layers={}, rate={}, disto_alloc={}, fixed_quality={}, irreversible={}, num_resolutions={}",
        params.tcp_numlayers,
        params.tcp_rates[0],
        params.cp_disto_alloc,
        params.cp_fixed_quality,
        params.irreversible,
        params.numresolution
    ));

    // Create a temporary file with a unique name
    let temp_dir = std::env::temp_dir();
    let temp_file = temp_dir.join(format!(
        "jp2_temp_{}_{}.jp2",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    let temp_path = temp_file
        .to_str()
        .ok_or_else(|| EncodingError::IoError("Invalid temporary file path".to_string()))?;

    // Create parent directories if they don't exist
    if let Some(parent) = temp_file.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            EncodingError::IoError(format!("Failed to create temp directory: {}", e))
        })?;
    }

    // Initialize codec
    let codec_fmt = CODEC_FORMAT::OPJ_CODEC_JP2;
    let mut codec = Codec::new_encoder(codec_fmt)
        .ok_or_else(|| EncodingError::EncoderError("Failed to create JP2 encoder".to_string()))?;

    #[cfg(feature = "debug-logging")]
    log_debug_message("JP2 codec created successfully");

    let setup_result = codec.setup_encoder(&mut params, &mut jp2_image);
    #[cfg(feature = "debug-logging")]
    log_debug_message(&format!("JP2 setup_encoder result: {}", setup_result));

    if setup_result == 0 {
        return Err(EncodingError::EncoderError(
            "JP2 setup_encoder failed".to_string(),
        ));
    }

    // Create output file stream with error handling
    #[cfg(feature = "debug-logging")]
    log_debug_message(&format!("JP2 creating output stream at: {}", temp_path));

    let mut stream = match Stream::new_file(temp_path, 1 << 20, false) {
        Ok(s) => s,
        Err(e) => {
            return Err(EncodingError::IoError(format!(
                "Failed to create JP2 output stream at {}: {}",
                temp_path, e
            )))
        }
    };

    #[cfg(feature = "debug-logging")]
    log_debug_message("JP2 output stream created successfully");

    // Perform encoding
    #[cfg(feature = "debug-logging")]
    log_debug_message("JP2 starting compression process");

    let encode_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let start_ok = codec.start_compress(&mut jp2_image, &mut stream) != 0;
        let encode_ok = if start_ok {
            codec.encode(&mut stream) != 0
        } else {
            false
        };
        let end_ok = if encode_ok {
            codec.end_compress(&mut stream) != 0
        } else {
            false
        };
        (start_ok, encode_ok, end_ok)
    }));

    // Ensure resources are cleaned up
    drop(stream);
    drop(codec);

    // Check encoding result
    match encode_result {
        Ok((start_ok, encode_ok, end_ok)) => {
            #[cfg(feature = "debug-logging")]
            log_debug_message(&format!(
                "JP2 encoding phases: start_ok={}, encode_ok={}, end_ok={}",
                start_ok, encode_ok, end_ok
            ));

            if start_ok && encode_ok && end_ok {
                // Read the encoded data
                let output = std::fs::read(&temp_file).map_err(|e| {
                    EncodingError::IoError(format!(
                        "Failed to read JP2 output file ({}): {}",
                        temp_path, e
                    ))
                })?;

                #[cfg(feature = "debug-logging")]
                log_debug_message(&format!("JP2 output file size: {} bytes", output.len()));

                // Clean up temporary file
                if let Err(e) = std::fs::remove_file(&temp_file) {
                    #[cfg(feature = "debug-logging")]
                    log_debug_message(&format!(
                        "Warning: Failed to remove temporary file {}: {}",
                        temp_path, e
                    ));
                }

                if output.is_empty() {
                    #[cfg(feature = "debug-logging")]
                    log_debug_message("JP2 encoding produced empty output - ERROR");
                    return Err(EncodingError::EncoderError(
                        "JP2 encoding produced empty output".to_string(),
                    ));
                }

                #[cfg(feature = "debug-logging")]
                log_debug_message("JP2 encoding completed successfully");
                Ok(output)
            } else if !start_ok {
                let _ = std::fs::remove_file(&temp_file);
                #[cfg(feature = "debug-logging")]
                log_debug_message("JP2 start_compress failed");
                Err(EncodingError::EncoderError(
                    "JP2 start_compress failed".to_string(),
                ))
            } else if !encode_ok {
                let _ = std::fs::remove_file(&temp_file);
                #[cfg(feature = "debug-logging")]
                log_debug_message("JP2 encode failed");
                Err(EncodingError::EncoderError("JP2 encode failed".to_string()))
            } else {
                let _ = std::fs::remove_file(&temp_file);
                #[cfg(feature = "debug-logging")]
                log_debug_message("JP2 end_compress failed");
                Err(EncodingError::EncoderError(
                    "JP2 end_compress failed".to_string(),
                ))
            }
        }
        Err(_) => {
            let _ = std::fs::remove_file(&temp_file);
            #[cfg(feature = "debug-logging")]
            log_debug_message("JP2 encoding panicked");
            Err(EncodingError::EncoderError(
                "JP2 encoding panicked".to_string(),
            ))
        }
    }
}

/// Encode JP2 aiming for a target file size by iteratively solving for `rate`.
/// Returns the chosen rate and the encoded bytes.
pub fn encode_to_target_size(
    input: &[u8],
    width: u32,
    height: u32,
    channels: u8,
    target_bytes: usize,
) -> Result<(f32, Vec<u8>)> {
    #[cfg(feature = "debug-logging")]
    log_debug_message(&format!(
        "JP2 target-size encode start: {}x{}, ch={}, target={} bytes",
        width, height, channels, target_bytes
    ));

    let orig_bytes = input.len() as f64;
    // Initial guess based on simple model size ≈ orig / rate
    let mut guess = (orig_bytes / target_bytes as f64).max(1.0) as f32;

    // Dynamic number of DWT levels based on min dimension (aim min level size ~ 32px)
    let min_dim = width.min(height) as f64;
    let mut num_levels = ((min_dim.ln() / 2f64.ln()) - 5.0).floor() as i32; // floor(log2(min_dim)) - 5
    num_levels = num_levels.clamp(3, 6);

    // Smaller codeblocks help at low bitrates for small images
    let area = (width as u64) * (height as u64);
    let codeblock = if area < 250_000 {
        Some((32, 32))
    } else if area > 4_000_000 {
        Some((64, 64))
    } else {
        None
    };

    // Adaptive iteration limits and tolerance by image size
    let (expand_iters, bisect_iters, tol_frac, grow_factor) = if area < 500_000 {
        (6usize, 6usize, 0.06f64, 1.6f32)
    } else if area < 3_000_000 {
        (8usize, 8usize, 0.05f64, 1.7f32)
    } else {
        (10usize, 10usize, 0.04f64, 1.8f32)
    };

    // Helper to encode at a given rate with our common settings
    let mut best: Option<(f32, usize, Vec<u8>)> = None;
    let mut encode_at = |rate: f32| -> Result<(usize, Vec<u8>)> {
        let data = encode(
            input,
            width,
            height,
            channels,
            &Jp2Settings {
                num_resolutions: Some(num_levels),
                prog_order: Some(openjp2::openjpeg::OPJ_LRCP),
                rate: Some(rate),
                rates: None,
                psnrs: None,
                irreversible: Some(true),
                tile_size: None,
                codeblock,
            },
        )?;
        let sz = data.len();
        if best
            .as_ref()
            .map(|b| (b.1 as i64 - target_bytes as i64).abs())
            .map_or(true, |best_err| {
                (sz as i64 - target_bytes as i64).abs() < best_err
            })
        {
            best = Some((rate, sz, data.clone()));
        }
        Ok((sz, data))
    };

    // Establish a bracket [lo, hi] where size_lo >= target >= size_hi
    let tol = (target_bytes as f64 * tol_frac) as i64;
    let mut r_lo = guess.clamp(0.5, 5000.0);
    let (mut sz_lo, mut data_lo) = encode_at(r_lo)?;
    let mut data_hi = data_lo.clone();
    if (sz_lo as i64 - target_bytes as i64).abs() <= tol {
        return Ok((r_lo, data_lo));
    }

    let mut r_hi = r_lo;
    let mut sz_hi = sz_lo;
    if sz_lo > target_bytes {
        // Need more compression -> increase rate until <= target
        for _ in 0..expand_iters {
            r_hi = (r_hi * grow_factor).clamp(0.5, 5000.0);
            let (s, d) = encode_at(r_hi)?;
            sz_hi = s;
            data_hi = d;
            if sz_hi <= target_bytes {
                break;
            }
        }
    } else {
        // Too small -> decrease rate until >= target
        for _ in 0..expand_iters {
            r_hi = (r_hi / grow_factor).clamp(0.5, 5000.0);
            let (s, d) = encode_at(r_hi)?;
            sz_hi = s;
            data_hi = d;
            if sz_hi >= target_bytes {
                break;
            }
        }
        // Swap so that sz_lo >= target and sz_hi <= target
        if sz_hi > target_bytes {
            std::mem::swap(&mut r_lo, &mut r_hi);
            std::mem::swap(&mut sz_lo, &mut sz_hi);
            std::mem::swap(&mut data_lo, &mut data_hi);
        }
    }

    // If we failed to bracket properly, return best so far
    if !(sz_lo >= target_bytes && sz_hi <= target_bytes) {
        if let Some((r, _s, d)) = best {
            return Ok((r, d));
        }
        return Ok((r_lo, data_lo));
    }

    // Binary search in log-rate space for precision
    for _ in 0..bisect_iters {
        if (sz_lo as i64 - target_bytes as i64).abs() <= tol {
            return Ok((r_lo, data_lo));
        }
        if (sz_hi as i64 - target_bytes as i64).abs() <= tol {
            return Ok((r_hi, data_hi));
        }
        let r_mid = ((r_lo as f64).ln() + (r_hi as f64).ln()) / 2.0;
        let r_mid = r_mid.exp() as f32;
        let (sz_mid, data_mid) = encode_at(r_mid)?;
        if sz_mid > target_bytes {
            r_lo = r_mid;
            sz_lo = sz_mid;
            data_lo = data_mid;
        } else {
            r_hi = r_mid;
            sz_hi = sz_mid;
            data_hi = data_mid;
        }
    }

    // Choose closer of lo/hi
    let (final_rate, final_data, final_size) = if (sz_lo as i64 - target_bytes as i64).abs()
        <= (sz_hi as i64 - target_bytes as i64).abs()
    {
        (r_lo, data_lo, sz_lo)
    } else {
        (r_hi, data_hi, sz_hi)
    };

    // Final layered pass for better perceptual quality at similar size
    let r3 = final_rate.max(0.1);
    let layered_rates = vec![0.50 * r3 as f64, 0.75 * r3 as f64, r3 as f64]
        .into_iter()
        .map(|v| v as f32)
        .collect::<Vec<f32>>();
    let layered = encode(
        input,
        width,
        height,
        channels,
        &Jp2Settings {
            num_resolutions: Some(num_levels),
            prog_order: Some(openjp2::openjpeg::OPJ_LRCP),
            rate: None,
            rates: Some(layered_rates),
            psnrs: None,
            irreversible: Some(true),
            tile_size: None,
            codeblock,
        },
    )?;
    let layered_diff = (layered.len() as i64 - target_bytes as i64).abs();
    if layered_diff <= (final_size as i64 - target_bytes as i64).abs() + (target_bytes as i64 / 20)
    {
        return Ok((final_rate, layered));
    }
    Ok((final_rate, final_data))
}
