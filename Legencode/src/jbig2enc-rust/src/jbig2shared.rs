//! Utility functions for the JBIG2 encoder

use crate::jbig2sym::BitImage;
use anyhow::Result;
use std::fs::{self, File};
use std::io::Write;
use std::path::Path;

// ==============================================
// Type conversion utilities
// ==============================================

/// Safely convert from u32 to usize with a panic if the value is too large.
#[inline]
pub fn u32_to_usize(x: u32) -> usize {
    x as usize
}

/// Safely convert from usize to u32 with a panic if the value is too large.
#[inline]
pub fn usize_to_u32(x: usize) -> u32 {
    u32::try_from(x).expect("value exceeds u32 range")
}

// ==============================================
// Debug utilities
// (Moved from jbig2sym.rs)
// ==============================================

/// Save a BitImage to a PBM file in the debug directory
/// Only saves files in debug builds (when not built with --release)
pub fn save_debug_pbm(image: &BitImage, filename: &str) -> std::io::Result<()> {
    if cfg!(debug_assertions) {
        let debug_dir = Path::new("debug-output");
        if !debug_dir.exists() {
            fs::create_dir_all(debug_dir)?;
        }

        let path = debug_dir.join(filename);
        let mut file = File::create(&path)?;

        // Write PBM header
        writeln!(&mut file, "P4\n{} {}\n", image.width, image.height)?;

        // Write image data
        file.write_all(&image.to_jbig2_format())?;
    }

    Ok(())
}

// (PDF helpers removed - this crate no longer creates PDFs directly.)

pub mod jbig2wrapper {
    

    pub fn push_file_header(out: &mut Vec<u8>) {
        out.extend_from_slice(&[0x97, 0x4A, 0x42, 0x32, 0x0D, 0x0A, 0x1A, 0x0A]);
    }

    pub fn push_page_info(out: &mut Vec<u8>, width: u32, height: u32) {
        // Segment header for Page Information Segment (Section 7.4.1)
        // Segment number (arbitrary, but 1 for first page info)
        out.extend_from_slice(&0u32.to_be_bytes());
        // Page Information Segment type (0x00)
        out.push(0x00);
        // Page Information Segment flags (Section 7.4.1.1)
        // Bit 7: Default Pixel Value (0 = black, 1 = white) - set to 1 for white
        // Bit 6: Page Striping (0 = no striping, 1 = striping) - set to 0
        // Bits 5-0: Page X-Resolution and Y-Resolution (0 = no resolution specified)
        out.push(0b10000000); // Flags1: DP=1, PS=0, R=0
        out.push(0x00); // Flags2: Reserved, set to 0

        // Page width and height
        out.extend_from_slice(&width.to_be_bytes());
        out.extend_from_slice(&height.to_be_bytes());

        // X and Y resolution (0 = no resolution specified)
        out.extend_from_slice(&0u32.to_be_bytes());
        out.extend_from_slice(&0u32.to_be_bytes());

        // Page segments (number of segments associated with this page)
        // For a single page with one generic region and EOF, this is 2
        out.extend_from_slice(&2u32.to_be_bytes());
    }

    pub fn push_eof(out: &mut Vec<u8>, segment_number: u32) {
        // Segment header for End of File Segment (Section 7.4.2)
        out.extend_from_slice(&segment_number.to_be_bytes());
        out.push(0x02); // End of File Segment type (0x02)
        out.extend_from_slice(&0u16.to_be_bytes()); // Flags: Reserved, set to 0
        out.extend_from_slice(&0u32.to_be_bytes()); // Segment page association: 0 for global
        out.extend_from_slice(&0u32.to_be_bytes()); // Segment data length: 0 for EOF
    }
}
