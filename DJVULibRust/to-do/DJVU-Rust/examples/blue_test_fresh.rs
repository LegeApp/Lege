use djvu_encoder::doc::{PageEncodeParams, PageComponents};
use image::RgbImage;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("Starting fresh blue test...");
    
    // Create a 100x100 solid blue image
    let width = 100;
    let height = 100;
    let mut rgb_image = RgbImage::new(width, height);
    
    // Fill with solid blue
    for pixel in rgb_image.pixels_mut() {
        *pixel = image::Rgb([0, 0, 255]); // Pure blue
    }
    
    // Create page components with blue background
    let page_components = PageComponents::new()
        .with_background(rgb_image)?;
    
    // Encode with default params
    let params = PageEncodeParams::default();
    println!("About to encode blue image...");
    
    let encoded_data = page_components.encode(&params, 1, 1200, 1, Some(2.2))?;
    
    println!("Encoding completed successfully! Size: {} bytes", encoded_data.len());
    
    Ok(())
}
