//! JP2 box parsing for the decoder.
//!
//! Implements the small Annex I subset needed by the current decoder plan:
//! signature, file type, JP2 header with `ihdr` and enumerated `colr`, and the
//! first contiguous codestream box (`jp2c`, I.5.4).

use crate::error::{Jp2LamError, Result};
use crate::model::{ColorEncoding, ColorSpace, IccComponentModel};

const BOX_SIGNATURE: [u8; 4] = *b"jP  ";
const BOX_FILE_TYPE: [u8; 4] = *b"ftyp";
const BOX_JP2_HEADER: [u8; 4] = *b"jp2h";
const BOX_IMAGE_HEADER: [u8; 4] = *b"ihdr";
const BOX_BITS_PER_COMPONENT: [u8; 4] = *b"bpcc";
const BOX_COLOR_SPEC: [u8; 4] = *b"colr";
const BOX_PALETTE: [u8; 4] = *b"pclr";
const BOX_COMPONENT_MAPPING: [u8; 4] = *b"cmap";
const BOX_CHANNEL_DEFINITION: [u8; 4] = *b"cdef";
const BOX_CODESTREAM: [u8; 4] = *b"jp2c";

const JP2_SIGNATURE_PAYLOAD: [u8; 4] = [0x0d, 0x0a, 0x87, 0x0a];
const JP2_COMPRESSION_TYPE_J2K: u8 = 7;
const ENUM_CMYK: u32 = 12;
const ENUM_SRGB: u32 = 16;
const ENUM_GRAY: u32 = 17;
const ENUM_SYCC: u32 = 18;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedJp2<'a> {
    pub(crate) header: Jp2Header,
    pub(crate) codestream: &'a [u8],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Jp2Header {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) component_count: u16,
    pub(crate) bits_per_component: u8,
    pub(crate) component_depths: Vec<(u8, bool)>,
    pub(crate) colorspace: ColorSpace,
    pub(crate) color_encoding: ColorEncoding,
    pub(crate) has_ipr_metadata: bool,
    /// A resolved `pclr`+`cmap` palette, when the single decoded index
    /// component must be expanded into the container's output channels.
    pub(crate) palette: Option<Palette>,
    /// An in-data opacity channel declared by a `cdef` box (I.5.3.6), when the
    /// codestream carries an alpha plane the PDF layer surfaces via
    /// `/SMaskInData`. `None` when every channel is colour.
    pub(crate) alpha: Option<AlphaChannel>,
}

/// A `cdef`-declared opacity channel: the codestream component that carries
/// opacity and whether it is premultiplied (ISO/IEC 15444-1 I.5.3.6 — channel
/// type `Typ` 1 = opacity, 2 = premultiplied opacity).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AlphaChannel {
    pub(crate) component: usize,
    pub(crate) premultiplied: bool,
}

/// A JP2 palette (`pclr`) resolved against its component mapping (`cmap`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Palette {
    /// Output channels in container order.
    pub(crate) output_columns: Vec<PaletteColumn>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PaletteColumn {
    /// `values[index]` is this channel's sample for a decoded palette index.
    pub(crate) values: Vec<i32>,
    pub(crate) precision: u8,
    pub(crate) signed: bool,
}

impl Palette {
    pub(crate) fn channel_count(&self) -> usize {
        self.output_columns.len()
    }
}

pub(crate) fn parse_jp2(bytes: &[u8]) -> Result<ParsedJp2<'_>> {
    let mut cursor = BoxCursor::new(bytes);
    let signature = cursor
        .next_box()?
        .ok_or_else(|| invalid("missing JP2 signature box"))?;
    if signature.box_type != BOX_SIGNATURE || signature.payload != JP2_SIGNATURE_PAYLOAD {
        return Err(invalid("invalid JP2 signature box"));
    }

    let file_type = cursor
        .next_box()?
        .ok_or_else(|| invalid("missing JP2 file type box"))?;
    if file_type.box_type != BOX_FILE_TYPE {
        return Err(invalid("JP2 file type box must follow signature box"));
    }
    validate_file_type(file_type.payload)?;

    let mut header = None;
    let mut codestream = None;
    let mut pclr_payload = None;
    // cmap may appear inside jp2h (spec) or as a top-level box (some encoders);
    // a top-level cmap wins only if jp2h did not carry one.
    let mut top_level_cmap = None;
    while let Some(box_) = cursor.next_box()? {
        match box_.box_type {
            BOX_JP2_HEADER => {
                let (parsed, pclr, cmap) = parse_jp2_header(box_.payload)?;
                header = Some(parsed);
                pclr_payload = pclr;
                if cmap.is_some() {
                    top_level_cmap = cmap;
                }
            }
            BOX_COMPONENT_MAPPING if top_level_cmap.is_none() => {
                top_level_cmap = Some(box_.payload)
            }
            BOX_CODESTREAM if codestream.is_none() => codestream = Some(box_.payload),
            BOX_CODESTREAM => {
                return Err(invalid(
                    "unsupported JP2 layout: multiple contiguous codestream boxes",
                ));
            }
            _ => {}
        }
    }

    let mut header = header.ok_or_else(|| invalid("missing JP2 header box"))?;
    let codestream = codestream.ok_or_else(|| invalid("missing contiguous codestream box"))?;
    header.palette = match (pclr_payload, top_level_cmap) {
        (Some(pclr), Some(cmap)) => Some(resolve_palette(pclr, cmap)?),
        (Some(_), None) => return Err(invalid("JP2 pclr box without a cmap box")),
        _ => None,
    };
    // A `pclr` box is authoritative for the number of output channels: OpenJPEG's
    // `opj_jp2_apply_pclr` expands the index component to the palette's channel
    // count regardless of the enumerated `colr`, which some encoders leave
    // inconsistent (e.g. a greyscale `colr` in front of a 4-column palette, with
    // a second vendor `colr`). Align the container colour space to the palette
    // channel count so the expanded channels pass through and match OpenJPEG,
    // instead of rejecting the stream on the colr/pclr disagreement. A count with
    // no standard enumerated space is left for the decode-scope check to reject.
    if let Some(palette) = &header.palette {
        header.colorspace = match palette.channel_count() {
            1 => ColorSpace::Gray,
            3 => ColorSpace::Srgb,
            4 => ColorSpace::Cmyk,
            _ => header.colorspace,
        };
    }
    Ok(ParsedJp2 { header, codestream })
}

