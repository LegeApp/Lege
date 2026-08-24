#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorSpace {
    Gray,
    #[doc(hidden)]
    Rgb,
    Srgb,
    /// 4-component subtractive CMYK (JP2 EnumCS 12). Decoded as four
    /// independent planes (no multiple-component transform); the consumer maps
    /// C/M/Y/K to output.
    Cmyk,
    #[doc(hidden)]
    Yuv,
    #[doc(hidden)]
    YCbCr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IccComponentModel {
    Gray,
    Rgb,
}

/// Explicit interpretation of decoded component samples in a JP2 container.
///
/// ICC profiles are embedded unchanged; the encoder does not color-convert the
/// supplied samples.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ColorEncoding {
    Gray,
    Srgb,
    /// Enumerated CMYK (JP2 EnumCS 12).
    Cmyk,
    IccProfile {
        bytes: Vec<u8>,
        component_model: IccComponentModel,
    },
}

impl ColorEncoding {
    pub fn restricted_icc(
        bytes: Vec<u8>,
        component_model: IccComponentModel,
    ) -> crate::error::Result<Self> {
        validate_restricted_icc(&bytes, component_model)?;
        Ok(Self::IccProfile {
            bytes,
            component_model,
        })
    }

    pub(crate) fn validate_for(self: &Self, colorspace: ColorSpace) -> crate::error::Result<()> {
        match (colorspace, self) {
            (ColorSpace::Gray, Self::Gray)
            | (ColorSpace::Cmyk, Self::Cmyk)
            | (ColorSpace::Srgb | ColorSpace::Rgb, Self::Srgb)
            | (
                ColorSpace::Gray,
                Self::IccProfile {
                    component_model: IccComponentModel::Gray,
                    ..
                },
            )
            | (
                ColorSpace::Srgb | ColorSpace::Rgb,
                Self::IccProfile {
                    component_model: IccComponentModel::Rgb,
                    ..
                },
            ) => {}
            _ => {
                return Err(invalid_input(
                    "color encoding is incompatible with the image component model",
                ));
            }
        }
        if let Self::IccProfile {
            bytes,
            component_model,
        } = self
        {
            validate_restricted_icc(bytes, *component_model)?;
        }
        Ok(())
    }
}

const MAX_ICC_PROFILE_BYTES: usize = 16 * 1024 * 1024;

pub(crate) fn validate_restricted_icc(
    bytes: &[u8],
    model: IccComponentModel,
) -> crate::error::Result<()> {
    if !(128..=MAX_ICC_PROFILE_BYTES).contains(&bytes.len()) {
        return Err(invalid_input(format!(
            "restricted ICC profile length {} is outside 128..={MAX_ICC_PROFILE_BYTES}",
            bytes.len()
        )));
    }
    let declared_len = u32::from_be_bytes(bytes[0..4].try_into().expect("four-byte ICC length"));
    if declared_len as usize != bytes.len() {
        return Err(invalid_input(
            "ICC header size does not match profile bytes",
        ));
    }
    if &bytes[36..40] != b"acsp" {
        return Err(invalid_input("ICC profile signature is not 'acsp'"));
    }
    if &bytes[20..24] != b"XYZ " {
        return Err(invalid_input(
            "restricted JP2 ICC profile PCS must be 'XYZ '",
        ));
    }
    let expected_space = match model {
        IccComponentModel::Gray => b"GRAY".as_slice(),
        IccComponentModel::Rgb => b"RGB ".as_slice(),
    };
    if &bytes[16..20] != expected_space {
        return Err(invalid_input(
            "ICC data color space does not match the declared component model",
        ));
    }
    Ok(())
}

impl ColorSpace {
    pub fn encoding_domain(self) -> Self {
        match self {
            Self::Gray => Self::Gray,
            Self::Cmyk => Self::Cmyk,
            Self::Rgb | Self::Srgb | Self::Yuv | Self::YCbCr => Self::Srgb,
        }
    }

