// src/geom.rs

//! Geometric primitives for 2D rectangles and coordinate transformations.
//!
//! This module provides a safe and idiomatic Rust implementation of the C++
//! `GRect` and `GRectMapper` classes.

use crate::utils::error::{DjvuError, Result};
use std::mem;

/// Represents a 2D rectangle with integer coordinates.
///
/// The rectangle is defined by its top-left corner (`x`, `y`) and its `width` and `height`.
/// This struct is `Copy`, so it can be passed around cheaply by value.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    /// Creates a new rectangle.
    pub fn new(x: i32, y: i32, width: u32, height: u32) -> Self {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    /// Creates an empty rectangle.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Returns the x-coordinate of the right edge (`x + width`).
    pub fn x_max(&self) -> i32 {
        self.x.saturating_add(self.width as i32)
    }

    /// Returns the y-coordinate of the bottom edge (`y + height`).
    pub fn y_max(&self) -> i32 {
        self.y.saturating_add(self.height as i32)
    }

    /// Checks if the rectangle has zero width or height.
    pub fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }

    /// Checks if a point is contained within the rectangle's bounds.
    /// The right and bottom edges are exclusive.
    pub fn contains(&self, px: i32, py: i32) -> bool {
        !self.is_empty() && px >= self.x && px < self.x_max() && py >= self.y && py < self.y_max()
    }

    /// Returns a new rectangle that is the intersection of `self` and `other`.
    pub fn intersection(&self, other: &Rect) -> Rect {
        if self.is_empty() || other.is_empty() {
            return Rect::empty();
        }

        let x = self.x.max(other.x);
        let y = self.y.max(other.y);

        let x_max = self.x_max().min(other.x_max());
        let y_max = self.y_max().min(other.y_max());

        if x >= x_max || y >= y_max {
            Rect::empty()
        } else {
            Rect::new(x, y, (x_max - x) as u32, (y_max - y) as u32)
        }
    }

    /// Returns a new rectangle that is the smallest bounding box containing both `self` and `other`.
    pub fn union(&self, other: &Rect) -> Rect {
        if self.is_empty() {
            return *other;
        }
        if other.is_empty() {
            return *self;
        }

        let x = self.x.min(other.x);
        let y = self.y.min(other.y);

        let x_max = self.x_max().max(other.x_max());
        let y_max = self.y_max().max(other.y_max());

        Rect::new(x, y, (x_max - x) as u32, (y_max - y) as u32)
    }

    /// Returns a new rectangle translated by `(dx, dy)`.
    pub fn translate(&self, dx: i32, dy: i32) -> Rect {
        if self.is_empty() {
            Rect::empty()
        } else {
            Rect::new(
                self.x.saturating_add(dx),
                self.y.saturating_add(dy),
                self.width,
                self.height,
            )
        }
    }

    /// Returns a new rectangle with size adjusted by `(dx, dy)` on each side.
    pub fn inflate(&self, dx: i32, dy: i32) -> Rect {
        if self.is_empty() {
            return Rect::empty();
        }
        let new_x = self.x.saturating_sub(dx);
        let new_y = self.y.saturating_sub(dy);
        let new_width = self.width as i32 + 2 * dx;
        let new_height = self.height as i32 + 2 * dy;

        if new_width <= 0 || new_height <= 0 {
            Rect::empty()
        } else {
            Rect::new(new_x, new_y, new_width as u32, new_height as u32)
        }
    }
}

/// Represents a rational number (p/q) for precise scaling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Ratio {
    p: i64,
    q: i64,
}

impl Ratio {
    fn new(p: i32, q: i32) -> Result<Self> {
        if q == 0 {
            return Err(DjvuError::InvalidArg(
                "Ratio denominator cannot be zero.".to_string(),
            ));
        }
        let mut p_64 = p as i64;
        let mut q_64 = q as i64;

        if p_64 == 0 {
            return Ok(Ratio { p: 0, q: 1 });
        }
        if q_64 < 0 {
            p_64 = -p_64;
            q_64 = -q_64;
        }
        let common = gcd(p_64.abs(), q_64);
        Ok(Ratio {
            p: p_64 / common,
            q: q_64 / common,
        })
    }
}

fn gcd(a: i64, b: i64) -> i64 {
    if b == 0 {
        a
    } else {
        gcd(b, a % b)
    }
}

/// Maps points and rectangles between an input and an output coordinate space.
#[derive(Debug, Clone)]
pub struct RectMapper {
    from: Rect,
    to: Rect,
    code: u8, // bitflags: 1=MIRRORX, 2=MIRRORY, 4=SWAPXY
    ratio_w: Ratio,
    ratio_h: Ratio,
}

