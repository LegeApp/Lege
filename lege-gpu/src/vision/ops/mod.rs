//! GPU op implementations: WGSL shader sources plus the build-time
//! [`compile::op_steps`] descriptors the resident executor
//! (`runtime::compiled`) dispatches from.
//!
//! A few ops also keep an async `run_*` wrapper (upload → dispatch → readback
//! of a single op) — those exist solely as GPU-vs-CPU-reference test
//! harnesses; production inference never goes through them.

pub(crate) mod activations;
pub(crate) mod common;
pub(crate) mod compile;
pub(crate) mod concat;
pub(crate) mod conv;
pub(crate) mod deskew;
pub(crate) mod elemwise;
pub(crate) mod gemm;
pub(crate) mod globalavgpool;
pub(crate) mod matmul;
pub(crate) mod maxpool;
pub(crate) mod resize;
pub(crate) mod sauvola;
pub(crate) mod sigmoid;
pub(crate) mod slice;
pub(crate) mod softmax;
pub(crate) mod split;
pub(crate) mod transpose;
pub(crate) mod winograd;