    pub fn component_count(self) -> usize {
        match self {
            Self::Gray => 1,
            Self::Cmyk => 4,
            Self::Rgb | Self::Srgb | Self::Yuv | Self::YCbCr => 3,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Jp2,
    J2k,
}

/// Validated meaningful sample precision supported by the native encoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct SamplePrecision(u8);

impl SamplePrecision {
    pub const MIN: u8 = 8;
    pub const MAX: u8 = 16;

    pub fn new(bits: u32) -> crate::error::Result<Self> {
        let bits = u8::try_from(bits).map_err(|_| {
            invalid_input(format!(
                "component precision {bits} is outside the supported 8..=16 range"
            ))
        })?;
        if !(Self::MIN..=Self::MAX).contains(&bits) {
            return Err(invalid_input(format!(
                "component precision {bits} is outside the supported 8..=16 range"
            )));
        }
        Ok(Self(bits))
    }

    pub const fn bits(self) -> u32 {
        self.0 as u32
    }

    pub const fn bits_u8(self) -> u8 {
        self.0
    }

    pub const fn unsigned_level_shift(self) -> i32 {
        1i32 << (self.0 - 1)
    }

    pub const fn unsigned_max(self) -> i32 {
        (1i32 << self.0) - 1
    }
}

impl TryFrom<u32> for SamplePrecision {
    type Error = crate::error::Jp2LamError;

    fn try_from(bits: u32) -> Result<Self, Self::Error> {
        Self::new(bits)
    }
}

/// High-resolution tile selection policy.
///
/// `Single` preserves one-full-image-tile behavior. `Fixed` selects a regular
/// tile grid and `Auto` derives a bounded tile size from [`ResourceLimits`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TilePolicy {
    #[default]
    Single,
    Fixed {
        width: u32,
        height: u32,
    },
    Auto,
}

/// Resource limits that will guide bounded-memory high-resolution encoding.
///
/// These limits are intentionally optional so the current default behavior
/// remains unchanged while Phase 4 wires them into tile-size and concurrency
/// planning.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ResourceLimits {
    pub max_working_memory: Option<usize>,
    pub max_threads: Option<usize>,
    pub encoded_store_memory_limit: Option<usize>,
    pub spill_directory: Option<std::path::PathBuf>,
}

/// Named preset for convenience construction of [`EncodeOptions`].
///
/// Each preset maps to a quality value tuned for that scenario.
/// Use [`Preset::quality`] to get the underlying `u8` value, or pass a
/// `quality` directly in [`EncodeOptions`] for full 0–100 control.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preset {
    /// Scanned book pages destined for a PDF — compressed but fully readable.
    DocumentLow,
    /// Scanned book pages destined for a PDF — high fidelity, near-archival.
    DocumentHigh,
    /// Web-derived images (screenshots, web-rips) destined for a PDF — compact.
    WebLow,
    /// Web-derived images (screenshots, web-rips) destined for a PDF — crisp.
    WebHigh,
    /// Compact continuous-tone photographic output.
    PhotoCompact,
    /// High-fidelity continuous-tone photographic output.
    PhotoHigh,
    /// Near-lossless photographic output while retaining irreversible 9/7.
    PhotoNearLossless,
}

