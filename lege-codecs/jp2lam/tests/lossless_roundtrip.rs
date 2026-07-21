use jp2lam::{
    ColorEncoding, ColorSpace, Component, EncodeOptions, IccComponentModel, Image, ImageView,
    OutputFormat, ResourceLimits, TilePolicy, decode_jp2, encode, encode_view,
};
use std::process::Command;

#[derive(Debug, Clone, Copy)]
enum FixtureKind {
    Gray,
    Rgb,
}

#[test]
fn lossless_gray_roundtrips_exactly_for_odd_gradient() {
    let image = gray_fixture(37, 29, |x, y| {
        ((x * 7 + y * 11 + (x ^ (3 * y))) & 0xff) as u8
    });

    assert_native_lossless_roundtrip("gray odd gradient", &image);
}

#[test]
fn lossless_gray_roundtrips_exactly_for_deterministic_random_samples() {
    let mut rng = XorShift32::new(0x4a50_324c);
    let image = gray_fixture(43, 31, |_x, _y| rng.next_u8());

    assert_native_lossless_roundtrip("gray deterministic random", &image);
}

#[test]
fn lossless_rgb_roundtrips_exactly_for_saturated_extrema() {
    let image = rgb_fixture(19, 17, |x, y| {
        let pattern = ((x & 1) << 1) | (y & 1);
        match pattern {
            0 => [0, 0, 0],
            1 => [255, 255, 255],
            2 => [255, 0, 0],
            _ => [0, 255, 255],
        }
    });

    assert_native_lossless_roundtrip("rgb saturated extrema", &image);
}

#[test]
fn lossless_rgb_roundtrips_exactly_for_alternating_high_frequency_colors() {
    let image = rgb_fixture(35, 33, |x, y| {
        if ((x + y) & 1) == 0 {
            [255, 0, 128]
        } else {
            [0, 255, 127]
        }
    });

    assert_native_lossless_roundtrip("rgb alternating high frequency", &image);
}

#[test]
fn lossless_rgb_roundtrips_exactly_for_deterministic_random_samples() {
    let mut rng = XorShift32::new(0x1544_1001);
    let image = rgb_fixture(41, 27, |_x, _y| {
        [rng.next_u8(), rng.next_u8(), rng.next_u8()]
    });

    assert_native_lossless_roundtrip("rgb deterministic random", &image);
}

#[test]
fn lossless_fixed_tiles_roundtrip_exactly_for_odd_grayscale_edges() {
    let image = gray_fixture(37, 29, |x, y| {
        ((x * 17 + y * 29 + (x ^ (5 * y))) & 0xff) as u8
    });

    assert_native_tiled_lossless_roundtrip(
        "gray fixed tiles",
        &image,
        TilePolicy::Fixed {
            width: 11,
            height: 9,
        },
    );
}

#[test]
fn lossless_fixed_tiles_roundtrip_exactly_for_odd_rgb_edges() {
    let image = rgb_fixture(35, 27, |x, y| {
        [
            ((x * 13 + y * 7) & 0xff) as u8,
            ((x * 3 + y * 23 + 41) & 0xff) as u8,
            ((x * y + x * 19 + y * 11) & 0xff) as u8,
        ]
    });

    assert_native_tiled_lossless_roundtrip(
        "rgb fixed tiles",
        &image,
        TilePolicy::Fixed {
            width: 11,
            height: 9,
        },
    );
}

#[test]
fn lossless_auto_tiles_honor_low_memory_plan_and_roundtrip_exactly() {
    let image = gray_fixture(130, 70, |x, y| {
        ((x * 7 + y * 31 + (x ^ (3 * y))) & 0xff) as u8
    });
    let encoded = encode(
        &image,
        &EncodeOptions {
            quality: 100,
            format: OutputFormat::Jp2,
            tile_policy: TilePolicy::Auto,
            resource_limits: ResourceLimits {
                // Two encoder-owned i32 tile planes at 64x64.
                max_working_memory: Some(64 * 64 * 8),
                // Exercise the production spill-backed payload path.
                encoded_store_memory_limit: Some(1),
                ..Default::default()
            },
            ..Default::default()
        },
    )
    .expect("auto tiled lossless encode");
    let metadata = jp2lam::inspect_jp2(&encoded).expect("inspect auto tiled JP2");
    assert_eq!(metadata.codestream.siz.tile_width, 64);
    assert_eq!(metadata.codestream.siz.tile_height, 64);
    assert_eq!(metadata.tile_part_count, 6);

    let decoded = decode_jp2(&encoded).expect("decode auto tiled JP2");
    assert_eq!(decoded.components[0].data, image.components[0].data);
}

