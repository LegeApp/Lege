//! The legencode prelude for easy importing of common types.

pub use crate::streamline::{
    EncodingManager,
    EncodingResult,
    EncodingSettings,
    ImageBuffer,
    Jbig2Settings,
    Jp2Settings,
    JpegSettings,
    // Indexed8Settings,  // Removed - indexed8 encoder deleted
};
pub use crate::{EncodingError, Result};
