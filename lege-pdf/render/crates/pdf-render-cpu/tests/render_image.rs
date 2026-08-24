#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panics are the assertion mechanism

//! Phase 6C: the CPU backend rasterizes images — RGB/Gray/Indexed sampling,
//! `/ImageMask` stencils, and `/SMask` soft masks.

use std::sync::Arc;

use pdf_page_ir::{
    Color, CompiledPage, DeviceSize, DisplayOp, IccCmykTransform, ImageColorSpace, ImageIr,
    ImageSMask, InterpolationMode, Matrix, PageBounds, PageComplexity, PageFeatures, Paint, Rect,
    ResourceKey,
};
use pdf_render_api::{
    AnnotationMode, Background, HostPage, OutputFormat, OutputResidency, PageTransform,
    RenderLimits, RenderQuality, RenderRequest,
};
use pdf_render_cpu::CpuBackend;

#[allow(clippy::too_many_arguments)]
fn image_ir(
    width: u32,
    height: u32,
    bpc: u8,
    cs: ImageColorSpace,
    samples: Vec<u8>,
    is_stencil: bool,
    smask: Option<ImageSMask>,
) -> ImageIr {
    ImageIr {
        key: ResourceKey {
            object_number: 0,
            generation: 0,
            variant: 0,
        },
        width,
        height,
        is_stencil,
        interpolation: InterpolationMode::Nearest,
        soft_mask: None,
        bits_per_component: bpc,
        color_space: cs,
        decode: None,
        samples: Some(Arc::from(samples)),
        codec: None,
        codec_data: None,
        codec_parms: None,
        smask: smask.map(Arc::new),
        mask: None,
        smask_in_data: 0,
        lowering_degraded: false,
    }
}

fn page(image: ImageIr, stencil_color: Color, scale: f64) -> CompiledPage {
    CompiledPage {
        schema_version: pdf_page_ir::IR_SCHEMA_VERSION,
        content_bounds: None,
        bounds: PageBounds {
            crop: Rect {
                x0: 0.0,
                y0: 0.0,
                x1: 8.0,
                y1: 8.0,
            },
            rotate: 0,
        },
        operations: Arc::from([
            DisplayOp::ConcatTransform(Matrix::scale(scale, scale)),
            DisplayOp::DrawImage {
                image: pdf_page_ir::ImageId(0),
                paint: pdf_page_ir::PaintId(0),
                transform: Matrix::IDENTITY,
                alpha: 1.0,
                blend: pdf_page_ir::BlendMode::Normal,
            },
        ]),
        paths: Arc::from([]),
        paints: Arc::from([Paint::Solid(stencil_color)]),
        stroke_styles: Arc::from([]),
        glyph_runs: Arc::from([]),
        fonts: Arc::from([]),
        images: Arc::from([image]),
        masks: Arc::from([]),
        groups: Arc::from([]),
        shadings: Arc::from([]),
        tilings: Arc::from([]),
        features: PageFeatures::IMAGES,
        complexity: PageComplexity::default(),
    }
}

fn request(page: CompiledPage, dim: u32) -> RenderRequest {
    RenderRequest {
        page: Arc::new(page),
        transform: PageTransform {
            matrix: Matrix::IDENTITY,
        },
        crop: None,
        output_size: DeviceSize {
            width: dim,
            height: dim,
        },
        output_format: OutputFormat::Rgba8PremultipliedSrgb,
        background: Background::White,
        annotations: AnnotationMode::None,
        color_policy: pdf_render_api::RenderColorPolicy::Original,
        quality: RenderQuality::Normal,
        limits: RenderLimits::default(),
        residency: OutputResidency::HostRequired,
    }
}

fn px(host: &HostPage, x: usize, y: usize) -> [u8; 4] {
    let i = y * host.stride + x * 4;
    [
        host.pixels[i],
        host.pixels[i + 1],
        host.pixels[i + 2],
        host.pixels[i + 3],
    ]
}

#[test]
fn rgb_image_samples_left_red_right_blue() {
    // 2x1 RGB: left red, right blue. Scaled to an 8x8 device region.
    let img = image_ir(
        2,
        1,
        8,
        ImageColorSpace::Rgb,
        vec![255, 0, 0, 0, 0, 255],
        false,
        None,
    );
    let backend = CpuBackend::default();
    let (host, _) = backend
        .render_to_host(&request(page(img, Color::BLACK, 8.0), 8))
        .unwrap();
    assert_eq!(px(&host, 1, 4), [255, 0, 0, 255], "left half red");
    assert_eq!(px(&host, 6, 4), [0, 0, 255, 255], "right half blue");
}

