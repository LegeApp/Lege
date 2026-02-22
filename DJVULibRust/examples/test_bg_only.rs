use djvu_encoder::doc::page_encoder::{PageComponents, PageEncodeParams};
use djvu_encoder::image::image_formats::{Pixmap, Pixel};
use std::fs;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("Testing IW44 background encoding...\n");
    
    let width = 600u32;
    let height = 800u32;
    
    // Create a simple gradient background
    let background = Pixmap::from_fn(width, height, |x, y| {
        let r = (x * 255 / width) as u8;
        let g = (y * 255 / height) as u8;
        let b = 128;
        Pixel::new(r, g, b)
    });
    
    // Encode page with ONLY background (no foreground)
    let page = PageComponents::new_with_dimensions(width, height)
        .with_background(background)?;
    
    let params = PageEncodeParams::default();
    let djvu_bytes = page.encode(&params, 1, 300, 1, Some(2.2))?;
    
    println!("Encoded page: {} bytes", djvu_bytes.len());
    fs::write("/tmp/test_bg_only.djvu", &djvu_bytes)?;
    
    println!("Wrote /tmp/test_bg_only.djvu");
    println!("Decoding to check colors...");
    
    std::process::Command::new("ddjvu")
        .args(["-format=ppm", "/tmp/test_bg_only.djvu", "/tmp/test_bg_decoded.ppm"])
        .output()?;
    
    println!("Check /tmp/test_bg_decoded.ppm to verify gradient");
    
    Ok(())
}
