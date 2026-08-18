//! Document catalog emission.
//!
//! The catalog object is written from `pages.rs` (it is emitted with the page
//! tree at finalization). This module exists so the conventional
//! `lege-pdf/write/src/catalog.rs` path resolves without hunting through
//! `pages.rs`.

pub use crate::pages::{CatalogExtras, write_catalog};