#[test]
fn image_mask_paints_fill_color_where_sample_is_zero() {
    // 2x1 1-bit stencil: pixel0 = 0 (paint), pixel1 = 1 (masked).
    // Row byte: bit7=pixel0=0, bit6=pixel1=1 → 0b0100_0000 = 0x40.
    let img = image_ir(2, 1, 1, ImageColorSpace::Gray, vec![0x40], true, None);
    let backend = CpuBackend::default();
    let (host, _) = backend
        .render_to_host(&request(page(img, Color::from_rgb(0.0, 1.0, 0.0), 8.0), 8))
        .unwrap();
    assert_eq!(
        px(&host, 1, 4),
        [0, 255, 0, 255],
        "sample 0 painted with fill (green)"
    );
    assert_eq!(
        px(&host, 6, 4),
        [255, 255, 255, 255],
        "sample 1 left as background"
    );
}

#[test]
fn indexed_image_uses_palette() {
    // 2x1 Indexed, base RGB, palette[0]=red, palette[1]=blue; samples [0,1].
    let cs = ImageColorSpace::Indexed {
        base: Box::new(ImageColorSpace::Rgb),
        hival: 1,
        lookup: Arc::from(vec![255u8, 0, 0, 0, 0, 255]),
    };
    // 8-bit indices, one byte each (padded row = 2 bytes).
    let img = image_ir(2, 1, 8, cs, vec![0, 1], false, None);
    let backend = CpuBackend::default();
    let (host, _) = backend
        .render_to_host(&request(page(img, Color::BLACK, 8.0), 8))
        .unwrap();
    assert_eq!(px(&host, 1, 4), [255, 0, 0, 255], "index 0 → red");
    assert_eq!(px(&host, 6, 4), [0, 0, 255, 255], "index 1 → blue");
}

#[test]
fn undecodable_codec_image_is_a_recorded_silent_blank() {
    // A JPX image whose codec_data is not a valid JP2: the decode fails and the
    // draw is dropped, leaving the page blank. Workstream B3 requires that this
    // be *observable* — counted and reasoned — so a page PDFium would paint but
    // we blanked can never be scored as a clean render (DEFERRED.md item 1).
    let img = ImageIr {
        key: ResourceKey {
            object_number: 1,
            generation: 0,
            variant: 0,
        },
        width: 4,
        height: 4,
        is_stencil: false,
        interpolation: InterpolationMode::Nearest,
        soft_mask: None,
        bits_per_component: 8,
        color_space: ImageColorSpace::Rgb,
        decode: None,
        samples: None,
        codec: Some(pdf_page_ir::ImageCodecKind::Jpx),
        codec_data: Some(vec![0u8; 16].into()), // not a JP2 container
        codec_parms: None,
        smask: None,
        mask: None,
        smask_in_data: 0,
        lowering_degraded: false,
    };
    let backend = CpuBackend::default();
    let (host, stats) = backend
        .render_to_host(&request(page(img, Color::BLACK, 8.0), 8))
        .unwrap();

    assert_eq!(
        stats.degraded_draws, 1,
        "the dropped JPX draw must be counted"
    );
    assert!(
        stats.is_silent_blank(),
        "a blank page from a dropped image is a silent blank"
    );
    assert!(
        !stats.recovery_notes.is_empty(),
        "a human-readable reason is recorded"
    );
    assert_eq!(
        px(&host, 4, 4),
        [255, 255, 255, 255],
        "surface really is blank"
    );
}

#[test]
fn decodable_content_is_not_flagged_degraded() {
    // Sanity: a normal image render records zero degradations and is not a
    // silent blank (it painted real pixels).
    let img = image_ir(
        2,
        1,
        8,
        ImageColorSpace::Rgb,
        vec![255, 0, 0, 0, 0, 255],
        false,
        None,
    );
    let backend = CpuBackend::default();
    let (_host, stats) = backend
        .render_to_host(&request(page(img, Color::BLACK, 8.0), 8))
        .unwrap();
    assert_eq!(stats.degraded_draws, 0);
    assert!(!stats.is_silent_blank());
}

