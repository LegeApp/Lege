use std::sync::Arc;

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
        self.pixels.resize(self.stride.saturating_mul(height as usize), 0);
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
        self.clips.push(current.intersection(rect).unwrap_or_default());
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

    /// Nearest-neighbor scaling is sufficient for the draft/stale fallback
    /// path. The software presenter should replace this with bilinear only
    /// after profiling; settled zoom always re-rasterizes exact tiles.
    pub fn blit_scaled(&mut self, source: &PixelSurface, destination: RectI) {
        let requested = destination;
        let Some(destination) = requested.intersection(self.clip()) else {
            return;
        };
        if requested.is_empty() || destination.is_empty() || source.width == 0 || source.height == 0 {
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

    fn clip(&self) -> RectI {
        *self.clips.last().expect("painter always owns a root clip")
    }
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
        return ExposedRegions { rects: vec![canvas] };
    }

    let copy_width = canvas.width - delta_x.unsigned_abs();
    let copy_height = canvas.height - delta_y.unsigned_abs();
    let src_x = if delta_x > 0 { canvas.x } else { canvas.x - delta_x };
    let dst_x = if delta_x > 0 { canvas.x + delta_x } else { canvas.x };
    let src_y = if delta_y > 0 { canvas.y } else { canvas.y - delta_y };
    let dst_y = if delta_y > 0 { canvas.y + delta_y } else { canvas.y };

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
