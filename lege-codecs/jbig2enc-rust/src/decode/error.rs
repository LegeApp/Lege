//! Decoder error types.
//!
//! The concrete error definitions live in [`crate::shared::error`] so that the
//! shared bitmap/reader (used by both halves) can reference them without
//! depending on the `decode` feature (jbig2dec-gaps-plan.md Gap A: "when
//! encode/decode feature gating lands, `ParseError` moves to shared"). This
//! module re-exports them so decoder code keeps using `crate::decode::error::*`.

pub use crate::shared::error::{
    DecodeError, LimitError, ParseError, UnsupportedFeature, usize_from_u32,
};
