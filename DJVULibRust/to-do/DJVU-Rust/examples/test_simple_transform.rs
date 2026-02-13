// examples/test_simple_transform.rs
//
// Test the wavelet transform with a simple constant input

use djvu_encoder::encode::iw44::transform::Encode;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== Testing Simple Wavelet Transform ===\n");

    // Test 1: 32x32 constant array (exactly one block)
    println!("Test 1: 32x32 constant array");
    let mut data32 = vec![-6784i32; 32 * 32];
    println!("Before transform: all values = {}", data32[0]);
    
    // Apply wavelet transform with 5 levels
    Encode::forward::<4>(&mut data32, 32, 32, 32, 5);
    
    println!("After transform:");
    println!("  DC coefficient: {}", data32[0]);
    println!("  AC[1..16]: {:?}", &data32[1..17]);
    
    // Count non-zero AC coefficients
    let mut non_zero_ac = 0;
    for i in 1..data32.len() {
        if data32[i] != 0 {
            non_zero_ac += 1;
        }
    }
    println!("  Non-zero AC coefficients: {} out of {}", non_zero_ac, data32.len() - 1);
    
    // Test 2: 64x64 constant array (no padding)
    println!("\nTest 2: 64x64 constant array");
    let mut data64 = vec![-6784i32; 64 * 64];
    println!("Before transform: all values = {}", data64[0]);
    
    // Apply wavelet transform with 6 levels (since 64 = 2^6)
    Encode::forward::<4>(&mut data64, 64, 64, 64, 6);
    
    println!("After transform:");
    println!("  DC coefficient: {}", data64[0]);
    println!("  AC[1..16]: {:?}", &data64[1..17]);
    
    // Count non-zero AC coefficients
    let mut non_zero_ac_64 = 0;
    for i in 1..data64.len() {
        if data64[i] != 0 {
            non_zero_ac_64 += 1;
        }
    }
    println!("  Non-zero AC coefficients: {} out of {}", non_zero_ac_64, data64.len() - 1);
    
    // Test 3: Small 8x8 array for easier analysis
    println!("\nTest 3: 8x8 constant array");
    let mut data8 = vec![-6784i32; 8 * 8];
    println!("Before transform: all values = {}", data8[0]);
    
    // Apply wavelet transform with 3 levels
    Encode::forward::<4>(&mut data8, 8, 8, 8, 3);
    
    println!("After transform:");
    println!("  All coefficients: {:?}", data8);
    
    // Count non-zero AC coefficients
    let mut non_zero_ac_8 = 0;
    for i in 1..data8.len() {
        if data8[i] != 0 {
            non_zero_ac_8 += 1;
        }
    }
    println!("  Non-zero AC coefficients: {} out of {}", non_zero_ac_8, data8.len() - 1);
    
    println!("\n=== Expected Result ===");
    println!("For a perfectly constant input, ALL AC coefficients should be 0!");
    println!("Only the DC coefficient should be non-zero.");
    
    Ok(())
}