#[test]
fn indexed_decode_array_remaps_sample_to_index() {
    // A 1-bit Indexed image with `/Decode [0 255]`: sample 1 must select palette
    // index **255**, not index 1 (which is how a bilevel scan with a
    // white/black palette avoids inverting). palette[0]=white, palette[1]=red
    // (the wrong answer if /Decode is ignored), palette[255]=black.
    let mut lookup = vec![0u8; 256 * 3]; // index 255 stays black
    lookup[0..3].copy_from_slice(&[255, 255, 255]); // index 0 = white
    lookup[3..6].copy_from_slice(&[255, 0, 0]); // index 1 = red (the pre-fix result)
    let cs = ImageColorSpace::Indexed {
        base: Box::new(ImageColorSpace::Rgb),
        hival: 255,
        lookup: Arc::from(lookup),
    };
    // 2×1 at 1 bpc: bit7=sample0=0, bit6=sample1=1 → 0b0100_0000 = 0x40.
    let img = ImageIr {
        key: ResourceKey {
            object_number: 2,
            generation: 0,
            variant: 0,
        },
        width: 2,
        height: 1,
        is_stencil: false,
        interpolation: InterpolationMode::Nearest,
        soft_mask: None,
        bits_per_component: 1,
        color_space: cs,
        decode: Some(Arc::from(vec![[0.0f32, 255.0]])),
        samples: Some(Arc::from(vec![0x40u8])),
        codec: None,
        codec_data: None,
        codec_parms: None,
        smask: None,
        mask: None,
        smask_in_data: 0,
        lowering_degraded: false,
    };
    let backend = CpuBackend::default();
    let (host, _) = backend
        .render_to_host(&request(page(img, Color::BLACK, 8.0), 8))
        .unwrap();
    assert_eq!(
        px(&host, 1, 4),
        [255, 255, 255, 255],
        "sample 0 → index 0 (white)"
    );
    assert_eq!(
        px(&host, 6, 4),
        [0, 0, 0, 255],
        "sample 1 → index 255 via /Decode (black), not index 1 (red)"
    );
}

#[test]
fn tint_lut_separation_image_routes_samples_through_lut() {
    // A `/Separation` image: the single sample per pixel is a *tint* routed
    // through the baked 256-entry sample→sRGB LUT, NOT read as DeviceGray.
    // Guards the fix for near-white scans inverting to near-black. LUT maps
    // sample 0 → (10,20,30) and sample 255 → (200,210,220); samples [0, 255].
    let mut lut = vec![0u8; 256 * 3];
    lut[0..3].copy_from_slice(&[10, 20, 30]);
    lut[255 * 3..255 * 3 + 3].copy_from_slice(&[200, 210, 220]);
    let cs = pdf_page_ir::ImageColorSpace::TintLut {
        rgb: Arc::from(lut),
    };
    let img = image_ir(2, 1, 8, cs, vec![0, 255], false, None);
    let backend = CpuBackend::default();
    let (host, _) = backend
        .render_to_host(&request(page(img, Color::BLACK, 8.0), 8))
        .unwrap();
    assert_eq!(
        px(&host, 1, 4),
        [10, 20, 30, 255],
        "sample 0 → LUT[0], not DeviceGray black"
    );
    assert_eq!(
        px(&host, 6, 4),
        [200, 210, 220, 255],
        "sample 255 → LUT[255]"
    );
}

#[test]
fn zero_dimension_image_paints_nothing() {
    // Malformed /Width 0 with samples present and a normal placement CTM:
    // must be skipped (no pixels to sample), never panic.
    let img = image_ir(0, 1, 8, ImageColorSpace::Rgb, vec![255, 0, 0], false, None);
    let backend = CpuBackend::default();
    let (host, _) = backend
        .render_to_host(&request(page(img, Color::BLACK, 8.0), 8))
        .unwrap();
    assert_eq!(
        px(&host, 4, 4),
        [255, 255, 255, 255],
        "background untouched"
    );
}

#[test]
fn soft_mask_scales_alpha() {
    // 1x1 white RGB image with a 1x1 SMask alpha of 0 (fully transparent) →
    // background shows through.
    let smask = ImageSMask {
        width: 1,
        height: 1,
        bits_per_component: 8,
        decode: None,
        samples: Arc::from(vec![0u8]),
        codec: None,
        codec_data: None,
        codec_parms: None,
    };
    let img = image_ir(
        1,
        1,
        8,
        ImageColorSpace::Rgb,
        vec![255, 0, 0],
        false,
        Some(smask),
    );
    let backend = CpuBackend::default();
    let (host, _) = backend
        .render_to_host(&request(page(img, Color::BLACK, 8.0), 8))
        .unwrap();
    assert_eq!(
        px(&host, 4, 4),
        [255, 255, 255, 255],
        "smask alpha 0 → fully transparent"
    );
}