fn validate_file_type(payload: &[u8]) -> Result<()> {
    if payload.len() < 12 {
        return Err(invalid("JP2 file type box is too short"));
    }
    // ISO/IEC 15444-1 §I.5.2: conformance is decided by the *compatibility*
    // list (the 4-byte brands from payload[8..]), NOT the major brand at
    // payload[0..4]. A file is readable as JP2 whenever `jp2\040` appears in
    // that list, even if its major brand is `jpx ` (ISO 15444-2) or another
    // superset — Kakadu and many archive.org scans do exactly this while
    // staying JP2 Part-1 conformant.
    //
    // We also accept a `jpx ` compatibility brand with no `jp2 ` entry: such
    // files still carry a standard `jp2h`+`jp2c` structure (confirmed against
    // the corpus and OpenJPEG, which decodes them via its JP2 path), and any
    // genuinely JPX-only construct the codestream then uses is rejected
    // downstream with a specific error — never a silent mis-decode.
    let accepted = |b: &[u8]| b == b"jp2 " || b == b"jpx ";
    if payload[8..].chunks_exact(4).any(accepted) {
        Ok(())
    } else {
        Err(invalid("JP2 file type lacks a jp2/jpx compatibility brand"))
    }
}

#[allow(clippy::type_complexity)]
fn parse_jp2_header(payload: &[u8]) -> Result<(Jp2Header, Option<&[u8]>, Option<&[u8]>)> {
    let mut cursor = BoxCursor::new(payload);
    let mut image_header = None;
    let mut colr_payload = None;
    let mut bpcc_payload = None;
    let mut pclr_payload = None;
    let mut cmap_payload = None;
    let mut alpha = None;
    while let Some(box_) = cursor.next_box()? {
        match box_.box_type {
            BOX_IMAGE_HEADER => image_header = Some(parse_image_header(box_.payload)?),
            BOX_BITS_PER_COMPONENT => bpcc_payload = Some(box_.payload),
            // The colr box is captured raw and resolved after the loop: methods
            // 3 (any-ICC fallback) and 4 (vendor colour) infer the colour space
            // from the component count and channel definitions, which are not
            // fully known until every jp2h box has been seen.
            BOX_COLOR_SPEC if colr_payload.is_none() => colr_payload = Some(box_.payload),
            // pclr and cmap are captured raw and resolved together after the
            // box loop (either order is legal, and cmap references pclr).
            BOX_PALETTE => pclr_payload = Some(box_.payload),
            BOX_COMPONENT_MAPPING => cmap_payload = Some(box_.payload),
            // cdef names channel types; a colour-only cdef is the identity
            // default (harmless), but a declared opacity channel is the in-data
            // alpha the PDF layer applies as a soft mask (`/SMaskInData`).
            BOX_CHANNEL_DEFINITION => alpha = parse_channel_definitions(box_.payload)?,
            _ => {}
        }
    }

    let mut header = image_header.ok_or_else(|| invalid("JP2 header lacks ihdr box"))?;
    match (header.bits_per_component, bpcc_payload) {
        (0, Some(payload)) => {
            header.component_depths =
                parse_component_depths(payload, usize::from(header.component_count))?;
            header.bits_per_component = header
                .component_depths
                .first()
                .map(|&(precision, _)| precision)
                .ok_or_else(|| invalid("JP2 bpcc box has no component entries"))?;
        }
        (0, None) => {
            return Err(invalid(
                "JP2 ihdr BPC=255 requires a bits-per-component box (bpcc)",
            ));
        }
        (_, Some(_)) => {
            return Err(invalid("JP2 bpcc box is only valid when ihdr BPC is 255"));
        }
        (_, None) => {}
    }
    header.alpha = alpha;
    let colr = colr_payload.ok_or_else(|| invalid("JP2 header lacks colr box"))?;
    let (colorspace, color_encoding) = parse_color_spec(colr, header.component_count)?;
    header.colorspace = colorspace;
    header.color_encoding = color_encoding;
    // pclr/cmap resolution happens in `parse_jp2`: cmap may sit inside jp2h
    // (per spec) OR as a top-level box (as some encoders emit), so both raw
    // payloads are handed back for the caller to combine.
    Ok((header, pclr_payload, cmap_payload))
}

