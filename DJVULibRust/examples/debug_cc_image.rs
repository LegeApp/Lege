use djvu_encoder::encode::jb2::cc_image::{analyze_page, CCImage};
use djvu_encoder::encode::jb2::symbol_dict::BitImage;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== Debugging CC Image Analysis ===\n");
    
    let width = 800u32;
    let height = 600u32;
    let mut mask = BitImage::new(width, height)?;
    
    // Draw title-like pattern at top
    for row in 30..80 {
        for col in 100..700 {
            if row > 35 && row < 75 && (col % 25 < 18) {
                mask.set_usize(col, row, true);
            }
        }
    }
    
    // Draw multiple "text lines" - using unambiguous boxes
    for line_num in 0..8 {
        let y_offset = 150 + (line_num * 50);
        let x_offset = 80;
        
        // Create "words" (simple rectangular blocks)
        for word in 0..6 {
            let word_x = x_offset + (word * 110);
            let word_width = 80;
            
            // Draw a solid rectangle for each word
            for row in 0..30 {
                for col in 0..word_width {
                    let x = word_x + col;
                    let y = y_offset + row;
                    
                    if x < width as usize && y < height as usize {
                        if !(row > 10 && row < 20 && col > 10 && col < 70) {
                            mask.set_usize(x, y, true);
                        }
                    }
                }
            }
        }
    }

    println!("Image created. Analyzing...");
    
    let dpi = 300;
    let losslevel = 1;
    
    let mut ccimg = CCImage::new(mask.width as i32, mask.height as i32, dpi);
    ccimg.add_bitmap_runs(&mask);
    println!("Runs: {}", ccimg.runs.len());

    ccimg.make_ccids_by_analysis();
    ccimg.make_ccs_from_ccids();
    println!("Initial CCs: {}", ccimg.ccs.len());
    
    // Dump initial CCs positions
    /*
    for (i, cc) in ccimg.ccs.iter().enumerate() {
        println!("CC {}: {:?}", i, cc.bb);
    }
    */

    if losslevel > 0 {
        ccimg.erase_tiny_ccs();
        println!("CCs after erase_tiny_ccs: {} (active)", ccimg.ccs.iter().filter(|c| c.nrun > 0).count());
    }

    ccimg.merge_and_split_ccs();
    println!("CCs after merge_and_split_ccs: {}", ccimg.ccs.len());
    
    ccimg.sort_in_reading_order();
    println!("CCs after sort: {}", ccimg.ccs.len());

    let shapes = ccimg.extract_shapes();
    println!("Final extracted shapes: {}", shapes.len());

    // Print Y coordinates of the first 10 shapes
    for (i, (_bm, bb)) in shapes.iter().enumerate().take(60) {
        println!("Shape {}: ymin={}, ymax={}, xmin={}", i, bb.ymin, bb.ymax, bb.xmin);
    }

    Ok(())
}
