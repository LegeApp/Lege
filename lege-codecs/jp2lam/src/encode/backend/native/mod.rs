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

pub(crate) use backend::{
    NativeBackend, NativeComponentCoefficients, UnquantizedComponentDwt, UnquantizedTileDwt,
    build_stored_tile_parts, complete_output_len, max_stored_body_bytes, select_stored_tile_passes,
};
#[cfg(test)]
pub(crate) use backend::{
    forward_dwt_call_count, reset_forward_dwt_calls, unquantized_dwt_cache_fits,
    unquantized_dwt_retention_bytes,
};

pub(crate) use t1::{NativeEncodedTier1Layout, NativeEncodedTier1Pass, NativeTier1SelectionLayout};
