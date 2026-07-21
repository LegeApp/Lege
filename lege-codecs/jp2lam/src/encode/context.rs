use crate::error::{Jp2LamError, Result};
use crate::model::{ColorSpace, EncodeOptions, Image, ImageView, SamplePrecision};
use crate::plan::EncodingPlan;

pub(crate) struct EncodeContext<'a> {
    pub image: ImageView<'a>,
    pub plan: EncodingPlan,
}

impl<'a> EncodeContext<'a> {
    pub(crate) fn new(image: &'a Image, options: &EncodeOptions) -> Result<Self> {
        Self::new_view(image.as_view()?, options)
    }

    pub(crate) fn new_view(image: ImageView<'a>, options: &EncodeOptions) -> Result<Self> {
        let plan = EncodingPlan::build_view(&image, options)?;
        Ok(Self { image, plan })
    }

    pub(crate) fn load_component_i32(&self, index: usize) -> Result<Vec<i32>> {
        let component = self.image.components.get(index).ok_or_else(|| {
            Jp2LamError::EncodeFailed(format!("missing component {index} samples"))
        })?;
        self.load_component_rect_i32(index, 0, 0, component.width, component.height)
    }

    /// Load one component rectangle into a dense row-major `i32` scratch plane.
    ///
    /// This is the Phase 4 tile-source boundary: future tile-by-tile encoding
    /// should call this with the active tile-component rectangle instead of
    /// loading a full image component. The current single-tile path calls it
    /// with the component's full extent, preserving existing behavior while
    /// making the tile-local data movement explicit and testable.
    pub(crate) fn load_component_rect_i32(
        &self,
        index: usize,
        x0: u32,
        y0: u32,
        width: u32,
        height: u32,
    ) -> Result<Vec<i32>> {
        validate_component_rect(&self.image, index, x0, y0, width, height)?;
        match self.image.colorspace {
            ColorSpace::Gray | ColorSpace::Rgb | ColorSpace::Srgb | ColorSpace::Cmyk => {
                self.load_raw_component_rect_i32(index, x0, y0, width, height)
            }
            ColorSpace::Yuv | ColorSpace::YCbCr => {
                self.load_yuv_family_component_rect_i32(index, x0, y0, width, height)
            }
        }
    }

    fn load_raw_component_rect_i32(
        &self,
        index: usize,
        x0: u32,
        y0: u32,
        width: u32,
        height: u32,
    ) -> Result<Vec<i32>> {
        let component = self.image.components.get(index).ok_or_else(|| {
            Jp2LamError::EncodeFailed(format!("missing component {index} samples"))
        })?;
        let mut out = Vec::with_capacity((width as usize).saturating_mul(height as usize));
        let max_sample = SamplePrecision::new(component.precision)?.unsigned_max();
        for y in y0..y0 + height {
            for x in x0..x0 + width {
                let sample = component.sample_at(x, y).ok_or_else(|| {
                    Jp2LamError::EncodeFailed(format!(
                        "component {index} sample ({x},{y}) is outside backing storage"
                    ))
                })?;
                if !(0..=max_sample).contains(&sample) {
                    return Err(Jp2LamError::InvalidInput(format!(
                        "component {index} contains sample {sample} outside 0..={max_sample}"
                    )));
                }
                out.push(sample);
            }
        }
        Ok(out)
    }

    fn load_yuv_family_component_rect_i32(
        &self,
        index: usize,
        x0: u32,
        y0: u32,
        width: u32,
        height: u32,
    ) -> Result<Vec<i32>> {
        if index > 2 {
            return Err(Jp2LamError::EncodeFailed(format!(
                "YUV-family conversion only has components 0..2, got {index}"
            )));
        }
        let y = self.load_raw_component_rect_i32(0, x0, y0, width, height)?;
        let u = self.load_raw_component_rect_i32(1, x0, y0, width, height)?;
        let v = self.load_raw_component_rect_i32(2, x0, y0, width, height)?;
        if y.len() != u.len() || y.len() != v.len() {
            return Err(Jp2LamError::EncodeFailed(
                "YUV-family component lengths differ".to_string(),
            ));
        }

        let mut out = Vec::with_capacity(y.len());
        for ((&yy, &uu), &vv) in y.iter().zip(u.iter()).zip(v.iter()) {
            let d = uu - 128;
            let e = vv - 128;
            // ITU-R BT.601 full-range integer approximation.
            let rr = yy + ((91881 * e) >> 16);
            let gg = yy - ((22554 * d + 46802 * e) >> 16);
            let bb = yy + ((116130 * d) >> 16);
            out.push(match index {
                0 => clamp_u8_range(rr),
                1 => clamp_u8_range(gg),
                2 => clamp_u8_range(bb),
                _ => unreachable!(),
            });
        }
        Ok(out)
    }
}

