// examples/test_border_fix.rs
//
// Test the border extension fix for wavelet transform

use djvu_encoder::encode::iw44::{coeff_map::CoeffMap, transform::Encode};
use djvu_encoder::encode::iw44::encoder::ycbcr_from_rgb;
use image::{RgbImage, Rgb};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== Testing Border Extension Fix ===\n");

    // Create a small solid blue image
    let w = 64;
    let h = 64;
    let mut img = RgbImage::new(w, h);
    
    // Fill with solid blue RGB(0, 0, 255)
    for pixel in img.pixels_mut() {
        *pixel = Rgb([0, 0, 255]);
    }
    
    println!("Created {}x{} solid blue image RGB(0,0,255)", w, h);
    
    // Convert to YCbCr
    let (y_buf, cb_buf, cr_buf) = ycbcr_from_rgb(&img);
    
    // Test Y channel with manual transform
    println!("\n=== Testing Y Channel Transform (Direct) ===");
    let mut data32 = vec![0i32; 96 * 96]; // 96 is the padded size for 64x64
    Encode::from_i8_channel_with_stride(&y_buf, &mut data32, 64, 64, 96);
    
    println!("After border extension:");
    println!("  data32[0] (top-left): {}", data32[0]);
    println!("  data32[63] (top-right edge): {}", data32[63]);
    println!("  data32[64] (top-right padding): {}", data32[64]);
    println!("  data32[95] (top-right corner): {}", data32[95]);
    println!("  data32[63*96] (bottom-left edge): {}", data32[63 * 96]);
    println!("  data32[64*96] (bottom padding start): {}", data32[64 * 96]);
    
    // Check if padding was applied correctly
    let top_edge = data32[63];  // Last pixel of first row
    let top_padding = data32[64]; // First padding pixel of first row
    let bottom_edge = data32[63 * 96]; // First pixel of last image row
    let bottom_padding = data32[64 * 96]; // First pixel of first padding row
    
    println!("Border extension check:");
    println!("  Top edge == top padding: {} ({} == {})", top_edge == top_padding, top_edge, top_padding);
    println!("  Bottom edge == bottom padding: {} ({} == {})", bottom_edge == bottom_padding, bottom_edge, bottom_padding);
    
    // Apply wavelet transform
    Encode::forward::<4>(&mut data32, 96, 96, 96, 5);
    
    // Count non-zero AC coefficients
    let mut non_zero_ac = 0;
    let mut max_ac = 0;
    for i in 1..1024 { // Only check first block (32x32)
        if data32[i].abs() > 0 {
            non_zero_ac += 1;
            max_ac = max_ac.max(data32[i].abs());
        }
    }
    
    println!("\nAfter wavelet transform:");
    println!("  DC coefficient: {}", data32[0]);
    println!("  AC[1..10]: {:?}", &data32[1..11]);
    println!("  Non-zero AC coefficients (first block): {}", non_zero_ac);
    println!("  Max AC magnitude: {}", max_ac);
    
    // Test with CoeffMap (normal path)
    println!("\n=== Testing CoeffMap Creation ===");
    let coeff_map = CoeffMap::create_from_signed_channel(&y_buf, w, h, None, "Y");
    println!("CoeffMap created successfully with {} blocks", coeff_map.blocks.len());
    
    println!("\n=== Success Criteria ===");
    println!("✓ Border extension should eliminate sharp transitions");
    println!("✓ Non-zero AC coefficients should be < 10 (was 633)");
    println!("✓ Max AC magnitude should be < 100 (was 18607)");
    
    if non_zero_ac < 10 && max_ac < 100 {
        println!("\n🎉 SUCCESS: Border extension fix appears to work!");
    } else {
        println!("\n❌ ISSUE: Still have too many AC coefficients");
    }
    
    Ok(())
}
