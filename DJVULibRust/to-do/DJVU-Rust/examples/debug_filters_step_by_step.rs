// examples/debug_filters_step_by_step.rs
//
// Debug the wavelet filters step by step

use djvu_encoder::encode::iw44::transform::Encode;

fn print_array_2d(data: &[i32], w: usize, h: usize, name: &str) {
    println!("=== {} ===", name);
    for y in 0..h.min(8) {
        print!("Row {}: ", y);
        for x in 0..w.min(8) {
            print!("{:6} ", data[y * w + x]);
        }
        if w > 8 { print!(" ..."); }
        println!();
    }
    if h > 8 { println!("..."); }
    println!();
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== Step-by-Step Filter Debug ===\n");

    // Test with 8x8 to see if update step works
    test_array_size(8);
    
    // Test with 4x4 to see the original issue
    test_array_size(4);
    
    Ok(())
}

fn test_array_size(size: usize) {
    println!("=== Testing {}x{} array ===", size, size);
    
    let w = size;
    let h = size;
    let constant_val = -6784i32;
    
    let mut data = vec![constant_val; w * h];
    print_array_2d(&data, w, h, &format!("Original {}x{}", w, h));
    
    // Apply horizontal filter with detailed debugging
    println!("Applying horizontal filter (scale=1)...");
    filter_helpers::filter_fh_debug(&mut data, w, h, w, 1);
    print_array_2d(&data, w, h, &format!("After Horizontal Filter {}x{}", w, h));
    
    // Count changes
    let changes = data.iter().enumerate()
        .filter(|(_, &val)| val != constant_val)
        .count();
    
    if changes == 0 {
        println!("✅ No changes - filter working correctly for constant input");
    } else {
        println!("❌ {} values changed - this indicates a problem", changes);
        
        // Show which positions changed
        for (i, &val) in data.iter().enumerate() {
            if val != constant_val {
                let x = i % w;
                let y = i / w;
                println!("  Position ({},{}) = {} (was {})", x, y, val, constant_val);
            }
        }
    }
    println!();
}

// Helper functions to access the filter functions
mod filter_helpers {
    use djvu_encoder::encode::iw44::transform::*;
    
    pub fn filter_fh(buf: &mut [i32], w: usize, h: usize, rowsize: usize, scale: usize) {
        // We need to call the private function, so let's reproduce the logic here
        // or use the public interface
        let mut test_data = buf.to_vec();
        
        // Since the filters are private, let's test through the public interface
        // Apply just horizontal filtering by modifying the forward function
        forward_single_step(buf, w, h, rowsize, scale, true, false);
    }
    
    pub fn filter_fv(buf: &mut [i32], w: usize, h: usize, rowsize: usize, scale: usize) {
        forward_single_step(buf, w, h, rowsize, scale, false, true);
    }
    