/// A 1-bit checkerboard: every other source texel is ink. Point-sampling it
/// down keeps whichever texels the grid happens to land on; averaging keeps
/// the *density*, which is what makes a downscaled scan readable.
fn checkerboard(w: u32, h: u32) -> Vec<u8> {
    let stride = (w as usize).div_ceil(8);
    let mut data = vec![0u8; stride * h as usize];
    for y in 0..h as usize {
        for x in 0..w as usize {
            if (x + y) % 2 == 0 {
                data[y * stride + x / 8] |= 0x80 >> (x % 8);
            }
        }
    }
    data
}

#[test]
fn minified_images_are_area_averaged_not_point_sampled() {
    // A 64x64 checkerboard drawn into 8x8 device pixels: each pixel covers an
    // 8x8 source block that is exactly half ink, so every pixel must land near
    // mid-grey. Point sampling would give hard black or white.
    let img = image_ir(
        64,
        64,
        1,
        ImageColorSpace::Gray,
        checkerboard(64, 64),
        false,
        None,
    );
    let backend = CpuBackend::default();
    let (host, _) = backend
        .render_to_host(&request(page(img, Color::BLACK, 8.0), 8))
        .unwrap();
    for (x, y) in [(2usize, 2usize), (4, 4), (5, 3)] {
        let p = px(&host, x, y);
        assert!(
            (100..=160).contains(&(p[0] as i32)),
            "pixel ({x},{y}) should average to mid-grey, got {p:?}"
        );
    }
}

#[test]
fn magnified_images_still_honour_the_interpolation_flag() {
    // Blowing 2x1 up to 8x8 is magnification: the footprint is under a texel,
    // so the area filter must stay out of the way and Nearest stay crisp.
    let img = image_ir(
        2,
        1,
        8,
        ImageColorSpace::Rgb,
        vec![255, 0, 0, 0, 0, 255],
        false,
        None,
    );
    let backend = CpuBackend::default();
    let (host, _) = backend
        .render_to_host(&request(page(img, Color::BLACK, 8.0), 8))
        .unwrap();
    assert_eq!(px(&host, 1, 4), [255, 0, 0, 255], "left stays pure red");
    assert_eq!(px(&host, 6, 4), [0, 0, 255, 255], "right stays pure blue");
}

#[test]
fn bilinear_image_sampling_truncates_like_reference_stretchers() {
    let mut img = image_ir(
        2,
        1,
        8,
        ImageColorSpace::Rgb,
        vec![255, 0, 0, 0, 0, 255],
        false,
        None,
    );
    img.interpolation = InterpolationMode::Bilinear;
    let backend = CpuBackend::default();
    let (host, _) = backend
        .render_to_host(&request(page(img, Color::BLACK, 8.0), 8))
        .unwrap();
    // At x=2 the source coordinate is 0.125: [223.125, 0, 31.875].
    // Fixed-point stretchers truncate those fractions rather than rounding.
    assert_eq!(px(&host, 2, 4), [223, 0, 31, 255]);
}

#[test]
fn minified_stencils_get_soft_edges_from_coverage() {
    // An /ImageMask minified 8:1 must carry partial coverage into alpha —
    // that is what stops a downscaled bilevel mask looking ragged.
    let img = image_ir(
        64,
        64,
        1,
        ImageColorSpace::Gray,
        checkerboard(64, 64),
        true,
        None,
    );
    let backend = CpuBackend::default();
    let (host, _) = backend
        .render_to_host(&request(page(img, Color::from_rgb(0.0, 0.0, 0.0), 8.0), 8))
        .unwrap();
    let p = px(&host, 4, 4);
    // Half coverage of black over white → mid-grey, not solid either way.
    assert!(
        (90..=170).contains(&(p[0] as i32)),
        "stencil coverage averaged: {p:?}"
    );
}

