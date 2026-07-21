// src/jb2/error.rs
use crate::encode::zc::ZCodecError;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum Jb2Error {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Z codec error: {0:?}")]
    ZCodec(#[from] ZCodecError),

    #[error("Invalid number encountered during encoding: {0}")]
    InvalidNumber(String),

    #[error("Invalid data: {0}")]
    InvalidData(String),

    #[error("Invalid parent shape index provided")]
    InvalidParentShape,

    #[error("Attempted to encode a blit with an invalid shape index: {0}")]
    InvalidBlitShapeIndex(u32),

    #[error("An empty or uninitialized JB2 object cannot be encoded")]
    EmptyObject,

    #[error("Bad number range or invalid data: {0}")]
    BadNumber(String),

    #[error("Context overflow - too many contexts allocated")]
    ContextOverflow,

    #[error("Invalid bitmap dimensions or malformed data")]
    InvalidBitmap,

    #[error("Invalid encoder state: {0}")]
    InvalidState(String),
}
