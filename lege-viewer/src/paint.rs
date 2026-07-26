use std::sync::{Arc, OnceLock};

use crate::geometry::{PointI, RectI};

#[derive(Debug, Clone)]
pub struct WindowBuffer {
    pub width: u32,
    pub height: u32,
    pub stride: usize,
    pub pixels: Vec<u32>,
}

impl WindowBuffer {
    pub fn new(width: u32, height: u32) -> Self {
        let mut buffer = Self {
            width: 0,
            height: 0,
            stride: 0,
            pixels: Vec::new(),
        };
        buffer.resize(width, height);
        buffer
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        self.width = width;
        self.height = height;
        self.stride = width as usize;
        self.pixels
            .resize(self.stride.saturating_mul(height as usize), 0);
    }

    pub fn bounds(&self) -> RectI {
        RectI {
            x: 0,
            y: 0,
            width: self.width,
            height: self.height,
        }
    }
}

#[derive(Debug, Clone)]
pub struct PixelSurface {
    pub width: u32,
    pub height: u32,
    pub stride: usize,
    pub pixels: Arc<[u32]>,
}

impl PixelSurface {
    pub fn byte_len(&self) -> u64 {
        (self.pixels.len() * std::mem::size_of::<u32>()) as u64
    }
}

#[derive(Debug)]
pub struct Painter<'a> {
    target: &'a mut WindowBuffer,
    clips: Vec<RectI>,
}

impl<'a> Painter<'a> {
    pub fn new(target: &'a mut WindowBuffer) -> Self {
        let bounds = target.bounds();
        Self {
            target,
            clips: vec![bounds],
        }
    }

    pub fn clear(&mut self, color: u32) {
        self.target.pixels.fill(color);
    }

    pub fn push_clip(&mut self, rect: RectI) {
        let current = self.clip();
        self.clips
            .push(current.intersection(rect).unwrap_or_default());
    }

    pub fn pop_clip(&mut self) {
        if self.clips.len() > 1 {
            self.clips.pop();
        }
    }

    pub fn fill_rect(&mut self, rect: RectI, color: u32) {
        let Some(rect) = rect.intersection(self.clip()) else {
            return;
        };
        for y in rect.y..rect.bottom() {
            let row = y as usize * self.target.stride;
            let start = row + rect.x as usize;
            let end = start + rect.width as usize;
            self.target.pixels[start..end].fill(color);
        }
    }

    pub fn blend_rect(&mut self, rect: RectI, argb: u32) {
        let Some(rect) = rect.intersection(self.clip()) else {
            return;
        };
        let alpha = (argb >> 24) & 0xff;
        if alpha == 0 {
            return;
        }
        if alpha == 255 {
            self.fill_rect(rect, argb & 0x00ff_ffff);
            return;
        }
        for y in rect.y..rect.bottom() {
            let row = y as usize * self.target.stride;
            for x in rect.x..rect.right() {
                let index = row + x as usize;
                self.target.pixels[index] =
                    blend_xrgb(self.target.pixels[index], argb & 0x00ff_ffff, alpha);
            }
        }
    }

    pub fn stroke_rect(&mut self, rect: RectI, width: u32, color: u32) {
        if width == 0 || rect.is_empty() {
            return;
        }
        self.fill_rect(
            RectI {
                height: width.min(rect.height),
                ..rect
            },
            color,
        );
        self.fill_rect(
            RectI {
                y: rect.bottom().saturating_sub(width as i32),
                height: width.min(rect.height),
                ..rect
            },
            color,
        );
        self.fill_rect(
            RectI {
                width: width.min(rect.width),
                ..rect
            },
            color,
        );
        self.fill_rect(
            RectI {
                x: rect.right().saturating_sub(width as i32),
                width: width.min(rect.width),
                ..rect
            },
            color,
        );
    }

    pub fn blit_opaque(&mut self, source: &PixelSurface, source_rect: RectI, destination: PointI) {
        let source_bounds = RectI {
            x: 0,
            y: 0,
            width: source.width,
            height: source.height,
        };
        let Some(source_rect) = source_rect.intersection(source_bounds) else {
            return;
        };
        let destination_rect = RectI {
            x: destination.x,
            y: destination.y,
            width: source_rect.width,
            height: source_rect.height,
        };
        let Some(clipped_destination) = destination_rect.intersection(self.clip()) else {
            return;
        };
        let offset_x = clipped_destination.x - destination_rect.x;
        let offset_y = clipped_destination.y - destination_rect.y;
        for row_index in 0..clipped_destination.height as usize {
            let src_y = source_rect.y as usize + offset_y as usize + row_index;
            let src_x = source_rect.x as usize + offset_x as usize;
            let src_start = src_y * source.stride + src_x;
            let src_end = src_start + clipped_destination.width as usize;

            let dst_y = clipped_destination.y as usize + row_index;
            let dst_x = clipped_destination.x as usize;
            let dst_start = dst_y * self.target.stride + dst_x;
            let dst_end = dst_start + clipped_destination.width as usize;
            self.target.pixels[dst_start..dst_end]
                .copy_from_slice(&source.pixels[src_start..src_end]);
        }
    }