#[test]
fn sub_byte_and_byte_aligned_samples_agree() {
    // The sampler fast-paths byte-aligned reads. 4bpc goes through the
    // bit walker, 8bpc through the fast path; the same grey must come out of
    // both, or the optimisation is not one.
    // 4bpc gray, two texels: 0x0 and 0xF -> black, white.
    let four = image_ir(2, 1, 4, ImageColorSpace::Gray, vec![0x0F], false, None);
    // 8bpc gray, same two texels.
    let eight = image_ir(
        2,
        1,
        8,
        ImageColorSpace::Gray,
        vec![0x00, 0xFF],
        false,
        None,
    );
    let backend = CpuBackend::default();
    let a = backend
        .render_to_host(&request(page(four, Color::BLACK, 8.0), 8))
        .unwrap()
        .0;
    let b = backend
        .render_to_host(&request(page(eight, Color::BLACK, 8.0), 8))
        .unwrap()
        .0;
    assert_eq!(px(&a, 1, 4), px(&b, 1, 4), "4bpc and 8bpc agree on black");
    assert_eq!(px(&a, 6, 4), px(&b, 6, 4), "4bpc and 8bpc agree on white");
    assert_eq!(px(&a, 6, 4), [255, 255, 255, 255]);
}

#[test]
fn sixteen_bit_samples_read_big_endian() {
    // 16bpc gray: 0x0000 then 0xFFFF. The fast path must keep MSB-first order.
    let img = image_ir(
        2,
        1,
        16,
        ImageColorSpace::Gray,
        vec![0x00, 0x00, 0xFF, 0xFF],
        false,
        None,
    );
    let backend = CpuBackend::default();
    let (host, _) = backend
        .render_to_host(&request(page(img, Color::BLACK, 8.0), 8))
        .unwrap();
    assert_eq!(px(&host, 1, 4), [0, 0, 0, 255], "0x0000 -> black");
    assert_eq!(px(&host, 6, 4), [255, 255, 255, 255], "0xFFFF -> white");
}

/// A stub codec that "decodes" any payload to a solid-white raster in a chosen
/// format, so a test can exercise the backend's handling of a *successful*
/// codec decode without hand-rolling real JPX/JPEG bytes. Registered under the
/// JPX filter (the format the regression fixture uses).
#[derive(Debug)]
struct StubJpxCodec {
    format: pdf_image::DecodedFormat,
    comps: usize,
    data: Option<Arc<[u8]>>,
}

impl pdf_image::ImageCodec for StubJpxCodec {
    fn filter(&self) -> pdf_image::StreamFilter {
        pdf_image::StreamFilter::Jpx
    }
    fn decode(
        &self,
        _data: &[u8],
        descriptor: &pdf_image::ImageDescriptor,
        _params: &pdf_image::DecodeParameters,
        _limits: &pdf_image::DecodeLimits,
    ) -> Result<pdf_image::DecodedImage, pdf_image::ImageError> {
        let (w, h) = (descriptor.width, descriptor.height);
        let stride = w as usize * self.comps;
        Ok(pdf_image::DecodedImage {
            width: w,
            height: h,
            format: self.format,
            stride,
            data: self
                .data
                .clone()
                .unwrap_or_else(|| Arc::from(vec![255u8; stride * h as usize])),
        })
    }
}

fn backend_with_stub_jpx(format: pdf_image::DecodedFormat, comps: usize) -> CpuBackend {
    backend_with_stub_jpx_data(format, comps, None)
}

fn backend_with_stub_jpx_data(
    format: pdf_image::DecodedFormat,
    comps: usize,
    data: Option<Arc<[u8]>>,
) -> CpuBackend {
    let codecs = pdf_image::CodecRegistry::new([Arc::new(StubJpxCodec {
        format,
        comps,
        data,
    }) as Arc<dyn pdf_image::ImageCodec>]);
    CpuBackend::new(pdf_render_cpu::CpuBackendOptions {
        codecs,
        ..Default::default()
    })
}

fn jpx_image(cs: ImageColorSpace) -> ImageIr {
    ImageIr {
        key: ResourceKey {
            object_number: 1,
            generation: 0,
            variant: 0,
        },
        width: 4,
        height: 4,
        is_stencil: false,
        interpolation: InterpolationMode::Nearest,
        soft_mask: None,
        bits_per_component: 8,
        color_space: cs,
        decode: None,
        samples: None,
        codec: Some(pdf_page_ir::ImageCodecKind::Jpx),
        codec_data: Some(vec![0u8; 16].into()),
        codec_parms: None,
        smask: None,
        mask: None,
        smask_in_data: 0,
        lowering_degraded: false,
    }
}