    fn forward_single_step(buf: &mut [i32], w: usize, h: usize, rowsize: usize, scale: usize, do_h: bool, do_v: bool) {
        // This is a workaround since the filter functions are private
        // We'll need to copy the filter logic here
        
        if do_h {
            // Copy of filter_fh logic
            let s = scale;
            let s3 = 3 * s;
            for y in (0..h).step_by(s) {
                let row_start = y * rowsize;
                let row = &mut buf[row_start..row_start + w];
                let mut x = s;
                
                let mut a1 = row[0];
                let mut a2 = row[0];
                let mut a3 = row[0];
                
                let mut b1 = 0_i32;
                let mut b2 = 0_i32;
                let mut b3 = 0_i32;

                // Special case: first element (x = s)
                if x < w {
                    a2 = if x + s < w { row[x + s] } else { a1 };
                    a3 = if x + s3 < w { row[x + s3] } else { a1 };
                    b3 = row[x] - ((a1 + a2 + 1) >> 1);
                    row[x] = b3;
                    x += 2 * s;
                }

                // Generic case: main loop
                while x + s3 < w {
                    let a0 = a1;
                    a1 = a2;
                    a2 = a3;
                    a3 = row[x + s3];
                    let b0 = b1;
                    b1 = b2;
                    b2 = b3;
                    let a_sum = a1 + a2;
                    b3 = row[x] - (((a_sum << 3) + a_sum - a0 - a3 + 8) >> 4);
                    row[x] = b3;
                    let b_sum = b1 + b2;
                    row[x - s3] += ((b_sum << 3) + b_sum - b0 - b3 + 16) >> 5;
                    x += 2 * s;
                }

                // Special case: near end (w - 3*s <= x < w)
                while x < w {
                    a1 = a2;
                    a2 = a3;
                    a3 = a1; // Per C++ reference
                    let b0 = b1;
                    b1 = b2;
                    b2 = b3;
                    b3 = row[x] - ((a1 + a2 + 1) >> 1);
                    row[x] = b3;
                    let b_sum = b1 + b2;
                    if x >= s3 {
                        row[x - s3] += ((b_sum << 3) + b_sum - b0 - b3 + 16) >> 5;
                    }
                    x += 2 * s;
                }

                // Additional updates beyond w
                let mut x_update = x;
                while x_update - s3 < w {
                    let b0 = b1;
                    b1 = b2;
                    b2 = b3;
                    b3 = 0;
                    let b_sum = b1 + b2;
                    if x_update >= s3 {
                        row[x_update - s3] += ((b_sum << 3) + b_sum - b0 - b3 + 16) >> 5;
                    }
                    x_update += 2 * s;
                }
            }
        }
        
        if do_v {
            // Copy of filter_fv logic
            let s = scale * rowsize;
            let s3 = 3 * s;
            let mut y = scale;

            while y < h {
                // Delta (high-pass)
                if y >= 3 * scale && y + 3 * scale < h {
                    // Generic case
                    for x in 0..w {
                        let idx = y * rowsize + x;
                        let a = buf[idx - s] + buf[idx + s];
                        let b = buf[idx - s3] + buf[idx + s3];
                        buf[idx] -= ((a << 3) + a - b + 8) >> 4;
                    }
                } else if y < h {
                    // Special cases
                    let q1 = if y + scale < h { y + scale } else { y - scale };
                    for x in 0..w {
                        let idx = y * rowsize + x;
                        let a = buf[(y - scale) * rowsize + x] + buf[q1 * rowsize + x];
                        buf[idx] -= (a + 1) >> 1;
                    }
                }

                // Update (low-pass)
                if y >= 6 * scale && y < h {
                    // Generic case
                    for x in 0..w {
                        let idx = (y - 3 * scale) * rowsize + x;
                        let a = buf[idx - s] + buf[idx + s];
                        let b = buf[idx - s3] + buf[idx + s3];
                        buf[idx] += ((a << 3) + a - b + 16) >> 5;
                    }
                } else if y >= 3 * scale {
                    // Special cases
                    let q1 = if y - 2 * scale < h { y - 2 * scale } else { h - 1 };
                    let q3 = if y < h { y } else { h - 1 };
                    for x in 0..w {
                        let idx = y.saturating_sub(3 * scale) * rowsize + x;
                        let a = buf[idx.saturating_sub(s)] + if q1 < h { buf[q1 * rowsize + x] } else { 0 };
                        let b = buf[idx.saturating_sub(s3)] + if q3 < h { buf[q3 * rowsize + x] } else { 0 };
                        buf[idx] += ((a << 3) + a - b + 16) >> 5;
                    }
                }

                y += 2 * scale;
            }
        }
    }
    
