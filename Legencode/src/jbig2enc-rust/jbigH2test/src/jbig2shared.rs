//! Utility functions for the JBIG2 encoder test harness

use crate::jbig2sym::BitImage;
use std::fs::{self, File};
use std::io::Write;
use std::path::Path;

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
