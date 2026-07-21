use crate::mq::T1_CTXNO_MAG;
use crate::plan::BandOrientation;

use super::consts::{
    T1_LUT_SGN_E, T1_LUT_SGN_N, T1_LUT_SGN_S, T1_LUT_SGN_W, T1_LUT_SIG_E, T1_LUT_SIG_N,
    T1_LUT_SIG_S, T1_LUT_SIG_W, T1_SIGMA_NEIGHBOURS,
};
use super::luts::{LUT_CTXNO_SC, LUT_CTXNO_ZC, LUT_SPB};

#[inline]
pub(crate) fn orientation_index(band: BandOrientation) -> usize {
    match band {
        BandOrientation::Ll => 0,
        BandOrientation::Hl => 1,
        BandOrientation::Lh => 2,
        BandOrientation::Hh => 3,
    }
}

#[inline]
pub(crate) fn zero_coding_context(band: BandOrientation, neighbour_mask: u32) -> u8 {
    LUT_CTXNO_ZC[orientation_index(band)][(neighbour_mask & T1_SIGMA_NEIGHBOURS) as usize]
}

#[inline]
pub(crate) fn sign_context(sign_lut_index: u32) -> u8 {
    LUT_CTXNO_SC[sign_lut_index as usize]
}

#[inline]
pub(crate) fn sign_prediction_bit(sign_lut_index: u32) -> u8 {
    LUT_SPB[sign_lut_index as usize]
}

#[inline]
pub(crate) fn magnitude_context(
    has_significant_neighbour: bool,
    has_refinement_history: bool,
) -> u8 {
    let base = if has_significant_neighbour {
        T1_CTXNO_MAG + 1
    } else {
        T1_CTXNO_MAG
    };
    if has_refinement_history {
        T1_CTXNO_MAG + 2
    } else {
        base
    }
}

#[inline]
pub(crate) fn sign_lut_index(
    west_significant: bool,
    west_sign: u8,
    north_significant: bool,
    north_sign: u8,
    east_significant: bool,
    east_sign: u8,
    south_significant: bool,
    south_sign: u8,
) -> u32 {
    let mut lu = 0u32;
    if west_significant {
        lu |= T1_LUT_SIG_W;
        if west_sign != 0 {
            lu |= T1_LUT_SGN_W;
        }
    }
    if north_significant {
        lu |= T1_LUT_SIG_N;
        if north_sign != 0 {
            lu |= T1_LUT_SGN_N;
        }
    }
    if east_significant {
        lu |= T1_LUT_SIG_E;
        if east_sign != 0 {
            lu |= T1_LUT_SGN_E;
        }
    }
    if south_significant {
        lu |= T1_LUT_SIG_S;
        if south_sign != 0 {
            lu |= T1_LUT_SGN_S;
        }
    }
    lu
}
