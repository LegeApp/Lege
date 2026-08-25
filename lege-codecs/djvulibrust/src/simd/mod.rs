//! Primitive dispatch scaffold, modeled on jp2lam's `src/simd/` architecture
//! (see `llm-docs/SIMD_AND_PARALLELISM_PLAN.md`). A single table of function
//! pointers, selected once at startup, so call sites don't need `cfg`s.
//!
//! Backend selection is controlled by the `DJVU_PRIMITIVES` env var:
//!
//! - `scalar` / `none` / `off` — force reference kernels everywhere.
//! - `wide` / `simd` — force every `wide` kernel on, **even ones known to
//!   regress** (e.g. color conversion — see below). This mode exists for
//!   testing/benchmarking a specific kernel, not for production use.
//! - `avx2` / `x86` — as above, plus AVX2 once implemented.
//! - unset / `auto` — installs only kernels that have *measured* faster
//!   than scalar via `examples/benchmark.rs`. Each `wide`/`x86` `setup_auto`
//!   function below decides per-primitive, not as a blanket switch.
//!
//! `auto` is deliberately conservative, decided per-kernel from measurement:
//!
//! - `color.rgb_to_ycbcr`: measured ~2x *slower* than scalar (per-pixel LUT
//!   gather dominates; batching the trivial combine step afterward adds
//!   overhead rather than removing it). Correctness-tested, not installed
//!   under `auto`.
//! - `iw44.filter_fv` (vertical wavelet transform, `scale == 1`): measured
//!   ~31% faster transform-stage wall time, ~8% faster full page encode
//!   (see `src/simd/wide/iw44.rs` module doc for numbers). Installed under
//!   `auto`.
//!
//! Same rule jp2lam applied to its regressed vertical-lift parallelism:
//! keep the code, gate it per-kernel from measurement, not from "it
//! compiles and matches scalar."
//!
//! The ZP arithmetic coder's Rust/ASM choice is a separate, compile-time-only
//! axis (the `asm_zp` feature) — it is not part of this table, since it's
//! tied to a linked object rather than a runtime-selectable kernel.

mod scalar;
#[cfg(feature = "simd")]
mod wide;
#[cfg(all(feature = "simd", target_arch = "x86_64"))]
mod x86;

use std::sync::LazyLock;

pub(crate) type RgbToYcbCr = fn(&[u8], &mut [i8], &mut [i8], &mut [i8]);
pub(crate) type Filter = fn(&mut [i16], usize, usize, usize, usize);

pub(crate) struct ColorPrimitives {
    pub rgb_to_ycbcr: RgbToYcbCr,
}

pub(crate) struct Iw44Primitives {
    pub filter_fh: Filter,
    pub filter_fv: Filter,
}

pub(crate) struct Primitives {
    pub color: ColorPrimitives,
    pub iw44: Iw44Primitives,
    /// For diagnostics/tests only — not part of the public API.
    /// Read through `crate::active_primitives_backend()`.
    pub backend: &'static str,
}

pub(crate) static PRIMITIVES: LazyLock<Primitives> = LazyLock::new(select_primitives);

fn select_primitives() -> Primitives {
    let primitives = Primitives {
        color: ColorPrimitives {
            rgb_to_ycbcr: scalar::rgb_to_ycbcr,
        },
        iw44: Iw44Primitives {
            filter_fh: crate::encode::iw44::transform::filter_fh,
            filter_fv: crate::encode::iw44::transform::filter_fv,
        },
        backend: "scalar",
    };

    let mode = std::env::var("DJVU_PRIMITIVES").unwrap_or_else(|_| "auto".to_string());
    let mode = mode.to_ascii_lowercase();

    if mode == "scalar" || mode == "none" || mode == "off" {
        return primitives;
    }

    #[cfg(feature = "simd")]
    let primitives = {
        let mut primitives = primitives;
        let force_all = matches!(mode.as_str(), "simd" | "wide" | "x86" | "avx2");
        if force_all {
            wide::setup_all(&mut primitives);
        } else if mode == "auto" {
            wide::setup_auto(&mut primitives);
        }

        #[cfg(target_arch = "x86_64")]
        {
            let avx2_enabled = matches!(mode.as_str(), "auto" | "simd" | "x86" | "avx2");
            if avx2_enabled {
                x86::setup(&mut primitives, &mode);
            }
        }
        primitives
    };

    primitives
}