/// A page that draws several images (all over the unit square), for coverage /
/// silent-blank accounting.
fn multi_image_page(images: Vec<ImageIr>) -> CompiledPage {
    let mut ops: Vec<DisplayOp> = vec![DisplayOp::ConcatTransform(Matrix::scale(8.0, 8.0))];
    for i in 0..images.len() {
        ops.push(DisplayOp::DrawImage {
            image: pdf_page_ir::ImageId(i as u32),
            paint: pdf_page_ir::PaintId(0),
            transform: Matrix::IDENTITY,
            alpha: 1.0,
            blend: pdf_page_ir::BlendMode::Normal,
        });
    }
    CompiledPage {
        schema_version: pdf_page_ir::IR_SCHEMA_VERSION,
        content_bounds: None,
        bounds: PageBounds {
            crop: Rect {
                x0: 0.0,
                y0: 0.0,
                x1: 8.0,
                y1: 8.0,
            },
            rotate: 0,
        },
        operations: Arc::from(ops),
        paths: Arc::from([]),
        paints: Arc::from([Paint::Solid(Color::BLACK)]),
        stroke_styles: Arc::from([]),
        glyph_runs: Arc::from([]),
        fonts: Arc::from([]),
        images: Arc::from(images),
        masks: Arc::from([]),
        groups: Arc::from([]),
        shadings: Arc::from([]),
        tilings: Arc::from([]),
        features: PageFeatures::IMAGES,
        complexity: PageComplexity::default(),
    }
}

#[test]
fn successful_image_draw_counts_coverage() {
    // A painted image must mark `covered_pixels`; otherwise an image-only page
    // is indistinguishable from a blank one and `is_silent_blank()` mislabels it.
    let backend = backend_with_stub_jpx(pdf_image::DecodedFormat::Rgb8, 3);
    let (_host, stats) = backend
        .render_to_host(&request(
            page(jpx_image(ImageColorSpace::Rgb), Color::BLACK, 8.0),
            8,
        ))
        .unwrap();
    assert!(
        stats.covered_pixels > 0,
        "a successful image draw marks coverage"
    );
    assert!(
        !stats.is_silent_blank(),
        "a page that painted an image is not a silent blank"
    );
}

#[test]
fn jpx_premultiplied_in_data_alpha_is_unassociated_before_compositing() {
    // Straight RGB [128,64,32] at alpha 128 is stored premultiplied as
    // [64,32,16,128]. The preparation seam must recover the straight colour
    // before applying alpha as an ordinary soft mask.
    let data: Vec<u8> = [64, 32, 16, 128]
        .into_iter()
        .cycle()
        .take(4 * 4 * 4)
        .collect();
    let backend =
        backend_with_stub_jpx_data(pdf_image::DecodedFormat::Rgba8, 4, Some(Arc::from(data)));
    let mut image = jpx_image(ImageColorSpace::Rgb);
    image.smask_in_data = 2;
    let (host, stats) = backend
        .render_to_host(&request(page(image, Color::BLACK, 8.0), 8))
        .unwrap();

    assert_eq!(stats.degraded_draws, 0);
    assert_eq!(
        px(&host, 4, 4),
        [191, 159, 143, 255],
        "unassociated [128,64,32] at alpha 128 composites over white"
    );
}

#[test]
fn codec_cmyk_uses_the_declared_icc_transform() {
    // A constant Lab-black transform makes the distinction unambiguous:
    // DeviceCMYK [0,0,0,0] is white, while this profile maps it to black.
    let linear: Arc<[f32]> = Arc::from([0.0, 1.0]);
    let transform = IccCmykTransform {
        grid: 2,
        input_tables: std::array::from_fn(|_| linear.clone()),
        clut: Arc::from([[0.0, 0.5, 0.5]; 16]),
        output_tables: std::array::from_fn(|_| linear.clone()),
    };
    let data: Arc<[u8]> = Arc::from(vec![0u8; 4 * 4 * 4]);
    let backend = backend_with_stub_jpx_data(pdf_image::DecodedFormat::Cmyk8, 4, Some(data));
    let image = jpx_image(ImageColorSpace::IccCmyk {
        transform: Arc::new(transform),
    });
    let (host, stats) = backend
        .render_to_host(&request(page(image, Color::BLACK, 8.0), 8))
        .unwrap();

    assert_eq!(stats.degraded_draws, 0);
    let pixel = px(&host, 4, 4);
    assert!(
        pixel[0] < 8 && pixel[1] < 8 && pixel[2] < 8,
        "the ICC transform must win over DeviceCMYK white: {pixel:?}"
    );
}