impl Preset {
    /// Quality value (0–100) associated with this preset.
    pub fn quality(self) -> u8 {
        match self {
            Self::DocumentLow => 30,
            Self::DocumentHigh => 85,
            Self::WebLow => 42,
            Self::WebHigh => 62,
            Self::PhotoCompact => 45,
            Self::PhotoHigh => 75,
            Self::PhotoNearLossless => 95,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Component {
    pub data: Vec<i32>,
    pub width: u32,
    pub height: u32,
    pub precision: u32,
    pub signed: bool,
    pub dx: u32,
    pub dy: u32,
}

#[derive(Debug, Clone)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub components: Vec<Component>,
    pub colorspace: ColorSpace,
}

/// Borrowed component sample storage for high-resolution encoding views.
///
/// The public zero-copy photographic path starts with `u8` and `u16` storage.
/// The `I32` variant lets the existing owned [`Image`] API adapt into the same
/// view model without cloning its current compatibility representation.
#[derive(Debug, Clone, Copy)]
pub enum ComponentSampleData<'a> {
    U8(&'a [u8]),
    U16(&'a [u16]),
    I32(&'a [i32]),
}

impl<'a> ComponentSampleData<'a> {
    fn len(self) -> usize {
        match self {
            Self::U8(samples) => samples.len(),
            Self::U16(samples) => samples.len(),
            Self::I32(samples) => samples.len(),
        }
    }

    fn sample_i32(self, index: usize) -> Option<i32> {
        match self {
            Self::U8(samples) => samples.get(index).map(|&sample| i32::from(sample)),
            Self::U16(samples) => samples.get(index).map(|&sample| i32::from(sample)),
            Self::I32(samples) => samples.get(index).copied(),
        }
    }

    fn storage_name(self) -> &'static str {
        match self {
            Self::U8(_) => "u8",
            Self::U16(_) => "u16",
            Self::I32(_) => "i32",
        }
    }
}

/// Borrowed view of one image component.
///
/// Strides and offsets are measured in samples of the backing storage, not in
/// bytes. This makes planar, interleaved, cropped, and padded inputs all use the
/// same representation without deinterleaving.
#[derive(Debug, Clone, Copy)]
pub struct ComponentView<'a> {
    pub samples: ComponentSampleData<'a>,
    pub offset: usize,
    pub row_stride: usize,
    pub sample_stride: usize,
    pub width: u32,
    pub height: u32,
    pub precision: u32,
    pub signed: bool,
    pub dx: u32,
    pub dy: u32,
}

impl<'a> ComponentView<'a> {
    pub fn new(
        samples: ComponentSampleData<'a>,
        offset: usize,
        row_stride: usize,
        sample_stride: usize,
        width: u32,
        height: u32,
        precision: u32,
        signed: bool,
        dx: u32,
        dy: u32,
    ) -> crate::error::Result<Self> {
        let view = Self {
            samples,
            offset,
            row_stride,
            sample_stride,
            width,
            height,
            precision,
            signed,
            dx,
            dy,
        };
        view.validate_bounds_and_metadata()?;
        Ok(view)
    }

    pub fn planar_u8(width: u32, height: u32, data: &'a [u8]) -> crate::error::Result<Self> {
        Self::new(
            ComponentSampleData::U8(data),
            0,
            width as usize,
            1,
            width,
            height,
            8,
            false,
            1,
            1,
        )
    }

    pub fn planar_u16(
        width: u32,
        height: u32,
        data: &'a [u16],
        precision: u32,
    ) -> crate::error::Result<Self> {
        Self::new(
            ComponentSampleData::U16(data),
            0,
            width as usize,
            1,
            width,
            height,
            precision,
            false,
            1,
            1,
        )
    }

    pub fn from_component(component: &'a Component) -> crate::error::Result<Self> {
        Self::new(
            ComponentSampleData::I32(&component.data),
            0,
            component.width as usize,
            1,
            component.width,
            component.height,
            component.precision,
            component.signed,
            component.dx,
            component.dy,
        )
    }

    pub fn sample_at(&self, x: u32, y: u32) -> Option<i32> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let index = self
            .offset
            .checked_add((y as usize).checked_mul(self.row_stride)?)?
            .checked_add((x as usize).checked_mul(self.sample_stride)?)?;
        self.samples.sample_i32(index)
    }

    fn validate_bounds_and_metadata(&self) -> crate::error::Result<()> {
        validate_nonzero_dimensions(self.width, self.height)?;
        validate_precision(self.precision)?;
        if self.dx == 0 || self.dy == 0 {
            return Err(invalid_input(
                "component sampling factors dx/dy must be non-zero",
            ));
        }
        if self.row_stride == 0 || self.sample_stride == 0 {
            return Err(invalid_input(
                "component row/sample strides must be non-zero",
            ));
        }
        if matches!(self.samples, ComponentSampleData::U8(_)) && self.precision > 8 {
            return Err(invalid_input(format!(
                "u8 component storage cannot represent {} meaningful bits",
                self.precision
            )));
        }

        let last_index = self
            .offset
            .checked_add(
                ((self.height - 1) as usize)
                    .checked_mul(self.row_stride)
                    .ok_or_else(|| invalid_input("component row stride overflows usize"))?,
            )
            .and_then(|base| {
                base.checked_add(((self.width - 1) as usize).checked_mul(self.sample_stride)?)
            })
            .ok_or_else(|| invalid_input("component view bounds overflow usize"))?;

        if last_index >= self.samples.len() {
            return Err(invalid_input(format!(
                "{} component view last index {} exceeds backing length {}",
                self.samples.storage_name(),
                last_index,
                self.samples.len()
            )));
        }
        Ok(())
    }
}

/// Borrowed image view accepted by the high-resolution encoding pipeline.
#[derive(Debug, Clone)]
pub struct ImageView<'a> {
    pub width: u32,
    pub height: u32,
    pub components: Vec<ComponentView<'a>>,
    pub colorspace: ColorSpace,
}

