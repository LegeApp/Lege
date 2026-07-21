#![allow(dead_code)]

use crate::plan::BandOrientation;

const DWT_NORMS_53: [[f64; 10]; 4] = [
    [
        1.000, 1.500, 2.750, 5.375, 10.68, 21.34, 42.67, 85.33, 170.7, 341.3,
    ],
    [
        1.038, 1.592, 2.919, 5.703, 11.33, 22.64, 45.25, 90.48, 180.9, 0.0,
    ],
    [
        1.038, 1.592, 2.919, 5.703, 11.33, 22.64, 45.25, 90.48, 180.9, 0.0,
    ],
    [
        0.7186, 0.9218, 1.586, 3.043, 6.019, 12.01, 24.00, 47.97, 95.93, 0.0,
    ],
];

const DWT_NORMS_97: [[f64; 10]; 4] = [
    [
        1.000, 1.965, 4.177, 8.403, 16.90, 33.84, 67.69, 135.3, 270.6, 540.9,
    ],
    [
        2.022, 3.989, 8.355, 17.04, 34.27, 68.63, 137.3, 274.6, 549.0, 0.0,
    ],
    [
        2.022, 3.989, 8.355, 17.04, 34.27, 68.63, 137.3, 274.6, 549.0, 0.0,
    ],
    [
        2.080, 3.865, 8.307, 17.18, 34.71, 69.59, 139.3, 278.6, 557.2, 0.0,
    ],
];

pub(crate) fn get_norm_53(level: u32, band: BandOrientation) -> f64 {
    let orient = band_orientation_index(band);
    let clamped = if orient == 0 {
        level.min(9)
    } else {
        level.min(8)
    };
    DWT_NORMS_53[orient][clamped as usize]
}

pub(crate) fn get_norm_97(level: u32, band: BandOrientation) -> f64 {
    let orient = band_orientation_index(band);
    let clamped = if orient == 0 {
        level.min(9)
    } else {
        level.min(8)
    };
    DWT_NORMS_97[orient][clamped as usize]
}

pub(crate) fn band_gain(band: BandOrientation) -> u8 {
    match band {
        BandOrientation::Ll => 0,
        BandOrientation::Hl | BandOrientation::Lh => 1,
        BandOrientation::Hh => 2,
    }
}

pub(crate) fn reversible_exponent(precision: u32, band: BandOrientation) -> u8 {
    let precision = precision.min(u8::MAX as u32) as u8;
    precision.saturating_add(band_gain(band))
}

pub(crate) fn irreversible_expounded_quant(
    precision: u32,
    num_resolutions: u8,
    resolution: u8,
    band: BandOrientation,
) -> (u8, u16) {
    let gain = u32::from(band_gain(band));
    let level = u32::from(num_resolutions.saturating_sub(1).saturating_sub(resolution));
    let norm = get_norm_97(level, band);
    let stepsize = (((1u32) << gain) as f64) / norm;
    encode_stepsize(
        (stepsize * 8192.0).floor() as i32,
        precision.saturating_add(gain),
    )
}

pub(crate) fn encode_stepsize(stepsize: i32, numbps: u32) -> (u8, u16) {
    let p = floor_log2(stepsize) - 13;
    let n = 11 - floor_log2(stepsize);
    let mantissa = if n < 0 { stepsize >> -n } else { stepsize << n } & 0x7ff;
    let exponent = (numbps as i32 - p).max(0) as u8;
    (exponent, mantissa as u16)
}

fn floor_log2(value: i32) -> i32 {
    31 - value.max(1).leading_zeros() as i32
}

fn band_orientation_index(band: BandOrientation) -> usize {
    match band {
        BandOrientation::Ll => 0,
        BandOrientation::Hl => 1,
        BandOrientation::Lh => 2,
        BandOrientation::Hh => 3,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        encode_stepsize, get_norm_53, get_norm_97, irreversible_expounded_quant,
        reversible_exponent,
    };
    use crate::plan::BandOrientation;

    #[test]
    fn reversible_exponents_match_expected_band_gains() {
        assert_eq!(reversible_exponent(8, BandOrientation::Ll), 8);
        assert_eq!(reversible_exponent(8, BandOrientation::Hl), 9);
        assert_eq!(reversible_exponent(8, BandOrientation::Lh), 9);
        assert_eq!(reversible_exponent(8, BandOrientation::Hh), 10);
    }

    #[test]
    fn irreversible_stepsize_packing_is_stable() {
        assert_eq!(encode_stepsize(8096, 8), (9, 2000));
        assert_eq!(
            irreversible_expounded_quant(8, 6, 0, BandOrientation::Ll),
            (14, 1824)
        );
    }

    #[test]
    fn norm_tables_match_reference_values() {
        assert_eq!(get_norm_53(0, BandOrientation::Ll), 1.0);
        assert_eq!(get_norm_97(1, BandOrientation::Hl), 3.989);
    }
}