/// Resolve a `pclr` palette against a `cmap` component mapping into per-output
/// channel lookup tables. Palette-mapped channels must be sourced from
/// component 0; entry widths from 1 through 32 bits, signed or unsigned, are
/// retained so downstream normalization sees the authored sample domain.
fn resolve_palette(pclr: &[u8], cmap: &[u8]) -> Result<Palette> {
    if pclr.len() < 3 {
        return Err(invalid("JP2 pclr box is too short"));
    }
    let num_entries = u16::from_be_bytes([pclr[0], pclr[1]]) as usize;
    let num_columns = pclr[2] as usize;
    if num_columns == 0 {
        return Err(invalid("JP2 pclr declares zero palette columns"));
    }
    let bit_depths = pclr
        .get(3..3 + num_columns)
        .ok_or_else(|| invalid("JP2 pclr bit-depth table is truncated"))?;
    if bit_depths.iter().any(|&b| (b & 0x7f) >= 32) {
        return Err(unsupported(
            "unsupported JP2 feature: palette entries wider than 32 bits",
        ));
    }
    // Column-major lookup tables: columns[col][entry].
    let mut columns: Vec<PaletteColumn> = bit_depths
        .iter()
        .map(|&depth| PaletteColumn {
            values: Vec::with_capacity(num_entries),
            precision: (depth & 0x7f) + 1,
            signed: depth & 0x80 != 0,
        })
        .collect();
    let mut offset = 3 + num_columns;
    for _ in 0..num_entries {
        for column in columns.iter_mut() {
            let byte_count = usize::from(column.precision).div_ceil(8);
            let bytes = pclr
                .get(offset..offset + byte_count)
                .ok_or_else(|| invalid("JP2 pclr entry table is truncated"))?;
            let mut raw = 0u32;
            for &byte in bytes {
                raw = (raw << 8) | u32::from(byte);
            }
            let mask = if column.precision == 32 {
                u32::MAX
            } else {
                (1u32 << column.precision) - 1
            };
            raw &= mask;
            let value = if column.signed && raw & (1u32 << (column.precision - 1)) != 0 {
                (raw | !mask) as i32
            } else {
                raw as i32
            };
            column.values.push(value);
            offset += byte_count;
        }
    }

    // cmap: one 4-byte record per output channel: CMP(2) MTYP(1) PCOL(1). A
    // well-formed palette cmap has every channel palette-mapped (MTYP=1) from
    // the single index component (CMP=0) selecting a distinct column. Some real
    // encoders emit a broken cmap (e.g. MTYP=0 on channels that should be
    // palette-mapped); OpenJPEG detects this and "corrects" it by mapping each
    // output channel to the palette column of the same index. We do the same:
    // build from the cmap when it is valid, else fall back to column order.
    let mut output_columns = Vec::with_capacity(columns.len());
    let mut cmap_valid = !cmap.is_empty() && cmap.len() % 4 == 0;
    if cmap_valid {
        for record in cmap.chunks_exact(4) {
            let component = u16::from_be_bytes([record[0], record[1]]);
            let mapping_type = record[2];
            let palette_column = record[3] as usize;
            if component != 0 || mapping_type != 1 || palette_column >= columns.len() {
                cmap_valid = false;
                break;
            }
            output_columns.push(columns[palette_column].clone());
        }
    }
    if !cmap_valid {
        // OpenJPEG's fallback: use the palette columns in their natural order.
        output_columns = columns;
    }
    Ok(Palette { output_columns })
}

fn parse_image_header(payload: &[u8]) -> Result<Jp2Header> {
    if payload.len() != 14 {
        return Err(invalid("JP2 ihdr box must be 14 bytes"));
    }
    let height = read_u32(payload, 0)?;
    let width = read_u32(payload, 4)?;
    let component_count = read_u16(payload, 8)?;
    let bpc = payload[10];
    let compression_type = payload[11];
    if compression_type != JP2_COMPRESSION_TYPE_J2K {
        return Err(invalid("JP2 ihdr compression type is not JPEG 2000"));
    }
    let bits_per_component = (bpc != 0xff).then_some((bpc & 0x7f) + 1);
    let component_depths = bits_per_component
        .map(|precision| vec![(precision, bpc & 0x80 != 0); usize::from(component_count)])
        .unwrap_or_default();
    Ok(Jp2Header {
        width,
        height,
        component_count,
        bits_per_component: bits_per_component.unwrap_or(0),
        component_depths,
        colorspace: ColorSpace::Gray,
        color_encoding: ColorEncoding::Gray,
        has_ipr_metadata: payload[13] != 0,
        palette: None,
        alpha: None,
    })
}

fn parse_component_depths(payload: &[u8], component_count: usize) -> Result<Vec<(u8, bool)>> {
    if payload.len() != component_count {
        return Err(invalid(format!(
            "JP2 bpcc entry count {} does not match ihdr component count {component_count}",
            payload.len()
        )));
    }
    payload
        .iter()
        .enumerate()
        .map(|(index, &value)| {
            let precision = (value & 0x7f) + 1;
            if !(1..=16).contains(&precision) {
                return Err(unsupported(format!(
                    "unsupported JP2 bpcc precision {precision} for component {index}"
                )));
            }
            Ok((precision, value & 0x80 != 0))
        })
        .collect()
}

