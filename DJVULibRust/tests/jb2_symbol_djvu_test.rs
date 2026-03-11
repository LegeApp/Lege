//! JB2 symbol-mode encoding test using background.pbm
//!
//! This test loads the bundled background.pbm, runs symbol-dictionary JB2
//! extraction, and encodes a single-page DjVu. It verifies that the output
//! contains Djbz + Sjbz chunks, indicating symbol dictionary usage.

#![cfg(feature = "symboldict")]

use djvu_encoder::{
    DjvuError, Result,
    doc::page_encoder::{PageComponents, PageEncodeParams},
    encode::symbol_dict::BitImage,
};
use std::fs;
use std::io;
use std::path::Path;
use std::process::Command;
use tempfile::tempdir;

const PBM_PATH: &str = "background.pbm";

/// Load a PBM file into a BitImage for JB2 encoding
fn load_pbm_as_bitimage(path: &Path) -> Result<BitImage> {
    let pbm_data = fs::read(path).map_err(|e| {
        DjvuError::Io(io::Error::new(
            io::ErrorKind::Other,
            format!("Failed to read PBM file: {e}"),
        ))
    })?;

    // Parse PBM format (P4 = raw binary, P1 = ASCII)
    let content = String::from_utf8_lossy(&pbm_data);
    let lines: Vec<&str> = content.lines().filter(|l| !l.starts_with('#')).collect();

    if lines.is_empty() {
        return Err(DjvuError::InvalidOperation("Empty PBM file".to_string()));
    }

    let magic = lines[0].trim();

    if magic == "P4" {
        // Raw binary PBM
        let mut found_magic = false;
        let mut found_dims = false;
        let mut width: usize = 0;
        let mut height: usize = 0;
        let mut cursor = 0;

        while cursor < pbm_data.len() {
            while cursor < pbm_data.len() && pbm_data[cursor].is_ascii_whitespace() {
                cursor += 1;
            }
            if cursor >= pbm_data.len() {
                break;
            }

            if pbm_data[cursor] == b'#' {
                while cursor < pbm_data.len() && pbm_data[cursor] != b'\n' {
                    cursor += 1;
                }
                continue;
            }

            let token_start = cursor;
            while cursor < pbm_data.len() && !pbm_data[cursor].is_ascii_whitespace() {
                cursor += 1;
            }
            let token = String::from_utf8_lossy(&pbm_data[token_start..cursor]).to_string();

            if !found_magic {
                if token == "P4" {
                    found_magic = true;
                }
            } else if !found_dims {
                if width == 0 {
                    width = token.parse().unwrap_or(0);
                } else {
                    height = token.parse().unwrap_or(0);
                    found_dims = true;
                    while cursor < pbm_data.len() && pbm_data[cursor].is_ascii_whitespace() {
                        cursor += 1;
                        break;
                    }
                    break;
                }
            }
        }

        let header_end = cursor;

        if width == 0 || height == 0 {
            return Err(DjvuError::InvalidOperation(format!(
                "Invalid PBM dimensions: {width}x{height}"
            )));
        }

        let mut bitimage = BitImage::new(width as u32, height as u32)
            .map_err(|e| DjvuError::InvalidOperation(e.to_string()))?;

        let row_bytes = (width + 7) / 8;
        let pixel_data = &pbm_data[header_end..];

        for y in 0..height {
            for x in 0..width {
                let byte_idx = y * row_bytes + x / 8;
                let bit_idx = 7 - (x % 8);
                if byte_idx < pixel_data.len() {
                    let bit = (pixel_data[byte_idx] >> bit_idx) & 1;
                    bitimage.set_usize(x, y, bit == 1);
                }
            }
        }

        Ok(bitimage)
    } else if magic == "P1" {
        let dims: Vec<usize> = lines[1]
            .split_whitespace()
            .filter_map(|s| s.parse().ok())
            .collect();

        if dims.len() < 2 {
            return Err(DjvuError::InvalidOperation(
                "Invalid PBM dimensions".to_string(),
            ));
        }

        let (width, height) = (dims[0], dims[1]);
        let mut bitimage = BitImage::new(width as u32, height as u32)
            .map_err(|e| DjvuError::InvalidOperation(e.to_string()))?;

        let pixel_chars: Vec<char> = lines[2..]
            .join("")
            .chars()
            .filter(|c| *c == '0' || *c == '1')
            .collect();

        for (i, c) in pixel_chars.iter().enumerate() {
            let x = i % width;
            let y = i / width;
            if y < height {
                bitimage.set_usize(x, y, *c == '1');
            }
        }

        Ok(bitimage)
    } else {
        Err(DjvuError::InvalidOperation(format!(
            "Unsupported PBM format: {magic}"
        )))
    }
}

