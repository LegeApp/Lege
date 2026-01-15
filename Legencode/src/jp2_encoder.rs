//! JP2 encoder module - wraps the openjp2 implementation

// Include the openjp2 library as a submodule
#[path = "openjp2/src/lib.rs"]
mod openjp2_impl;

// Re-export the main types and functions we need
pub use openjp2_impl::{
    Codec,
    CODEC_FORMAT,
    opj_image,
    opj_image_comptparm,
    opj_cparameters_t,
    Stream,
    OPJ_CLRSPC_SRGB,
    OPJ_CLRSPC_GRAY,
    openjpeg::opj_set_default_encoder_parameters,
};

// Re-export error handling
pub use crate::EncodingError;