fn validate_component_rect(
    image: &ImageView<'_>,
    index: usize,
    x0: u32,
    y0: u32,
    width: u32,
    height: u32,
) -> Result<()> {
    if width == 0 || height == 0 {
        return Err(Jp2LamError::InvalidInput(
            "component load rectangle must be non-empty".to_string(),
        ));
    }
    let component = image
        .components
        .get(index)
        .ok_or_else(|| Jp2LamError::EncodeFailed(format!("missing component {index} samples")))?;
    let x1 = x0.checked_add(width).ok_or_else(|| {
        Jp2LamError::InvalidInput("component load rectangle x range overflows".to_string())
    })?;
    let y1 = y0.checked_add(height).ok_or_else(|| {
        Jp2LamError::InvalidInput("component load rectangle y range overflows".to_string())
    })?;
    if x1 > component.width || y1 > component.height {
        return Err(Jp2LamError::InvalidInput(format!(
            "component {index} load rectangle ({x0},{y0}) {width}x{height} exceeds component extent {}x{}",
            component.width, component.height
        )));
    }
    Ok(())
}

#[inline]
fn clamp_u8_range(value: i32) -> i32 {
    value.clamp(0, 255)
}

#[cfg(test)]
mod tests {
    use super::EncodeContext;
    use crate::model::{
        ColorSpace, Component, EncodeOptions, Image, ImageView, OutputFormat, Preset,
    };
    use crate::plan::{EncodeLane, QuantizationStyle};

    #[test]
    fn context_exposes_plan_and_loads_samples() {
        let image = Image {
            width: 2,
            height: 2,
            components: vec![Component {
                data: vec![1, 2, 3, 4],
                width: 2,
                height: 2,
                precision: 8,
                signed: false,
                dx: 1,
                dy: 1,
            }],
            colorspace: ColorSpace::Gray,
        };
        let context = EncodeContext::new(
            &image,
            &EncodeOptions {
                quality: Preset::DocumentHigh.quality(),
                format: OutputFormat::J2k,
                profile: Default::default(),
                ..Default::default()
            },
        )
        .expect("build context");
        assert_eq!(context.plan.lane, EncodeLane::GrayLossy);
        assert_eq!(
            context.plan.quantization_style,
            QuantizationStyle::ScalarExpounded
        );
        assert_eq!(context.load_component_i32(0).unwrap(), vec![1, 2, 3, 4]);
        assert!(context.load_component_i32(1).is_err());
    }

    #[test]
    fn context_loads_interleaved_rgb_view_without_source_plane_clone() {
        let data = [
            1u8, 2, 3, //
            4, 5, 6,
        ];
        let view = ImageView::from_rgb8_interleaved(2, 1, &data).expect("rgb view");
        let context = EncodeContext::new_view(
            view,
            &EncodeOptions {
                quality: 100,
                format: OutputFormat::J2k,
                profile: Default::default(),
                ..Default::default()
            },
        )
        .expect("build context");

        assert_eq!(context.load_component_i32(0).unwrap(), vec![1, 4]);
        assert_eq!(context.load_component_i32(1).unwrap(), vec![2, 5]);
        assert_eq!(context.load_component_i32(2).unwrap(), vec![3, 6]);
    }