/// Resolve a `colr` colour-specification box (ISO/IEC 15444-1 I.5.3.3) to a
/// decoder colour space plus its container encoding.
///
/// METH 1 (enumerated) and METH 2 (restricted ICC) are self-describing. METH 3
/// (any-ICC) reuses the METH 2 profile-header layout, and METH 4 (vendor
/// colour) carries no usable profile; for both, when the profile does not name a
/// Gray/RGB space, the colour space is inferred from the SIZ component count —
/// the same 1→Gray / 3→sRGB / 4→CMYK mapping the raw-codestream path uses, which
/// is how OpenJPEG behaves when it ignores a non-regular colr method
/// (`opj_jp2_read_colr` warns "meth value is not a regular value" and falls back
/// to the component layout). A 2-component stream is treated as Gray plus an
/// auxiliary plane. PDF image decoding remains responsible for interpreting
/// that auxiliary plane (typically alpha) from the image dictionary.
fn parse_color_spec(payload: &[u8], component_count: u16) -> Result<(ColorSpace, ColorEncoding)> {
    if payload.len() < 3 {
        return Err(invalid("JP2 colr box is too short"));
    }
    let method = payload[0];
    match method {
        1 => {
            if payload.len() != 7 {
                return Err(invalid("enumerated JP2 colr box must be 7 bytes"));
            }
            match read_u32(payload, 3)? {
                ENUM_GRAY => Ok((ColorSpace::Gray, ColorEncoding::Gray)),
                ENUM_SRGB => Ok((ColorSpace::Srgb, ColorEncoding::Srgb)),
                // sYCC (EnumCS 18): three Y/Cb/Cr planes carried without MCT;
                // the decoder applies the inverse sYCC→sRGB matrix and outputs
                // sRGB, so the container encoding is sRGB.
                ENUM_SYCC => Ok((ColorSpace::YCbCr, ColorEncoding::Srgb)),
                ENUM_CMYK => Ok((ColorSpace::Cmyk, ColorEncoding::Cmyk)),
                value => Err(unsupported(format!("unsupported JP2 EnumCS value {value}"))),
            }
        }
        2 => {
            let profile = payload
                .get(3..)
                .ok_or_else(|| invalid("restricted ICC colr box is too short"))?;
            let data_space = profile
                .get(16..20)
                .ok_or_else(|| invalid("restricted ICC profile header is too short"))?;
            let (colorspace, component_model) = match data_space {
                b"GRAY" => (ColorSpace::Gray, IccComponentModel::Gray),
                b"RGB " => (ColorSpace::Srgb, IccComponentModel::Rgb),
                _ => {
                    return Err(unsupported(
                        "restricted ICC component model is not Gray/RGB",
                    ));
                }
            };
            let encoding = ColorEncoding::restricted_icc(profile.to_vec(), component_model)
                .map_err(|error| invalid(error.to_string()))?;
            Ok((colorspace, encoding))
        }
        // METH 3 (any ICC): the payload after method/prec/approx is a full ICC
        // profile with the same header layout METH 2 reads. Use the profile's
        // data colour space when it is Gray or RGB (embedding the profile as
        // METH 2 does); otherwise fall back to component-count inference, the
        // behaviour OpenJPEG lands on for profiles it cannot map to an
        // enumerated space.
        3 => {
            let profile = payload
                .get(3..)
                .ok_or_else(|| invalid("any-ICC colr box is too short"))?;
            match profile.get(16..20) {
                // An any-ICC profile need not satisfy the restricted-profile
                // constraints (XYZ PCS, etc.), so embed it when it happens to
                // validate as one — preserving the profile like METH 2 — but
                // fall back to the plain enumerated encoding otherwise. Either
                // way the decoded colour space (and thus the pixels) is the same;
                // never reject a decodable stream over profile validation.
                Some(b"GRAY") => Ok((
                    ColorSpace::Gray,
                    ColorEncoding::restricted_icc(profile.to_vec(), IccComponentModel::Gray)
                        .unwrap_or(ColorEncoding::Gray),
                )),
                Some(b"RGB ") => Ok((
                    ColorSpace::Srgb,
                    ColorEncoding::restricted_icc(profile.to_vec(), IccComponentModel::Rgb)
                        .unwrap_or(ColorEncoding::Srgb),
                )),
                // Profile too short to name a space, or a non-Gray/RGB model
                // (e.g. CMYK): defer to the component count.
                _ => infer_colorspace_from_components(component_count, "any-ICC (colr METH 3)"),
            }
        }
        // METH 4 (vendor colour): no interoperable profile, so the only signal
        // is the component layout.
        4 => infer_colorspace_from_components(component_count, "vendor colour (colr METH 4)"),
        other => Err(unsupported(format!(
            "unsupported JP2 colour specification method {other}"
        ))),
    }
}

/// Infer a colour space from the SIZ component count for colr methods that do
/// not name one (METH 4, and the METH 3 non-Gray/RGB fallback), mirroring the
/// raw-codestream default (1→Gray, 3→sRGB, 4→CMYK). Two components are
/// genuinely ambiguous — gray+alpha vs two colour channels — and grayscale
/// plus in-data alpha is not reconstructable here, so it is rejected cleanly
/// rather than guessed; a clean blank beats a wrong colour interpretation.
fn infer_colorspace_from_components(
    component_count: u16,
    context: &str,
) -> Result<(ColorSpace, ColorEncoding)> {
    match component_count {
        1 => Ok((ColorSpace::Gray, ColorEncoding::Gray)),
        3 => Ok((ColorSpace::Srgb, ColorEncoding::Srgb)),
        4 => Ok((ColorSpace::Cmyk, ColorEncoding::Cmyk)),
        // Two components is Gray plus one auxiliary plane. Which it is —
        // opacity, or a second colorant — is genuinely ambiguous here, and JP2
        // has no 2-channel enumerated space to name it, so report Gray and hand
        // both planes to the consumer. For a PDF that is the right answer
        // either way: ISO 32000-1 §7.4.9 makes the PDF `/ColorSpace`
        // authoritative over the container's, so the caller knows whether the
        // second plane is a soft mask or a `/DeviceN` colorant. Guessing here
        // instead cost whole pages: rejecting the stream rendered them blank.
        2 => Ok((ColorSpace::Gray, ColorEncoding::Gray)),
        n => Err(unsupported(format!(
            "{context}: {n}-component JP2 has no default colour space"
        ))),
    }
}