#[test]
fn image_only_page_with_one_drop_and_one_paint_is_not_silent_blank() {
    // One image paints (Rgb over an Rgb codec) and one drops (Indexed over the
    // multi-component codec). A dropped draw exists, but the page DID paint, so
    // it must be reported `degraded`, never `silent-blank`.
    let painted = jpx_image(ImageColorSpace::Rgb);
    let dropped = jpx_image(ImageColorSpace::Indexed {
        base: Box::new(ImageColorSpace::Rgb),
        hival: 0,
        lookup: Arc::from(vec![255u8, 255, 255]),
    });
    let backend = backend_with_stub_jpx(pdf_image::DecodedFormat::Rgb8, 3);
    let (_host, stats) = backend
        .render_to_host(&request(multi_image_page(vec![painted, dropped]), 8))
        .unwrap();
    assert_eq!(stats.degraded_draws, 1, "the mismatched draw is counted");
    assert!(
        stats.covered_pixels > 0,
        "the successful image marked coverage"
    );
    assert!(
        !stats.is_silent_blank(),
        "a page that painted content is not a silent blank"
    );
}

#[test]
fn image_only_page_with_only_a_drop_is_silent_blank() {
    // The complement: the sole image drops and nothing else paints. That IS a
    // silent blank — content PDFium would paint that we blanked.
    let dropped = jpx_image(ImageColorSpace::Indexed {
        base: Box::new(ImageColorSpace::Rgb),
        hival: 0,
        lookup: Arc::from(vec![255u8, 255, 255]),
    });
    let backend = backend_with_stub_jpx(pdf_image::DecodedFormat::Rgb8, 3);
    let (_host, stats) = backend
        .render_to_host(&request(multi_image_page(vec![dropped]), 8))
        .unwrap();
    assert_eq!(stats.degraded_draws, 1);
    assert_eq!(stats.covered_pixels, 0, "nothing painted");
    assert!(
        stats.is_silent_blank(),
        "a page blanked by a dropped draw is a silent blank"
    );
}

#[test]
fn indexed_over_multicomponent_codec_is_dropped_and_tracked() {
    // The R1 regression shape: a JPXDecode image declares an `[/Indexed …]`
    // color space (a single-component palette; `hival 0`, lookup = one white
    // entry) but the codec decodes to *multi-component* RGB. The palette cannot
    // index 3-channel samples, and painting the raw channels would blank the
    // real content underneath with a solid white overlay. The draw must be
    // dropped rather than painted — AND the drop must be counted, so a page a
    // *successful* decode blanked is never mistaken for a clean render (the
    // tracking hole R1 exposed).
    let cs = ImageColorSpace::Indexed {
        base: Box::new(ImageColorSpace::Rgb),
        hival: 0,
        lookup: Arc::from(vec![255u8, 255, 255]),
    };
    let backend = backend_with_stub_jpx(pdf_image::DecodedFormat::Rgb8, 3);
    let (host, stats) = backend
        .render_to_host(&request(page(jpx_image(cs), Color::BLACK, 8.0), 8))
        .unwrap();

    assert_eq!(
        stats.degraded_draws, 1,
        "the mismatched draw must be counted, not silent"
    );
    assert!(
        stats
            .recovery_notes
            .iter()
            .any(|n| n.contains("Indexed") && n.contains("multi-component")),
        "a human-readable reason names the mismatch: {:?}",
        stats.recovery_notes
    );
    assert_eq!(
        px(&host, 4, 4),
        [255, 255, 255, 255],
        "no white overlay was painted"
    );
}

#[test]
fn indexed_over_single_component_codec_still_reinterprets() {
    // The complement: when the codec decodes to a *single* component (Gray),
    // that component is a palette index, so the `[/Indexed …]` space is honored
    // (reinterpreted) and the draw paints — it is neither dropped nor degraded.
    // The stub decodes to solid 255 (Gray); a full 256-entry palette maps index
    // 255 → blue, so no clamp ambiguity.
    let mut lookup = vec![0u8; 256 * 3];
    lookup[255 * 3..255 * 3 + 3].copy_from_slice(&[0, 0, 255]); // index 255 → blue
    let cs = ImageColorSpace::Indexed {
        base: Box::new(ImageColorSpace::Rgb),
        hival: 255,
        lookup: Arc::from(lookup),
    };
    let backend = backend_with_stub_jpx(pdf_image::DecodedFormat::Gray8, 1);
    let (host, stats) = backend
        .render_to_host(&request(page(jpx_image(cs), Color::BLACK, 8.0), 8))
        .unwrap();

    assert_eq!(
        stats.degraded_draws, 0,
        "a single-component codec honors the palette, no drop"
    );
    assert_eq!(
        px(&host, 4, 4),
        [0, 0, 255, 255],
        "palette reinterpreted, draw painted"
    );
}

