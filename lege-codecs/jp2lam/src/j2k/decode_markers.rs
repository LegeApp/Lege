//! Annex A marker-segment decoding for the narrow decoder path.

use crate::error::{Jp2LamError, Result};
use crate::j2k::types::{CodestreamParts, TilePartHeader};
use crate::j2k::{
    MARKER_CAP, MARKER_COC, MARKER_COM, MARKER_CRG, MARKER_PLM, MARKER_PLT, MARKER_POC, MARKER_PPM,
    MARKER_PPT, MARKER_QCC, MARKER_RGN, MARKER_TLM,
};

use super::markers::{MARKER_COD, MARKER_QCD, MARKER_SIZ};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodestreamHeader {
    pub siz: SizSegment,
    pub cod: CodSegment,
    pub qcd: QcdSegment,
    /// Per-component quantization overrides from main-header QCC markers
    /// (Annex A.6.5): `(component_index, quant)`. Empty when every component
    /// uses the default QCD. Selected via [`Self::quant_for`].
    pub qcc: Vec<(usize, QcdSegment)>,
    /// Per-component wavelet-transform overrides from main-header COC markers
    /// (Annex A.6.2): `(component_index, transform)`. A COC may in principle
    /// override the whole coding style, but the decoder only accepts one that
    /// changes the transform (the common CMYK "lossless K" pattern — colour
    /// components 9/7, the K channel 5/3); any other override is rejected at
    /// parse time. Empty when every component uses the default COD. Selected via
    /// [`Self::transform_for`].
    pub coc: Vec<(usize, WaveletTransform)>,
    pub comment_count: usize,
}

impl CodestreamHeader {
    /// The effective quantization for `component`: its QCC override if one was
    /// signalled, otherwise the default QCD (ISO/IEC 15444-1 Annex A.6.5).
    pub(crate) fn quant_for(&self, component: usize) -> &QcdSegment {
        self.qcc
            .iter()
            .find(|(index, _)| *index == component)
            .map(|(_, quant)| quant)
            .unwrap_or(&self.qcd)
    }