    pub fn filter_fh_debug(buf: &mut [i32], w: usize, h: usize, rowsize: usize, scale: usize) {
        println!("filter_fh_debug: w={}, h={}, rowsize={}, scale={}", w, h, rowsize, scale);
        let s = scale;
        let s3 = 3 * s;
        println!("s={}, s3={}", s, s3);
        println!();
        
        for y in (0..h).step_by(s) {
            println!("Processing row y={}", y);
            let row_start = y * rowsize;
            let row = &mut buf[row_start..row_start + w];
            println!("Initial row values: {:?}", row);
            
            let mut x = s;
            
            let mut a1 = row[0];
            let mut a2 = row[0];
            let mut a3 = row[0];
            println!("Initial a1={}, a2={}, a3={}", a1, a2, a3);
            
            let mut b1 = 0_i32;
            let mut b2 = 0_i32;
            let mut b3 = 0_i32;

            // Special case: first element (x = s)
            if x < w {
                println!("First element: x={}", x);
                a2 = if x + s < w { 
                    println!("a2 = row[{}] = {}", x + s, row[x + s]);
                    row[x + s] 
                } else { 
                    println!("a2 = {} (a1, out of bounds)", a1);
                    a1 
                };
                a3 = if x + s3 < w { 
                    println!("a3 = row[{}] = {}", x + s3, row[x + s3]);
                    row[x + s3] 
                } else { 
                    println!("a3 = {} (a1, out of bounds)", a1);
                    a1 
                };
                let old_val = row[x];
                b3 = row[x] - ((a1 + a2 + 1) >> 1);
                println!("b3 = {} - (({} + {} + 1) >> 1) = {} - {} = {}", 
                         old_val, a1, a2, old_val, (a1 + a2 + 1) >> 1, b3);
                row[x] = b3;
                println!("row[{}] changed from {} to {}", x, old_val, b3);
                x += 2 * s;
            }

            // Generic case: main loop
            println!("Main loop starting at x={}", x);
            println!("Main loop condition: x + s3 < w → {} + {} < {} → {} < {} → {}", 
                     x, s3, w, x + s3, w, x + s3 < w);
            
            while x + s3 < w {
                println!("Main loop iteration: x={}", x);
                let a0 = a1;
                a1 = a2;
                a2 = a3;
                a3 = row[x + s3];
                let b0 = b1;
                b1 = b2;
                b2 = b3;
                let a_sum = a1 + a2;
                let old_val = row[x];
                b3 = row[x] - (((a_sum << 3) + a_sum - a0 - a3 + 8) >> 4);
                row[x] = b3;
                println!("  High-pass: row[{}] = {} → {}", x, old_val, b3);
                
                let b_sum = b1 + b2;
                let update_pos = x - s3;
                let old_low = row[update_pos];
                row[update_pos] += ((b_sum << 3) + b_sum - b0 - b3 + 16) >> 5;
                println!("  Low-pass update: row[{}] = {} → {} (added {})", 
                         update_pos, old_low, row[update_pos], 
                         ((b_sum << 3) + b_sum - b0 - b3 + 16) >> 5);
                x += 2 * s;
            }

            // Special case: near end (w - 3*s <= x < w)
            println!("Near-end loop starting at x={}", x);
            while x < w {
                println!("Near-end iteration: x={}", x);
                a1 = a2;
                a2 = a3;
                a3 = a1; // Per C++ reference
                let b0 = b1;
                b1 = b2;
                b2 = b3;
                let old_val = row[x];
                b3 = row[x] - ((a1 + a2 + 1) >> 1);
                row[x] = b3;
                println!("  High-pass: row[{}] = {} → {}", x, old_val, b3);
                
                let b_sum = b1 + b2;
                if x >= s3 {
                    let update_pos = x - s3;
                    let old_low = row[update_pos];
                    row[update_pos] += ((b_sum << 3) + b_sum - b0 - b3 + 16) >> 5;
                    println!("  Low-pass update: row[{}] = {} → {} (added {})", 
                             update_pos, old_low, row[update_pos],
                             ((b_sum << 3) + b_sum - b0 - b3 + 16) >> 5);
                } else {
                    println!("  No low-pass update (x < s3: {} < {})", x, s3);
                }
                x += 2 * s;
            }

            // Additional updates beyond w
            let mut x_update = x;
            println!("Additional updates starting at x_update={}", x_update);
            while x_update - s3 < w {
                println!("Additional update iteration: x_update={}", x_update);
                let b0 = b1;
                b1 = b2;
                b2 = b3;
                b3 = 0;
                let b_sum = b1 + b2;
                if x_update >= s3 {
                    let update_pos = x_update - s3;
                    let old_low = row[update_pos];
                    row[update_pos] += ((b_sum << 3) + b_sum - b0 - b3 + 16) >> 5;
                    println!("  Final low-pass update: row[{}] = {} → {} (added {})", 
                             update_pos, old_low, row[update_pos],
                             ((b_sum << 3) + b_sum - b0 - b3 + 16) >> 5);
                }
                x_update += 2 * s;
            }
            
            println!("Final row values: {:?}", row);
            println!();
        }
    }
}
