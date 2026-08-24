//! `/JPXDecode` (JPEG 2000) via the `jp2lam` crate.
//!
//! `jp2lam` is a deliberate external dependency, not a vendored module: Lege
//! calls the same decoder, so it is developed as its own project and consumed
//! here through the codec seam like any other.
//!
//! A JPX stream carries its own colour space and bit depth, which take
//! precedence over the image dictionary's (ISO 32000-1 §7.4.9: `/ColorSpace`
//! may be absent, and when present it is only a hint for `/SMaskInData`
//! handling). The decoded format therefore comes from the codestream.
//!
//! Decode uses jp2lam's optimized request API via a thread-local
//! [`jp2lam::Jp2Decoder`] session (retained Tier-1 scratch + request-sized
//! Rayon pool). For 8-bit components the codec writes the packed 8-bit raster
//! directly (`Gray8`/`Rgb8`/`Rgba8`/`Cmyk8`), removing the renderer-side
//! full-image interleave pass and the planar `i32` intermediates. Streams
//! whose components are not 8-bit (1/2/4-bit bitonal and palette scans, 12/16-bit
//! deep scans) fall back to the native planar decode and PDFium's own
//! shift-based widening (`src << (8 - prec)` below 8 bits, `src >> (prec - 8)`
//! above), which is what the legacy adapter did and is bit-exact with it.
//!
//! jp2lam also exposes opaque pad layouts (`Rgbx8`/`Bgra8`) for four-byte
//! destinations without a codestream alpha plane. The CPU paint path still
//! samples tight DeviceRGB; wire those formats when a GPU upload or BGRA
//! surface path is ready (see `lege-codecs/jp2lam/decode-fix-plan/`).

use std::cell::RefCell;
use std::sync::Arc;

use jp2lam::{
    ColorSpace, DecodeConcurrency, DecodeLimits as Jp2DecodeLimits, DecodeOutputFormat,
    DecodeRequest, DecodeResolution, DecodeResult, DecodeTarget,
};

use crate::codec::{DecodeLimits, DecodedFormat, DecodedImage, ImageCodec};
use crate::{DecodeParameters, ImageDescriptor, ImageError, StreamFilter};

thread_local! {
    /// A reusable decoder session per render worker. `jp2lam::Jp2Decoder`
    /// retains bounded Tier-1 scratch and a cached, request-sized Rayon pool
    /// across calls, so repeated JPX draws on a worker avoid re-allocating
    /// scratch and rebuilding a thread pool on every decode. The `ImageCodec`
    /// trait is stateless-by-`&self`; the mutable session lives here.
    static JPX_DECODER: RefCell<jp2lam::Jp2Decoder> = RefCell::new(jp2lam::Jp2Decoder::new());
}

/// Reduced-resolution quality margin (Phase 2). When the renderer supplies a
/// device-footprint hint, the decoder drops wavelet resolutions but only while
/// the decoded resolution stays at least this multiple of the destination
/// footprint. 1.0 is the principled floor — never decode below one texel per
/// destination pixel — and, because wavelet reduction is discrete (each level
/// halves), it still leaves the decoded image between 1x and 2x the destination
/// in practice (real supersample headroom). On the two profiled hotspot pages
/// this engages one reduction level each with PDFium differential severity of
/// 0.000096 (jpx-scan) and 0.003781 (mrc) — both well under the 0.005 budget,
/// and the mrc figure is below its full-resolution baseline. A larger margin
/// (measured up through 1.35) foregoes the mrc reduction for no severity gain.
/// NOTE: the renderer-side hint (`codec_target_size`) additionally applies a
/// supersampling headroom before this margin — see
/// `pdf-render-cpu/src/prepared.rs` — so the decoded raster the resampler sees
/// stays comfortably above the destination footprint.
/// See corpus/perf/optimization-jpx-integration-20260720.md.
const JPX_QUALITY_MARGIN: f32 = 1.0;

