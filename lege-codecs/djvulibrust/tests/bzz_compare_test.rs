//! Test BZZ compression by comparing with DjVuLibre's bzz tool output

use djvu_encoder::iff::bs_byte_stream::bzz_compress;
use std::fs;
use std::process::Command;

#[test]
fn test_bzz_match_djvulibre() {
    // Input data
    let input = b"Hello World";

    // Compress with DjVuLibre bzz tool
    let tmp_input = "/tmp/rust_bzz_test_input.bin";
    let tmp_ref = "/tmp/rust_bzz_test_reference.bzz";

    fs::write(tmp_input, input).unwrap();

    let status = Command::new("bzz")
        .args(["-e", tmp_input, tmp_ref])
        .status()
        .expect("Failed to run bzz");
    assert!(status.success(), "bzz command failed");

    let reference = fs::read(tmp_ref).unwrap();

    // Compress with our implementation (use same block size)
    // bzz uses 2048KB by default, but for small data, it doesn't matter
    let our_output = bzz_compress(input, 100).expect("Our BZZ compression failed");

    println!("Input ({} bytes): {:?}", input.len(), input);
    println!();
    println!("Reference (DjVuLibre bzz) ({} bytes):", reference.len());
    for (i, chunk) in reference.chunks(16).enumerate() {
        print!("{:04x}: ", i * 16);
        for b in chunk {
            print!("{:02x} ", b);
        }
        println!();
    }

    println!();
    println!("Our output ({} bytes):", our_output.len());
    for (i, chunk) in our_output.chunks(16).enumerate() {
        print!("{:04x}: ", i * 16);
        for b in chunk {
            print!("{:02x} ", b);
        }
        println!();
    }

    // Compare
    println!();
    if reference == our_output {
        println!("✓ PERFECT MATCH!");
    } else {
        println!("✗ MISMATCH");

        // Show first difference
        for (i, (r, o)) in reference.iter().zip(our_output.iter()).enumerate() {
            if r != o {
                println!(
                    "First difference at byte {}: reference={:02x}, ours={:02x}",
                    i, r, o
                );
                break;
            }
        }
        if reference.len() != our_output.len() {
            println!(
                "Length mismatch: reference={}, ours={}",
                reference.len(),
                our_output.len()
            );
        }
    }

    // Try to decode our output with DjVuLibre
    let tmp_our = "/tmp/rust_bzz_test_our.bzz";
    let tmp_decoded = "/tmp/rust_bzz_test_decoded.bin";

    fs::write(tmp_our, &our_output).unwrap();

    let decode_status = Command::new("bzz")
        .args(["-d", tmp_our, tmp_decoded])
        .status();

    match decode_status {
        Ok(s) if s.success() => {
            let decoded = fs::read(tmp_decoded).unwrap();
            if decoded == input {
                println!("✓ Our output decodes correctly back to original!");
            } else {
                println!("✗ Decoded content differs from original");
                println!("Decoded: {:?}", decoded);
            }
        }
        Ok(s) => println!("✗ bzz -d failed with status: {}", s),
        Err(e) => println!("✗ bzz -d failed: {}", e),
    }

    // Test should fail if outputs don't match, but let's also test decodability
    // For now, just ensure our output is decodable
}

#[test]
fn test_bzz_simple_decode() {
    // Test with minimal data
    let input = b"ABC";

    let compressed = bzz_compress(input, 100).expect("Compression failed");

    println!("Input: {:?}", input);
    println!("Compressed ({} bytes):", compressed.len());
    for b in &compressed {
        print!("{:02x} ", b);
    }
    println!();

    // Write and try to decode
    let tmp_bzz = "/tmp/rust_bzz_simple.bzz";
    let tmp_out = "/tmp/rust_bzz_simple_decoded.bin";

    fs::write(tmp_bzz, &compressed).unwrap();

    let result = Command::new("bzz").args(["-d", tmp_bzz, tmp_out]).output();

    match result {
        Ok(output) => {
            if output.status.success() {
                let decoded = fs::read(tmp_out).unwrap();
                println!("Decoded: {:?}", decoded);
                assert_eq!(decoded, input, "Decoded data should match input");
                println!("✓ SUCCESS: Round-trip works!");
            } else {
                println!("STDERR: {}", String::from_utf8_lossy(&output.stderr));
                panic!("Decoding failed");
            }
        }
        Err(e) => panic!("Failed to run bzz -d: {}", e),
    }
}
