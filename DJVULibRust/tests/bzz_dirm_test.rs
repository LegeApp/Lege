//! Test BZZ compression on DIRM-like data

use djvu_encoder::iff::bs_byte_stream::bzz_compress;
use std::fs;
use std::process::Command;

#[test]
fn test_bzz_dirm_data() {
    // Simulate DIRM data for 3 pages:
    // - 3 sizes (3 bytes each = 9 bytes)
    // - 3 flags (1 byte each = 3 bytes)
    // - 3 IDs with null terminators

    let mut dirm_data = Vec::new();

    // Sizes (INT24 big-endian)
    // Let's use actual sizes from the test
    let sizes: [u32; 3] = [0x04be, 0x0560, 0x0e2c]; // Example sizes
    for size in sizes {
        dirm_data.push((size >> 16) as u8);
        dirm_data.push((size >> 8) as u8);
        dirm_data.push(size as u8);
    }

    // Flags (all pages = 0x01)
    dirm_data.extend_from_slice(&[0x01, 0x01, 0x01]);

    // IDs (null-terminated)
    for i in 1..=3 {
        let id = format!("p{:04}.djvu", i);
        dirm_data.extend_from_slice(id.as_bytes());
        dirm_data.push(0); // null terminator
    }

    println!("DIRM data to compress ({} bytes):", dirm_data.len());
    for (i, chunk) in dirm_data.chunks(16).enumerate() {
        print!("{:04x}: ", i * 16);
        for b in chunk {
            print!("{:02x} ", b);
        }
        print!("  ");
        for b in chunk {
            let c = if *b >= 0x20 && *b < 0x7f {
                *b as char
            } else {
                '.'
            };
            print!("{}", c);
        }
        println!();
    }

    // Compress with our BZZ
    let compressed = bzz_compress(&dirm_data, 50).expect("BZZ compression failed");

    println!("\nCompressed ({} bytes):", compressed.len());
    for (i, chunk) in compressed.chunks(16).enumerate() {
        print!("{:04x}: ", i * 16);
        for b in chunk {
            print!("{:02x} ", b);
        }
        println!();
    }

    // Write to file and test with bzz -d
    let bzz_file = "/tmp/test_dirm.bzz";
    let decoded_file = "/tmp/test_dirm_decoded.bin";

    fs::write(bzz_file, &compressed).unwrap();

    let result = Command::new("bzz")
        .args(["-d", bzz_file, decoded_file])
        .output()
        .expect("Failed to run bzz");

    if result.status.success() {
        let decoded = fs::read(decoded_file).unwrap();
        println!("\n✓ Decoded successfully ({} bytes)", decoded.len());
        if decoded == dirm_data {
            println!("✓ Round-trip matches!");
        } else {
            println!("✗ Round-trip mismatch!");
            println!("Original: {:?}", &dirm_data);
            println!("Decoded:  {:?}", &decoded);
        }
    } else {
        println!("\n✗ Decode failed:");
        println!("STDERR: {}", String::from_utf8_lossy(&result.stderr));
        panic!("BZZ decode failed");
    }

    // Also test with DjVuLibre's bzz to compress and compare
    let ref_input = "/tmp/test_dirm_input.bin";
    let ref_bzz = "/tmp/test_dirm_ref.bzz";

    fs::write(ref_input, &dirm_data).unwrap();

    let status = Command::new("bzz")
        .args(["-e", ref_input, ref_bzz])
        .status()
        .expect("Failed to run bzz -e");

    if status.success() {
        let reference = fs::read(ref_bzz).unwrap();
        println!("\nReference BZZ ({} bytes):", reference.len());
        for (i, chunk) in reference.chunks(16).enumerate() {
            print!("{:04x}: ", i * 16);
            for b in chunk {
                print!("{:02x} ", b);
            }
            println!();
        }

        if reference == compressed {
            println!("\n✓ PERFECT MATCH with DjVuLibre!");
        } else {
            println!("\n✗ Differs from DjVuLibre reference");
        }
    }
}
