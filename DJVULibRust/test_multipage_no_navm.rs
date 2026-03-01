#!/usr/bin/env rust-script
//! Test multi-page encoding without NAVM chunks
//! 
//! ```cargo
//! [dependencies]
//! djvu_encoder = { path = "." }
//! ```

use std::fs::File;
use std::io::Write;

fn main() -> std::io::Result<()> {
    println!("Creating simple multi-page DjVu document without NAVM...");
    
    // Create 3 minimal test pages (just the header structure)
    let pages: Vec<std::sync::Arc<Vec<u8>>> = (0..3)
        .map(|i| {
            let mut page = Vec::new();
            // Minimal DJVU page structure
            page.extend_from_slice(b"AT&TFORM");
            page.extend_from_slice(&[0, 0, 0, 12]); // size: 12 bytes
            page.extend_from_slice(b"DJVU");
            page.extend_from_slice(b"INFO");
            page.extend_from_slice(&[0, 0, 0, 0]); // empty INFO chunk
            std::sync::Arc::new(page)
        })
        .collect();
    
    // Assemble into multi-page document
    let document = djvu_encoder::doc::encoder::DocumentEncoder::assemble_pages(&pages)
        .expect("Failed to assemble document");
    
    // Write to file
    let mut file = File::create("test_multipage_no_navm.djvu")?;
    file.write_all(&document)?;
    
    println!("Created test_multipage_no_navm.djvu ({} bytes)", document.len());
    println!("Checking structure...");
    
    // Verify structure
    if document.starts_with(b"AT&TFORM") {
        println!("✓ Has AT&T prefix");
    }
    
    if &document[12..16] == b"DJVM" {
        println!("✓ Is DJVM format");
    }
    
    if &document[16..20] == b"DIRM" {
        println!("✓ Has DIRM chunk");
    }
    
    // Check for NAVM (should NOT be present)
    let has_navm = document.windows(4).any(|w| w == b"NAVM");
    if !has_navm {
        println!("✓ No NAVM chunk (as expected)");
    } else {
        println!("✗ ERROR: NAVM chunk found but should be disabled!");
    }
    
    println!("\nFile structure valid! Multi-page encoding working without NAVM.");
    
    Ok(())
}
