//! JPEG 2000 encoder entry point.

pub use crate::jp2_encoder::{
    Jp2Settings, encode, encode_gray, encode_rgb, encode_to_target_size, encode_with_quality,
    jp2_config,
};
