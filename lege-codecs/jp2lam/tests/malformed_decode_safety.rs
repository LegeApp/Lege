//! Deterministic malformed-input smoke corpus suitable for ordinary tests and Miri.

use jp2lam::{EncodeOptions, Image, decode_jp2, encode};

fn encoded_fixture() -> Vec<u8> {
    let samples = (0..16 * 16)
        .map(|index| (index as u8).wrapping_mul(29))
        .collect::<Vec<_>>();
    let image = Image::from_gray_bytes(16, 16, &samples).expect("fixture image");
    encode(
        &image,
        &EncodeOptions {
            quality: 100,
            ..EncodeOptions::default()
        },
    )
    .expect("fixture encode")
}

#[test]
fn deterministic_truncations_and_bit_flips_never_panic() {
    let encoded = encoded_fixture();
    let mut candidates = Vec::new();
    for length in (0..encoded.len()).step_by((encoded.len() / 32).max(1)) {
        candidates.push(encoded[..length].to_vec());
    }
    for index in (0..encoded.len()).step_by((encoded.len() / 64).max(1)) {
        let mut mutated = encoded.clone();
        mutated[index] ^= 0x80;
        candidates.push(mutated);
    }

    for (index, candidate) in candidates.into_iter().enumerate() {
        let outcome = std::panic::catch_unwind(|| decode_jp2(&candidate));
        assert!(outcome.is_ok(), "malformed candidate {index} panicked");
    }
}
