// examples/debug_minimal_filter.rs
//
// Minimal test to debug the filter arithmetic step by step

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== Minimal Filter Debug ===\n");

    // Test with a tiny 4x4 constant array to trace exactly what happens
    let w = 4;
    let h = 4;
    let constant_val = -6784i32;
    
    let mut data = vec![constant_val; w * h];
    println!("Initial 4x4 array (all values = {}):", constant_val);
    print_array(&data, w, h);
    
    // Test horizontal filter with scale=1
    println!("Applying horizontal filter (scale=1)...");
    filter_fh_debug(&mut data, w, h, w, 1);
    print_array(&data, w, h);
    
    // Check if any values changed
    let mut changed_count = 0;
    for (i, &val) in data.iter().enumerate() {
        if val != constant_val {
            println!("Position {} changed from {} to {}", i, constant_val, val);
            changed_count += 1;
        }
    }
    
    if changed_count == 0 {
        println!("✓ No values changed in horizontal filter - this is expected for constant input");
    } else {
        println!("❌ {} values changed - this indicates a problem", changed_count);
    }
    
    Ok(())
}

fn print_array(data: &[i32], w: usize, h: usize) {
    for y in 0..h {
        for x in 0..w {
            print!("{:8} ", data[y * w + x]);
        }
        println!();
    }
    println!();
}

fn filter_fh_debug(buf: &mut [i32], w: usize, h: usize, rowsize: usize, scale: usize) {
    let s = scale;
    let s3 = 3 * s;
    
    println!("filter_fh_debug: w={}, h={}, rowsize={}, scale={}", w, h, rowsize, scale);
    println!("s={}, s3={}", s, s3);
    
    for y in (0..h).step_by(s) {
        println!("\nProcessing row y={}", y);
        
        let row_start = y * rowsize;
        let row = &mut buf[row_start..row_start + w];
        let mut x = s;
        
        println!("Initial row values: {:?}", &row[0..w]);
        
        let mut a1 = row[0];
        let mut a2 = row[0];
        let mut a3 = row[0];
        
        let mut b1 = 0_i32;
        let mut b2 = 0_i32;
        let mut b3 = 0_i32;
        
        println!("Initial a1={}, a2={}, a3={}", a1, a2, a3);

        // Special case: first element (x = s)
        println!("First element: x={}", x);
        if x < w {
            a2 = if x + s < w { row[x + s] } else { a1 };
            a3 = if x + s3 < w { row[x + s3] } else { a1 };
            
            println!("a2 = row[{}] = {} (or a1 if out of bounds)", x + s, a2);
            println!("a3 = row[{}] = {} (or a1 if out of bounds)", x + s3, a3);
            
            let old_val = row[x];
            b3 = row[x] - ((a1 + a2 + 1) >> 1);
            row[x] = b3;
            
            println!("b3 = {} - (({} + {} + 1) >> 1) = {} - {} = {}", 
                     old_val, a1, a2, old_val, (a1 + a2 + 1) >> 1, b3);
            println!("row[{}] changed from {} to {}", x, old_val, b3);
            
            x += 2 * s;
        }

        // Generic case: main loop  
        println!("Main loop starting at x={}", x);
        while x + s3 < w {
            println!("Generic case: x={}", x);
            
            let a0 = a1;
            a1 = a2;
            a2 = a3;
            a3 = row[x + s3];
            let b0 = b1;
            b1 = b2;
            b2 = b3;
            
            println!("Updated: a0={}, a1={}, a2={}, a3={}", a0, a1, a2, a3);
            println!("Updated: b0={}, b1={}, b2={}, b3(old)={}", b0, b1, b2, b3);
            
            let old_val = row[x];
            let a_sum = a1 + a2;
            b3 = row[x] - (((a_sum << 3) + a_sum - a0 - a3 + 8) >> 4);
            row[x] = b3;
            
            println!("a_sum = {} + {} = {}", a1, a2, a_sum);
            println!("delta = ((({} << 3) + {} - {} - {} + 8) >> 4) = {}", 
                     a_sum, a_sum, a0, a3, ((a_sum << 3) + a_sum - a0 - a3 + 8) >> 4);
            println!("b3 = {} - {} = {}", old_val, ((a_sum << 3) + a_sum - a0 - a3 + 8) >> 4, b3);
            
            // Update previous coefficient
            if x >= s3 {
                let update_idx = x - s3;
                let old_update_val = row[update_idx];
                let b_sum = b1 + b2;
                let update_delta = ((b_sum << 3) + b_sum - b0 - b3 + 16) >> 5;
                row[update_idx] += update_delta;
                
                println!("Updating row[{}]: {} + {} = {}", 
                         update_idx, old_update_val, update_delta, row[update_idx]);
            }
            
            x += 2 * s;
        }
        
        println!("Final row values: {:?}", &row[0..w]);
    }
}
