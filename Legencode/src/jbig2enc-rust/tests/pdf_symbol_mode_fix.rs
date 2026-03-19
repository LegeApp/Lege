//! Test to verify the PDF symbol mode fix
//!
//! This test verifies that when encoding in PDF split mode with symbol mode,
//! the page streams don't reference non-existent global dictionary segments.
//!
//! Run with: cargo test --test pdf_symbol_mode_fix --release -- --nocapture

use jbig2enc_rust::jbig2enc::Jbig2Encoder;
use jbig2enc_rust::jbig2structs::Jbig2Config;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::Command;

/// Parse JBIG2 segment header to extract referred_to field
fn parse_segment_referred_to(data: &[u8]) -> Option<Vec<u32>> {
    if data.len() < 13 {
        return None;
    }

    let segment_number = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
    let flags1 = data[4];
    let flags2 = data[5];

    let page_size_is_4_bytes = (flags1 & 0x40) != 0;
    let referred_to_count = (flags2 >> 5) & 0x07;

    let ref_size = if segment_number <= 256 {
        1
    } else if segment_number <= 65536 {
        2
    } else {
        4
    };

    let mut offset = 6;
    let mut referred_to = Vec::with_capacity(referred_to_count as usize);

    for _ in 0..referred_to_count {
        if offset + ref_size > data.len() {
            return None;
        }

        let ref_num = match ref_size {
            1 => data[offset] as u32,
            2 => u16::from_be_bytes([data[offset], data[offset + 1]]) as u32,
            4 => u32::from_be_bytes([data[offset], data[offset + 1], data[offset + 2], data[offset + 3]]),
            _ => unreachable!(),
        };
        referred_to.push(ref_num);
        offset += ref_size;
    }

    Some(referred_to)
}

fn next_segment_offset(data: &[u8], offset: usize) -> Option<usize> {
    if offset + 11 > data.len() {
        return None;
    }
    let seg_num = u32::from_be_bytes(data[offset..offset + 4].try_into().ok()?);
    let flags1 = data[offset + 4];
    let flags2 = data[offset + 5];
    let referred_to_count = (flags2 >> 5) & 0x07;
    let ref_size = if seg_num <= 256 {
        1
    } else if seg_num <= 65536 {
        2
    } else {
        4
    };
    let page_assoc_size = if (flags1 & 0x40) != 0 { 4 } else { 1 };
    let header_size = 6 + referred_to_count as usize * ref_size + page_assoc_size + 4;
    let payload_len_offset = offset + header_size - 4;
    let payload_len =
        u32::from_be_bytes(data[payload_len_offset..payload_len_offset + 4].try_into().ok()?)
            as usize;
    Some(offset + header_size + payload_len)
}

/// Create a synthetic text-like image with repeated characters
fn create_synthetic_text_page(page_num: usize, width: u32, height: u32) -> ndarray::Array2<u8> {
    let mut array = ndarray::Array2::from_elem((height as usize, width as usize), 0u8);

    // Draw some repeated "character" patterns (5x7 pixel glyphs)
    let char_patterns = vec![
        // 'H'
        vec![(0,0), (0,4), (1,0), (1,4), (2,0), (2,1), (2,2), (2,3), (2,4), (3,0), (3,4), (4,0), (4,4)],
        // 'E'
        vec![(0,0), (0,1), (0,2), (0,3), (0,4), (1,0), (2,0), (2,1), (2,2), (2,3), (2,4), (3,0), (4,0), (4,1), (4,2), (4,3), (4,4)],
        // 'L'
        vec![(0,0), (1,0), (2,0), (3,0), (4,0), (4,1), (4,2), (4,3), (4,4)],
        // 'O'
        vec![(0,1), (0,2), (0,3), (1,0), (1,4), (2,0), (2,4), (3,0), (3,4), (4,1), (4,2), (4,3)],
    ];

    let chars_per_line = 20;
    let lines_per_page = 30;
    let char_width = 6;
    let char_height = 8;
    let start_x = 50;
    let start_y = 50;

    // Create repeating text based on page number
    let text: Vec<usize> = (0..chars_per_line * lines_per_page)
        .map(|i| (i + page_num) % char_patterns.len())
        .collect();

    for (line_idx, line_start) in text.chunks(chars_per_line).enumerate() {
        if line_idx >= lines_per_page {
            break;
        }
        for (char_idx, &pattern_idx) in line_start.iter().enumerate() {
            let base_x = start_x + (char_idx * char_width) as u32;
            let base_y = start_y + (line_idx * char_height) as u32;

            for &(dx, dy) in &char_patterns[pattern_idx] {
                let x = base_x + dx;
                let y = base_y + dy;
                if x < width && y < height {
                    array[[y as usize, x as usize]] = 1;
                }
            }
        }
    }

    array
}

/// Load a PBM file into a 2D array
fn load_pbm_to_array(path: &Path) -> (ndarray::Array2<u8>, u32, u32) {
    let file = File::open(path).unwrap_or_else(|e| panic!("Failed to open PBM {}: {}", path.display(), e));
    let mut reader = BufReader::new(file);

    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert_eq!(line.trim(), "P4", "Only raw PBM (P4) supported");

    let (width, height) = loop {
        line.clear();
        reader.read_line(&mut line).unwrap();
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let dims: Vec<usize> = trimmed
            .split_whitespace()
            .map(|s| s.parse().unwrap())
            .collect();
        if dims.len() == 2 {
            break (dims[0], dims[1]);
        }
    };

    let bytes_per_row = (width + 7) / 8;
    let mut packed = vec![0u8; bytes_per_row * height];
    reader.read_exact(&mut packed).unwrap();

    let array = ndarray::Array2::from_shape_fn((height, width), |(y, x)| {
        let byte = packed[y * bytes_per_row + x / 8];
        let bit = 7 - (x % 8);
        ((byte >> bit) & 1) as u8
    });

    (array, width as u32, height as u32)
}