// --- A10: image-edge partial-coverage anti-aliasing --------------------------

#[test]
fn fractional_image_edge_paints_partial_coverage() {
    // A 1x1 all-painting black STENCIL placed at x in [2.25, 13.75]: the edge
    // device pixels are covered 75% and must paint at ~75% alpha (edge AA),
    // not 0 or 100%. (Stencils/Type 3 glyphs are the workstream-H boldness
    // family; the opaque axis-aligned RGB fast paths deliberately keep hard
    // edges, matching PDFium's own blit there.)
    let img = image_ir(1, 1, 1, ImageColorSpace::Gray, vec![0x00], true, None);
    let page = {
        let mut p = page(img, Color::BLACK, 1.0);
        p.operations = Arc::from([
            DisplayOp::ConcatTransform(
                Matrix::scale(11.5, 16.0).then(Matrix::translate(2.25, 0.0)),
            ),
            DisplayOp::DrawImage {
                image: pdf_page_ir::ImageId(0),
                paint: pdf_page_ir::PaintId(0),
                transform: Matrix::IDENTITY,
                alpha: 1.0,
                blend: pdf_page_ir::BlendMode::Normal,
            },
        ]);
        p
    };
    let backend = CpuBackend::default();
    let (host, _) = backend.render_to_host(&request(page, 16)).unwrap();

    // Interior: solid black. Outside: white.
    assert_eq!(px(&host, 8, 8), [0, 0, 0, 255], "interior solid");
    assert_eq!(px(&host, 0, 8), [255, 255, 255, 255], "outside untouched");
    assert_eq!(px(&host, 15, 8), [255, 255, 255, 255], "outside untouched");
    // Left edge pixel x=2 is 75% covered -> ~25% white shows through.
    let p = px(&host, 2, 8);
    assert!(
        (p[0] as i32 - 64).abs() <= 3 && p[0] == p[1] && p[1] == p[2],
        "left edge at ~75% coverage: {p:?}"
    );
    // Right edge pixel x=13 is 75% covered too.
    let p = px(&host, 13, 8);
    assert!(
        (p[0] as i32 - 64).abs() <= 3,
        "right edge at ~75% coverage: {p:?}"
    );
}

#[test]
fn rotated_image_edges_are_antialiased() {
    // A black 1x1 image under a 45-degree rotation: diagonal edge pixels must
    // take intermediate values (partial coverage), not a hard staircase.
    let s = std::f64::consts::FRAC_1_SQRT_2;
    let rot = Matrix {
        a: s,
        b: s,
        c: -s,
        d: s,
        e: 8.0,
        f: 0.0,
    };
    let img = image_ir(1, 1, 8, ImageColorSpace::Rgb, vec![0, 0, 0], false, None);
    let page = {
        let mut p = page(img, Color::BLACK, 1.0);
        p.operations = Arc::from([
            DisplayOp::ConcatTransform(Matrix::scale(8.0, 8.0).then(rot)),
            DisplayOp::DrawImage {
                image: pdf_page_ir::ImageId(0),
                paint: pdf_page_ir::PaintId(0),
                transform: Matrix::IDENTITY,
                alpha: 1.0,
                blend: pdf_page_ir::BlendMode::Normal,
            },
        ]);
        p
    };
    let backend = CpuBackend::default();
    let (host, _) = backend.render_to_host(&request(page, 16)).unwrap();

    // Some pixel along the diagonal edge holds an intermediate gray.
    let mut intermediate = 0;
    for y in 0..16 {
        for x in 0..16 {
            let p = px(&host, x, y);
            if p[0] > 16 && p[0] < 239 && p[0] == p[1] && p[1] == p[2] {
                intermediate += 1;
            }
        }
    }
    assert!(
        intermediate >= 8,
        "diagonal edges antialiased: {intermediate} intermediate pixels"
    );
    // The rotated square's center is solid black.
    assert_eq!(px(&host, 8, 5), [0, 0, 0, 255], "interior solid");
}