impl<'a> ImageView<'a> {
    pub fn from_gray8(width: u32, height: u32, data: &'a [u8]) -> crate::error::Result<Self> {
        validate_exact_len("gray8", data.len(), width, height, 1)?;
        Self::from_planar_components(
            width,
            height,
            ColorSpace::Gray,
            vec![ComponentView::planar_u8(width, height, data)?],
        )
    }

    pub fn from_gray16(
        width: u32,
        height: u32,
        data: &'a [u16],
        precision: u32,
    ) -> crate::error::Result<Self> {
        validate_exact_len("gray16", data.len(), width, height, 1)?;
        Self::from_planar_components(
            width,
            height,
            ColorSpace::Gray,
            vec![ComponentView::planar_u16(width, height, data, precision)?],
        )
    }

    pub fn from_rgb8_interleaved(
        width: u32,
        height: u32,
        data: &'a [u8],
    ) -> crate::error::Result<Self> {
        validate_exact_len("rgb8", data.len(), width, height, 3)?;
        let row_stride = (width as usize)
            .checked_mul(3)
            .ok_or_else(|| invalid_input("RGB row stride overflows usize"))?;
        let components = (0..3)
            .map(|offset| {
                ComponentView::new(
                    ComponentSampleData::U8(data),
                    offset,
                    row_stride,
                    3,
                    width,
                    height,
                    8,
                    false,
                    1,
                    1,
                )
            })
            .collect::<crate::error::Result<Vec<_>>>()?;
        Self::from_planar_components(width, height, ColorSpace::Srgb, components)
    }

    pub fn from_rgb16_interleaved(
        width: u32,
        height: u32,
        data: &'a [u16],
        precision: u32,
    ) -> crate::error::Result<Self> {
        validate_exact_len("rgb16", data.len(), width, height, 3)?;
        let row_stride = (width as usize)
            .checked_mul(3)
            .ok_or_else(|| invalid_input("RGB row stride overflows usize"))?;
        let components = (0..3)
            .map(|offset| {
                ComponentView::new(
                    ComponentSampleData::U16(data),
                    offset,
                    row_stride,
                    3,
                    width,
                    height,
                    precision,
                    false,
                    1,
                    1,
                )
            })
            .collect::<crate::error::Result<Vec<_>>>()?;
        Self::from_planar_components(width, height, ColorSpace::Srgb, components)
    }

    pub fn from_planar_components(
        width: u32,
        height: u32,
        colorspace: ColorSpace,
        components: Vec<ComponentView<'a>>,
    ) -> crate::error::Result<Self> {
        validate_nonzero_dimensions(width, height)?;
        let expected_components = colorspace.component_count();
        if components.len() != expected_components {
            return Err(invalid_input(format!(
                "{colorspace:?} image view expects {expected_components} components, got {}",
                components.len()
            )));
        }
        for (index, component) in components.iter().enumerate() {
            if component.width != width || component.height != height {
                return Err(invalid_input(format!(
                    "component {index} dimensions {}×{} do not match image view {}×{}",
                    component.width, component.height, width, height
                )));
            }
        }
        Ok(Self {
            width,
            height,
            components,
            colorspace,
        })
    }

    pub fn from_image(image: &'a Image) -> crate::error::Result<Self> {
        let components = image
            .components
            .iter()
            .map(ComponentView::from_component)
            .collect::<crate::error::Result<Vec<_>>>()?;
        Self::from_planar_components(image.width, image.height, image.colorspace, components)
    }
}