    /// The effective wavelet transform for `component`: its COC override if one
    /// was signalled, otherwise the default COD (ISO/IEC 15444-1 Annex A.6.2).
    pub(crate) fn transform_for(&self, component: usize) -> WaveletTransform {
        self.coc
            .iter()
            .find(|(index, _)| *index == component)
            .map(|(_, transform)| *transform)
            .unwrap_or(self.cod.transform)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SizSegment {
    pub rsiz: u16,
    pub width: u32,
    pub height: u32,
    pub x_origin: u32,
    pub y_origin: u32,
    pub tile_width: u32,
    pub tile_height: u32,
    pub tile_x_origin: u32,
    pub tile_y_origin: u32,
    pub components: Vec<ComponentSiz>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ComponentSiz {
    pub precision: u8,
    pub signed: bool,
    pub dx: u8,
    pub dy: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodSegment {
    pub progression_order: ProgressionOrder,
    pub layers: u16,
    pub use_mct: bool,
    pub decomposition_levels: u8,
    pub code_block_width: u32,
    pub code_block_height: u32,
    pub code_block_style: CodeBlockStyle,
    pub transform: WaveletTransform,
    pub uses_precincts: bool,
    pub sop_markers: bool,
    pub eph_markers: bool,
    pub precinct_sizes: Vec<PrecinctSize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrecinctSize {
    pub pp_x: u8,
    pub pp_y: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressionOrder {
    Lrcp,
    Rlcp,
    Rpcl,
    Pcrl,
    Cprl,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaveletTransform {
    Irreversible97,
    Reversible53,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CodeBlockStyle {
    pub bypass: bool,
    pub reset_contexts: bool,
    pub terminate_each_pass: bool,
    pub vertical_causal: bool,
    pub predictable_termination: bool,
    pub segmentation_symbols: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QcdSegment {
    pub style: QuantizationStyle,
    pub guard_bits: u8,
    pub steps: Vec<QuantizationStep>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuantizationStyle {
    NoQuantization,
    ScalarDerived,
    ScalarExpounded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuantizationStep {
    pub exponent: u8,
    pub mantissa: u16,
}

impl CodestreamHeader {
    #[allow(dead_code)]
    pub(crate) fn from_parts(parts: &CodestreamParts) -> Result<Self> {
        let first_tile = parts
            .tile_parts
            .first()
            .ok_or_else(|| invalid("codestream has no tile-parts"))?;
        Self::from_marker_segments(
            parts.main_header_segments.iter().map(Vec::as_slice),
            first_tile.header,
            parts.tile_parts.len(),
        )
    }

    pub(crate) fn from_marker_segments<'a, I>(
        main_header_segments: I,
        first_tile_header: TilePartHeader,
        tile_part_count: usize,
    ) -> Result<Self>
    where
        I: IntoIterator<Item = &'a [u8]>,
    {
        Self::from_marker_segments_with_tile_headers(
            main_header_segments,
            std::iter::empty::<&'a [u8]>(),
            first_tile_header,
            tile_part_count,
        )
    }

    pub(crate) fn from_marker_segments_with_tile_headers<'a, I, J>(
        main_header_segments: I,
        tile_header_segments: J,
        first_tile_header: TilePartHeader,
        tile_part_count: usize,
    ) -> Result<Self>
    where
        I: IntoIterator<Item = &'a [u8]>,
        J: IntoIterator<Item = &'a [u8]>,
    {
        let mut siz = None;
        let mut cod = None;
        let mut qcd = None;
        let mut qcc_segments: Vec<&[u8]> = Vec::new();
        let mut coc_segments: Vec<&[u8]> = Vec::new();
        let mut comment_count = 0;

        for segment in main_header_segments {
            let marker = marker(segment)?;
            reject_unsupported_main_marker(marker)?;
            match marker {
                MARKER_SIZ => siz = Some(parse_siz(segment)?),
                MARKER_COD => cod = Some(parse_cod(segment)?),
                MARKER_QCD => qcd = Some(parse_qcd(segment)?),
                // QCC/COC carry a component index; parse after SIZ fixes the
                // component count (which selects the 1- vs 2-byte index field)
                // and after COD (COC is reconciled against the default coding).
                MARKER_QCC => qcc_segments.push(segment),
                MARKER_COC => coc_segments.push(segment),
                MARKER_COM => comment_count += 1,
                // TLM/PLM/PLT length hints are accepted and ignored.
                MARKER_TLM | MARKER_PLM | MARKER_PLT => {}
                _ => unreachable!("main marker allowlist accepted an unhandled marker"),
            }
        }
        for segment in tile_header_segments {
            let marker = marker(segment)?;
            reject_unsupported_tile_marker(marker)?;
            if marker == MARKER_COM {
                comment_count += 1;
            }
        }

        let siz = siz.ok_or_else(|| invalid("codestream main header lacks SIZ"))?;
        let cod = cod.ok_or_else(|| invalid("codestream main header lacks COD"))?;
        let mut qcd = qcd.ok_or_else(|| invalid("codestream main header lacks QCD"))?;
        // Derived quantization signals only the NLLL step; materialize the full
        // per-subband list so downstream dequant is style-agnostic (Annex E.1.1).
        expand_derived_steps(&mut qcd, cod.decomposition_levels);

        let component_count = siz.components.len();
        let mut qcc = Vec::with_capacity(qcc_segments.len());
        for segment in qcc_segments {
            let (component, mut quant) = parse_qcc(segment, component_count)?;
            expand_derived_steps(&mut quant, cod.decomposition_levels);
            if component >= component_count {
                return Err(invalid(format!(
                    "QCC references component {component}, but SIZ defines {component_count}"
                )));
            }
            // Last occurrence wins if a component is overridden twice.
            if let Some(slot) = qcc.iter_mut().find(|(c, _)| *c == component) {
                *slot = (component, quant);
            } else {
                qcc.push((component, quant));
            }
        }

        let mut coc = Vec::with_capacity(coc_segments.len());
        for segment in coc_segments {
            let (component, transform) = parse_coc(segment, component_count, &cod)?;
            if component >= component_count {
                return Err(invalid(format!(
                    "COC references component {component}, but SIZ defines {component_count}"
                )));
            }
            // A transform override that actually differs from COD is only
            // supported outside the multi-component (MCT) colour set: a lone
            // grayscale component, or an auxiliary channel (component >= 3, e.g.
            // a CMYK K plane). Overriding one of the three colour components
            // would break the shared inverse colour transform, so reject it
            // cleanly rather than mis-decode.
            if transform != cod.transform && component_count > 1 && component < 3 {
                unsupported_marker(
                    MARKER_COC,
                    "COC transform override on an MCT colour component",
                )?;
            }
            // Last occurrence wins if a component is overridden twice.
            if let Some(slot) = coc.iter_mut().find(|(c, _)| *c == component) {
                *slot = (component, transform);
            } else {
                coc.push((component, transform));
            }
        }

        validate_decoder_scope(first_tile_header, tile_part_count, &siz, &cod, &qcd, &qcc, &coc)?;
        Ok(Self {
            siz,
            cod,
            qcd,
            qcc,
            coc,
            comment_count,
        })
    }

    /// Apply a tile-part header's override markers onto a copy of these
    /// main-header defaults, producing the effective header for one tile
    /// (ISO/IEC 15444-1 Annex A.4.2). QCD/QCC override the tile's quantization
    /// and COC a per-component wavelet transform — all consumed downstream via
    /// [`Self::quant_for`]/[`Self::transform_for`] with no other plumbing.
    ///
    /// A COD in a tile header is accepted only when it restates the main coding
    /// structure (a common redundant emission); a structurally different COD, or
    /// RGN/POC/PPT/PPM, would reshape the Tier-2 packet iteration per tile and is
    /// rejected cleanly rather than mis-decoded. Segments may be gathered across
    /// all of a tile's tile-parts; per A.4.2 the markers only appear in the
    /// `TPsot == 0` part, so later parts contribute nothing.
    pub(crate) fn with_tile_overrides<'a, J>(&self, tile_header_segments: J) -> Result<Self>
    where
        J: IntoIterator<Item = &'a [u8]>,
    {
        let mut local = self.clone();
        let component_count = self.siz.components.len();

        let mut tile_qcd: Option<QcdSegment> = None;
        let mut qcc_segments: Vec<&[u8]> = Vec::new();
        let mut coc_segments: Vec<&[u8]> = Vec::new();
        let mut saw_override = false;

        for segment in tile_header_segments {
            let marker = marker(segment)?;
            match marker {
                MARKER_COD => {
                    // The effective COD must match the main header so the tile's
                    // decomposition depth, code-block geometry, precincts, and
                    // progression stay uniform (Tier-2 and reduce-level logic
                    // read one global structure). A restatement is a no-op; a
                    // real change is out of scope.
                    if parse_cod(segment)? != self.cod {
                        unsupported_marker(
                            MARKER_COD,
                            "per-tile COD override that changes the coding structure",
                        )?;
                    }
                    saw_override = true;
                }
                MARKER_QCD => {
                    tile_qcd = Some(parse_qcd(segment)?);
                    saw_override = true;
                }
                MARKER_QCC => {
                    qcc_segments.push(segment);
                    saw_override = true;
                }
                MARKER_COC => {
                    coc_segments.push(segment);
                    saw_override = true;
                }
                MARKER_PLT | MARKER_COM => {}
                _ => reject_unsupported_tile_marker(marker)?,
            }
        }

        if !saw_override {
            return Ok(local);
        }

        // Quantization steps and reconciliation depend on the decomposition
        // depth; a tile COD is required to equal the main COD, so use it.
        let effective_cod = &self.cod;

        if let Some(mut qcd) = tile_qcd {
            expand_derived_steps(&mut qcd, effective_cod.decomposition_levels);
            local.qcd = qcd;
        }

        for segment in qcc_segments {
            let (component, mut quant) = parse_qcc(segment, component_count)?;
            expand_derived_steps(&mut quant, effective_cod.decomposition_levels);
            if component >= component_count {
                return Err(invalid(format!(
                    "tile QCC references component {component}, but SIZ defines {component_count}"
                )));
            }
            if let Some(slot) = local.qcc.iter_mut().find(|(c, _)| *c == component) {
                *slot = (component, quant);
            } else {
                local.qcc.push((component, quant));
            }
        }

        for segment in coc_segments {
            let (component, transform) = parse_coc(segment, component_count, effective_cod)?;
            if component >= component_count {
                return Err(invalid(format!(
                    "tile COC references component {component}, but SIZ defines {component_count}"
                )));
            }
            // Same MCT guard as the main-header path: a transform override on one
            // of the three colour components would break the shared inverse
            // colour transform.
            if transform != effective_cod.transform && component_count > 1 && component < 3 {
                unsupported_marker(
                    MARKER_COC,
                    "tile COC transform override on an MCT colour component",
                )?;
            }
            if let Some(slot) = local.coc.iter_mut().find(|(c, _)| *c == component) {
                *slot = (component, transform);
            } else {
                local.coc.push((component, transform));
            }
        }

        // Re-validate: the tile's quantization must be a supported (transform,
        // style) pair carrying one step per subband, so a 5/3 + scalar override
        // can't reach the reversible reconstruct's `NoQuantization` assert. Each
        // QCC is checked against its component's effective (post-COC) transform.
        validate_quant(
            effective_cod.transform,
            effective_cod.decomposition_levels,
            &local.qcd,
            "tile QCD",
        )?;
        for (component, quant) in &local.qcc {
            let transform = effective_transform(effective_cod, &local.coc, *component);
            validate_quant(
                transform,
                effective_cod.decomposition_levels,
                quant,
                &format!("tile QCC component {component}"),
            )?;
        }

        Ok(local)
    }
}

fn parse_siz(segment: &[u8]) -> Result<SizSegment> {
    let body = body(segment)?;
    if body.len() < 38 {
        return Err(invalid("SIZ marker body is too short"));
    }
    let component_count = read_u16(body, 34)? as usize;
    let expected_len = 36usize
        .checked_add(
            component_count
                .checked_mul(3)
                .ok_or_else(|| invalid("SIZ component length overflow"))?,
        )
        .ok_or_else(|| invalid("SIZ length overflow"))?;
    if body.len() != expected_len {
        return Err(invalid("SIZ marker length does not match component count"));
    }

    let mut components = Vec::with_capacity(component_count);
    for idx in 0..component_count {
        let off = 36 + idx * 3;
        let ssiz = body[off];
        components.push(ComponentSiz {
            precision: (ssiz & 0x7f) + 1,
            signed: ssiz & 0x80 != 0,
            dx: body[off + 1],
            dy: body[off + 2],
        });
    }

    Ok(SizSegment {
        rsiz: read_u16(body, 0)?,
        width: read_u32(body, 2)?,
        height: read_u32(body, 6)?,
        x_origin: read_u32(body, 10)?,
        y_origin: read_u32(body, 14)?,
        tile_width: read_u32(body, 18)?,
        tile_height: read_u32(body, 22)?,
        tile_x_origin: read_u32(body, 26)?,
        tile_y_origin: read_u32(body, 30)?,
        components,
    })
}

fn parse_cod(segment: &[u8]) -> Result<CodSegment> {
    let body = body(segment)?;
    if body.len() < 10 {
        return Err(invalid("COD marker body is too short"));
    }
    let scod = body[0];
    let uses_precincts = scod & 0x01 != 0;
    let progression_order = match body[1] {
        0 => ProgressionOrder::Lrcp,
        1 => ProgressionOrder::Rlcp,
        2 => ProgressionOrder::Rpcl,
        3 => ProgressionOrder::Pcrl,
        4 => ProgressionOrder::Cprl,
        value => return Err(invalid(format!("unsupported progression order {value}"))),
    };
    let decomposition_levels = body[5];
    let expected_len = if uses_precincts {
        10usize
            .checked_add(usize::from(decomposition_levels) + 1)
            .ok_or_else(|| invalid("COD precinct length overflow"))?
    } else {
        10
    };
    if body.len() != expected_len {
        return Err(invalid(
            "COD marker length does not match Scod precinct flag",
        ));
    }

    let style = body[8];
    if style & !0x3f != 0 {
        return Err(invalid("COD code-block style has reserved bits set"));
    }
    let transform = wavelet_transform_from_byte(body[9])?;
    let precinct_sizes = body[10..]
        .iter()
        .map(|value| PrecinctSize {
            pp_x: value & 0x0f,
            pp_y: value >> 4,
        })
        .collect();

    let code_block_width = code_block_dimension(body[6], "width")?;
    let code_block_height = code_block_dimension(body[7], "height")?;

    Ok(CodSegment {
        progression_order,
        layers: read_u16(body, 2)?,
        use_mct: body[4] != 0,
        decomposition_levels,
        code_block_width,
        code_block_height,
        code_block_style: code_block_style_from_byte(style),
        transform,
        uses_precincts,
        sop_markers: scod & 0x02 != 0,
        eph_markers: scod & 0x04 != 0,
        precinct_sizes,
    })
}

/// Decode the 1-byte code-block style field shared by COD/COC (T.88 §A.6.1).
fn code_block_style_from_byte(style: u8) -> CodeBlockStyle {
    CodeBlockStyle {
        bypass: style & 0x01 != 0,
        reset_contexts: style & 0x02 != 0,
        terminate_each_pass: style & 0x04 != 0,
        vertical_causal: style & 0x08 != 0,
        predictable_termination: style & 0x10 != 0,
        segmentation_symbols: style & 0x20 != 0,
    }
}

fn wavelet_transform_from_byte(value: u8) -> Result<WaveletTransform> {
    match value {
        0 => Ok(WaveletTransform::Irreversible97),
        1 => Ok(WaveletTransform::Reversible53),
        value => Err(invalid(format!("unsupported wavelet transform {value}"))),
    }
}

/// Parse a COC marker (Annex A.6.2) and reconcile it against the default COD,
/// returning `(component, transform)`.
///
/// A COC overrides the whole per-component coding style, but this decoder only
/// implements a transform override (colour components 9/7, an auxiliary channel
/// 5/3 — the CMYK "lossless K" pattern). A COC that changes the decomposition
/// depth, code-block geometry/style, or precinct partition would require the
/// Tier-2 packet iterator and reconstruction to run per-component structure and
/// is rejected cleanly rather than mis-decoded.
fn parse_coc(segment: &[u8], component_count: usize, cod: &CodSegment) -> Result<(usize, WaveletTransform)> {
    let body = body(segment)?;
    // Ccoc is 1 byte for <257 components, 2 bytes otherwise (mirrors Cqcc).
    let (component, rest) = if component_count < 257 {
        let component = *body
            .first()
            .ok_or_else(|| invalid("COC marker body is too short"))?;
        (usize::from(component), &body[1..])
    } else {
        if body.len() < 2 {
            return Err(invalid("COC marker body is too short"));
        }
        (usize::from(read_u16(body, 0)?), &body[2..])
    };
    // Scoc (1 byte) + SPcoc: levels, cblk w/h, style, transform (5 bytes),
    // plus one precinct byte per resolution when Scoc signals precincts.
    if rest.len() < 6 {
        return Err(invalid("COC marker body is too short"));
    }
    let scoc = rest[0];
    let uses_precincts = scoc & 0x01 != 0;
    let sp = &rest[1..];
    let decomposition_levels = sp[0];
    let code_block_width = code_block_dimension(sp[1], "width")?;
    let code_block_height = code_block_dimension(sp[2], "height")?;
    let style_byte = sp[3];
    if style_byte & !0x3f != 0 {
        return Err(invalid("COC code-block style has reserved bits set"));
    }
    let transform = wavelet_transform_from_byte(sp[4])?;
    let expected_len = if uses_precincts {
        6usize + usize::from(decomposition_levels) + 1
    } else {
        6
    };
    if rest.len() != expected_len {
        return Err(invalid(
            "COC marker length does not match Scoc precinct flag",
        ));
    }
    let precinct_sizes: Vec<PrecinctSize> = sp[5..]
        .iter()
        .map(|value| PrecinctSize {
            pp_x: value & 0x0f,
            pp_y: value >> 4,
        })
        .collect();

    // Accept only a transform-only override; anything that reshapes the Tier-2
    // packet structure is out of scope and must fail cleanly (see the doc note).
    if decomposition_levels != cod.decomposition_levels
        || code_block_width != cod.code_block_width
        || code_block_height != cod.code_block_height
        || code_block_style_from_byte(style_byte) != cod.code_block_style
        || uses_precincts != cod.uses_precincts
        || precinct_sizes != cod.precinct_sizes
    {
        unsupported_marker(MARKER_COC, "COC that overrides more than the wavelet transform")?;
    }
    Ok((component, transform))
}

fn parse_qcd(segment: &[u8]) -> Result<QcdSegment> {
    let body = body(segment)?;
    parse_quant_fields(body, "QCD")
}

/// Expand a scalar-*derived* quantization segment (Annex E.1.1) into the full
/// per-subband step list an *expounded* segment would carry, so the rest of the
/// pipeline (which indexes `steps` by subband) needs no derived-specific path.
///
/// A derived segment signals only the `NLLL` step `(ε0, μ0)`; every other
/// subband reuses `μ0` and takes `εb = max(0, ε0 − (b−1)/3)` where `b` is the
/// 0-based subband index (0 = `NLLL`, then three bands — `HL`, `LH`, `HH` — per
/// resolution). This mirrors OpenJPEG's derivation in `opj_j2k_read_SQcd_SQcc`.
fn expand_derived_steps(quant: &mut QcdSegment, decomposition_levels: u8) {
    if quant.style != QuantizationStyle::ScalarDerived {
        return;
    }
    let Some(&base) = quant.steps.first() else {
        return;
    };
    let count = 1 + usize::from(decomposition_levels) * 3;
    let mut steps = Vec::with_capacity(count);
    steps.push(base);
    for band_no in 1..count {
        let decrement = ((band_no - 1) / 3) as i32;
        let exponent = (i32::from(base.exponent) - decrement).max(0) as u8;
        steps.push(QuantizationStep {
            exponent,
            mantissa: base.mantissa,
        });
    }
    quant.steps = steps;
}

/// Parse a QCC marker (Annex A.6.5): a component index (`Cqcc`, 1 byte when the
/// codestream has fewer than 257 components, else 2 bytes) followed by the same
/// `SQcc`/`SPqcc` quantization fields a QCD carries. Returns the component and
/// its quantization.
fn parse_qcc(segment: &[u8], component_count: usize) -> Result<(usize, QcdSegment)> {
    let body = body(segment)?;
    let (component, rest) = if component_count < 257 {
        let c = *body
            .first()
            .ok_or_else(|| invalid("QCC marker body is too short"))?;
        (usize::from(c), &body[1..])
    } else {
        (usize::from(read_u16(body, 0)?), &body[2..])
    };
    let quant = parse_quant_fields(rest, "QCC")?;
    Ok((component, quant))
}

/// Shared `SQxx`/`SPxx` quantization-field parser used by both QCD and QCC
/// (they differ only in the QCC component-index prefix).
fn parse_quant_fields(body: &[u8], marker_name: &str) -> Result<QcdSegment> {
    if body.is_empty() {
        return Err(invalid(format!("{marker_name} marker body is too short")));
    }
    let sqcd = body[0];
    let guard_bits = sqcd >> 5;
    let style = match sqcd & 0x1f {
        0 => QuantizationStyle::NoQuantization,
        1 => QuantizationStyle::ScalarDerived,
        2 => QuantizationStyle::ScalarExpounded,
        value => return Err(invalid(format!("unsupported {marker_name} style {value}"))),
    };
    let payload = &body[1..];
    let steps = match style {
        QuantizationStyle::NoQuantization => payload
            .iter()
            .map(|value| QuantizationStep {
                exponent: value >> 3,
                mantissa: 0,
            })
            .collect(),
        QuantizationStyle::ScalarDerived | QuantizationStyle::ScalarExpounded => {
            if payload.len() % 2 != 0 {
                return Err(invalid(format!(
                    "{marker_name} 16-bit step payload has odd length"
                )));
            }
            let mut steps = Vec::with_capacity(payload.len() / 2);
            for chunk in payload.chunks_exact(2) {
                let packed = u16::from_be_bytes([chunk[0], chunk[1]]);
                steps.push(QuantizationStep {
                    exponent: (packed >> 11) as u8,
                    mantissa: packed & 0x07ff,
                });
            }
            steps
        }
    };
    Ok(QcdSegment {
        style,
        guard_bits,
        steps,
    })
}

fn validate_decoder_scope(
    _first_tile_header: TilePartHeader,
    tile_part_count: usize,
    siz: &SizSegment,
    cod: &CodSegment,
    qcd: &QcdSegment,
    qcc: &[(usize, QcdSegment)],
    coc: &[(usize, WaveletTransform)],
) -> Result<()> {
    if tile_part_count == 0 {
        return Err(invalid("codestream has no tile-parts"));
    }
    if siz.rsiz != 0 {
        return Err(invalid("only Part 1 Rsiz=0 codestreams are supported"));
    }
    if siz.x_origin != 0 || siz.y_origin != 0 || siz.tile_x_origin != 0 || siz.tile_y_origin != 0 {
        return Err(invalid("non-zero image or tile origins are unsupported"));
    }
    if siz.tile_width == 0 || siz.tile_height == 0 {
        return Err(invalid("SIZ tile dimensions must be non-zero"));
    }
    if siz.width == 0 || siz.height == 0 {
        return Err(invalid("SIZ image dimensions must be non-zero"));
    }
    if !matches!(siz.components.len(), 1 | 3 | 4) {
        return Err(invalid(format!(
            "unsupported component layout: decoder supports 1 (grayscale), 3 (sRGB), or 4 (CMYK) components, found {} components",
            siz.components.len()
        )));
    }
    // MCT (the color transform) needs at least 3 components; on a 4-component
    // CMYK image it applies to the first three, K passing through (ISO/IEC
    // 15444-1 Annex G / OpenJPEG's `opj_mct_decode`). The single-component
    // rejection below still stands.
    for (idx, component) in siz.components.iter().enumerate() {
        if !(8..=16).contains(&component.precision) || component.signed {
            return Err(invalid(format!(
                "unsupported sample precision: decoder supports unsigned 8..=16-bit samples, component {idx} is {}-bit {}",
                component.precision,
                if component.signed {
                    "signed"
                } else {
                    "unsigned"
                }
            )));
        }
        if component.dx != 1 || component.dy != 1 {
            return Err(invalid(format!(
                "subsampled components are unsupported: component {idx} has dx={} dy={}",
                component.dx, component.dy
            )));
        }
    }
    // Tier-2 materializes all five Annex B.12 progression orders, including
    // explicit precinct-position traversal.
    let max_decompositions = max_dwt_decompositions(siz.width, siz.height);
    if cod.decomposition_levels > max_decompositions {
        return Err(invalid(format!(
            "COD decomposition levels {} exceed DWT limit {} for image dimensions {}x{}",
            cod.decomposition_levels, max_decompositions, siz.width, siz.height
        )));
    }
    if cod.layers == 0 {
        return Err(invalid("COD must signal at least one layer"));
    }
    if siz.components.len() == 1 && cod.use_mct {
        return Err(invalid("MCT is unsupported for grayscale decoder slice"));
    }
    if cod.code_block_style.bypass {
        return Err(invalid(
            "unsupported code-block style: arithmetic bypass is not implemented",
        ));
    }
    if cod.code_block_width > 1024
        || cod.code_block_height > 1024
        || cod.code_block_width * cod.code_block_height > 4096
    {
        return Err(invalid("COD code-block size exceeds Part 1 limits"));
    }
    // The default QCD and every per-component QCC override must each be a
    // quantization style the reconstruction path supports and carry the step
    // count the decomposition depth implies. Each QCC is checked against its
    // component's effective transform so a COC-overridden 5/3 channel accepts
    // its no-quantization steps.
    validate_quant(cod.transform, cod.decomposition_levels, qcd, "QCD")?;
    for (component, quant) in qcc {
        let transform = effective_transform(cod, coc, *component);
        validate_quant(
            transform,
            cod.decomposition_levels,
            quant,
            &format!("QCC component {component}"),
        )?;
    }
    Ok(())
}

/// Validate one quantization segment (a QCD or a per-component QCC) against the
/// *effective* transform for the component it governs: only the (transform,
/// style) pairs the reconstruction path implements are accepted, and the step
/// count must match the decomposition depth. The transform is passed in
/// explicitly because a per-component COC override can make one component 5/3
/// while the default COD is 9/7 (the CMYK "lossless K" pattern) — validating a
/// K-plane QCC against the global 9/7 would wrongly reject its no-quantization.
fn validate_quant(
    transform: WaveletTransform,
    decomposition_levels: u8,
    quant: &QcdSegment,
    label: &str,
) -> Result<()> {
    match (transform, quant.style) {
        (WaveletTransform::Reversible53, QuantizationStyle::NoQuantization) => {}
        // Derived steps are expanded to the expounded form before validation
        // (see `expand_derived_steps`), so both irreversible scalar styles take
        // the same path here.
        (
            WaveletTransform::Irreversible97,
            QuantizationStyle::ScalarExpounded | QuantizationStyle::ScalarDerived,
        ) => {}
        (WaveletTransform::Irreversible97, QuantizationStyle::NoQuantization) => {
            return Err(invalid(format!(
                "unsupported quantization: irreversible 9/7 requires scalar quantization ({label})"
            )));
        }
        (
            WaveletTransform::Reversible53,
            QuantizationStyle::ScalarDerived | QuantizationStyle::ScalarExpounded,
        ) => {
            return Err(invalid(format!(
                "unsupported quantization: reversible 5/3 currently supports no-quantization only ({label})"
            )));
        }
    }
    if quant.steps.is_empty() {
        return Err(invalid(format!(
            "{label} must provide at least one quantization step"
        )));
    }
    let expected_steps = 1usize + usize::from(decomposition_levels) * 3;
    // Every accepted style now carries one step per subband (derived segments
    // were expanded upstream), so the count must match the decomposition depth.
    if quant.steps.len() != expected_steps {
        return Err(invalid(format!(
            "{label} step count {} does not match decomposition levels {decomposition_levels}; expected {expected_steps} steps",
            quant.steps.len(),
        )));
    }
    Ok(())
}

/// The effective wavelet transform for `component`: its COC override if one was
/// signalled, otherwise the default COD transform. Mirrors
/// [`CodestreamHeader::transform_for`] for the pre-construction validation path.
fn effective_transform(
    cod: &CodSegment,
    coc: &[(usize, WaveletTransform)],
    component: usize,
) -> WaveletTransform {
    coc.iter()
        .find(|(index, _)| *index == component)
        .map(|(_, transform)| *transform)
        .unwrap_or(cod.transform)
}

fn code_block_dimension(encoded: u8, axis: &str) -> Result<u32> {
    if encoded > 8 {
        return Err(invalid(format!(
            "unsupported code-block {axis}: encoded exponent {encoded} exceeds Part 1 limit"
        )));
    }
    Ok(1u32 << (u32::from(encoded) + 2))
}

fn max_dwt_decompositions(width: u32, height: u32) -> u8 {
    let max_dim = width.max(height);
    if max_dim <= 1 {
        return 0;
    }
    // Annex F applies the separable transform independently on both axes. Once
    // one axis reaches a one-sample low-pass interval, later even-origin
    // decomposition steps on that axis are identities while the longer axis
    // continues to split. Therefore the usable level count is bounded by the
    // larger dimension, not the smaller one. `ceil(log2(max_dim))` is the
    // number of levels needed for both axes to reach one sample.
    (u32::BITS - (max_dim - 1).leading_zeros()) as u8
}

fn reject_unsupported_main_marker(marker: u16) -> Result<()> {
    match marker {
        MARKER_SIZ | MARKER_COD | MARKER_COC | MARKER_QCD | MARKER_QCC | MARKER_COM => Ok(()),
        // TLM/PLM/PLT are packet/tile-part *length* markers — random-access
        // hints. Sequential decoding derives the same lengths from SOT `Psot`
        // and the packet headers, so they are safely ignored (as OpenJPEG does
        // when not indexing).
        MARKER_TLM | MARKER_PLM | MARKER_PLT => Ok(()),
        MARKER_CAP => unsupported_marker(marker, "CAP capabilities marker"),
        MARKER_RGN => unsupported_marker(marker, "RGN region-of-interest marker"),
        MARKER_POC => unsupported_marker(marker, "POC progression-order change marker"),
        MARKER_PPM => unsupported_marker(marker, "PPM packed packet headers"),
        MARKER_PPT => unsupported_marker(marker, "PPT packed packet headers"),
        MARKER_CRG => unsupported_marker(marker, "CRG component registration marker"),
        _ => Err(unsupported(format!(
            "unsupported codestream marker 0x{marker:04x} in main header"
        ))),
    }
}

fn reject_unsupported_tile_marker(marker: u16) -> Result<()> {
    match marker {
        // COM comments and PLT packet-length hints are accepted and ignored;
        // sequential decode does not need PLT (see the main-header note).
        MARKER_COM | MARKER_PLT => Ok(()),
        MARKER_COD => unsupported_marker(marker, "tile-part COD coding-style override"),
        MARKER_COC => unsupported_marker(marker, "tile-part COC component coding-style override"),
        MARKER_QCD => unsupported_marker(marker, "tile-part QCD quantization override"),
        MARKER_QCC => unsupported_marker(marker, "tile-part QCC component quantization override"),
        MARKER_RGN => unsupported_marker(marker, "tile-part RGN region-of-interest marker"),
        MARKER_POC => unsupported_marker(marker, "tile-part POC progression-order change marker"),
        MARKER_PPT => unsupported_marker(marker, "tile-part PPT packed packet headers"),
        _ => Err(unsupported(format!(
            "unsupported codestream marker 0x{marker:04x} in tile-part header"
        ))),
    }
}

fn unsupported_marker(marker: u16, name: &str) -> Result<()> {
    Err(unsupported(format!(
        "unsupported JPEG 2000 feature: {name} (marker 0x{marker:04x})"
    )))
}

fn marker(segment: &[u8]) -> Result<u16> {
    if segment.len() < 4 {
        return Err(invalid("truncated marker segment"));
    }
    Ok(u16::from_be_bytes([segment[0], segment[1]]))
}

fn body(segment: &[u8]) -> Result<&[u8]> {
    if segment.len() < 4 {
        return Err(invalid("truncated marker segment"));
    }
    let marker_len = u16::from_be_bytes([segment[2], segment[3]]) as usize;
    if marker_len < 2 || marker_len + 2 != segment.len() {
        return Err(invalid("marker segment length mismatch"));
    }
    Ok(&segment[4..])
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16> {
    let slice = bytes
        .get(offset..offset + 2)
        .ok_or_else(|| invalid("unexpected end of marker u16"))?;
    Ok(u16::from_be_bytes([slice[0], slice[1]]))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32> {
    let slice = bytes
        .get(offset..offset + 4)
        .ok_or_else(|| invalid("unexpected end of marker u32"))?;
    Ok(u32::from_be_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

fn invalid(message: impl Into<String>) -> Jp2LamError {
    Jp2LamError::DecodeFailed(message.into())
}

fn unsupported(message: impl Into<String>) -> Jp2LamError {
    Jp2LamError::UnsupportedFeature(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::j2k::{
        MARKER_CAP, MARKER_COC, MARKER_CRG, MARKER_PLM, MARKER_PLT, MARKER_POC, MARKER_PPM,
        MARKER_PPT, MARKER_QCC, MARKER_RGN, MARKER_TLM,
    };

    #[test]
    fn max_dwt_decompositions_uses_ceil_log2() {
        // `ceil(log2(max_dim))`: the shorter axis may already be a one-sample
        // identity while the longer axis continues decomposing.
        assert_eq!(max_dwt_decompositions(256, 256), 8); // 2^8
        assert_eq!(max_dwt_decompositions(176, 18), 8);
        assert_eq!(max_dwt_decompositions(176, 16), 8);
        assert_eq!(max_dwt_decompositions(100, 100), 7); // ceil(log2(100)) = 7
        assert_eq!(max_dwt_decompositions(1, 512), 9);
        assert_eq!(max_dwt_decompositions(1, 1), 0);
    }

    #[test]
    fn unsupported_main_marker_fails_fast_with_feature_name() {
        let mut segments = supported_segments();
        segments.push(segment(MARKER_RGN, &[0x00, 0x00, 0x00]));

        let err = CodestreamHeader::from_marker_segments(
            segments.iter().map(Vec::as_slice),
            tile_header(0),
            1,
        )
        .expect_err("RGN should be rejected");

        assert!(matches!(err, Jp2LamError::UnsupportedFeature(_)), "{err:?}");
        let err = err.to_string();
        assert!(err.contains("RGN region-of-interest marker"), "{err}");
    }

    #[test]
    fn unsupported_main_marker_matrix_fails_fast_with_feature_names() {
        // TLM/PLM/PLT are length hints now accepted and ignored, so they are
        // deliberately absent from this rejection matrix.
        for (marker, expected) in [
            (MARKER_CAP, "CAP capabilities marker"),
            (MARKER_RGN, "RGN region-of-interest marker"),
            (MARKER_POC, "POC progression-order change marker"),
            (MARKER_PPM, "PPM packed packet headers"),
            (MARKER_PPT, "PPT packed packet headers"),
            (MARKER_CRG, "CRG component registration marker"),
        ] {
            let mut segments = supported_segments();
            segments.push(segment(marker, &[0x00, 0x00]));

            let err = CodestreamHeader::from_marker_segments(
                segments.iter().map(Vec::as_slice),
                tile_header(0),
                1,
            )
            .unwrap_err()
            .to_string();

            assert!(err.contains(expected), "marker 0x{marker:04x}: {err}");
        }
    }

    #[test]
    fn unsupported_tile_header_marker_fails_fast_with_feature_name() {
        let tile_segments = [segment(MARKER_QCC, &[0x00, 0x00])];

        let err = CodestreamHeader::from_marker_segments_with_tile_headers(
            supported_segments().iter().map(Vec::as_slice),
            tile_segments.iter().map(Vec::as_slice),
            tile_header(0),
            1,
        )
        .expect_err("QCC should be rejected")
        .to_string();

        assert!(
            err.contains("tile-part QCC component quantization override"),
            "{err}"
        );
    }

    #[test]
    fn unsupported_tile_header_marker_matrix_fails_fast_with_feature_names() {
        // PLT is a length hint now accepted and ignored in tile headers.
        for (marker, expected) in [
            (MARKER_COD, "tile-part COD coding-style override"),
            (MARKER_COC, "tile-part COC component coding-style override"),
            (MARKER_QCD, "tile-part QCD quantization override"),
            (MARKER_QCC, "tile-part QCC component quantization override"),
            (MARKER_RGN, "tile-part RGN region-of-interest marker"),
            (MARKER_POC, "tile-part POC progression-order change marker"),
            (MARKER_PPT, "tile-part PPT packed packet headers"),
        ] {
            let tile_segments = [segment(marker, &[0x00, 0x00])];

            let err = CodestreamHeader::from_marker_segments_with_tile_headers(
                supported_segments().iter().map(Vec::as_slice),
                tile_segments.iter().map(Vec::as_slice),
                tile_header(0),
                1,
            )
            .unwrap_err()
            .to_string();

            assert!(err.contains(expected), "marker 0x{marker:04x}: {err}");
        }
    }

    #[test]
    fn multiple_layers_are_accepted() {
        // Multi-layer codestreams are the norm for rate-controlled encoders
        // (Kakadu's `-slope` output uses 20). Each layer appends coding
        // passes to the same code-blocks; Tier-2 merges the contributions.
        for layers in [1u16, 2, 20] {
            let mut segments = supported_segments();
            segments[1] = cod_segment(layers, 0);

            let header = CodestreamHeader::from_marker_segments(
                segments.iter().map(Vec::as_slice),
                tile_header(0),
                1,
            )
            .expect("multi-layer COD should be accepted");

            assert_eq!(header.cod.layers, layers);
        }
    }

    #[test]
    fn zero_layers_are_rejected() {
        let mut segments = supported_segments();
        segments[1] = cod_segment(0, 0);

        let err = CodestreamHeader::from_marker_segments(
            segments.iter().map(Vec::as_slice),
            tile_header(0),
            1,
        )
        .expect_err("zero layers is malformed")
        .to_string();

        assert!(err.contains("at least one layer"), "{err}");
    }

    #[test]
    fn segmentation_symbols_are_accepted() {
        // Segmentation symbols only add four UNIFORM-context symbols per
        // cleanup pass (D.5); Tier-1 consumes them, packet syntax is
        // unaffected.
        let mut segments = supported_segments();
        segments[1] = cod_segment(1, 0x20);

        let header = CodestreamHeader::from_marker_segments(
            segments.iter().map(Vec::as_slice),
            tile_header(0),
            1,
        )
        .expect("segmentation symbols should be accepted");

        assert!(header.cod.code_block_style.segmentation_symbols);
    }

    #[test]
    fn hdr_precision_fails_before_packet_decode() {
        let mut segments = supported_segments();
        segments[0] = siz_segment(17, false, 1);

        let err = CodestreamHeader::from_marker_segments(
            segments.iter().map(Vec::as_slice),
            tile_header(0),
            1,
        )
        .expect_err("HDR precision should be rejected")
        .to_string();

        assert!(err.contains("unsupported sample precision"), "{err}");
    }

    #[test]
    fn progression_change_marker_fails_fast() {
        let mut segments = supported_segments();
        segments.push(segment(MARKER_POC, &[0x00, 0x00]));

        let err = CodestreamHeader::from_marker_segments(
            segments.iter().map(Vec::as_slice),
            tile_header(0),
            1,
        )
        .expect_err("POC should be rejected")
        .to_string();

        assert!(err.contains("POC progression-order change marker"), "{err}");
    }

    /// A transform-only COC: same coding style as `cod_segment(1, 0)` (levels 5,
    /// code-block exponents 4/4, style 0, no precincts) but a chosen transform.
    fn coc_segment(component: u8, transform: u8) -> Vec<u8> {
        segment(MARKER_COC, &[component, 0, 5, 4, 4, 0, transform])
    }

    #[test]
    fn coc_transform_override_on_grayscale_is_accepted() {
        // supported_segments() is a single-component (grayscale) 9/7 stream; a COC
        // that only flips component 0 to 5/3 is honoured.
        let mut segments = supported_segments();
        segments.push(coc_segment(0, 1));
        let header = CodestreamHeader::from_marker_segments(
            segments.iter().map(Vec::as_slice),
            tile_header(0),
            1,
        )
        .expect("transform-only COC should decode");
        assert_eq!(header.transform_for(0), WaveletTransform::Reversible53);
    }

    #[test]
    fn coc_structural_override_is_rejected() {
        // A COC that changes the code-block width (exponent 3, not COD's 4) would
        // reshape the Tier-2 packet structure and is refused cleanly.
        let mut segments = supported_segments();
        segments.push(segment(MARKER_COC, &[0, 0, 5, 3, 4, 0, 1]));
        let err = CodestreamHeader::from_marker_segments(
            segments.iter().map(Vec::as_slice),
            tile_header(0),
            1,
        )
        .expect_err("structural COC should be rejected")
        .to_string();
        assert!(err.contains("more than the wavelet transform"), "{err}");
    }

    #[test]
    fn coc_transform_override_on_colour_component_is_rejected() {
        // In a three-component image, components 0-2 form the MCT colour set; a
        // transform override on one of them would break the shared colour
        // transform and is refused.
        let segments = vec![
            siz_segment(8, false, 3),
            cod_segment(1, 0),
            qcd_segment(),
            coc_segment(1, 1),
        ];
        let err = CodestreamHeader::from_marker_segments(
            segments.iter().map(Vec::as_slice),
            tile_header(0),
            1,
        )
        .expect_err("colour-component COC override should be rejected")
        .to_string();
        assert!(err.contains("MCT colour component"), "{err}");
    }

    /// A QCC marker: component prefix + `SQcc` + `step_count` scalar-expounded
    /// steps all sharing `exponent` (mantissa 0).
    fn qcc_segment(component: u8, sqcd: u8, exponent: u16, step_count: usize) -> Vec<u8> {
        let packed = exponent << 11;
        let mut body = vec![component, sqcd];
        for _ in 0..step_count {
            body.extend_from_slice(&packed.to_be_bytes());
        }
        segment(MARKER_QCC, &body)
    }

    fn base_header(segments: &[Vec<u8>]) -> CodestreamHeader {
        CodestreamHeader::from_marker_segments(segments.iter().map(Vec::as_slice), tile_header(0), 1)
            .expect("base header")
    }

    #[test]
    fn tile_qcc_overrides_steps() {
        // Grayscale 9/7 base (QCD exponent 8); a tile-part QCC for component 0
        // with exponent 6 changes that tile's quantization, leaving the base
        // header untouched.
        let base = base_header(&supported_segments());
        let base_steps = base.quant_for(0).steps.clone();

        let qcc = qcc_segment(0, 0x22, 6, 16);
        let tile = base
            .with_tile_overrides(std::iter::once(qcc.as_slice()))
            .expect("tile QCC override");

        assert_eq!(tile.quant_for(0).steps[0].exponent, 6);
        assert_ne!(tile.quant_for(0).steps, base_steps);
        assert_eq!(base.quant_for(0).steps, base_steps);
    }

    #[test]
    fn tile_qcd_overrides_default() {
        // A tile QCD (0xff5c) replaces the default for every component.
        let base = base_header(&supported_segments());
        let mut body = vec![0x22u8];
        let packed: u16 = 4 << 11;
        for _ in 0..16 {
            body.extend_from_slice(&packed.to_be_bytes());
        }
        let qcd = segment(MARKER_QCD, &body);

        let tile = base
            .with_tile_overrides(std::iter::once(qcd.as_slice()))
            .expect("tile QCD override");

        assert_eq!(tile.quant_for(0).steps[0].exponent, 4);
        assert_ne!(
            tile.quant_for(0).steps[0].exponent,
            base.quant_for(0).steps[0].exponent
        );
    }

    #[test]
    fn tile_cod_identical_restatement_is_noop() {
        // Encoders routinely restate the main COD in the tile header; an
        // identical restatement must be accepted without changing anything.
        let base = base_header(&supported_segments());
        let cod = cod_segment(1, 0);
        let tile = base
            .with_tile_overrides(std::iter::once(cod.as_slice()))
            .expect("identical COD restatement is a no-op");
        assert_eq!(tile.cod, base.cod);
    }

    #[test]
    fn tile_cod_structural_change_rejected() {
        // cod_segment(_, 0) declares 5 decomposition levels; a tile COD with 4
        // reshapes the coding structure and is refused cleanly.
        let base = base_header(&supported_segments());
        let cod = segment(MARKER_COD, &[0, 0, 0, 1, 0, 4, 4, 4, 0, 0]);
        let err = base
            .with_tile_overrides(std::iter::once(cod.as_slice()))
            .expect_err("structural per-tile COD should be rejected")
            .to_string();
        assert!(err.contains("changes the coding structure"), "{err}");
    }

    #[test]
    fn tile_coc_transform_override_on_aux_channel() {
        // 4-component (CMYK) base; a tile COC flips the K plane (component 3) to
        // 5/3 while the colour trio stays 9/7 — the per-tile "lossless K" case.
        let base = base_header(&[siz_segment(8, false, 4), cod_segment(1, 0), qcd_segment()]);
        let coc = coc_segment(3, 1);
        let tile = base
            .with_tile_overrides(std::iter::once(coc.as_slice()))
            .expect("aux-channel tile COC override");
        assert_eq!(tile.transform_for(3), WaveletTransform::Reversible53);
        assert_eq!(tile.transform_for(0), WaveletTransform::Irreversible97);
    }

    #[test]
    fn tile_coc_transform_on_colour_component_rejected() {
        // In a three-component image, a transform override on a colour component
        // (0-2) would break the shared inverse colour transform.
        let base = base_header(&[siz_segment(8, false, 3), cod_segment(1, 0), qcd_segment()]);
        let coc = coc_segment(1, 1);
        let err = base
            .with_tile_overrides(std::iter::once(coc.as_slice()))
            .expect_err("colour-component tile COC should be rejected")
            .to_string();
        assert!(err.contains("MCT colour component"), "{err}");
    }

    #[test]
    fn tile_scalar_qcc_on_reversible_base_rejected() {
        // A 5/3 base must stay no-quantization; a scalar-expounded tile QCC is
        // caught by re-validation before it can reach the reversible
        // reconstruct's NoQuantization assertion.
        let cod = segment(MARKER_COD, &[0, 0, 0, 1, 0, 5, 4, 4, 0, 1]);
        let mut qcd_body = vec![0x20u8];
        qcd_body.extend(std::iter::repeat_n(0x08u8, 16));
        let qcd = segment(MARKER_QCD, &qcd_body);
        let base = base_header(&[siz_segment(8, false, 1), cod, qcd]);

        let qcc = qcc_segment(0, 0x22, 6, 16);
        let err = base
            .with_tile_overrides(std::iter::once(qcc.as_slice()))
            .expect_err("scalar QCC on a 5/3 tile should be rejected")
            .to_string();
        assert!(err.contains("no-quantization only"), "{err}");
    }

    #[test]
    fn tile_structural_markers_are_rejected() {
        let base = base_header(&supported_segments());
        for (marker, expected) in [
            (MARKER_RGN, "RGN"),
            (MARKER_POC, "POC"),
            (MARKER_PPT, "PPT"),
        ] {
            let seg = segment(marker, &[0x00, 0x00]);
            let err = base
                .with_tile_overrides(std::iter::once(seg.as_slice()))
                .expect_err("structural tile marker should be rejected")
                .to_string();
            assert!(err.contains(expected), "marker 0x{marker:04x}: {err}");
        }
    }

    #[test]
    fn scalar_derived_quantization_expands_to_per_subband_steps() {
        // A derived QCD signals only the NLLL step (exponent 8); the header must
        // materialize the full per-subband list, halving-per-resolution the
        // exponent per Annex E.1.1 (bands are grouped three-per-resolution).
        let mut segments = supported_segments();
        segments[2] = qcd_segment_with_step_count(0x21, 1);

        let header = CodestreamHeader::from_marker_segments(
            segments.iter().map(Vec::as_slice),
            tile_header(0),
            1,
        )
        .expect("scalar-derived QCD should now decode");

        let steps = &header.qcd.steps;
        // supported_segments() declares 5 decomposition levels → 1 + 5*3 steps.
        assert_eq!(steps.len(), 16);
        assert_eq!(header.qcd.style, QuantizationStyle::ScalarDerived);
        // εb = max(0, ε0 - (b-1)/3), μb = μ0; ε0 = 8, μ0 = 0.
        let expected: [u8; 16] = [8, 8, 8, 8, 7, 7, 7, 6, 6, 6, 5, 5, 5, 4, 4, 4];
        for (band, &exp) in expected.iter().enumerate() {
            assert_eq!(steps[band].exponent, exp, "band {band}");
            assert_eq!(steps[band].mantissa, 0, "band {band}");
        }
    }

    #[test]
    fn oversized_codeblock_dimension_fails_during_cod_parse() {
        let mut segments = supported_segments();
        segments[1][10] = 9;

        let err = CodestreamHeader::from_marker_segments(
            segments.iter().map(Vec::as_slice),
            tile_header(0),
            1,
        )
        .expect_err("oversized code-block width should be rejected")
        .to_string();

        assert!(err.contains("code-block width"), "{err}");
    }

    #[test]
    fn arithmetic_bypass_codeblock_style_fails_fast() {
        for style in [0x01, 0x03, 0x3f] {
            let mut segments = supported_segments();
            segments[1] = cod_segment(1, style);

            let err = CodestreamHeader::from_marker_segments(
                segments.iter().map(Vec::as_slice),
                tile_header(0),
                1,
            )
            .unwrap_err()
            .to_string();

            assert!(
                err.contains("arithmetic bypass"),
                "style 0x{style:02x}: {err}"
            );
        }
    }

    #[test]
    fn non_bypass_codeblock_style_matrix_is_accepted() {
        for style in [0x02, 0x04, 0x08, 0x10, 0x20, 0x3e] {
            let mut segments = supported_segments();
            segments[1] = cod_segment(1, style);
            let header = CodestreamHeader::from_marker_segments(
                segments.iter().map(Vec::as_slice),
                tile_header(0),
                1,
            )
            .unwrap_or_else(|err| panic!("style 0x{style:02x}: {err}"));
            assert_eq!(header.cod.code_block_style.bypass, false);
        }
    }

    #[test]
    fn short_qcd_step_count_fails_before_packet_decode() {
        let mut segments = supported_segments();
        segments[2] = qcd_segment_with_step_count(0x22, 15);

        let err = CodestreamHeader::from_marker_segments(
            segments.iter().map(Vec::as_slice),
            tile_header(0),
            1,
        )
        .expect_err("short QCD step table should fail")
        .to_string();

        assert!(err.contains("QCD step count"), "{err}");
    }

    #[test]
    fn extra_qcd_step_count_fails_before_packet_decode() {
        let mut segments = supported_segments();
        segments[2] = qcd_segment_with_step_count(0x22, 17);

        let err = CodestreamHeader::from_marker_segments(
            segments.iter().map(Vec::as_slice),
            tile_header(0),
            1,
        )
        .expect_err("extra QCD step table should fail")
        .to_string();

        assert!(err.contains("QCD step count"), "{err}");
    }

    #[test]
    fn excessive_decomposition_levels_fail_before_packet_decode() {
        let mut segments = supported_segments();
        segments[0] = siz_segment_with_size(8, false, 1, 8, 8);

        let err = CodestreamHeader::from_marker_segments(
            segments.iter().map(Vec::as_slice),
            tile_header(0),
            1,
        )
        .expect_err("excessive decomposition levels should fail")
        .to_string();

        assert!(err.contains("decomposition levels"), "{err}");
    }

    fn supported_segments() -> Vec<Vec<u8>> {
        vec![siz_segment(8, false, 1), cod_segment(1, 0), qcd_segment()]
    }

    fn tile_header(total_parts: u8) -> TilePartHeader {
        TilePartHeader {
            tile_index: 0,
            part_index: 0,
            total_parts,
        }
    }

    fn siz_segment(precision: u8, signed: bool, components: u16) -> Vec<u8> {
        siz_segment_with_size(precision, signed, components, 256, 256)
    }

    fn siz_segment_with_size(
        precision: u8,
        signed: bool,
        components: u16,
        width: u32,
        height: u32,
    ) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&0u16.to_be_bytes());
        body.extend_from_slice(&width.to_be_bytes());
        body.extend_from_slice(&height.to_be_bytes());
        body.extend_from_slice(&0u32.to_be_bytes());
        body.extend_from_slice(&0u32.to_be_bytes());
        body.extend_from_slice(&width.to_be_bytes());
        body.extend_from_slice(&height.to_be_bytes());
        body.extend_from_slice(&0u32.to_be_bytes());
        body.extend_from_slice(&0u32.to_be_bytes());
        body.extend_from_slice(&components.to_be_bytes());
        for _ in 0..components {
            body.push((precision - 1) | if signed { 0x80 } else { 0 });
            body.push(1);
            body.push(1);
        }
        segment(MARKER_SIZ, &body)
    }

    fn cod_segment(layers: u16, style: u8) -> Vec<u8> {
        let mut body = Vec::new();
        body.push(0);
        body.push(0);
        body.extend_from_slice(&layers.to_be_bytes());
        body.push(0);
        body.push(5);
        body.push(4);
        body.push(4);
        body.push(style);
        body.push(0);
        segment(MARKER_COD, &body)
    }

    fn qcd_segment() -> Vec<u8> {
        qcd_segment_with_step_count(0x22, 16)
    }

    fn qcd_segment_with_style(sqcd: u8) -> Vec<u8> {
        qcd_segment_with_step_count(sqcd, 16)
    }

    fn qcd_segment_with_step_count(sqcd: u8, step_count: usize) -> Vec<u8> {
        let exponent = 8u16 << 11;
        let mut body = vec![sqcd];
        for _ in 0..step_count {
            body.extend_from_slice(&exponent.to_be_bytes());
        }
        segment(MARKER_QCD, &body)
    }

    fn segment(marker: u16, body: &[u8]) -> Vec<u8> {
        let marker_len = u16::try_from(body.len() + 2).expect("marker length");
        let mut out = Vec::with_capacity(body.len() + 4);
        out.extend_from_slice(&marker.to_be_bytes());
        out.extend_from_slice(&marker_len.to_be_bytes());
        out.extend_from_slice(body);
        out
    }
}
