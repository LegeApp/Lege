mod backend;
// Superseded by the tile-aware, bounded-memory "stored" Tier-1 pipeline
// (`backend::prepare_stored_tier1_for_tile_rect` and
// `t1::encode_component_coefficients_for_tile_with_max_bitplanes`). Kept
// under `#[cfg(test)]` as a tested reference implementation of the
// pre-Phase-5 layout/analyze/encode/packet path.
#[cfg(test)]
mod layout;
mod rate;
mod t1;
mod t2;

pub(crate) use backend::{NativeBackend, NativeComponentCoefficients};

pub(crate) use t1::{NativeEncodedTier1Layout, NativeEncodedTier1Pass};