    /// Nearest-neighbor scaling is used for exact-bucket, one-to-one tiles.
    pub fn blit_scaled(&mut self, source: &PixelSurface, destination: RectI) {
        let requested = destination;
        let Some(destination) = requested.intersection(self.clip()) else {
            return;
        };
        if requested.is_empty() || destination.is_empty() || source.width == 0 || source.height == 0
        {
            return;
        }
        let offset_x = (destination.x - requested.x) as u32;
        let offset_y = (destination.y - requested.y) as u32;
        for dy in 0..destination.height {
            let requested_y = offset_y + dy;
            let sy = (u64::from(requested_y) * u64::from(source.height)
                / u64::from(requested.height))
            .min(u64::from(source.height - 1)) as usize;
            for dx in 0..destination.width {
                let requested_x = offset_x + dx;
                let sx = (u64::from(requested_x) * u64::from(source.width)
                    / u64::from(requested.width))
                .min(u64::from(source.width - 1)) as usize;
                let src = source.pixels[sy * source.stride + sx];
                let x = destination.x as usize + dx as usize;
                let y = destination.y as usize + dy as usize;
                self.target.pixels[y * self.target.stride + x] = src;
            }
        }
    }

    pub fn blit_scaled_linear(&mut self, source: &PixelSurface, destination: RectI) {
        let requested = destination;
        let Some(destination) = requested.intersection(self.clip()) else {
            return;
        };
        if requested.is_empty() || destination.is_empty() || source.width == 0 || source.height == 0
        {
            return;
        }
        let scale_x = f64::from(source.width) / f64::from(requested.width);
        let scale_y = f64::from(source.height) / f64::from(requested.height);
        for dy in 0..destination.height {
            let requested_y = f64::from(destination.y - requested.y) + f64::from(dy);
            let source_y =
                ((requested_y + 0.5) * scale_y - 0.5).clamp(0.0, f64::from(source.height - 1));
            let y0 = source_y.floor() as usize;
            let y1 = (y0 + 1).min(source.height as usize - 1);
            let fy = source_y - y0 as f64;
            for dx in 0..destination.width {
                let requested_x = f64::from(destination.x - requested.x) + f64::from(dx);
                let source_x =
                    ((requested_x + 0.5) * scale_x - 0.5).clamp(0.0, f64::from(source.width - 1));
                let x0 = source_x.floor() as usize;
                let x1 = (x0 + 1).min(source.width as usize - 1);
                let fx = source_x - x0 as f64;
                let top = lerp_xrgb(
                    source.pixels[y0 * source.stride + x0],
                    source.pixels[y0 * source.stride + x1],
                    fx,
                );
                let bottom = lerp_xrgb(
                    source.pixels[y1 * source.stride + x0],
                    source.pixels[y1 * source.stride + x1],
                    fx,
                );
                let pixel = lerp_xrgb(top, bottom, fy);
                let x = destination.x as usize + dx as usize;
                let y = destination.y as usize + dy as usize;
                self.target.pixels[y * self.target.stride + x] = pixel;
            }
        }
    }

    fn clip(&self) -> RectI {
        self.clips.last().copied().unwrap_or_default()
    }
}

fn lerp_xrgb(left: u32, right: u32, amount: f64) -> u32 {
    let channel = |shift: u32| {
        let left = f64::from((left >> shift) & 0xff_u32);
        let right = f64::from((right >> shift) & 0xff_u32);
        (left + (right - left) * amount).round().clamp(0.0, 255.0) as u32
    };
    (channel(16) << 16) | (channel(8) << 8) | channel(0)
}

fn blend_xrgb(destination: u32, source: u32, alpha: u32) -> u32 {
    let inverse = 255 - alpha;
    let channel = |shift: u32| {
        let source = srgb_to_linear((source >> shift) & 0xff);
        let destination = srgb_to_linear((destination >> shift) & 0xff);
        let linear = (u32::from(source) * alpha + u32::from(destination) * inverse + 127) / 255;
        u32::from(linear_to_srgb(linear as u16))
    };
    (channel(16) << 16) | (channel(8) << 8) | channel(0)
}

fn srgb_to_linear(channel: u32) -> u16 {
    static TABLE: OnceLock<[u16; 256]> = OnceLock::new();
    TABLE.get_or_init(|| {
        std::array::from_fn(|index| {
            let srgb = index as f64 / 255.0;
            let linear = if srgb <= 0.04045 {
                srgb / 12.92
            } else {
                ((srgb + 0.055) / 1.055).powf(2.4)
            };
            (linear * 65535.0).round() as u16
        })
    })[channel.min(255) as usize]
}

fn linear_to_srgb(channel: u16) -> u8 {
    static TABLE: OnceLock<Box<[u8]>> = OnceLock::new();
    TABLE.get_or_init(|| {
        (0..=u16::MAX)
            .map(|value| {
                let linear = f64::from(value) / 65535.0;
                let srgb = if linear <= 0.003_130_8 {
                    linear * 12.92
                } else {
                    1.055 * linear.powf(1.0 / 2.4) - 0.055
                };
                (srgb * 255.0).round().clamp(0.0, 255.0) as u8
            })
            .collect()
    })[usize::from(channel)]
}