/// The effective quality margin, overridable via `PDF_RENDERER_JPX_MARGIN` for
/// margin sweeps during measurement. Read once.
fn jpx_quality_margin() -> f32 {
    use std::sync::OnceLock;
    static MARGIN: OnceLock<f32> = OnceLock::new();
    *MARGIN.get_or_init(|| {
        std::env::var("PDF_RENDERER_JPX_MARGIN")
            .ok()
            .and_then(|v| v.parse::<f32>().ok())
            .filter(|m| m.is_finite() && *m >= 1.0)
            .unwrap_or(JPX_QUALITY_MARGIN)
    })
}

/// A fixed concurrency override from `PDF_RENDERER_JPX_CONCURRENCY` (`serial`,
/// `budgeted:N`, or `auto`), read once. `None` selects the load-aware policy.
/// Keeps the concurrency sweep and pinning reproducible on the shipping binary.
fn jpx_concurrency_override() -> Option<DecodeConcurrency> {
    use std::sync::OnceLock;
    static C: OnceLock<Option<DecodeConcurrency>> = OnceLock::new();
    *C.get_or_init(|| match std::env::var("PDF_RENDERER_JPX_CONCURRENCY") {
        Ok(v) => {
            let v = v.trim().to_ascii_lowercase();
            if let Some(n) = v.strip_prefix("budgeted:") {
                n.parse::<usize>()
                    .ok()
                    .filter(|n| *n >= 1)
                    .map(DecodeConcurrency::Budgeted)
            } else if v == "serial" {
                Some(DecodeConcurrency::Serial)
            } else {
                // "auto" or anything unrecognised falls through to load-aware.
                None
            }
        }
        Err(_) => None,
    })
}