impl Image {
    /// Construct from interleaved sRGB bytes (3 bytes per pixel, R-G-B order).
    ///
    /// `data.len()` must equal `width * height * 3`.
    pub fn from_rgb_bytes(width: u32, height: u32, data: &[u8]) -> crate::error::Result<Self> {
        validate_exact_len("RGB", data.len(), width, height, 3)?;
        let pixel_count = data.len() / 3;
        let mut r = Vec::with_capacity(pixel_count);
        let mut g = Vec::with_capacity(pixel_count);
        let mut b = Vec::with_capacity(pixel_count);
        for px in data.chunks_exact(3) {
            r.push(i32::from(px[0]));
            g.push(i32::from(px[1]));
            b.push(i32::from(px[2]));
        }
        Ok(Self {
            width,
            height,
            components: vec![
                make_component(r, width, height),
                make_component(g, width, height),
                make_component(b, width, height),
            ],
            colorspace: ColorSpace::Srgb,
        })
    }

    /// Construct from grayscale bytes (1 byte per pixel).
    ///
    /// `data.len()` must equal `width * height`.
    pub fn from_gray_bytes(width: u32, height: u32, data: &[u8]) -> crate::error::Result<Self> {
        validate_exact_len("Gray", data.len(), width, height, 1)?;
        let samples: Vec<i32> = data.iter().map(|&v| i32::from(v)).collect();
        Ok(Self {
            width,
            height,
            components: vec![make_component(samples, width, height)],
            colorspace: ColorSpace::Gray,
        })
    }

    pub fn as_view(&self) -> crate::error::Result<ImageView<'_>> {
        ImageView::from_image(self)
    }
}

fn make_component(data: Vec<i32>, width: u32, height: u32) -> Component {
    Component {
        data,
        width,
        height,
        precision: 8,
        signed: false,
        dx: 1,
        dy: 1,
    }
}

fn validate_nonzero_dimensions(width: u32, height: u32) -> crate::error::Result<()> {
    if width == 0 || height == 0 {
        return Err(invalid_input(format!(
            "image dimensions must be non-zero, got {width}×{height}"
        )));
    }
    Ok(())
}

fn validate_precision(precision: u32) -> crate::error::Result<()> {
    SamplePrecision::new(precision).map(|_| ())
}

fn validate_exact_len(
    label: &str,
    actual: usize,
    width: u32,
    height: u32,
    samples_per_pixel: usize,
) -> crate::error::Result<()> {
    validate_nonzero_dimensions(width, height)?;
    let expected = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(samples_per_pixel))
        .ok_or_else(|| invalid_input(format!("{label} buffer length overflows usize")))?;
    if actual != expected {
        return Err(invalid_input(format!(
            "{label} buffer length {actual} does not match {width}×{height}×{samples_per_pixel}={expected}"
        )));
    }
    Ok(())
}

fn invalid_input(message: impl Into<String>) -> crate::error::Jp2LamError {
    crate::error::Jp2LamError::InvalidInput(message.into())
}

/// Content profile selecting the lossy rate-control strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ContentProfile {
    /// General-purpose behavior (photo-safe): quality-scaled quantization steps
    /// plus lambda-based PCRD pass selection with the heuristic distortion model.
    #[default]
    General,
    /// Continuous-tone photographic content. This is currently the explicit
    /// spelling of the photo-safe `General` behavior; `General` remains as a
    /// compatibility alias during API migration.
    Photo,
    /// Scanned document pages (text/line art on a flat background).
    /// Rate control is quantization-driven (much coarser steps per quality)
    /// with a light measured-ΔMSE PCRD trim. On cleaned text pages this gives
    /// ~35% smaller output than `General` at equal PSNR. Not suitable for
    /// continuous-tone (photo) content.
    Document,
}

/// Caller-visible compression intent.
///
/// Exact-rate variants target the complete selected output: the raw codestream
/// for [`OutputFormat::J2k`], or the complete JP2 file for
/// [`OutputFormat::Jp2`]. `TargetBitsPerPixel` is the total compressed output
/// bits divided by full-image pixel count, across all components.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RateControl {
    /// Reversible 5/3 coding with no truncation target.
    Lossless,
    /// Calibrated photographic quality in the inclusive range 0..=99.
    Quality(u8),
    /// Target complete output size in bytes.
    TargetBytes(u64),
    /// Target complete output bits per full-image pixel, across all components.
    TargetBitsPerPixel(f32),
    /// Meaningful source bits divided by complete compressed output bits.
    /// Source bits use declared component precision and sampling geometry, not
    /// the Rust storage width.
    CompressionRatio(f32),
}

