//! JB2 (JBIG2-like) bilevel image compression for DjVu.
//!
//! ## Architecture
//!
//! Three-stage pipeline:
//! 1. **Connected component analysis** (`cc_image`) - Extract shapes from page
//! 2. **Dictionary building** (user code + `symbol_dict::Comparator`) - Match/refine shapes
//! 3. **Encoding** (`encoder`) - Emit DjVu-compatible JB2 bitstream
//!
//! ## Module Map
//!
//! - `cc_image` - cjb2-based CC analysis (run-length + union-find)
//! - `symbol_dict` - BitImage, Comparator, SharedDict
//! - `encoder` - JB2Encoder with all 12 DjVu record types
//! - `num_coder` - Tree-based integer coder (DjVuLibre-compatible)
//! - `error` - Error types

pub mod cc_image;
pub mod encoder;
pub mod error;
pub mod num_coder;
pub mod symbol_dict;

pub use encoder::JB2Encoder;
pub use cc_image::{analyze_page, shapes_to_encoder_format, BBox, CCImage, CC, Run};
pub use symbol_dict::{BitImage, Rect, Comparator, SharedDict};
