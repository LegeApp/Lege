//! The encoder half of the crate.
//!
//! Modules were renamed from their historical `jbig2*` prefixes as part of
//! the Phase 0 restructure (see `jbig2dec-gaps-plan.md`, Gap A). The old
//! top-level paths remain available through compatibility re-exports in
//! `lib.rs`.

pub mod api;
pub mod arith;
#[cfg(feature = "symboldict")]
pub mod cc;
pub mod classify;
pub mod comparator;
pub mod context;
pub mod cost;
pub mod document;
pub mod halftone;
pub mod shared;
pub(crate) mod simd;
pub mod structs;
pub mod sym;
pub mod unify;