#[derive(Debug, Clone)]
pub struct EncodeOptions {
    /// Quality 0–100. 100 = lossless (reversible 5/3 wavelet, no rate cap).
    /// Values below 100 use the irreversible 9/7 wavelet with lossy compression.
    pub quality: u8,
    /// Authoritative rate-control intent. When absent, `quality` retains its
    /// compatibility meaning (`100` lossless, otherwise `Quality(quality)`).
    pub rate_control: Option<RateControl>,
    pub format: OutputFormat,
    /// Lossy rate-control strategy. Ignored for lossless (quality 100).
    pub profile: ContentProfile,
    /// Tile policy for bounded-memory high-resolution encoding.
    pub tile_policy: TilePolicy,
    /// Optional resource limits for future bounded-memory planning.
    pub resource_limits: ResourceLimits,
    /// Optional explicit JP2 color description. Required for ambiguous legacy
    /// `ColorSpace::Rgb`; ordinary Gray/Srgb constructors infer their matching
    /// enumerated description when this is `None`.
    pub color_encoding: Option<ColorEncoding>,
}

impl EncodeOptions {
    /// Convenience constructor from a named preset.
    pub fn from_preset(preset: Preset, format: OutputFormat) -> Self {
        let is_photo = matches!(
            preset,
            Preset::PhotoCompact | Preset::PhotoHigh | Preset::PhotoNearLossless
        );
        Self {
            quality: preset.quality(),
            rate_control: is_photo.then(|| RateControl::Quality(preset.quality())),
            format,
            profile: if is_photo {
                ContentProfile::Photo
            } else {
                ContentProfile::General
            },
            tile_policy: if is_photo {
                TilePolicy::Auto
            } else {
                TilePolicy::Single
            },
            resource_limits: ResourceLimits::default(),
            color_encoding: None,
        }
    }

    /// Options for scanned document pages (see [`ContentProfile::Document`]).
    pub fn document(quality: u8, format: OutputFormat) -> Self {
        Self {
            quality,
            rate_control: None,
            format,
            profile: ContentProfile::Document,
            tile_policy: TilePolicy::Single,
            resource_limits: ResourceLimits::default(),
            color_encoding: None,
        }
    }

    /// Options for continuous-tone photographs using the explicit photo-safe
    /// quality/PCRD path.
    pub fn photo(quality: u8, format: OutputFormat) -> Self {
        Self {
            quality,
            rate_control: Some(RateControl::Quality(quality.min(99))),
            format,
            profile: ContentProfile::Photo,
            tile_policy: TilePolicy::Auto,
            resource_limits: ResourceLimits::default(),
            color_encoding: None,
        }
    }
}