/// Live count of JPX decodes in flight across all render workers. Used to size
/// each decode's internal Tier-1 thread budget to the *current* machine load.
static JPX_IN_FLIGHT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Increments the in-flight decode count for its lifetime.
struct InFlightGuard;
impl InFlightGuard {
    fn enter() -> Self {
        JPX_IN_FLIGHT.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        Self
    }
    /// In-flight decodes right now, including this one (always ≥ 1).
    fn current(&self) -> usize {
        JPX_IN_FLIGHT
            .load(std::sync::atomic::Ordering::Acquire)
            .max(1)
    }
}
impl Drop for InFlightGuard {
    fn drop(&mut self) {
        JPX_IN_FLIGHT.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

/// Lower and upper bound on the per-decode Tier-1 thread budget.
const JPX_BUDGET_MIN: usize = 2;
const JPX_BUDGET_MAX: usize = 8;

/// Load-aware per-decode concurrency. Render workers already parallelise across
/// draws, so a decode must not grab the whole machine when many are running —
/// but when few are (the single-page / viewer case) it should use more of a big
/// JPX's internal Tier-1 parallelism. Budget = clamp(cores / in_flight, 2, 8):
/// one lone decode on a 20-core host runs at 8; a saturated scheduler settles to
/// the `Budgeted(2)` cap that protects the render pool from oversubscription.
fn load_aware_budget(in_flight: usize) -> DecodeConcurrency {
    use std::sync::OnceLock;
    static CORES: OnceLock<usize> = OnceLock::new();
    let cores = *CORES.get_or_init(|| {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(JPX_BUDGET_MIN)
    });
    let budget = (cores / in_flight).clamp(JPX_BUDGET_MIN, JPX_BUDGET_MAX);
    DecodeConcurrency::Budgeted(budget)
}

/// The `/JPXDecode` codec for a [`crate::CodecRegistry`].
#[derive(Debug, Default)]
pub struct JpxCodec;

impl ImageCodec for JpxCodec {
    fn filter(&self) -> StreamFilter {
        StreamFilter::Jpx
    }

    fn decode(
        &self,
        data: &[u8],
        descriptor: &ImageDescriptor,
        params: &DecodeParameters,
        limits: &DecodeLimits,
    ) -> Result<DecodedImage, ImageError> {
        limits.check_input(data.len())?;
        // Inspect the container/codestream header first: it is cheap relative to
        // the full decode and tells us the output colour space and component
        // precision, which decide the packed-vs-native path below.
        let codec_limits = decoder_limits(limits);
        let meta = jp2lam::inspect_jp2_with_limits(data, &codec_limits)
            .map_err(|e| ImageError::Decode(format!("JPX: {e}")))?;
        if limits.is_cancelled() {
            return Err(ImageError::Cancelled);
        }

        let (width, height) = (meta.width, meta.height);
        if width == 0 || height == 0 {
            return Err(ImageError::Decode("JPX: zero dimension".into()));
        }
        if u64::from(width) * u64::from(height) > limits.max_pixels {
            return Err(ImageError::TooLarge { width, height });
        }

        // A JP2 may carry its own `pclr` palette, but ISO 32000-1 §7.4.9 makes
        // the PDF `/ColorSpace` authoritative over the one in the JPEG 2000
        // data. With `/Indexed`, then, the codestream's single component is an
        // index into the *PDF* palette and the container's palette must not be
        // applied — its output channels are not a colour space at all. In
        // `Tirpitz-and-the-Imperial-German-Navy.pdf` the `pclr` has three
        // columns feeding a `/DeviceN [/Magenta /Yellow /Black]` base, so
        // expanding it produced something that looked like RGB, was rejected as
        // "Indexed over a multi-component codec output", and dropped the cover
        // portrait entirely.
        //
        // Only when the container actually has a palette: a `/JPXDecode` image
        // whose codestream is genuinely multi-component under an `/Indexed`
        // space is a different defect, and the renderer still declines that one
        // rather than painting channels neither space describes.
        let ignore_container_palette = meta.container_palette_channels.is_some()
            && descriptor
                .color_space
                .as_ref()
                .is_some_and(|cs| cs.family == pdf_color::ColorSpaceFamily::Indexed);

        let (format, channels) = if ignore_container_palette {
            // The palette's input is one 8-bit index component.
            (DecodedFormat::Gray8, 1usize)
        } else {
            match meta.colorspace {
                // ISO 32000-1 §7.4.9 makes the PDF `/ColorSpace` authoritative
                // over the container's. A 2-component codestream under a
                // 2-component space (a `/DeviceN` spot+black duotone) is two
                // *colorants*, not gray+alpha: hand both channels over and let
                // the declared space interpret them.
                ColorSpace::Gray
                    if meta.codestream.siz.components.len() == 2
                        && descriptor
                            .color_space
                            .as_ref()
                            .is_some_and(|cs| cs.components == 2) =>
                {
                    (DecodedFormat::Multi2, 2usize)
                }
                // Grayscale with an in-data (`cdef`) opacity channel decodes to
                // Gray+alpha, split the same way as RGBA below.
                ColorSpace::Gray if meta.in_data_alpha.is_some() => (DecodedFormat::GrayA8, 2usize),
                ColorSpace::Gray => (DecodedFormat::Gray8, 1usize),
                // An sRGB image with an in-data (`cdef`) opacity channel decodes to
                // RGBA; the renderer splits the alpha into a soft mask when the PDF
                // dict opts in via `/SMaskInData`.
                ColorSpace::Srgb if meta.in_data_alpha.is_some() => (DecodedFormat::Rgba8, 4usize),
                ColorSpace::Srgb => (DecodedFormat::Rgb8, 3usize),
                // sYCC decodes to sRGB after the inverse sYCC matrix.
                ColorSpace::YCbCr => (DecodedFormat::Rgb8, 3usize),
                ColorSpace::Cmyk => (DecodedFormat::Cmyk8, 4usize),
                other => {
                    return Err(ImageError::Decode(format!(
                        "JPX: unsupported colorspace {other:?}"
                    )));
                }
            }
        };

        // Phase 2: a device-footprint hint lets the decoder drop wavelet
        // resolutions for a minified draw. `AtLeast` is inherently conservative
        // — it never reduces below the destination footprint scaled by the
        // quality margin, so magnified and near-1:1 draws decode at full
        // resolution. Absent a hint (whole-document paths, Phase-1 gate) the
        // resolution is `Full` and output is byte-identical to the legacy path.
        let resolution = match params.target_size {
            Some((tw, th)) if tw > 0 && th > 0 => DecodeResolution::AtLeast {
                width: tw,
                height: th,
                quality_margin: jpx_quality_margin(),
            },
            _ => DecodeResolution::Full,
        };

        // Direct packed 8-bit output is bit-exact with the legacy planar
        // interleave: both level-shift, clamp, and (for 8-bit) map identity to
        // the output byte. Any other precision scales differently on the two
        // paths — jp2lam's packed writer maps the full range, PDFium shifts —
        // so non-8-bit components stay on the native planar path, which
        // reproduces PDFium's widening exactly. See `decode_native_interleaved`.
        let all_8bit = meta
            .codestream
            .siz
            .components
            .iter()
            .all(|component| component.precision == 8);

        // Size this decode's internal thread budget to current load: mark it
        // in flight for the duration, then either honour a fixed override or
        // pick load-aware. The guard's count includes this decode.
        let in_flight = InFlightGuard::enter();
        let concurrency =
            jpx_concurrency_override().unwrap_or_else(|| load_aware_budget(in_flight.current()));

        // `decode_into` writes directly into the renderer's final Arc allocation.
        // Container-palette expansion still needs jp2lam's planar fallback; an
        // ignored PDF-overridden palette is eligible because the raw index
        // component is decoded as Gray8.
        let packed_direct =
            all_8bit && (meta.container_palette_channels.is_none() || ignore_container_palette);
        let result = if packed_direct {
            let (decoded_width, decoded_height) = meta
                .decoded_dimensions(resolution)
                .map_err(|e| ImageError::Decode(format!("JPX: {e}")))?;
            self.decode_packed(
                data,
                resolution,
                format,
                channels,
                decoded_width,
                decoded_height,
                limits,
                concurrency,
                ignore_container_palette,
            )
        } else {
            self.decode_native_interleaved(
                data,
                resolution,
                format,
                channels,
                width,
                height,
                limits,
                concurrency,
                ignore_container_palette,
            )
        };
        drop(in_flight);
        if limits.is_cancelled() {
            return Err(ImageError::Cancelled);
        }
        result
    }
}

impl JpxCodec {
    /// Decode straight into a packed 8-bit raster (the fast path for 8-bit
    /// components).
    #[allow(clippy::too_many_arguments)]
    fn decode_packed(
        &self,
        data: &[u8],
        resolution: DecodeResolution,
        format: DecodedFormat,
        channels: usize,
        width: u32,
        height: u32,
        limits: &DecodeLimits,
        concurrency: DecodeConcurrency,
        ignore_container_palette: bool,
    ) -> Result<DecodedImage, ImageError> {
        let output = match format {
            DecodedFormat::Gray8 => DecodeOutputFormat::Gray8,
            // Both carry two interleaved planes; only their meaning differs.
            DecodedFormat::GrayA8 | DecodedFormat::Multi2 => DecodeOutputFormat::GrayA8,
            DecodedFormat::Rgb8 => DecodeOutputFormat::Rgb8,
            DecodedFormat::Rgba8 => DecodeOutputFormat::Rgba8,
            DecodedFormat::Cmyk8 => DecodeOutputFormat::Cmyk8,
            other => {
                return Err(ImageError::Decode(format!(
                    "JPX: packed output not applicable to {other:?}"
                )));
            }
        };
        let request = DecodeRequest {
            resolution,
            output,
            region: None,
            concurrency,
            ignore_container_palette,
            limits: decoder_limits(limits),
        };
        let stride = (width as usize)
            .checked_mul(channels)
            .ok_or_else(|| ImageError::Decode("JPX: stride overflow".into()))?;
        let bytes = stride
            .checked_mul(height as usize)
            .ok_or_else(|| ImageError::Decode("JPX: size overflow".into()))?;
        if bytes as u64 > limits.max_output_bytes {
            return Err(ImageError::TooLarge { width, height });
        }
        let mut output_data = zeroed_arc(bytes);
        let Some(output_slice) = Arc::get_mut(&mut output_data) else {
            return Err(ImageError::Decode(
                "JPX: output allocation unexpectedly became shared".into(),
            ));
        };
        let info = JPX_DECODER
            .with(|decoder| {
                decoder.borrow_mut().decode_into(
                    data,
                    &request,
                    DecodeTarget {
                        data: output_slice,
                        width,
                        height,
                        stride,
                        format: output,
                        premultiplied: false,
                    },
                )
            })
            .map_err(|e| ImageError::Decode(format!("JPX: {e}")))?;
        if limits.is_cancelled() {
            return Err(ImageError::Cancelled);
        }
        Ok(DecodedImage {
            width: info.width,
            height: info.height,
            format,
            stride: info.stride,
            data: output_data,
        })
    }

    /// Native planar decode + the historical hand interleave, preserving the
    /// legacy `v >> (precision - 8)` scaling exactly. Used for non-8-bit
    /// components.
    #[allow(clippy::too_many_arguments)]
    fn decode_native_interleaved(
        &self,
        data: &[u8],
        resolution: DecodeResolution,
        format: DecodedFormat,
        channels: usize,
        _width: u32,
        _height: u32,
        limits: &DecodeLimits,
        concurrency: DecodeConcurrency,
        ignore_container_palette: bool,
    ) -> Result<DecodedImage, ImageError> {
        let request = DecodeRequest {
            resolution,
            output: DecodeOutputFormat::NativePlanarI32,
            region: None,
            concurrency,
            ignore_container_palette,
            limits: decoder_limits(limits),
        };
        let result = JPX_DECODER
            .with(|decoder| decoder.borrow_mut().decode(data, &request))
            .map_err(|e| ImageError::Decode(format!("JPX: {e}")))?;
        if limits.is_cancelled() {
            return Err(ImageError::Cancelled);
        }
        let image = match result {
            DecodeResult::Native(image) => image,
            DecodeResult::Raster(_) => {
                return Err(ImageError::Decode(
                    "JPX: native decode returned a packed raster".into(),
                ));
            }
        };

        // The decoded dimensions win over the container header (a reduced-
        // resolution decode returns a smaller image).
        let width = image.width;
        let height = image.height;
        if width == 0 || height == 0 {
            return Err(ImageError::Decode("JPX: zero dimension".into()));
        }
        let ncomp = image.components.len();
        if ncomp != channels {
            return Err(ImageError::Decode(format!(
                "JPX: {format:?} expects {channels} components, decoded {ncomp}"
            )));
        }
        let stride = (width as usize)
            .checked_mul(ncomp)
            .ok_or_else(|| ImageError::Decode("JPX: stride overflow".into()))?;
        let bytes = stride
            .checked_mul(height as usize)
            .ok_or_else(|| ImageError::Decode("JPX: size overflow".into()))?;
        if bytes as u64 > limits.max_output_bytes {
            return Err(ImageError::TooLarge { width, height });
        }

        // jp2lam yields planar i32 samples at each component's own precision;
        // interleave and scale to 8-bit.
        let pixels = (width as usize)
            .checked_mul(height as usize)
            .ok_or_else(|| ImageError::Decode("JPX: pixel count overflow".into()))?;
        let mut out = vec![0u8; bytes];
        // When the PDF declares `/Indexed`, ignoring a JP2 container palette
        // leaves the codestream's samples as literal PDF palette indices. A
        // 4-bit index 15 must remain byte value 15 in the decoded Gray8
        // carrier; widening it to 240 makes the renderer clamp nearly every
        // sample to `/HiVal` and collapses the image to one palette color
        // (pdf.js issue12213).
        let preserve_palette_indices = ignore_container_palette && ncomp == 1;
        for (ci, comp) in image.components.iter().enumerate() {
            if comp.data.len() < pixels {
                return Err(ImageError::Decode("JPX: short component plane".into()));
            }
            let shift = comp.precision.saturating_sub(8);
            // Sub-8-bit components (bitonal scans are 1-bit) widen by a LEFT
            // shift, matching PDFium's `src << (8 - prec)` in
            // `CJpx_Decoder::Decode`. This is deliberately *not* the full-range
            // scaling OpenJPEG's writers use: at 1 bit a set sample becomes 128,
            // not 255, so a bitonal scan renders on a mid-grey field. Widening
            // to full range instead leaves pdfbox/3246 at inkΔ 0.81/0.99 and
            // pdfbox/4326 at 0.98 against PDFium; the shift puts all three at
            // exactly 0.00000. Codec correctness is unaffected either way —
            // jp2lam's samples are bit-exact with OpenJPEG (max abs diff 0) —
            // this is only how the renderer widens them into an 8-bit buffer,
            // and it is the same convention the deeper-than-8-bit branch below
            // already follows. Flip both branches together if PDFium parity is
            // ever traded for spec-literal colour mapping.
            let max_sample = (1u32 << comp.precision.clamp(1, 31)) - 1;
            for y in 0..height as usize {
                if y & 31 == 0 && limits.is_cancelled() {
                    return Err(ImageError::Cancelled);
                }
                let row = y * width as usize;
                for x in 0..width as usize {
                    let i = row + x;
                    out[i * ncomp + ci] = component_sample_to_u8(
                        comp.data[i],
                        comp.precision,
                        max_sample,
                        shift,
                        preserve_palette_indices,
                    );
                }
            }
        }

        Ok(DecodedImage {
            width,
            height,
            format,
            stride,
            data: Arc::from(out),
        })
    }
}

/// Allocate the renderer's final shared JPX output directly. The allocation is
/// uniquely owned while jp2lam fills it, then handed to `DecodedImage` unchanged.
#[allow(
    unsafe_code,
    reason = "Arc::new_zeroed_slice hands back MaybeUninit; see SAFETY comment"
)]
fn zeroed_arc(len: usize) -> Arc<[u8]> {
    let data = Arc::<[u8]>::new_zeroed_slice(len);
    // SAFETY: `new_zeroed_slice` initialized every `u8` to a valid zero value.
    unsafe { data.assume_init() }
}