/// Write JBIG2 fragments and decode with jbig2dec
fn write_and_decode(
    globals: Option<&Vec<u8>>,
    page_stream: &[u8],
    globals_path: &Path,
    fragment_path: &Path,
    decoded_path: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    // Write globals
    if let Some(g) = globals {
        std::fs::write(globals_path, g)?;
    }

    std::fs::write(fragment_path, page_stream)?;

    // Decode with jbig2dec
    let mut cmd = Command::new("jbig2dec");
    cmd.arg("--format")
        .arg("pbm")
        .arg("--output")
        .arg(decoded_path);

    if globals.is_some() {
        cmd.arg(globals_path).arg(fragment_path);
    } else {
        cmd.arg("--embedded").arg(fragment_path);
    }

    let output = cmd.output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        return Err(format!(
            "jbig2dec failed: {}\nstdout:\n{}\nstderr:\n{}",
            output.status, stdout, stderr
        ).into());
    }

    Ok(())
}

#[test]
fn test_symbol_mode_pdf_split_decodable() {
    // Test with synthetic text pages that will actually use symbol mode
    let width = 400;
    let height = 600;
    let num_pages = 3;

    println!("Creating {} synthetic text pages ({}x{})", num_pages, width, height);
    let pages: Vec<_> = (0..num_pages)
        .map(|i| {
            let array = create_synthetic_text_page(i, width, height);
            (array, width, height)
        })
        .collect();

    println!("Loaded {} pages for testing", pages.len());

    // Configure for symbol mode with PDF embedding
    let mut cfg = Jbig2Config::text();
    cfg.want_full_headers = false; // PDF mode - no file headers

    // Encode
    let mut encoder = Jbig2Encoder::new(&cfg);
    for (array, width, height) in &pages {
        println!("Adding page: {}x{}", width, height);
        encoder.add_page(array).expect("add_page failed");
    }

    let split = encoder.flush_pdf_split().expect("flush_pdf_split failed");

    println!("\nEncode complete:");
    println!("  Global segments: {}", split.global_segments.as_ref().map_or(0, |g| g.len()));
    println!("  Page streams: {}", split.page_streams.len());
    println!("  Local dict bytes: {:?}", split.local_dict_bytes_per_page);
    println!("  Text region bytes: {:?}", split.text_region_bytes_per_page);
    println!("  Generic region bytes: {:?}", split.generic_region_bytes_per_page);

    let metrics = encoder.metrics_snapshot();
    println!("\n  Symbols discovered: {}", metrics.symbol_stats.symbols_discovered);
    println!("  Symbols exported: {}", metrics.symbol_stats.symbols_exported);
    println!("  Global symbols: {}", metrics.symbol_stats.global_symbol_count);
    println!("  Local symbols: {}", metrics.symbol_stats.local_symbol_count);
    println!("  Avg symbol reuse: {:.2}", metrics.symbol_stats.avg_symbol_reuse);

    // Verify we got the right number of page streams
    assert_eq!(split.page_streams.len(), pages.len(), "Wrong page stream count");

    // CRITICAL FIX VERIFICATION:
    // In PDF split mode, the page fragment still needs the JBIG2 text region
    // to refer to the imported global symbol dictionary as segment 1.
    // The PDF /DecodeParms binds that logical segment 1 to the JBIG2Globals object.
    println!("\n--- Verifying referred_to field in page segments ---");
    for (i, page_stream) in split.page_streams.iter().enumerate() {
        let text_region_offset = next_segment_offset(page_stream, 0).expect("page info segment");
        let referred_to = parse_segment_referred_to(&page_stream[text_region_offset..]);
        println!("Page {} text region header: referred_to = {:?}", i + 1, referred_to);

        let refs = referred_to.unwrap_or_default();
        assert_eq!(
            refs,
            vec![1],
            "Page {} has referred_to = {:?}, expected [1] so the text region can bind to the imported JBIG2 global dictionary.",
            i + 1,
            refs
        );
    }

    // Create temp directory for fragments
    let temp_dir = std::env::temp_dir().join("jbig2_test_pdf_split");
    std::fs::create_dir_all(&temp_dir).unwrap();

    // Write and decode each page
    let globals_path = temp_dir.join("globals.jb2");
    for (i, page_stream) in split.page_streams.iter().enumerate() {
        let fragment_path = temp_dir.join(format!("page_{:03}.jb2", i + 1));
        let decoded_path = temp_dir.join(format!("page_{:03}.pbm", i + 1));
        println!("\nDecoding page {}...", i + 1);

        let result = write_and_decode(
            split.global_segments.as_ref(),
            page_stream,
            &globals_path,
            &fragment_path,
            &decoded_path,
        );

        assert!(
            result.is_ok(),
            "Failed to decode page {}: {:?}",
            i + 1,
            result.err()
        );

        // Verify decoded file exists and has content
        assert!(decoded_path.exists(), "Decoded file not created");
        let decoded_size = std::fs::metadata(&decoded_path).unwrap().len();
        assert!(decoded_size > 0, "Decoded file is empty");

        println!("  Page {} decoded successfully ({} bytes)", i + 1, decoded_size);
    }

    // Cleanup
    let _ = std::fs::remove_dir_all(&temp_dir);

    println!("\n✓ All pages decoded successfully with jbig2dec");
}
