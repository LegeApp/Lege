use crate::error::{Jp2LamError, Result};
use crate::plan::{EncodingPlan, ProgressionOrder, QuantizationStyle, WaveletTransform};

pub(crate) const MARKER_SIZ: u16 = 0xff51;
pub(crate) const MARKER_COD: u16 = 0xff52;
pub(crate) const MARKER_QCD: u16 = 0xff5c;

pub(crate) fn encode_siz(plan: &EncodingPlan) -> Result<Vec<u8>> {
    let component_count = usize::from(plan.component_count);
    let length = 38usize
        .checked_add(component_count.checked_mul(3).ok_or_else(|| {
            Jp2LamError::EncodeFailed("SIZ component length overflow".to_string())
        })?)
        .ok_or_else(|| Jp2LamError::EncodeFailed("SIZ length overflow".to_string()))?;
    let length = u16::try_from(length)
        .map_err(|_| Jp2LamError::EncodeFailed("SIZ length exceeds u16".to_string()))?;

    let mut out = Vec::with_capacity(2 + length as usize);
    out.extend_from_slice(&MARKER_SIZ.to_be_bytes());
    out.extend_from_slice(&length.to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes());
    out.extend_from_slice(&plan.width.to_be_bytes());
    out.extend_from_slice(&plan.height.to_be_bytes());
    out.extend_from_slice(&0u32.to_be_bytes());
    out.extend_from_slice(&0u32.to_be_bytes());
    out.extend_from_slice(&plan.tile.width.to_be_bytes());
    out.extend_from_slice(&plan.tile.height.to_be_bytes());
    out.extend_from_slice(&0u32.to_be_bytes());
    out.extend_from_slice(&0u32.to_be_bytes());
    out.extend_from_slice(&plan.component_count.to_be_bytes());
    for component in &plan.components {
        let precision = component.precision.min(38);
        let signed = if component.signed { 0x80 } else { 0x00 };
        out.push(signed | ((precision as u8).saturating_sub(1) & 0x7f));
        out.push(u8::try_from(component.dx).map_err(|_| {
            Jp2LamError::EncodeFailed("component dx exceeds SIZ range".to_string())
        })?);
        out.push(u8::try_from(component.dy).map_err(|_| {
            Jp2LamError::EncodeFailed("component dy exceeds SIZ range".to_string())
        })?);
    }
    Ok(out)
}

pub(crate) fn encode_cod(plan: &EncodingPlan) -> Vec<u8> {
    let transform = match plan.transform {
        WaveletTransform::Reversible53 => 1u8,
        WaveletTransform::Irreversible97 => 0u8,
    };
    let progression = match plan.progression_order {
        ProgressionOrder::Lrcp => 0u8,
    };
    let cblk_width = code_block_exponent(plan.code_block_size.width);
    let cblk_height = code_block_exponent(plan.code_block_size.height);

    let mut out = Vec::with_capacity(14);
    out.extend_from_slice(&MARKER_COD.to_be_bytes());
    out.extend_from_slice(&12u16.to_be_bytes());
    out.push(0);
    out.push(progression);
    out.extend_from_slice(&(plan.layers.len() as u16).to_be_bytes());
    out.push(if plan.use_mct { 1 } else { 0 });
    out.push(plan.decomposition_levels);
    out.push(cblk_width);
    out.push(cblk_height);
    out.push(0);
    out.push(transform);
    out
}

pub(crate) fn encode_qcd(plan: &EncodingPlan) -> Result<Vec<u8>> {
    match plan.quantization_style {
        QuantizationStyle::NoQuantization => {
            let length = 3usize
                .checked_add(plan.subband_quants.len())
                .ok_or_else(|| Jp2LamError::EncodeFailed("QCD length overflow".to_string()))?;
            let length = u16::try_from(length)
                .map_err(|_| Jp2LamError::EncodeFailed("QCD length exceeds u16".to_string()))?;

            let mut out = Vec::with_capacity(2 + length as usize);
            out.extend_from_slice(&MARKER_QCD.to_be_bytes());
            out.extend_from_slice(&length.to_be_bytes());
            out.push(plan.guard_bits << 5);
            for band in &plan.subband_quants {
                out.push(band.exponent << 3);
            }
            Ok(out)
        }
        QuantizationStyle::ScalarExpounded => {
            let length =
                3usize
                    .checked_add(plan.subband_quants.len().checked_mul(2).ok_or_else(|| {
                        Jp2LamError::EncodeFailed("QCD length overflow".to_string())
                    })?)
                    .ok_or_else(|| Jp2LamError::EncodeFailed("QCD length overflow".to_string()))?;
            let length = u16::try_from(length)
                .map_err(|_| Jp2LamError::EncodeFailed("QCD length exceeds u16".to_string()))?;

            let mut out = Vec::with_capacity(2 + length as usize);
            out.extend_from_slice(&MARKER_QCD.to_be_bytes());
            out.extend_from_slice(&length.to_be_bytes());
            out.push((plan.guard_bits << 5) | 2);
            for band in &plan.subband_quants {
                let packed = ((u16::from(band.exponent)) << 11) | (band.mantissa & 0x7ff);
                out.extend_from_slice(&packed.to_be_bytes());
            }
            Ok(out)
        }
    }
}

fn code_block_exponent(size: u32) -> u8 {
    size.trailing_zeros().saturating_sub(2) as u8
}