    #[test]
    fn context_loads_interleaved_rgb_tile_rect_without_full_component_clone() {
        let data = [
            1u8, 2, 3, //
            4, 5, 6, //
            7, 8, 9, //
            10, 11, 12, //
            13, 14, 15, //
            16, 17, 18,
        ];
        let view = ImageView::from_rgb8_interleaved(3, 2, &data).expect("rgb view");
        let context = EncodeContext::new_view(
            view,
            &EncodeOptions {
                quality: 100,
                format: OutputFormat::J2k,
                profile: Default::default(),
                ..Default::default()
            },
        )
        .expect("build context");

        assert_eq!(
            context.load_component_rect_i32(0, 1, 0, 2, 2).unwrap(),
            vec![4, 7, 13, 16]
        );
        assert_eq!(
            context.load_component_rect_i32(1, 1, 0, 2, 2).unwrap(),
            vec![5, 8, 14, 17]
        );
    }

    #[test]
    fn context_rejects_empty_and_out_of_bounds_tile_rects() {
        let data = [1u8, 2, 3, 4];
        let view = ImageView::from_gray8(2, 2, &data).expect("gray view");
        let context =
            EncodeContext::new_view(view, &EncodeOptions::default()).expect("build context");

        let empty = context
            .load_component_rect_i32(0, 0, 0, 0, 1)
            .expect_err("empty tile-component rect should fail");
        assert!(empty.to_string().contains("non-empty"), "{empty}");

        let oob = context
            .load_component_rect_i32(0, 1, 1, 2, 1)
            .expect_err("out-of-bounds tile-component rect should fail");
        assert!(
            oob.to_string().contains("exceeds component extent"),
            "{oob}"
        );
    }

    #[test]
    fn context_rejects_out_of_range_samples_while_loading() {
        let image = Image {
            width: 1,
            height: 1,
            components: vec![Component {
                data: vec![300],
                width: 1,
                height: 1,
                precision: 8,
                signed: false,
                dx: 1,
                dy: 1,
            }],
            colorspace: ColorSpace::Gray,
        };

        let context = EncodeContext::new(&image, &EncodeOptions::default()).expect("build context");
        let err = context
            .load_component_i32(0)
            .expect_err("sample range should be checked while loading");
        assert!(err.to_string().contains("outside 0..=255"), "{err}");
    }

    #[test]
    fn context_rejects_ambiguous_ycbcr_input() {
        let image = Image {
            width: 2,
            height: 1,
            components: vec![
                Component {
                    data: vec![64, 200],
                    width: 2,
                    height: 1,
                    precision: 8,
                    signed: false,
                    dx: 1,
                    dy: 1,
                },
                Component {
                    data: vec![128, 90],
                    width: 2,
                    height: 1,
                    precision: 8,
                    signed: false,
                    dx: 1,
                    dy: 1,
                },
                Component {
                    data: vec![128, 170],
                    width: 2,
                    height: 1,
                    precision: 8,
                    signed: false,
                    dx: 1,
                    dy: 1,
                },
            ],
            colorspace: ColorSpace::YCbCr,
        };

        let error = EncodeContext::new(
            &image,
            &EncodeOptions {
                quality: Preset::DocumentHigh.quality(),
                format: OutputFormat::J2k,
                profile: Default::default(),
                ..Default::default()
            },
        )
        .err()
        .expect("ambiguous YCbCr must be rejected");
        assert!(error.to_string().contains("not an advertised photographic input"));
    }

    #[test]
    fn context_rejects_ambiguous_ycbcr_before_tile_loading() {
        let image = Image {
            width: 3,
            height: 2,
            components: vec![
                Component {
                    data: vec![10, 20, 30, 40, 50, 60],
                    width: 3,
                    height: 2,
                    precision: 8,
                    signed: false,
                    dx: 1,
                    dy: 1,
                },
                Component {
                    data: vec![128; 6],
                    width: 3,
                    height: 2,
                    precision: 8,
                    signed: false,
                    dx: 1,
                    dy: 1,
                },
                Component {
                    data: vec![128; 6],
                    width: 3,
                    height: 2,
                    precision: 8,
                    signed: false,
                    dx: 1,
                    dy: 1,
                },
            ],
            colorspace: ColorSpace::YCbCr,
        };

        let error = EncodeContext::new(
            &image,
            &EncodeOptions {
                quality: 100,
                format: OutputFormat::J2k,
                profile: Default::default(),
                ..Default::default()
            },
        )
        .err()
        .expect("ambiguous YCbCr must be rejected before tile loading");
        assert!(error.to_string().contains("not an advertised photographic input"));
    }
}
