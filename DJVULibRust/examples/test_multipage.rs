use djvu_encoder::doc::DjvuBuilder;
use std::fs;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("Testing multi-page DjVu encoding without NAVM...\n");
    
    // Create a builder with 3 pages
    let mut builder = DjvuBuilder::new();
    
    // Load the 3 test pages and add them
    for i in 0..3 {
        let filename = format!("test_page_{}.djvu", i);
        let data = fs::read(&filename)?;
        println!("Loaded {} ({} bytes)", filename, data.len());
        builder = builder.add_encoded_page(data);
    }
    
    // Build the document
    println!("\nBuilding multi-page document...");
    let document = builder.build()?;
    
    println!("Created DJVM document ({} bytes)", document.len());
    
    // Write output
    fs::write("test_multipage_no_navm.djvu", &document)?;
    println!("Written to test_multipage_no_navm.djvu");
    
    // Verify structure
    println!("\nVerifying structure:");
    
    if document.starts_with(b"AT&TFORM") {
        println!("  ✓ Has AT&T prefix");
    }
    
    if document.len() >= 16 && &document[12..16] == b"DJVM" {
        println!("  ✓ Is DJVM format");
    }
    
    if document.len() >= 20 && &document[16..20] == b"DIRM" {
        println!("  ✓ Has DIRM chunk at offset 16");
    }
    
    // Check for NAVM (should NOT be present)
    let has_navm = document.windows(4).any(|w| w == b"NAVM");
    if !has_navm {
        println!("  ✓ No NAVM chunk (correctly disabled)");
    } else {
        println!("  ✗ ERROR: NAVM chunk found but should be disabled!");
        return Err("NAVM chunk should not be present".into());
    }
    
    // Count FORM chunks (should be 3 pages)
    let form_count = document.windows(4).filter(|w| *w == b"FORM").count();
    println!("  ✓ Found {} FORM chunks (3 pages)", form_count);
    
    println!("\n✓ SUCCESS: Multi-page encoding works without NAVM!");
    
    Ok(())
}