#[derive(Debug, Default, Clone)]
pub struct ExposedRegions {
    pub rects: Vec<RectI>,
}

pub fn scroll_blit(
    buffer: &mut WindowBuffer,
    canvas: RectI,
    delta_x: i32,
    delta_y: i32,
) -> ExposedRegions {
    let Some(canvas) = canvas.intersection(buffer.bounds()) else {
        return ExposedRegions::default();
    };
    if delta_x == 0 && delta_y == 0 {
        return ExposedRegions::default();
    }
    if delta_x.unsigned_abs() >= canvas.width || delta_y.unsigned_abs() >= canvas.height {
        return ExposedRegions {
            rects: vec![canvas],
        };
    }

    let copy_width = canvas.width - delta_x.unsigned_abs();
    let copy_height = canvas.height - delta_y.unsigned_abs();
    let src_x = if delta_x > 0 {
        canvas.x
    } else {
        canvas.x - delta_x
    };
    let dst_x = if delta_x > 0 {
        canvas.x + delta_x
    } else {
        canvas.x
    };
    let src_y = if delta_y > 0 {
        canvas.y
    } else {
        canvas.y - delta_y
    };
    let dst_y = if delta_y > 0 {
        canvas.y + delta_y
    } else {
        canvas.y
    };

    let mut copy_row = |row: u32| {
        let src_start = (src_y as usize + row as usize) * buffer.stride + src_x as usize;
        let src_end = src_start + copy_width as usize;
        let dst_start = (dst_y as usize + row as usize) * buffer.stride + dst_x as usize;
        buffer.pixels.copy_within(src_start..src_end, dst_start);
    };
    if dst_y > src_y {
        for row in (0..copy_height).rev() {
            copy_row(row);
        }
    } else {
        for row in 0..copy_height {
            copy_row(row);
        }
    }

    let mut exposed = Vec::with_capacity(2);
    if delta_y > 0 {
        exposed.push(RectI {
            x: canvas.x,
            y: canvas.y,
            width: canvas.width,
            height: delta_y as u32,
        });
    } else if delta_y < 0 {
        exposed.push(RectI {
            x: canvas.x,
            y: canvas.bottom() + delta_y,
            width: canvas.width,
            height: (-delta_y) as u32,
        });
    }
    if delta_x > 0 {
        exposed.push(RectI {
            x: canvas.x,
            y: canvas.y,
            width: delta_x as u32,
            height: canvas.height,
        });
    } else if delta_x < 0 {
        exposed.push(RectI {
            x: canvas.right() + delta_x,
            y: canvas.y,
            width: (-delta_x) as u32,
            height: canvas.height,
        });
    }
    ExposedRegions { rects: exposed }
}

pub fn scroll_exposed_regions(canvas: RectI, delta_x: i32, delta_y: i32) -> ExposedRegions {
    if delta_x == 0 && delta_y == 0 {
        return ExposedRegions::default();
    }
    if delta_x.unsigned_abs() >= canvas.width || delta_y.unsigned_abs() >= canvas.height {
        return ExposedRegions {
            rects: vec![canvas],
        };
    }
    let mut exposed = Vec::with_capacity(2);
    if delta_y > 0 {
        exposed.push(RectI {
            x: canvas.x,
            y: canvas.y,
            width: canvas.width,
            height: delta_y as u32,
        });
    } else if delta_y < 0 {
        exposed.push(RectI {
            x: canvas.x,
            y: canvas.bottom() + delta_y,
            width: canvas.width,
            height: (-delta_y) as u32,
        });
    }
    if delta_x > 0 {
        exposed.push(RectI {
            x: canvas.x,
            y: canvas.y,
            width: delta_x as u32,
            height: canvas.height,
        });
    } else if delta_x < 0 {
        exposed.push(RectI {
            x: canvas.right() + delta_x,
            y: canvas.y,
            width: (-delta_x) as u32,
            height: canvas.height,
        });
    }
    ExposedRegions { rects: exposed }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linear_scaling_interpolates_xrgb_channels() {
        let source = PixelSurface {
            width: 2,
            height: 1,
            stride: 2,
            pixels: vec![0x00ff_0000, 0x0000_00ff].into(),
        };
        let mut target = WindowBuffer::new(4, 1);
        Painter::new(&mut target).blit_scaled_linear(
            &source,
            RectI {
                x: 0,
                y: 0,
                width: 4,
                height: 1,
            },
        );
        assert_eq!(
            target.pixels,
            [0x00ff_0000, 0x00bf_0040, 0x0040_00bf, 0x0000_00ff]
        );
    }

    #[test]
    fn alpha_blending_matches_linear_gpu_composition() {
        let blended = blend_xrgb(0x0000_0000, 0x00ff_ffff, 128);
        for shift in [0, 8, 16] {
            let channel = (blended >> shift) & 0xff;
            assert!((187..=188).contains(&channel), "{blended:08x}");
        }
    }
}
