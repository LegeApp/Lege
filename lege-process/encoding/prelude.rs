//! Convenient imports for Lege's encoding API.

#[cfg(feature = "jp2-lam")]
pub use crate::encoding::streamline::Jp2Settings;
pub use crate::encoding::streamline::{
    EncodingManager,
    EncodingResult,
    EncodingSettings,
    ImageBuffer,
    Jbig2Mode,
    Jbig2Settings,
    JpegSettings,
    // Indexed8Settings,  // Removed - indexed8 encoder deleted
};
pub use crate::encoding::{EncodingError, Result};
