//! JPEG 2000 encoder entry point.

pub use crate::encoding::jp2::{
    Jp2Settings, encode, encode_gray, encode_rgb, encode_to_target_size, encode_with_quality,
    jp2_config,
};
