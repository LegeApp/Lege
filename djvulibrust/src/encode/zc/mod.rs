#[cfg(feature = "asm_zp")]
pub mod asm;
pub mod table;
pub mod zcodec;

// Keep BitContext and errors/types from the Rust implementation for a unified API
pub use zcodec::BitContext;
pub use zcodec::ZCodecError;

// Always export the Rust ZEncoder by default
pub use zcodec::ZEncoder;

use std::io::Cursor;

/// A minimal trait to abstract over ZP encoders that write into a Cursor<Vec<u8>>.
/// This lets IW44 pick either the Rust or Assembly implementation without
/// disturbing other parts of the codebase (e.g., JB2, BZZ) which remain on Rust.
pub trait ZpEncoderCursor {
    fn encode(&mut self, bit: bool, ctx: &mut BitContext) -> Result<(), ZCodecError>;
    fn iwencoder(&mut self, bit: bool) -> Result<(), ZCodecError>;
    fn encode_raw_bit(&mut self, bit: bool) -> Result<(), ZCodecError>;
    fn tell_bytes(&self) -> usize;
    fn finish(self) -> Result<Cursor<Vec<u8>>, ZCodecError>
    where
        Self: Sized;
}
