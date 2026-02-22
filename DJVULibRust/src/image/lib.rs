// src/lib.rs
pub mod iw44 {
    pub mod encoder;
    mod coeff_map;
    mod codec;
    mod constants;
    mod transform;

    // Placeholder for a real bit-level I/O library
    // This mock allows the codec logic to be written.
    pub mod zp_codec_mock {
        use super::codec::BitContext;

        pub struct ZPCodec;
        impl ZPCodec {
            pub fn create_encoder() -> Self { ZPCodec }
            pub fn encoder(&mut self, _bit: bool, _context: &mut BitContext) {}
            pub fn iw_encoder(&mut self, _bit: bool) {}
            pub fn finish(self) -> Vec<u8> { vec![] }
        }
    }
}