#[test]
fn lossless_borrowed_gray8_view_roundtrips_exactly() {
    let width = 17;
    let height = 13;
    let samples = (0..height)
        .flat_map(|y| (0..width).map(move |x| ((x * 9 + y * 5 + (x ^ y)) & 0xff) as u8))
        .collect::<Vec<_>>();
    let view = ImageView::from_gray8(width, height, &samples).expect("gray view");
    let encoded = encode_view(
        view,
        &EncodeOptions {
            quality: 100,
            format: OutputFormat::Jp2,
            profile: Default::default(),
            ..Default::default()
        },
    )
    .expect("encode borrowed gray view");
    let decoded = decode_jp2(&encoded).expect("decode borrowed gray view");
    assert_eq!(decoded.colorspace, ColorSpace::Gray);
    assert_eq!(
        decoded.components[0].data,
        samples
            .iter()
            .map(|&sample| i32::from(sample))
            .collect::<Vec<_>>()
    );
}

#[test]
fn lossless_borrowed_interleaved_rgb8_view_roundtrips_exactly() {
    let width = 11;
    let height = 9;
    let mut samples = Vec::with_capacity(width as usize * height as usize * 3);
    let mut expected = [Vec::new(), Vec::new(), Vec::new()];
    for y in 0..height {
        for x in 0..width {
            let rgb = [
                ((x * 17 + y * 3) & 0xff) as u8,
                ((x * 5 + y * 19 + 33) & 0xff) as u8,
                ((x * y + x * 11 + y * 7) & 0xff) as u8,
            ];
            samples.extend_from_slice(&rgb);
            for component in 0..3 {
                expected[component].push(i32::from(rgb[component]));
            }
        }
    }
    let view = ImageView::from_rgb8_interleaved(width, height, &samples).expect("rgb view");
    let encoded = encode_view(
        view,
        &EncodeOptions {
            quality: 100,
            format: OutputFormat::Jp2,
            profile: Default::default(),
            ..Default::default()
        },
    )
    .expect("encode borrowed rgb view");
    let decoded = decode_jp2(&encoded).expect("decode borrowed rgb view");
    assert_eq!(decoded.colorspace, ColorSpace::Srgb);
    for (actual, expected) in decoded.components.iter().zip(expected.iter()) {
        assert_eq!(&actual.data, expected);
    }
}

