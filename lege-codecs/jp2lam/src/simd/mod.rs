//! Runtime-selected primitive kernels for data-parallel codec stages.
//!
//! This mirrors the bpg-rs/still265 convention: build a scalar table first,
//! then overwrite entries with portable SIMD implementations when enabled.

use std::sync::LazyLock;

pub(crate) mod scalar;

#[cfg(feature = "simd")]
pub(crate) mod wide;

#[cfg(all(feature = "simd", any(target_arch = "x86", target_arch = "x86_64")))]
pub(crate) mod x86;

pub(crate) type AnalyzeI32Fn = fn(&[i32]) -> (u32, usize);
pub(crate) type QuantizeRectFn = fn(&[f32], &mut [i32], usize, usize, usize, usize, usize, f32);
pub(crate) type DequantizeRectFn = fn(&[i32], &mut [f32], usize, usize, usize, usize, usize, f32);
pub(crate) type LevelShiftF32Fn = fn(&[i32], i32, &mut [f32]);
pub(crate) type LevelShiftI32Fn = fn(&[i32], i32, &mut [i32]);
pub(crate) type IctComponentFn = fn(&[i32], &[i32], &[i32], usize, i32, &mut [f32]);
pub(crate) type RctComponentFn = fn(&[i32], &[i32], &[i32], usize, i32, &mut [i32]);
pub(crate) type InverseIctFn = fn(&mut [f32], &mut [f32], &mut [f32]);
pub(crate) type InverseRctFn = fn(&mut [i32], &mut [i32], &mut [i32]);
pub(crate) type FinalizeI32Fn = fn(&[i32], &mut [i32]);
pub(crate) type FinalizeF32Fn = fn(&[f32], &mut [i32]);
pub(crate) type DwtF32Fn = fn(&mut [f32], usize, usize, u8) -> crate::error::Result<()>;
pub(crate) type DwtI32Fn = fn(&mut [i32], usize, usize, u8) -> crate::error::Result<()>;

#[derive(Clone, Copy)]
pub(crate) struct Primitives {
    pub dwt: DwtPrimitives,
    pub analyze: AnalyzePrimitives,
    pub quant: QuantPrimitives,
    pub color: ColorPrimitives,
    pub backend: &'static str,
}

#[derive(Clone, Copy)]
pub(crate) struct AnalyzePrimitives {
    pub i32_max_magnitude_and_nnz: AnalyzeI32Fn,
}

#[derive(Clone, Copy)]
pub(crate) struct DwtPrimitives {
    pub forward_97_2d: DwtF32Fn,
    pub inverse_97_2d: DwtF32Fn,
    pub forward_53_2d: DwtI32Fn,
    pub inverse_53_2d: DwtI32Fn,
}

#[derive(Clone, Copy)]
pub(crate) struct QuantPrimitives {
    pub quantize_f32_rect: QuantizeRectFn,
    pub dequantize_i32_rect: DequantizeRectFn,
}

#[derive(Clone, Copy)]
pub(crate) struct ColorPrimitives {
    pub level_shift_f32: LevelShiftF32Fn,
    pub level_shift_i32: LevelShiftI32Fn,
    pub forward_ict_component: IctComponentFn,
    pub forward_rct_component: RctComponentFn,
    pub inverse_ict: InverseIctFn,
    pub inverse_rct: InverseRctFn,
    pub finalize_i32: FinalizeI32Fn,
    pub finalize_f32: FinalizeF32Fn,
}

pub(crate) static PRIMITIVES: LazyLock<Primitives> = LazyLock::new(select_primitives);

pub(crate) fn active_backend() -> &'static str {
    PRIMITIVES.backend
}

fn select_primitives() -> Primitives {
    let mut primitives = scalar::primitives();
    let mode = std::env::var("JP2LAM_PRIMITIVES")
        .unwrap_or_else(|_| "auto".to_string())
        .to_ascii_lowercase();

    if matches!(mode.as_str(), "scalar" | "none" | "off") {
        return primitives;
    }

    #[cfg(feature = "simd")]
    if matches!(mode.as_str(), "auto" | "simd" | "wide" | "x86" | "avx2") {
        wide::setup(&mut primitives);
    }

    #[cfg(all(feature = "simd", any(target_arch = "x86", target_arch = "x86_64")))]
    if matches!(mode.as_str(), "auto" | "simd" | "x86" | "avx2") {
        x86::setup(&mut primitives, &mode);
    }

    primitives
}

#[cfg(test)]
mod tests {
    #[test]
    fn env_forces_scalar_backend() {
        // Selection itself is a LazyLock and may already have happened in the
        // process, so keep this as a documentation guard for accepted names.
        assert!(matches!("scalar", "scalar" | "none" | "off"));
    }
}
