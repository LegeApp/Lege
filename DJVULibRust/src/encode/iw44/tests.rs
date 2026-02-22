#[cfg(test)]
mod tests {
    use crate::encode::iw44::encoder::{
        rgb_to_ycbcr_planes, ycbcr_from_rgb, CrcbMode, EncoderParams,
    };
    #[cfg(test)]
    use image::{ImageBuffer, Rgb, RgbImage};

    /// Test color conversion with known values
    #[test]
    fn test_rgb_to_ycbcr_conversion() {
        // Test pure red (255, 0, 0)
        let red_rgb = [255u8, 0, 0];
        let mut y = [0i8; 1];
        let mut cb = [0i8; 1];
        let mut cr = [0i8; 1];

        rgb_to_ycbcr_planes(&red_rgb, &mut y, &mut cb, &mut cr);

        // Expected values for pure red using DjVu coefficients
        // Y = 0.304348*255 + 0.608696*0 + 0.086956*0 = 77.609 -> 78 - 128 = -50
        // Cb = -0.173913*255 - 0.347826*0 + 0.521739*0 = -44.348 -> -44
        // Cr = 0.463768*255 - 0.405797*0 - 0.057971*0 = 118.261 -> 118

        assert_eq!(y[0], -50, "Y component for pure red");
        assert_eq!(cb[0], -44, "Cb component for pure red");
        assert_eq!(cr[0], 118, "Cr component for pure red");
    }

    #[test]
    fn test_rgb_to_ycbcr_green() {
        // Test pure green (0, 255, 0)
        let green_rgb = [0u8, 255, 0];
        let mut y = [0i8; 1];
        let mut cb = [0i8; 1];
        let mut cr = [0i8; 1];

        rgb_to_ycbcr_planes(&green_rgb, &mut y, &mut cb, &mut cr);

        // Expected values for pure green using DjVu coefficients
        // Y = 0.304348*0 + 0.608696*255 + 0.086956*0 = 155.218 -> 155 - 128 = 27
        // Cb = -0.173913*0 - 0.347826*255 + 0.521739*0 = -88.696 -> -89
        // Cr = 0.463768*0 - 0.405797*255 - 0.057971*0 = -103.478 -> -103

        assert_eq!(y[0], 27, "Y component for pure green");
        assert_eq!(cb[0], -89, "Cb component for pure green");
        assert_eq!(cr[0], -103, "Cr component for pure green");
    }

    #[test]
    fn test_rgb_to_ycbcr_blue() {
        // Test pure blue (0, 0, 255)
        let blue_rgb = [0u8, 0, 255];
        let mut y = [0i8; 1];
        let mut cb = [0i8; 1];
        let mut cr = [0i8; 1];

        rgb_to_ycbcr_planes(&blue_rgb, &mut y, &mut cb, &mut cr);

        // Expected values for pure blue using DjVu coefficients
        // Y = 0.304348*0 + 0.608696*0 + 0.086956*255 = 22.174 -> 22 - 128 = -106
        // Cb = -0.173913*0 - 0.347826*0 + 0.521739*255 = 133.043 -> 127 (clamped)
        // Cr = 0.463768*0 - 0.405797*0 - 0.057971*255 = -14.783 -> -15

        assert_eq!(y[0], -106, "Y component for pure blue");
        assert_eq!(cb[0], 127, "Cb component for pure blue");
        assert_eq!(cr[0], -15, "Cr component for pure blue");
    }

    #[test]
    fn test_rgb_to_ycbcr_white() {
        // Test white (255, 255, 255)
        let white_rgb = [255u8, 255, 255];
        let mut y = [0i8; 1];
        let mut cb = [0i8; 1];
        let mut cr = [0i8; 1];

        rgb_to_ycbcr_planes(&white_rgb, &mut y, &mut cb, &mut cr);

        // Expected values for white (with rounding adjustments for fixed-point math)
        // Y = 0.299*255 + 0.587*255 + 0.114*255 = 255 -> 255 - 128 = 127
        // Cb and Cr should be very close to 0, but may have small rounding errors

        assert_eq!(y[0], 127, "Y component for white");
        assert!(
            cb[0].abs() <= 1,
            "Cb component for white should be close to 0, got {}",
            cb[0]
        );
        assert!(
            cr[0].abs() <= 1,
            "Cr component for white should be close to 0, got {}",
            cr[0]
        );
    }

    #[test]
    fn test_rgb_to_ycbcr_black() {
        // Test black (0, 0, 0)
        let black_rgb = [0u8, 0, 0];
        let mut y = [0i8; 1];
        let mut cb = [0i8; 1];
        let mut cr = [0i8; 1];

        rgb_to_ycbcr_planes(&black_rgb, &mut y, &mut cb, &mut cr);

        // Expected values for black
        // Y = 0 -> 0 - 128 = -128
        // Cb = 0 (close to)
        // Cr = 0 (close to)

        assert_eq!(y[0], -128, "Y component for black");
        assert!(
            cb[0].abs() <= 1,
            "Cb component for black should be close to 0, got {}",
            cb[0]
        );
        assert!(
            cr[0].abs() <= 1,
            "Cr component for black should be close to 0, got {}",
            cr[0]
        );
    }

