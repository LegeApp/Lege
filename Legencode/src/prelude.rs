//! The legencode prelude for easy importing of common types.

#[cfg(feature = "jp2-lam")]
pub use crate::streamline::Jp2Settings;
pub use crate::streamline::{
    EncodingManager,
    EncodingResult,
    EncodingSettings,
    ImageBuffer,
    Jbig2Mode,
    Jbig2Settings,
    JpegSettings,
    // Indexed8Settings,  // Removed - indexed8 encoder deleted
};
pub use crate::{EncodingError, Result};
