//! Simple BZZ compression test

use djvu_encoder::iff::bs_byte_stream::bzz_compress;

#[test]
fn test_bzz_simple_compress() {
    // Create simple test data like DIRM would have
    let mut data = Vec::new();
    
    // Sizes (3 bytes each)
    data.extend_from_slice(&[0x00, 0x04, 0xBE]); // 1214 bytes
    data.extend_from_slice(&[0x00, 0x04, 0xC6]); // 1222 bytes
    data.extend_from_slice(&[0x00, 0x08, 0xCC]); // 2252 bytes
    
    // Flags
    data.push(0x01); // page
    data.push(0x01); // page
    data.push(0x01); // page
    
    // IDs (null terminated)
    data.extend_from_slice(b"p0001\0");
    data.extend_from_slice(b"p0002\0");
    data.extend_from_slice(b"p0003\0");
    
    println!("Input data ({} bytes):", data.len());
    for (i, chunk) in data.chunks(16).enumerate() {
        print!("{:04x}: ", i * 16);
        for b in chunk {
            print!("{:02x} ", b);
        }
        println!();
    }
    
    // Compress
    let compressed = bzz_compress(&data, 50).expect("BZZ compression failed");
    
    println!("\nCompressed data ({} bytes):", compressed.len());
    for (i, chunk) in compressed.chunks(16).enumerate() {
        print!("{:04x}: ", i * 16);
        for b in chunk {
            print!("{:02x} ", b);
        }
        println!();
    }
    
    // The compressed data should be decodable by DjVuLibre
    // Let's write it to a file and try to decode with ddjvu
    use std::fs;
    use std::process::Command;
    
    // Create a minimal DJVM with just DIRM to test
    let mut djvm = Vec::new();
    djvm.extend_from_slice(b"AT&TFORM");
    // Size placeholder
    let size_pos = djvm.len();
    djvm.extend_from_slice(&[0, 0, 0, 0]);
    djvm.extend_from_slice(b"DJVM");
    
    // DIRM chunk
    djvm.extend_from_slice(b"DIRM");
    let dirm_data_len = 3 + 12 + compressed.len(); // header + offsets + bzz
    djvm.extend_from_slice(&(dirm_data_len as u32).to_be_bytes());
    
    // DIRM header
    djvm.push(0x81); // version 1, bundled
    djvm.extend_from_slice(&(3u16).to_be_bytes()); // 3 files
    
    // Offsets (dummy, just for structure test)
    djvm.extend_from_slice(&100u32.to_be_bytes());
    djvm.extend_from_slice(&200u32.to_be_bytes());
    djvm.extend_from_slice(&300u32.to_be_bytes());
    
    // BZZ data
    djvm.extend_from_slice(&compressed);
    
    // Padding
    if dirm_data_len % 2 != 0 {
        djvm.push(0);
    }
    
    // Update size
    let form_size = (djvm.len() - 12) as u32;
    djvm[size_pos..size_pos + 4].copy_from_slice(&form_size.to_be_bytes());
    
    let test_file = "/tmp/bzz_dirm_test.djvu";
    fs::write(test_file, &djvm).expect("Failed to write test file");
    
    println!("\nWrote test file to: {}", test_file);
    println!("File size: {} bytes", djvm.len());
    
    // Try djvudump
    println!("\n=== djvudump output ===");
    let output = Command::new("djvudump")
        .arg(test_file)
        .output()
        .expect("Failed to run djvudump");
    
    println!("{}", String::from_utf8_lossy(&output.stdout));
    if !output.stderr.is_empty() {
        println!("STDERR: {}", String::from_utf8_lossy(&output.stderr));
    }
}