    #[test]
    fn test_ycbcr_from_rgb_image() {
        // Create a small test image with known colors
        let mut img: RgbImage = ImageBuffer::new(2, 2);

        // Set pixels: red, green, blue, white
        img.put_pixel(0, 0, Rgb([255, 0, 0])); // red
        img.put_pixel(1, 0, Rgb([0, 255, 0])); // green
        img.put_pixel(0, 1, Rgb([0, 0, 255])); // blue
        img.put_pixel(1, 1, Rgb([255, 255, 255])); // white

        let (y_buf, cb_buf, cr_buf) = ycbcr_from_rgb(&img);

        assert_eq!(y_buf.len(), 4);
        assert_eq!(cb_buf.len(), 4);
        assert_eq!(cr_buf.len(), 4);

        // Check red pixel
        assert_eq!(y_buf[0], -50);
        assert_eq!(cb_buf[0], -44);
        assert_eq!(cr_buf[0], 118);

        // Check green pixel
        assert_eq!(y_buf[1], 27);
        assert_eq!(cb_buf[1], -89);
        assert_eq!(cr_buf[1], -103);

        // Check blue pixel
        assert_eq!(y_buf[2], -106);
        assert_eq!(cb_buf[2], 127);
        assert_eq!(cr_buf[2], -15);

        // Check white pixel
        assert_eq!(y_buf[3], 127);
        assert_eq!(cb_buf[3], 0);
        assert_eq!(cr_buf[3], 0);
    }

    #[test]
    fn test_rgb_planes_length_mismatch() {
        let rgb_data = [255u8, 0, 0, 0, 255, 0]; // 2 pixels
        let mut y = [0i8; 1]; // Wrong length
        let mut cb = [0i8; 2];
        let mut cr = [0i8; 2];

        // This should panic due to assertion - testing in a different way to avoid UnwindSafe issues
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            rgb_to_ycbcr_planes(&rgb_data, &mut y, &mut cb, &mut cr);
        }));

        assert!(result.is_err(), "Should panic on length mismatch");
    }

    #[test]
    fn test_rgb_input_not_multiple_of_3() {
        let rgb_data = [255u8, 0]; // Not divisible by 3
        let mut y = [0i8; 1];
        let mut cb = [0i8; 1];
        let mut cr = [0i8; 1];

        // This should panic due to assertion
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            rgb_to_ycbcr_planes(&rgb_data, &mut y, &mut cb, &mut cr);
        }));

        assert!(result.is_err(), "Should panic on invalid RGB data length");
    }

    #[test]
    fn test_encoder_params_default() {
        let params = EncoderParams::default();
        assert_eq!(params.decibels, None);
        assert_eq!(params.slices, Some(74));
        assert!(matches!(params.crcb_mode, CrcbMode::Full));
        assert_eq!(params.db_frac, 0.35);
    }

    #[test]
    fn test_crcb_mode_values() {
        // Test enum variants exist
        let _none = CrcbMode::None;
        let _half = CrcbMode::Half;
        let _normal = CrcbMode::Normal;
        let _full = CrcbMode::Full;

        // Test default
        let default_mode = CrcbMode::default();
        assert!(matches!(default_mode, CrcbMode::None));
    }
}

#[cfg(test)]
mod integration_tests {
    use crate::encode::iw44::encoder::{CrcbMode, EncoderParams, IWEncoder};
    #[cfg(test)]
    use image::{GrayImage, ImageBuffer, Luma, Rgb, RgbImage};

    #[test]
    fn test_encoder_from_grayscale() {
        let img: GrayImage = ImageBuffer::from_fn(32, 32, |x, y| Luma([((x + y) % 256) as u8]));

        let params = EncoderParams {
            decibels: Some(80.0),
            slices: None,
            bytes: None,
            crcb_mode: CrcbMode::None,
            db_frac: 0.35,
            lossless: false,
            quant_multiplier: 1.0,
        };

        let result = IWEncoder::from_gray(&img, None, params);
        assert!(result.is_ok(), "Should create encoder from grayscale image");
    }

    #[test]
    fn test_encoder_from_rgb() {
        let img: RgbImage = ImageBuffer::from_fn(32, 32, |x, y| {
            Rgb([
                ((x * 4) % 256) as u8,
                ((y * 4) % 256) as u8,
                (((x + y) * 2) % 256) as u8,
            ])
        });

        let params = EncoderParams {
            decibels: Some(85.0),
            slices: None,
            bytes: None,
            crcb_mode: CrcbMode::Full,
            db_frac: 0.35,
            lossless: false,
            quant_multiplier: 1.0,
        };

        let result = IWEncoder::from_rgb(&img, None, params);
        assert!(result.is_ok(), "Should create encoder from RGB image");
    }

    #[test]
    fn test_encode_chunk_progression() {
        let img: GrayImage = ImageBuffer::from_fn(64, 64, |x, y| Luma([((x ^ y) % 256) as u8]));

        let params = EncoderParams::default();
        let mut encoder = IWEncoder::from_gray(&img, None, params).unwrap();

        // Encode first chunk
        let (chunk1, has_more1) = encoder.encode_chunk(10).unwrap();
        assert!(!chunk1.is_empty(), "First chunk should not be empty");

        // If there's more data, encode another chunk
        if has_more1 {
            let (chunk2, _has_more2) = encoder.encode_chunk(10).unwrap();
            // Second chunk might be empty if we've encoded all meaningful data
        }
    }
}
