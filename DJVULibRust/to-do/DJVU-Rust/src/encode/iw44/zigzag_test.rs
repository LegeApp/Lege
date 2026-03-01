/// Test for verifying zigzag coefficient ordering
#[test]
fn test_zigzag_roundtrip() {
    use super::*;
    
    // Create a test liftblock with known pattern
    let mut original_liftblock = [0i16; 1024];
    
    // Set a few test coefficients at known positions
    original_liftblock[0] = 100;     // DC coefficient
    original_liftblock[1] = 50;      // First AC coefficient  
    original_liftblock[32] = 25;     // Another known position
    original_liftblock[256] = 12;    // Another known position
    
    // Create a block and read the liftblock
    let mut block = Block::default();
    block.read_liftblock(&original_liftblock);
    
    // Write it back to a new liftblock
    let mut reconstructed_liftblock = [0i16; 1024];
    block.write_liftblock(&mut reconstructed_liftblock);
    
    // Verify round-trip integrity
    assert_eq!(original_liftblock, reconstructed_liftblock,
        "Zigzag round-trip failed: original != reconstructed");
    
    // Check specific test values
    assert_eq!(reconstructed_liftblock[0], 100, "DC coefficient mismatch");
    assert_eq!(reconstructed_liftblock[1], 50, "First AC coefficient mismatch");
    assert_eq!(reconstructed_liftblock[32], 25, "Position 32 mismatch");
    assert_eq!(reconstructed_liftblock[256], 12, "Position 256 mismatch");
    
    println!("✓ Zigzag round-trip test passed");
}

/// Test zigzag ordering against known C++ reference
#[test] 
fn test_zigzag_reference_values() {
    use super::*;
    
    // Test first few zigzag positions against known C++ values
    let expected_first_16 = [
        0, 16, 512, 528, 8, 24, 520, 536, 256, 272, 768, 784, 264, 280, 776, 792
    ];
    
    for (i, &expected) in expected_first_16.iter().enumerate() {
        assert_eq!(ZIGZAG_LOC[i], expected,
            "Zigzag position {} should be {}, got {}", i, expected, ZIGZAG_LOC[i]);
    }
    
    println!("✓ Zigzag reference values test passed");
}
