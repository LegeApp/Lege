//! lege-pdf-write: a typed, append-only, image-oriented PDF emitter.
//!
//! Not a general PDF library. It accepts `PdfPageArtifact`s in arbitrary
//! completion order, writes their objects immediately (arrival-order object
//! IDs), and at finalization writes the page tree, outlines, metadata, xref,
//! and trailer. No parser, no editing, no encryption, no incremental update,
//! no linearization. See ../../PLAN.md for the module contract.

pub mod artifact;
pub mod content;
pub mod font;
pub mod images;
pub mod meta;
pub mod outline;
pub mod pages;
pub mod resources;
pub mod serialize;
pub mod sink;
pub mod text;
pub mod types;
pub mod writer;
pub mod xref;
