//! Agent-facing structured PDF service used by the `lege-pdf` binary and
//! `lege-pdf serve --stdio`.

pub mod bounds;
pub mod cache;
pub mod commands;
pub mod open;
pub mod pages;
pub mod schema;
pub mod views;

pub use bounds::Bounds;
pub use cache::SnapshotCache;
pub use open::{DocumentIdentity, open_document};
pub use pages::{PageZero, parse_page_range};
pub use schema::{Envelope, OutputMode, SCHEMA_ID, Status};