#[test]
fn lossless_gray_u16_storage_roundtrips_exactly_at_supported_precisions() {
    let (width, height) = (17u32, 13u32);
    for precision in [8u32, 10, 12, 14, 16] {
        let max = ((1u32 << precision) - 1) as u16;
        let mut rng = XorShift32::new(0x1544_0000 | precision);
        let mut samples = Vec::with_capacity((width * height) as usize);
        for y in 0..height {
            for x in 0..width {
                samples.push(if x == 0 && y == 0 {
                    0
                } else if x == 1 && y == 0 {
                    max
                } else if y == 0 {
                    ((x * u32::from(max)) / (width - 1)) as u16
                } else {
                    rng.next_u16() & max
                });
            }
        }
        let encoded = encode_view(
            ImageView::from_gray16(width, height, &samples, precision).expect("gray16 view"),
            &EncodeOptions {
                quality: 100,
                format: OutputFormat::Jp2,
                tile_policy: if precision == 16 {
                    TilePolicy::Fixed {
                        width: 8,
                        height: 8,
                    }
                } else {
                    TilePolicy::Single
                },
                resource_limits: ResourceLimits {
                    encoded_store_memory_limit: (precision == 16).then_some(1),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap_or_else(|error| panic!("{precision}-bit gray encode failed: {error}"));
        let decoded = decode_jp2(&encoded)
            .unwrap_or_else(|error| panic!("{precision}-bit gray decode failed: {error}"));
        assert_eq!(decoded.components[0].precision, precision);
        assert_eq!(
            decoded.components[0].data,
            samples.iter().map(|&sample| i32::from(sample)).collect::<Vec<_>>(),
            "{precision}-bit grayscale samples"
        );
    }
}

#[test]
fn lossless_rgb_u16_storage_roundtrips_exactly_at_supported_precisions() {
    let (width, height) = (11u32, 9u32);
    for precision in [8u32, 10, 12, 14, 16] {
        let max = ((1u32 << precision) - 1) as u16;
        let mut rng = XorShift32::new(0x5247_0000 | precision);
        let mut samples = Vec::with_capacity(width as usize * height as usize * 3);
        let mut expected = [Vec::new(), Vec::new(), Vec::new()];
        for y in 0..height {
            for x in 0..width {
                let rgb = if y == 0 && x == 0 {
                    [0, max, max / 2]
                } else if y == 0 {
                    let ramp = ((x * u32::from(max)) / (width - 1)) as u16;
                    [ramp, max - ramp, ramp / 2]
                } else {
                    [rng.next_u16() & max, rng.next_u16() & max, rng.next_u16() & max]
                };
                samples.extend_from_slice(&rgb);
                for component in 0..3 {
                    expected[component].push(i32::from(rgb[component]));
                }
            }
        }
        let encoded = encode_view(
            ImageView::from_rgb16_interleaved(width, height, &samples, precision)
                .expect("rgb16 view"),
            &EncodeOptions {
                quality: 100,
                format: OutputFormat::Jp2,
                tile_policy: if precision == 16 {
                    TilePolicy::Fixed {
                        width: 8,
                        height: 8,
                    }
                } else {
                    TilePolicy::Single
                },
                resource_limits: ResourceLimits {
                    encoded_store_memory_limit: (precision == 16).then_some(1),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap_or_else(|error| panic!("{precision}-bit RGB encode failed: {error}"));
        let decoded = decode_jp2(&encoded)
            .unwrap_or_else(|error| panic!("{precision}-bit RGB decode failed: {error}"));
        for (actual, expected) in decoded.components.iter().zip(&expected) {
            assert_eq!(actual.precision, precision);
            assert_eq!(&actual.data, expected, "{precision}-bit RGB samples");
        }
    }
}

#[test]
fn lossless_roundtrip_decodes_exactly_with_imagemagick_when_available() {
    if !imagemagick_jp2_available() {
        eprintln!("skipping ImageMagick JP2 roundtrip check; `magick` JP2 support not available");
        return;
    }

    let gray = gray_fixture(29, 21, |x, y| ((x * 13 + y * 17 + (x ^ y)) & 0xff) as u8);
    let encoded_gray = encode_lossless_jp2(&gray);
    let decoded_gray =
        decode_with_imagemagick(&encoded_gray, gray.width, gray.height, FixtureKind::Gray);
    assert_eq!(decoded_gray, planar_source_bytes(&gray));

    let rgb = rgb_fixture(23, 19, |x, y| {
        [
            ((x * 17 + y * 3) & 0xff) as u8,
            ((x * 5 + y * 19 + 33) & 0xff) as u8,
            ((x * y + x * 11 + y * 7) & 0xff) as u8,
        ]
    });
    let encoded_rgb = encode_lossless_jp2(&rgb);
    let decoded_rgb =
        decode_with_imagemagick(&encoded_rgb, rgb.width, rgb.height, FixtureKind::Rgb);
    assert_eq!(decoded_rgb, planar_source_bytes(&rgb));
}

#[test]
fn lossless_fixed_tiles_decode_exactly_with_imagemagick_when_available() {
    if !imagemagick_jp2_available() {
        eprintln!("skipping tiled ImageMagick check; `magick` JP2 support not available");
        return;
    }

    let image = rgb_fixture(33, 27, |x, y| {
        [
            ((x * 17 + y * 3) & 0xff) as u8,
            ((x * 5 + y * 19 + 33) & 0xff) as u8,
            ((x * y + x * 11 + y * 7) & 0xff) as u8,
        ]
    });
    let encoded = encode(
        &image,
        &EncodeOptions {
            quality: 100,
            format: OutputFormat::Jp2,
            tile_policy: TilePolicy::Fixed {
                width: 11,
                height: 9,
            },
            ..Default::default()
        },
    )
    .expect("tiled lossless encode");
    let decoded = decode_with_imagemagick(&encoded, image.width, image.height, FixtureKind::Rgb);
    assert_eq!(decoded, planar_source_bytes(&image));
}

#[test]
fn lossless_odd_origin_gray_tiles_decode_exactly_with_imagemagick_when_available() {
    if !imagemagick_jp2_available() {
        eprintln!("skipping odd-origin grayscale ImageMagick check; JP2 support not available");
        return;
    }
    let image = gray_fixture(33, 27, |x, y| {
        ((x * 17 + y * 29 + (x ^ (5 * y))) & 0xff) as u8
    });
    let encoded = encode(
        &image,
        &EncodeOptions {
            quality: 100,
            format: OutputFormat::Jp2,
            tile_policy: TilePolicy::Fixed {
                width: 11,
                height: 9,
            },
            ..Default::default()
        },
    )
    .expect("odd-origin grayscale tiled lossless encode");
    let decoded = decode_with_imagemagick(&encoded, image.width, image.height, FixtureKind::Gray);
    assert_eq!(decoded, planar_source_bytes(&image));
}

#[test]
fn lossless_16_bit_gray_decodes_exactly_with_imagemagick_when_available() {
    if !imagemagick_jp2_available() {
        eprintln!("skipping 16-bit ImageMagick JP2 check; JP2 support not available");
        return;
    }
    let (width, height) = (9u32, 7u32);
    let samples = (0..height)
        .flat_map(|y| {
            (0..width).map(move |x| {
                if x == 0 && y == 0 {
                    0
                } else if x == 1 && y == 0 {
                    u16::MAX
                } else {
                    (x * 4099 + y * 8191 + (x ^ y) * 257) as u16
                }
            })
        })
        .collect::<Vec<_>>();
    let encoded = encode_view(
        ImageView::from_gray16(width, height, &samples, 16).expect("gray16 view"),
        &EncodeOptions {
            quality: 100,
            format: OutputFormat::Jp2,
            ..Default::default()
        },
    )
    .expect("16-bit lossless encode");

    let decoded = decode_u16_with_imagemagick(&encoded, width, height, FixtureKind::Gray);
    assert_eq!(decoded, samples);
}

#[test]
fn lossless_16_bit_rgb_decodes_exactly_with_imagemagick_when_available() {
    if !imagemagick_jp2_available() {
        eprintln!("skipping 16-bit RGB ImageMagick JP2 check; JP2 support not available");
        return;
    }
    let (width, height) = (9u32, 7u32);
    let mut samples = Vec::with_capacity(width as usize * height as usize * 3);
    for y in 0..height {
        for x in 0..width {
            let rgb = if x == 0 && y == 0 {
                [0, u16::MAX, 32_768]
            } else {
                [
                    (x * 4099 + y * 257) as u16,
                    (x * 911 + y * 8191 + 17) as u16,
                    (x * y * 521 + x * 1237 + y * 733) as u16,
                ]
            };
            samples.extend_from_slice(&rgb);
        }
    }
    let encoded = encode_view(
        ImageView::from_rgb16_interleaved(width, height, &samples, 16).expect("rgb16 view"),
        &EncodeOptions {
            quality: 100,
            format: OutputFormat::Jp2,
            ..Default::default()
        },
    )
    .expect("16-bit RGB lossless encode");

    let decoded = decode_u16_with_imagemagick(&encoded, width, height, FixtureKind::Rgb);
    assert_eq!(decoded, samples);
}

#[test]
fn restricted_rgb_icc_profile_is_preserved_exactly() {
    let image = rgb_fixture(9, 7, |x, y| {
        [
            ((x * 17 + y * 3) & 0xff) as u8,
            ((x * 5 + y * 19 + 33) & 0xff) as u8,
            ((x * y + x * 11 + y * 7) & 0xff) as u8,
        ]
    });
    let profile = minimal_restricted_icc(*b"RGB ");
    let encoded = encode(
        &image,
        &EncodeOptions {
            quality: 100,
            format: OutputFormat::Jp2,
            color_encoding: Some(
                ColorEncoding::restricted_icc(profile.clone(), IccComponentModel::Rgb)
                    .expect("valid restricted ICC"),
            ),
            ..Default::default()
        },
    )
    .expect("ICC-described encode");
    let metadata = jp2lam::inspect_jp2(&encoded).expect("inspect ICC JP2");
    assert_eq!(
        metadata.color_encoding,
        ColorEncoding::IccProfile {
            bytes: profile,
            component_model: IccComponentModel::Rgb,
        }
    );
    let decoded = decode_jp2(&encoded).expect("decode ICC JP2");
    for (actual, expected) in decoded.components.iter().zip(&image.components) {
        assert_eq!(actual.data, expected.data);
    }
}

#[test]
fn ambiguous_rgb_requires_explicit_color_encoding() {
    let mut image = rgb_fixture(3, 3, |x, y| [x as u8, y as u8, (x + y) as u8]);
    image.colorspace = ColorSpace::Rgb;
    let error = encode(&image, &EncodeOptions::default()).expect_err("ambiguous RGB must fail");
    assert!(error.to_string().contains("explicit ColorEncoding"), "{error}");

    encode(
        &image,
        &EncodeOptions {
            quality: 100,
            color_encoding: Some(ColorEncoding::Srgb),
            ..Default::default()
        },
    )
    .expect("explicit sRGB description");
}

fn assert_native_lossless_roundtrip(name: &str, image: &Image) {
    let encoded = encode_lossless_jp2(image);
    let decoded = decode_jp2(&encoded).unwrap_or_else(|err| panic!("{name}: decode failed: {err}"));

    assert_eq!(decoded.width, image.width, "{name}: width");
    assert_eq!(decoded.height, image.height, "{name}: height");
    assert_eq!(decoded.colorspace, image.colorspace, "{name}: colorspace");
    assert_eq!(
        decoded.components.len(),
        image.components.len(),
        "{name}: component count"
    );

    for (component_index, (actual, expected)) in decoded
        .components
        .iter()
        .zip(image.components.iter())
        .enumerate()
    {
        assert_eq!(
            actual.width, expected.width,
            "{name}: component {component_index} width"
        );
        assert_eq!(
            actual.height, expected.height,
            "{name}: component {component_index} height"
        );
        assert_eq!(
            actual.precision, expected.precision,
            "{name}: component {component_index} precision"
        );
        assert_eq!(
            actual.signed, expected.signed,
            "{name}: component {component_index} signedness"
        );
        assert_eq!(
            actual.data, expected.data,
            "{name}: component {component_index} samples"
        );
    }
}

fn assert_native_tiled_lossless_roundtrip(name: &str, image: &Image, tile_policy: TilePolicy) {
    let encoded = encode(
        image,
        &EncodeOptions {
            quality: 100,
            format: OutputFormat::Jp2,
            tile_policy,
            ..Default::default()
        },
    )
    .unwrap_or_else(|err| panic!("{name}: encode failed: {err}"));
    let decoded = decode_jp2(&encoded).unwrap_or_else(|err| panic!("{name}: decode failed: {err}"));

    assert_eq!(decoded.width, image.width, "{name}: width");
    assert_eq!(decoded.height, image.height, "{name}: height");
    assert_eq!(decoded.colorspace, image.colorspace, "{name}: colorspace");
    for (component_index, (actual, expected)) in
        decoded.components.iter().zip(&image.components).enumerate()
    {
        assert_eq!(
            actual.data, expected.data,
            "{name}: component {component_index} samples"
        );
    }
}

fn encode_lossless_jp2(image: &Image) -> Vec<u8> {
    encode(
        image,
        &EncodeOptions {
            quality: 100,
            format: OutputFormat::Jp2,
            profile: Default::default(),
            ..Default::default()
        },
    )
    .expect("lossless encode")
}

fn gray_fixture<F>(width: u32, height: u32, mut sample: F) -> Image
where
    F: FnMut(u32, u32) -> u8,
{
    let mut data = Vec::with_capacity(width as usize * height as usize);
    for y in 0..height {
        for x in 0..width {
            data.push(i32::from(sample(x, y)));
        }
    }
    Image {
        width,
        height,
        components: vec![component(data, width, height)],
        colorspace: ColorSpace::Gray,
    }
}

fn rgb_fixture<F>(width: u32, height: u32, mut sample: F) -> Image
where
    F: FnMut(u32, u32) -> [u8; 3],
{
    let pixels = width as usize * height as usize;
    let mut r = Vec::with_capacity(pixels);
    let mut g = Vec::with_capacity(pixels);
    let mut b = Vec::with_capacity(pixels);
    for y in 0..height {
        for x in 0..width {
            let [rr, gg, bb] = sample(x, y);
            r.push(i32::from(rr));
            g.push(i32::from(gg));
            b.push(i32::from(bb));
        }
    }
    Image {
        width,
        height,
        components: vec![
            component(r, width, height),
            component(g, width, height),
            component(b, width, height),
        ],
        colorspace: ColorSpace::Srgb,
    }
}

fn component(data: Vec<i32>, width: u32, height: u32) -> Component {
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

fn planar_source_bytes(image: &Image) -> Vec<u8> {
    image
        .components
        .iter()
        .flat_map(|component| component.data.iter().map(|&sample| sample as u8))
        .collect()
}

fn imagemagick_jp2_available() -> bool {
    let Ok(output) = Command::new("magick").arg("-list").arg("format").output() else {
        return false;
    };
    output.status.success() && String::from_utf8_lossy(&output.stdout).contains("JP2")
}

fn decode_with_imagemagick(encoded: &[u8], width: u32, height: u32, kind: FixtureKind) -> Vec<u8> {
    let tempdir = tempfile::tempdir().expect("temporary directory");
    let jp2_path = tempdir.path().join("input.jp2");
    let raw_path = tempdir.path().join("output.raw");
    std::fs::write(&jp2_path, encoded).expect("write temporary JP2");

    let raw_format = match kind {
        FixtureKind::Gray => "gray",
        FixtureKind::Rgb => "rgb",
    };
    let output_arg = format!("{raw_format}:{}", raw_path.display());
    let status = Command::new("magick")
        .arg(&jp2_path)
        .arg("-depth")
        .arg("8")
        .arg(&output_arg)
        .status()
        .expect("run ImageMagick");
    assert!(status.success(), "ImageMagick failed to decode JP2");

    let bytes = std::fs::read(raw_path).expect("read ImageMagick raw output");
    assert_eq!(
        bytes.len(),
        width as usize
            * height as usize
            * match kind {
                FixtureKind::Gray => 1,
                FixtureKind::Rgb => 3,
            }
    );

    match kind {
        FixtureKind::Gray => bytes,
        FixtureKind::Rgb => {
            let pixels = width as usize * height as usize;
            let mut planar = Vec::with_capacity(bytes.len());
            for channel in 0..3 {
                for pixel in 0..pixels {
                    planar.push(bytes[pixel * 3 + channel]);
                }
            }
            planar
        }
    }
}

fn decode_u16_with_imagemagick(
    encoded: &[u8],
    width: u32,
    height: u32,
    kind: FixtureKind,
) -> Vec<u16> {
    let tempdir = tempfile::tempdir().expect("temporary directory");
    let jp2_path = tempdir.path().join("input.jp2");
    let raw_path = tempdir.path().join("output.raw");
    std::fs::write(&jp2_path, encoded).expect("write temporary JP2");
    let raw_format = match kind {
        FixtureKind::Gray => "gray",
        FixtureKind::Rgb => "rgb",
    };
    let status = Command::new("magick")
        .arg(&jp2_path)
        .arg("-depth")
        .arg("16")
        .arg("-endian")
        .arg("MSB")
        .arg(format!("{raw_format}:{}", raw_path.display()))
        .status()
        .expect("run ImageMagick");
    assert!(status.success(), "ImageMagick failed to decode 16-bit JP2");
    let bytes = std::fs::read(raw_path).expect("read ImageMagick raw output");
    let channels = match kind {
        FixtureKind::Gray => 1,
        FixtureKind::Rgb => 3,
    };
    assert_eq!(bytes.len(), width as usize * height as usize * channels * 2);
    bytes
        .chunks_exact(2)
        .map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
        .collect()
}

fn minimal_restricted_icc(data_space: [u8; 4]) -> Vec<u8> {
    let mut profile = vec![0u8; 128];
    profile[0..4].copy_from_slice(&128u32.to_be_bytes());
    profile[12..16].copy_from_slice(b"scnr");
    profile[16..20].copy_from_slice(&data_space);
    profile[20..24].copy_from_slice(b"XYZ ");
    profile[36..40].copy_from_slice(b"acsp");
    profile
}

#[derive(Clone, Copy)]
struct XorShift32 {
    state: u32,
}

impl XorShift32 {
    fn new(seed: u32) -> Self {
        assert_ne!(seed, 0);
        Self { state: seed }
    }

    fn next_u8(&mut self) -> u8 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.state = x;
        (x >> 24) as u8
    }

    fn next_u16(&mut self) -> u16 {
        (u16::from(self.next_u8()) << 8) | u16::from(self.next_u8())
    }
}