fn decoder_limits(limits: &DecodeLimits) -> Jp2DecodeLimits {
    let defaults = Jp2DecodeLimits::default();
    Jp2DecodeLimits {
        max_input_bytes: defaults
            .max_input_bytes
            .min(usize::try_from(limits.max_input_bytes).unwrap_or(usize::MAX)),
        max_pixels: limits.max_pixels,
        max_working_bytes: defaults
            .max_working_bytes
            .min(usize::try_from(limits.max_working_bytes).unwrap_or(usize::MAX)),
        ..defaults
    }
}

fn component_sample_to_u8(
    value: i32,
    precision: u32,
    max_sample: u32,
    right_shift: u32,
    preserve_palette_index: bool,
) -> u8 {
    if preserve_palette_index {
        return value.clamp(0, max_sample.min(255) as i32) as u8;
    }
    if precision < 8 {
        ((value.clamp(0, max_sample as i32) as u32) << (8 - precision)).min(255) as u8
    } else {
        let value = if right_shift > 0 {
            value >> right_shift
        } else {
            value
        };
        value.clamp(0, 255) as u8
    }
}

#[cfg(test)]
mod tests {
    use super::{component_sample_to_u8, decoder_limits};
    use crate::codec::DecodeLimits;

    #[test]
    fn sub8_pdf_palette_indices_are_not_color_widened() {
        assert_eq!(component_sample_to_u8(15, 4, 15, 0, true), 15);
        assert_eq!(
            component_sample_to_u8(15, 4, 15, 0, false),
            240,
            "ordinary 4-bit color components retain PDFium's widening"
        );
    }

    #[test]
    fn renderer_limits_are_pushed_into_jp2_decode() {
        let renderer = DecodeLimits {
            max_input_bytes: 2048,
            max_pixels: 1234,
            max_output_bytes: 4096,
            max_working_bytes: 3072,
            ..DecodeLimits::default()
        };
        let mapped = decoder_limits(&renderer);
        assert_eq!(mapped.max_pixels, 1234);
        assert_eq!(mapped.max_input_bytes, 2048);
        assert_eq!(mapped.max_working_bytes, 3072);
    }
}
