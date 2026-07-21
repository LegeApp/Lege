//! Protocol definitions shared by the encoder and decoder halves:
//! segment syntax, the MQ probability table, arithmetic procedure
//! identifiers, packed bitmap conventions, and combination operators.
//!
//! Populated by the Phase 0 refactor (see `jbig2decplan.md` §23 and
//! `jbig2dec-gaps-plan.md` Gap A). Encoder/decoder *state machines* never
//! live here.

pub mod bitmap;
pub mod error;
pub mod int_proc;
pub mod limits;
pub mod mq_table;
pub mod reader;
pub mod segment;
