//! Lege viewer skeleton.
//!
//! This crate intentionally contains application-specific document viewing
//! machinery rather than a reusable widget framework. The renderer is treated
//! as a document engine: semantic, text, compiled-IR, and tile artifacts all
//! remain visible to the viewer.

pub mod app;
pub mod chrome;
pub mod damage;
pub mod diagnostics;
pub mod document;
pub mod event;
pub mod frame;
pub mod geometry;
pub mod input;
pub mod paint;
pub mod present;
pub mod scroll;
pub mod text;
pub mod trace;
pub mod theme;

pub use app::ViewerApp;
pub use document::{DocumentId, PageIndex};