/// Parse a `cdef` channel-definition box (ISO/IEC 15444-1 I.5.3.6): a `u16`
/// count `N` followed by `N` records of `(Cn, Typ, Asoc)` `u16`s. `Typ` 0 is a
/// colour channel, 1 opacity, 2 premultiplied opacity, 65535 unspecified.
/// Returns the single opacity channel, if any — `Cn` is the codestream
/// component it maps to (identity when no `cmap` reorders channels, which is the
/// layout these archival scans use).
fn parse_channel_definitions(payload: &[u8]) -> Result<Option<AlphaChannel>> {
    if payload.len() < 2 {
        return Err(invalid("JP2 cdef box is too short"));
    }
    let count = u16::from_be_bytes([payload[0], payload[1]]) as usize;
    let records = payload
        .get(2..2 + count * 6)
        .ok_or_else(|| invalid("JP2 cdef channel records are truncated"))?;
    let mut alpha: Option<AlphaChannel> = None;
    for record in records.chunks_exact(6) {
        let channel = u16::from_be_bytes([record[0], record[1]]);
        let typ = u16::from_be_bytes([record[2], record[3]]);
        // record[4..6] is Asoc (the colour component this channel is associated
        // with); with the identity mapping these scans use it is not needed.
        let premultiplied = match typ {
            0 | 65535 => continue, // colour or unspecified
            1 => false,            // opacity
            2 => true,             // premultiplied opacity
            other => {
                return Err(unsupported(format!(
                    "unsupported JP2 cdef channel type {other}"
                )));
            }
        };
        if alpha.is_some() {
            return Err(unsupported(
                "unsupported JP2 feature: more than one cdef opacity channel",
            ));
        }
        alpha = Some(AlphaChannel {
            component: usize::from(channel),
            premultiplied,
        });
    }
    Ok(alpha)
}

#[derive(Debug, Clone, Copy)]
struct BoxRecord<'a> {
    box_type: [u8; 4],
    payload: &'a [u8],
}

#[derive(Debug, Clone, Copy)]
struct BoxCursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> BoxCursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    fn next_box(&mut self) -> Result<Option<BoxRecord<'a>>> {
        if self.pos == self.bytes.len() {
            return Ok(None);
        }
        if self.bytes.len().saturating_sub(self.pos) < 8 {
            return Err(invalid("truncated JP2 box header"));
        }

        let start = self.pos;
        let lbox = read_u32(self.bytes, start)? as u64;
        let box_type = read_box_type(self.bytes, start + 4)?;
        self.pos += 8;

        let end = match lbox {
            0 => self.bytes.len(),
            1 => {
                let xlbox = read_u64(self.bytes, self.pos)?;
                self.pos += 8;
                let end = start
                    .checked_add(
                        usize::try_from(xlbox).map_err(|_| invalid("JP2 XLBox exceeds usize"))?,
                    )
                    .ok_or_else(|| invalid("JP2 XLBox overflow"))?;
                if xlbox < 16 {
                    return Err(invalid("invalid extended JP2 box length"));
                }
                end
            }
            2..=7 => return Err(invalid("invalid JP2 box length below header size")),
            len => start
                .checked_add(
                    usize::try_from(len).map_err(|_| invalid("JP2 box length exceeds usize"))?,
                )
                .ok_or_else(|| invalid("JP2 box length overflow"))?,
        };
        if end > self.bytes.len() || end < self.pos {
            return Err(invalid("JP2 box extends past input"));
        }
        let payload = &self.bytes[self.pos..end];
        self.pos = end;
        Ok(Some(BoxRecord { box_type, payload }))
    }
}