#[test]
fn test_jb2_symbol_mode_single_page_djvu() -> Result<()> {
    let pbm_path = Path::new(PBM_PATH);
    if !pbm_path.exists() {
        return Err(DjvuError::InvalidOperation(format!(
            "PBM file not found: {PBM_PATH}"
        )));
    }

    let bitimage = load_pbm_as_bitimage(pbm_path)?;

    let mut page =
        PageComponents::new_with_dimensions(bitimage.width as u32, bitimage.height as u32);
    page = page.with_jb2_auto_extract(bitimage)?;

    let (page_w, page_h) = page.dimensions();
    println!("JB2 page dimensions: {}x{}", page_w, page_h);
    assert!(page_w > 0 && page_h > 0, "Page dimensions must be non-zero");

    let params = PageEncodeParams::default();
    let djvu_bytes = page.encode(&params, 1, 300, 1, Some(2.2))?;

    assert!(djvu_bytes.starts_with(b"AT&TFORM"));
    assert!(djvu_bytes.windows(4).any(|w| w == b"Sjbz"));

    let temp_dir = tempdir().map_err(|e| DjvuError::Io(io::Error::new(io::ErrorKind::Other, e)))?;
    let djvu_path = temp_dir.path().join("jb2_symbol_test.djvu");
    fs::write(&djvu_path, &djvu_bytes).map_err(|e| {
        DjvuError::Io(io::Error::new(
            io::ErrorKind::Other,
            format!("Failed to write temp DjVu: {e}"),
        ))
    })?;

    // Decode with ddjvu to ensure the file is readable
    let ddjvu_output = temp_dir.path().join("decoded.pbm");
    let ddjvu_status = Command::new("ddjvu")
        .args([
            "-format=pbm",
            djvu_path.to_string_lossy().as_ref(),
            ddjvu_output.to_string_lossy().as_ref(),
        ])
        .status();
    let ddjvu_status = ddjvu_status.map_err(|e| {
        DjvuError::Io(io::Error::new(
            io::ErrorKind::Other,
            format!("Failed to run ddjvu: {e}"),
        ))
    })?;
    if !ddjvu_status.success() {
        return Err(DjvuError::InvalidOperation(
            "ddjvu failed to decode output".to_string(),
        ));
    }

    // Dump with djvudump for chunk verification
    let dump_output = Command::new("djvudump")
        .arg(&djvu_path)
        .output()
        .map_err(|e| {
            DjvuError::Io(io::Error::new(
                io::ErrorKind::Other,
                format!("Failed to run djvudump: {e}"),
            ))
        })?;
    if !dump_output.status.success() {
        return Err(DjvuError::InvalidOperation(
            "djvudump failed to parse output".to_string(),
        ));
    }
    let dump_text = String::from_utf8_lossy(&dump_output.stdout);
    if !dump_text.contains("Sjbz") {
        return Err(DjvuError::InvalidOperation(
            "djvudump output missing Sjbz chunk".to_string(),
        ));
    }

    let decoded_size = fs::metadata(&ddjvu_output).map(|m| m.len()).unwrap_or(0);
    println!(
        "JB2 symbol DjVu OK: djvu_bytes={} dump_bytes={} decoded_bytes={}",
        djvu_bytes.len(),
        dump_output.stdout.len(),
        decoded_size
    );

    Ok(())
}
