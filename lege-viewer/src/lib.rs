//! Lege viewer skeleton.
//!
//! This crate intentionally contains application-specific document viewing
//! machinery rather than a reusable widget framework. The renderer is treated
//! as a document engine: semantic, text, compiled-IR, and tile artifacts all
//! remain visible to the viewer.

pub mod app;
pub mod chrome;
pub mod cli;
pub mod damage;
pub mod diagnostics;
pub mod document;
pub mod event;
pub mod frame;
pub mod geometry;
pub mod input;
pub mod paint;
pub mod present;
pub mod processing;
pub mod scene;
pub mod scroll;
mod settings;
pub mod text;
pub mod theme;
pub mod trace;
pub mod ui;

pub use app::{ViewerApp, ViewerStartError};
pub use document::{DocumentId, PageIndex};