impl Default for EncodeOptions {
    fn default() -> Self {
        Self {
            quality: Preset::DocumentHigh.quality(),
            rate_control: None,
            format: OutputFormat::Jp2,
            profile: ContentProfile::General,
            tile_policy: TilePolicy::Single,
            resource_limits: ResourceLimits::default(),
            color_encoding: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ColorSpace, ComponentView, ContentProfile, EncodeOptions, Image, ImageView, OutputFormat,
        Preset, RateControl, SamplePrecision, TilePolicy,
    };

    #[test]
    fn photographic_presets_and_constructor_use_explicit_photo_profile() {
        assert_eq!(Preset::PhotoCompact.quality(), 45);
        assert_eq!(Preset::PhotoHigh.quality(), 75);
        assert_eq!(Preset::PhotoNearLossless.quality(), 95);
        let options = EncodeOptions::photo(75, OutputFormat::Jp2);
        assert_eq!(options.profile, ContentProfile::Photo);
        assert_eq!(options.rate_control, Some(RateControl::Quality(75)));
        assert_eq!(options.tile_policy, TilePolicy::Auto);
        let preset = EncodeOptions::from_preset(Preset::PhotoHigh, OutputFormat::Jp2);
        assert_eq!(preset.profile, ContentProfile::Photo);
        assert_eq!(preset.rate_control, Some(RateControl::Quality(75)));
    }

    #[test]
    fn sample_precision_derives_checked_limits() {
        let precision = SamplePrecision::new(16).expect("16-bit precision");
        assert_eq!(precision.bits(), 16);
        assert_eq!(precision.unsigned_level_shift(), 32_768);
        assert_eq!(precision.unsigned_max(), 65_535);
        assert!(SamplePrecision::new(7).is_err());
        assert!(SamplePrecision::new(17).is_err());
    }

    #[test]
    fn rgb8_interleaved_view_uses_shared_storage_and_strides() {
        let data = [
            10u8, 20, 30, //
            40, 50, 60, //
            70, 80, 90, //
            100, 110, 120,
        ];
        let view = ImageView::from_rgb8_interleaved(2, 2, &data).expect("rgb view");

        assert_eq!(view.width, 2);
        assert_eq!(view.height, 2);
        assert_eq!(view.colorspace, ColorSpace::Srgb);
        assert_eq!(view.components.len(), 3);
        assert_eq!(view.components[0].offset, 0);
        assert_eq!(view.components[1].offset, 1);
        assert_eq!(view.components[2].offset, 2);
        assert_eq!(view.components[0].row_stride, 6);
        assert_eq!(view.components[0].sample_stride, 3);
        assert_eq!(view.components[0].sample_at(1, 1), Some(100));
        assert_eq!(view.components[1].sample_at(1, 1), Some(110));
        assert_eq!(view.components[2].sample_at(1, 1), Some(120));
    }

    #[test]
    fn gray16_view_preserves_meaningful_precision() {
        let data = [0u16, 1023, 17, 33];
        let view = ImageView::from_gray16(2, 2, &data, 10).expect("gray16 view");

        assert_eq!(view.components[0].precision, 10);
        assert_eq!(view.components[0].sample_at(1, 0), Some(1023));
    }

    #[test]
    fn rgb16_interleaved_view_preserves_precision_and_strides() {
        let data = [
            0u16, 1023, 17, //
            33, 512, 999,
        ];
        let view = ImageView::from_rgb16_interleaved(2, 1, &data, 10).expect("rgb16 view");

        assert_eq!(view.colorspace, ColorSpace::Srgb);
        assert_eq!(view.components[0].precision, 10);
        assert_eq!(view.components[0].row_stride, 6);
        assert_eq!(view.components[0].sample_stride, 3);
        assert_eq!(view.components[0].sample_at(1, 0), Some(33));
        assert_eq!(view.components[1].sample_at(1, 0), Some(512));
        assert_eq!(view.components[2].sample_at(1, 0), Some(999));
    }

    #[test]
    fn image_view_rejects_invalid_precision_and_short_buffers() {
        let data = [0u16; 4];
        let err = ImageView::from_gray16(2, 2, &data, 17).expect_err("precision should fail");
        assert!(err.to_string().contains("8..=16"), "{err}");

        let short = [0u8; 5];
        let err = ImageView::from_rgb8_interleaved(2, 1, &short).expect_err("length should fail");
        assert!(err.to_string().contains("buffer length"), "{err}");
    }

    #[test]
    fn component_view_rejects_out_of_bounds_strides() {
        let data = [0u8; 8];
        let err = ComponentView::new(
            super::ComponentSampleData::U8(&data),
            2,
            10,
            1,
            2,
            2,
            8,
            false,
            1,
            1,
        )
        .expect_err("view should exceed backing storage");

        assert!(err.to_string().contains("exceeds backing length"), "{err}");
    }

    #[test]
    fn owned_image_adapts_to_view_without_component_clone() {
        let image = Image::from_gray_bytes(2, 2, &[1, 2, 3, 4]).expect("owned image");
        let view = image.as_view().expect("image view");

        assert_eq!(view.colorspace, ColorSpace::Gray);
        assert_eq!(view.components[0].sample_at(0, 1), Some(3));
        assert_eq!(view.components[0].sample_at(1, 1), Some(4));
    }

    #[test]
    fn owned_image_constructors_reject_zero_and_overflowing_dimensions() {
        for result in [
            Image::from_gray_bytes(0, 1, &[]),
            Image::from_rgb_bytes(1, 0, &[]),
            Image::from_gray_bytes(u32::MAX, u32::MAX, &[]),
            Image::from_rgb_bytes(u32::MAX, u32::MAX, &[]),
        ] {
            assert!(result.is_err());
        }
    }
}