impl RectMapper {
    const MIRRORX: u8 = 1;
    const MIRRORY: u8 = 2;
    const SWAPXY: u8 = 4;

    /// Creates a default mapper (1:1 mapping of a 1x1 rectangle).
    pub fn new() -> Self {
        RectMapper {
            from: Rect::new(0, 0, 1, 1),
            to: Rect::new(0, 0, 1, 1),
            code: 0,
            ratio_w: Ratio { p: 1, q: 1 },
            ratio_h: Ratio { p: 1, q: 1 },
        }
    }

    pub fn set_input(&mut self, rect: Rect) -> Result<()> {
        if rect.is_empty() {
            return Err(DjvuError::InvalidArg(
                "Input rectangle cannot be empty.".to_string(),
            ));
        }
        self.from = rect;
        self.recalculate_ratios()?;
        Ok(())
    }

    pub fn set_output(&mut self, rect: Rect) -> Result<()> {
        if rect.is_empty() {
            return Err(DjvuError::InvalidArg(
                "Output rectangle cannot be empty.".to_string(),
            ));
        }
        self.to = rect;
        self.recalculate_ratios()?;
        Ok(())
    }

    /// Rotates the mapping by a multiple of 90 degrees counter-clockwise.
    pub fn rotate(&mut self, count: i32) -> Result<()> {
        match count.rem_euclid(4) {
            1 => {
                let mirror_flag = if (self.code & Self::SWAPXY) != 0 {
                    Self::MIRRORY
                } else {
                    Self::MIRRORX
                };
                self.code ^= mirror_flag;
                self.code ^= Self::SWAPXY;
            }
            2 => self.code ^= Self::MIRRORX | Self::MIRRORY,
            3 => {
                let mirror_flag = if (self.code & Self::SWAPXY) != 0 {
                    Self::MIRRORX
                } else {
                    Self::MIRRORY
                };
                self.code ^= mirror_flag;
                self.code ^= Self::SWAPXY;
            }
            _ => {}
        }
        self.recalculate_ratios()
    }

    fn recalculate_ratios(&mut self) -> Result<()> {
        let from_w = if (self.code & Self::SWAPXY) != 0 {
            self.from.height
        } else {
            self.from.width
        };
        let from_h = if (self.code & Self::SWAPXY) != 0 {
            self.from.width
        } else {
            self.from.height
        };

        self.ratio_w = Ratio::new(self.to.width as i32, from_w as i32)?;
        self.ratio_h = Ratio::new(self.to.height as i32, from_h as i32)?;
        Ok(())
    }

    /// Maps a point from the input space to the output space.
    pub fn map(&self, x: i32, y: i32) -> (i32, i32) {
        let (mut mx, mut my) = (x as i64, y as i64);

        if (self.code & Self::SWAPXY) != 0 {
            mem::swap(&mut mx, &mut my);
        }

        let from_w = if (self.code & Self::SWAPXY) != 0 {
            self.from.height
        } else {
            self.from.width
        } as i64;
        let from_h = if (self.code & Self::SWAPXY) != 0 {
            self.from.width
        } else {
            self.from.height
        } as i64;
        let from_x = if (self.code & Self::SWAPXY) != 0 {
            self.from.y
        } else {
            self.from.x
        } as i64;
        let from_y = if (self.code & Self::SWAPXY) != 0 {
            self.from.x
        } else {
            self.from.y
        } as i64;

        if (self.code & Self::MIRRORX) != 0 {
            mx = from_x + (from_x + from_w) - mx;
        }
        if (self.code & Self::MIRRORY) != 0 {
            my = from_y + (from_y + from_h) - my;
        }

        let out_x = self.to.x as i64
            + ((mx - from_x) * self.ratio_w.p + self.ratio_w.q / 2) / self.ratio_w.q;
        let out_y = self.to.y as i64
            + ((my - from_y) * self.ratio_h.p + self.ratio_h.q / 2) / self.ratio_h.q;

        (out_x as i32, out_y as i32)
    }

    /// Maps a rectangle from the input space to the output space.
    pub fn map_rect(&self, rect: Rect) -> Rect {
        let (x1, y1) = self.map(rect.x, rect.y);
        let (x2, y2) = self.map(rect.x_max(), rect.y_max());

        Rect::new(
            x1.min(x2),
            y1.min(y2),
            (x1 - x2).abs() as u32,
            (y1 - y2).abs() as u32,
        )
    }
}