fn read_box_type(bytes: &[u8], offset: usize) -> Result<[u8; 4]> {
    let slice = bytes
        .get(offset..offset + 4)
        .ok_or_else(|| invalid("unexpected end of JP2 box type"))?;
    Ok([slice[0], slice[1], slice[2], slice[3]])
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16> {
    let slice = bytes
        .get(offset..offset + 2)
        .ok_or_else(|| invalid("unexpected end of JP2 u16"))?;
    Ok(u16::from_be_bytes([slice[0], slice[1]]))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32> {
    let slice = bytes
        .get(offset..offset + 4)
        .ok_or_else(|| invalid("unexpected end of JP2 u32"))?;
    Ok(u32::from_be_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64> {
    let slice = bytes
        .get(offset..offset + 8)
        .ok_or_else(|| invalid("unexpected end of JP2 u64"))?;
    Ok(u64::from_be_bytes([
        slice[0], slice[1], slice[2], slice[3], slice[4], slice[5], slice[6], slice[7],
    ]))
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

    #[test]
    fn file_type_accepts_jpx_major_brand_with_jp2_compat() {
        // Major brand `jpx `, minor version 0, compatibility list [`jpx `,`jp2 `]
        // — the layout real Kakadu/archive.org scans use. Must be accepted
        // (ISO/IEC 15444-1 §I.5.2: conformance is by the compatibility list).
        let mut payload = Vec::new();
        payload.extend_from_slice(b"jpx ");
        payload.extend_from_slice(&0u32.to_be_bytes());
        payload.extend_from_slice(b"jpx ");
        payload.extend_from_slice(b"jp2 ");
        assert!(validate_file_type(&payload).is_ok());
    }

    #[test]
    fn file_type_accepts_jpx_only_compat_brand() {
        // Major+compat `jpx ` with no `jp2 ` entry: real JPX-branded scans that
        // still carry a standard jp2h/jp2c structure and decode correctly
        // (verified bit-close to OpenJPEG). Accepted; JPX-only constructs fail
        // downstream, never silently.
        let mut payload = Vec::new();
        payload.extend_from_slice(b"jpx ");
        payload.extend_from_slice(&0u32.to_be_bytes());
        payload.extend_from_slice(b"jpx ");
        assert!(validate_file_type(&payload).is_ok());
    }

    #[test]
    fn file_type_rejects_when_no_known_compat_brand() {
        // A brand that is neither jp2 nor jpx is genuinely unreadable.
        let mut payload = Vec::new();
        payload.extend_from_slice(b"mjp2");
        payload.extend_from_slice(&0u32.to_be_bytes());
        payload.extend_from_slice(b"mjp2");
        let err = validate_file_type(&payload)
            .expect_err("no jp2/jpx compat brand")
            .to_string();
        assert!(err.contains("compatibility brand"), "{err}");
    }

    #[test]
    fn bpcc_is_rejected_when_ihdr_declares_a_shared_depth() {
        let bytes = jp2_with_extra_header_box(BOX_BITS_PER_COMPONENT, &[7]);

        let err = parse_jp2(&bytes)
            .expect_err("redundant bpcc should be rejected")
            .to_string();

        assert!(err.contains("only valid when ihdr BPC is 255"), "{err}");
    }

    #[test]
    fn bpcc_preserves_mixed_component_depths() {
        let mut ihdr = ihdr_payload();
        ihdr[8..10].copy_from_slice(&4u16.to_be_bytes());
        ihdr[10] = 0xff;
        let mut header = Vec::new();
        push_box(&mut header, BOX_IMAGE_HEADER, &ihdr);
        push_box(&mut header, BOX_BITS_PER_COMPONENT, &[7, 9, 11, 15]);
        push_box(
            &mut header,
            BOX_COLOR_SPEC,
            &enumerated_colr_payload(ENUM_CMYK),
        );

        let bytes = wrap_with_header_and_codestream(header);
        let parsed = parse_jp2(&bytes).expect("mixed-depth JP2 header");
        assert_eq!(parsed.header.bits_per_component, 8);
        assert_eq!(
            parsed.header.component_depths,
            vec![(8, false), (10, false), (12, false), (16, false)]
        );
    }

    #[test]
    fn palette_without_cmap_is_rejected() {
        // A pclr box needs a cmap to map the index component to output channels;
        // pclr alone is incomplete (palettes are otherwise now supported).
        let bytes = jp2_with_extra_header_box(BOX_PALETTE, &[0, 0, 0, 0]);

        let err = parse_jp2(&bytes)
            .expect_err("pclr without cmap should be rejected")
            .to_string();

        assert!(err.contains("pclr box without a cmap box"), "{err}");
    }

    #[test]
    fn component_mapping_box_without_palette_is_ignored() {
        // A cmap with no pclr has nothing to map; it is ignored, not fatal.
        let bytes = jp2_with_extra_header_box(BOX_COMPONENT_MAPPING, &[0, 0, 0, 0]);
        let parsed = parse_jp2(&bytes).expect("cmap without pclr should parse");
        assert!(parsed.header.palette.is_none());
    }

    #[test]
    fn colour_only_channel_definition_declares_no_alpha() {
        // A cdef whose single channel is colour (Typ=0) is the identity default:
        // parsed, with no opacity channel surfaced.
        let cdef = [
            0, 1, /*Cn*/ 0, 0, /*Typ*/ 0, 0, /*Asoc*/ 0, 1,
        ];
        let bytes = jp2_with_extra_header_box(BOX_CHANNEL_DEFINITION, &cdef);
        let parsed = parse_jp2(&bytes).expect("colour-only cdef should parse");
        assert!(parsed.header.alpha.is_none());
    }

    #[test]
    fn channel_definition_opacity_channel_is_parsed() {
        // A cdef marking channel 3 as opacity (Typ=1) surfaces the in-data alpha
        // channel for `/SMaskInData` handling.
        let cdef = [
            0, 4, // N = 4 channels
            0, 0, 0, 0, 0, 1, // Cn0 colour
            0, 1, 0, 0, 0, 2, // Cn1 colour
            0, 2, 0, 0, 0, 3, // Cn2 colour
            0, 3, 0, 1, 0, 0, // Cn3 opacity (Typ=1), whole image
        ];
        let bytes = jp2_with_extra_header_box(BOX_CHANNEL_DEFINITION, &cdef);
        let parsed = parse_jp2(&bytes).expect("opacity cdef should parse");
        let alpha = parsed.header.alpha.expect("alpha channel");
        assert_eq!(alpha.component, 3);
        assert!(!alpha.premultiplied);
    }

    #[test]
    fn malformed_icc_profile_colr_fails_fast() {
        let bytes = jp2_with_colr_payload(&[2, 0, 0, 0]);

        let err = parse_jp2(&bytes)
            .expect_err("malformed ICC colr should be rejected")
            .to_string();

        assert!(
            err.contains("restricted ICC profile header is too short"),
            "{err}"
        );
    }

    #[test]
    fn multiple_codestream_boxes_fail_fast() {
        let mut bytes = minimal_jp2_header();
        push_box(&mut bytes, BOX_CODESTREAM, &[0xff, 0x4f]);
        push_box(&mut bytes, BOX_CODESTREAM, &[0xff, 0x4f]);

        let err = parse_jp2(&bytes)
            .expect_err("multiple jp2c boxes should be rejected")
            .to_string();

        assert!(
            err.contains("multiple contiguous codestream boxes"),
            "{err}"
        );
    }

    fn jp2_with_extra_header_box(box_type: [u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut header = Vec::new();
        push_box(&mut header, BOX_IMAGE_HEADER, &ihdr_payload());
        push_box(&mut header, box_type, payload);
        push_box(
            &mut header,
            BOX_COLOR_SPEC,
            &enumerated_colr_payload(ENUM_GRAY),
        );
        wrap_with_header_and_codestream(header)
    }

    fn jp2_with_colr_payload(colr: &[u8]) -> Vec<u8> {
        let mut header = Vec::new();
        push_box(&mut header, BOX_IMAGE_HEADER, &ihdr_payload());
        push_box(&mut header, BOX_COLOR_SPEC, colr);
        wrap_with_header_and_codestream(header)
    }

    fn wrap_with_header_and_codestream(header_payload: Vec<u8>) -> Vec<u8> {
        let mut bytes = minimal_jp2_header();
        push_box(&mut bytes, BOX_JP2_HEADER, &header_payload);
        push_box(&mut bytes, BOX_CODESTREAM, &[0xff, 0x4f]);
        bytes
    }

    fn minimal_jp2_header() -> Vec<u8> {
        let mut bytes = Vec::new();
        push_box(&mut bytes, BOX_SIGNATURE, &JP2_SIGNATURE_PAYLOAD);
        let mut ftyp = Vec::new();
        ftyp.extend_from_slice(b"jp2 ");
        ftyp.extend_from_slice(&0u32.to_be_bytes());
        ftyp.extend_from_slice(b"jp2 ");
        push_box(&mut bytes, BOX_FILE_TYPE, &ftyp);
        bytes
    }

    fn ihdr_payload() -> [u8; 14] {
        let mut payload = [0u8; 14];
        payload[0..4].copy_from_slice(&8u32.to_be_bytes());
        payload[4..8].copy_from_slice(&8u32.to_be_bytes());
        payload[8..10].copy_from_slice(&1u16.to_be_bytes());
        payload[10] = 7;
        payload[11] = JP2_COMPRESSION_TYPE_J2K;
        payload
    }

    fn enumerated_colr_payload(enum_cs: u32) -> [u8; 7] {
        let mut payload = [0u8; 7];
        payload[0] = 1;
        payload[3..7].copy_from_slice(&enum_cs.to_be_bytes());
        payload
    }

    #[test]
    fn enum_cs_12_is_cmyk() {
        let (space, encoding) = parse_color_spec(&enumerated_colr_payload(ENUM_CMYK), 4).unwrap();
        assert_eq!(space, ColorSpace::Cmyk);
        assert_eq!(encoding, ColorEncoding::Cmyk);
    }

    #[test]
    fn colr_meth4_vendor_infers_space_from_component_count() {
        // METH 4 carries no usable profile: 1/3/4 components map to Gray/sRGB/
        // CMYK exactly like a raw codestream. Two components remain decodable
        // as Gray plus an auxiliary plane for the PDF layer to interpret.
        let vendor = |ncomp: u16| parse_color_spec(&[4, 0, 0], ncomp);
        assert_eq!(vendor(1).unwrap().0, ColorSpace::Gray);
        assert_eq!(vendor(2).unwrap().0, ColorSpace::Gray);
        assert_eq!(vendor(3).unwrap().0, ColorSpace::Srgb);
        assert_eq!(vendor(4).unwrap().0, ColorSpace::Cmyk);
    }

    #[test]
    fn colr_meth3_any_icc_uses_gray_rgb_else_component_count() {
        // METH 3 reuses the restricted-ICC header layout. A profile naming GRAY
        // or RGB is honoured; a profile that names neither falls back to the
        // component count (here a 3-component stream → sRGB).
        let mut gray = vec![3u8, 0, 0];
        gray.extend_from_slice(&[0u8; 16]);
        gray.extend_from_slice(b"GRAY");
        gray.extend_from_slice(&[0u8; 108]);
        assert_eq!(parse_color_spec(&gray, 1).unwrap().0, ColorSpace::Gray);

        let mut rgb = vec![3u8, 0, 0];
        rgb.extend_from_slice(&[0u8; 16]);
        rgb.extend_from_slice(b"RGB ");
        rgb.extend_from_slice(&[0u8; 108]);
        assert_eq!(parse_color_spec(&rgb, 3).unwrap().0, ColorSpace::Srgb);

        let mut cmyk_profile = vec![3u8, 0, 0];
        cmyk_profile.extend_from_slice(&[0u8; 16]);
        cmyk_profile.extend_from_slice(b"CMYK");
        cmyk_profile.extend_from_slice(&[0u8; 108]);
        assert_eq!(
            parse_color_spec(&cmyk_profile, 4).unwrap().0,
            ColorSpace::Cmyk
        );
    }

    #[test]
    fn resolve_palette_maps_index_through_columns() {
        // 2-entry, 3-column palette; a valid cmap sends output channel j to
        // palette column j.
        let mut pclr = vec![0, 2, 3, 7, 7, 7]; // NE=2, NPC=3, Bi=[7,7,7]
        pclr.extend_from_slice(&[10, 11, 12]); // entry 0
        pclr.extend_from_slice(&[20, 21, 22]); // entry 1
        let cmap = [0, 0, 1, 0, 0, 0, 1, 1, 0, 0, 1, 2]; // 3 palette-mapped channels
        let palette = resolve_palette(&pclr, &cmap).unwrap();
        assert_eq!(palette.channel_count(), 3);
        assert_eq!(palette.output_columns[0].values, vec![10, 20]);
        assert_eq!(palette.output_columns[1].values, vec![11, 21]);
        assert_eq!(palette.output_columns[2].values, vec![12, 22]);
    }

    #[test]
    fn resolve_palette_falls_back_on_malformed_cmap() {
        // Channels 1 and 2 have MTYP=0 (direct) — a broken cmap. Match
        // OpenJPEG: fall back to using the palette columns in order.
        let mut pclr = vec![0, 2, 3, 7, 7, 7];
        pclr.extend_from_slice(&[10, 11, 12]);
        pclr.extend_from_slice(&[20, 21, 22]);
        let cmap = [0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        let palette = resolve_palette(&pclr, &cmap).unwrap();
        assert_eq!(palette.channel_count(), 3);
        assert_eq!(palette.output_columns[2].values, vec![12, 22]);
    }

    #[test]
    fn resolve_palette_preserves_precision_and_sign() {
        // Two one-column palettes: unsigned 4-bit and signed 12-bit.
        let unsigned = resolve_palette(&[0, 2, 1, 3, 0x0f, 0x02], &[0, 0, 1, 0]).unwrap();
        assert_eq!(unsigned.output_columns[0].precision, 4);
        assert!(!unsigned.output_columns[0].signed);
        assert_eq!(unsigned.output_columns[0].values, vec![15, 2]);

        let signed =
            resolve_palette(&[0, 2, 1, 0x80 | 11, 0x0f, 0xff, 0x08, 0x00], &[0, 0, 1, 0]).unwrap();
        assert_eq!(signed.output_columns[0].precision, 12);
        assert!(signed.output_columns[0].signed);
        assert_eq!(signed.output_columns[0].values, vec![-1, -2048]);
    }

    #[test]
    fn resolve_palette_rejects_entries_wider_than_32_bits() {
        let err = resolve_palette(&[0, 1, 1, 32], &[0, 0, 1, 0]).unwrap_err();
        assert!(err.to_string().contains("wider than 32 bits"));
    }

    #[test]
    fn palette_channel_count_overrides_inconsistent_colr_space() {
        // A real-world malformed layout: a greyscale enumerated colr (EnumCS 17)
        // in front of a 4-column palette. OpenJPEG's opj_jp2_apply_pclr expands
        // the index to the palette's 4 channels regardless, so the container
        // colour space must follow the palette (4 → CMYK) rather than the
        // inconsistent greyscale colr — otherwise the stream is wrongly rejected.
        let mut jp2h = Vec::new();
        push_box(&mut jp2h, BOX_IMAGE_HEADER, &ihdr_payload()); // ihdr nc = 1
        push_box(
            &mut jp2h,
            BOX_COLOR_SPEC,
            &enumerated_colr_payload(ENUM_GRAY),
        );
        // pclr: NE = 2 entries, NPC = 4 columns, all 8-bit, then 2×4 samples.
        let mut pclr = vec![0u8, 2, 4, 7, 7, 7, 7];
        pclr.extend_from_slice(&[10, 11, 12, 13]); // entry 0
        pclr.extend_from_slice(&[20, 21, 22, 23]); // entry 1
        push_box(&mut jp2h, BOX_PALETTE, &pclr);
        // cmap: four channels, each palette-mapped (MTYP = 1) from index comp 0
        // to columns 0..=3.
        let cmap = [0u8, 0, 1, 0, 0, 0, 1, 1, 0, 0, 1, 2, 0, 0, 1, 3];
        push_box(&mut jp2h, BOX_COMPONENT_MAPPING, &cmap);

        let mut bytes = minimal_jp2_header();
        push_box(&mut bytes, BOX_JP2_HEADER, &jp2h);
        push_box(&mut bytes, BOX_CODESTREAM, &[0xff, 0x4f]);

        let parsed = parse_jp2(&bytes).expect("palette/colr mismatch must not be rejected");
        assert_eq!(parsed.header.colorspace, ColorSpace::Cmyk);
        assert_eq!(
            parsed.header.palette.as_ref().map(|p| p.channel_count()),
            Some(4)
        );
    }

    fn push_box(out: &mut Vec<u8>, box_type: [u8; 4], payload: &[u8]) {
        let len = u32::try_from(payload.len() + 8).expect("box length");
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(&box_type);
        out.extend_from_slice(payload);
    }
}
