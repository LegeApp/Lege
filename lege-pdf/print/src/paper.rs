//! Paper, orientation, margins, and the rectangle type the imposition maths
//! works in.
//!
//! Everything here is in PostScript points (1/72 inch), which is the unit the
//! PDF page boxes and both target spooler APIs agree on.

use crate::PrintError;

/// Points per inch.
pub const POINTS_PER_INCH: f64 = 72.0;
/// Points per millimetre.
pub const POINTS_PER_MM: f64 = POINTS_PER_INCH / 25.4;

/// The conservative hardware margin assumed when the driver will not say:
/// 6.35 mm, a quarter inch, on every edge.
///
/// Assuming zero is the classic cause of clipped printouts, so the default
/// errs inward and callers with real driver numbers override it.
pub const DEFAULT_HARDWARE_MARGIN_PT: f64 = 6.35 * POINTS_PER_MM;

/// An axis-aligned rectangle in points, `y` up, as PDF user space has it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x0: f64,
    pub y0: f64,
    pub x1: f64,
    pub y1: f64,
}

impl Rect {
    pub const ZERO: Self = Self {
        x0: 0.0,
        y0: 0.0,
        x1: 0.0,
        y1: 0.0,
    };

    #[must_use]
    pub const fn new(x0: f64, y0: f64, x1: f64, y1: f64) -> Self {
        Self { x0, y0, x1, y1 }
    }

    /// A rectangle from a corner and a size.
    #[must_use]
    pub fn from_size(x: f64, y: f64, width: f64, height: f64) -> Self {
        Self::new(x, y, x + width, y + height)
    }

    /// Normalize so `x0 <= x1` and `y0 <= y1`.
    #[must_use]
    pub fn normalized(self) -> Self {
        Self {
            x0: self.x0.min(self.x1),
            y0: self.y0.min(self.y1),
            x1: self.x0.max(self.x1),
            y1: self.y0.max(self.y1),
        }
    }

    #[must_use]
    pub fn width(self) -> f64 {
        self.x1 - self.x0
    }

    #[must_use]
    pub fn height(self) -> f64 {
        self.y1 - self.y0
    }

    #[must_use]
    pub fn is_empty(self) -> bool {
        !(self.width() > 0.0 && self.height() > 0.0)
    }

    #[must_use]
    pub fn center(self) -> (f64, f64) {
        ((self.x0 + self.x1) / 2.0, (self.y0 + self.y1) / 2.0)
    }

    /// Shrink by `dx` horizontally and `dy` vertically on every side.
    #[must_use]
    pub fn inset(self, dx: f64, dy: f64) -> Self {
        Self {
            x0: self.x0 + dx,
            y0: self.y0 + dy,
            x1: self.x1 - dx,
            y1: self.y1 - dy,
        }
    }

    #[must_use]
    pub fn intersect(self, other: Self) -> Self {
        Self {
            x0: self.x0.max(other.x0),
            y0: self.y0.max(other.y0),
            x1: self.x1.min(other.x1),
            y1: self.y1.min(other.y1),
        }
    }

    /// Whether `self` lies inside `other`, allowing `epsilon` of float slop.
    #[must_use]
    pub fn contained_by(self, other: Self, epsilon: f64) -> bool {
        self.x0 >= other.x0 - epsilon
            && self.y0 >= other.y0 - epsilon
            && self.x1 <= other.x1 + epsilon
            && self.y1 <= other.y1 + epsilon
    }

    /// Whether two rectangles share any area beyond `epsilon`.
    #[must_use]
    pub fn overlaps(self, other: Self, epsilon: f64) -> bool {
        let overlap = self.intersect(other);
        overlap.width() > epsilon && overlap.height() > epsilon
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Orientation {
    Portrait,
    Landscape,
    /// Rotate each page whichever way fits more of it on the sheet.
    #[default]
    Auto,
}

/// Margins in points, one per paper edge.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Margins {
    pub left: f64,
    pub right: f64,
    pub top: f64,
    pub bottom: f64,
}

impl Margins {
    pub const ZERO: Self = Self {
        left: 0.0,
        right: 0.0,
        top: 0.0,
        bottom: 0.0,
    };

    #[must_use]
    pub const fn uniform(points: f64) -> Self {
        Self {
            left: points,
            right: points,
            top: points,
            bottom: points,
        }
    }

    #[must_use]
    pub fn millimetres(mm: f64) -> Self {
        Self::uniform(mm * POINTS_PER_MM)
    }

    #[must_use]
    pub fn inches(inches: f64) -> Self {
        Self::uniform(inches * POINTS_PER_INCH)
    }

    /// The larger of `self` and `other` on each edge.
    ///
    /// This is how user margins and hardware margins combine: a user margin
    /// narrower than the printer's unprintable border is silently widened to
    /// it, because the alternative is content that vanishes on paper.
    #[must_use]
    pub fn max_with(self, other: Self) -> Self {
        Self {
            left: self.left.max(other.left),
            right: self.right.max(other.right),
            top: self.top.max(other.top),
            bottom: self.bottom.max(other.bottom),
        }
    }

    /// Rotate the margins with the sheet: a quarter turn counter-clockwise
    /// sends the left edge to the bottom.
    #[must_use]
    pub fn rotated_quarter_turns(self, turns: i32) -> Self {
        match turns.rem_euclid(4) {
            0 => self,
            1 => Self {
                left: self.top,
                top: self.right,
                right: self.bottom,
                bottom: self.left,
            },
            2 => Self {
                left: self.right,
                right: self.left,
                top: self.bottom,
                bottom: self.top,
            },
            _ => Self {
                left: self.bottom,
                bottom: self.right,
                right: self.top,
                top: self.left,
            },
        }
    }

    /// Mirror left and right. Duplex long-edge binding needs this on back
    /// sides so that the binding margin stays on the binding side.
    #[must_use]
    pub fn mirrored_horizontally(self) -> Self {
        Self {
            left: self.right,
            right: self.left,
            ..self
        }
    }

    /// Mirror top and bottom, for short-edge duplex binding.
    #[must_use]
    pub fn mirrored_vertically(self) -> Self {
        Self {
            top: self.bottom,
            bottom: self.top,
            ..self
        }
    }

    pub fn validate(&self) -> Result<(), PrintError> {
        for (name, value) in [
            ("left", self.left),
            ("right", self.right),
            ("top", self.top),
            ("bottom", self.bottom),
        ] {
            if !value.is_finite() || value < 0.0 {
                return Err(PrintError::InvalidOptions(format!(
                    "{name} margin {value} is not a non-negative finite number"
                )));
            }
        }
        Ok(())
    }
}

impl Default for Margins {
    fn default() -> Self {
        Self::ZERO
    }
}

/// A sheet of paper, either a named standard size or explicit dimensions.
///
/// Named sizes carry their *portrait* extent; [`PaperSize::size`] returns it
/// that way and callers swap for landscape.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub enum PaperSize {
    A3,
    A4,
    A5,
    A6,
    B5,
    Letter,
    Legal,
    Tabloid,
    Executive,
    /// Explicit portrait dimensions in points.
    Custom { width: f64, height: f64 },
}

impl PaperSize {
    /// Portrait `(width, height)` in points.
    #[must_use]
    pub fn size(self) -> (f64, f64) {
        // ISO sizes are the exact millimetre dimensions converted to points;
        // the imperial ones are exact in inches.
        match self {
            Self::A3 => (297.0 * POINTS_PER_MM, 420.0 * POINTS_PER_MM),
            Self::A4 => (210.0 * POINTS_PER_MM, 297.0 * POINTS_PER_MM),
            Self::A5 => (148.0 * POINTS_PER_MM, 210.0 * POINTS_PER_MM),
            Self::A6 => (105.0 * POINTS_PER_MM, 148.0 * POINTS_PER_MM),
            Self::B5 => (176.0 * POINTS_PER_MM, 250.0 * POINTS_PER_MM),
            Self::Letter => (612.0, 792.0),
            Self::Legal => (612.0, 1008.0),
            Self::Tabloid => (792.0, 1224.0),
            Self::Executive => (522.0, 756.0),
            Self::Custom { width, height } => (width, height),
        }
    }

    #[must_use]
    pub fn width(self) -> f64 {
        self.size().0
    }

    #[must_use]
    pub fn height(self) -> f64 {
        self.size().1
    }

    /// The sheet rectangle at `orientation`, with its origin at (0, 0).
    ///
    /// [`Orientation::Auto`] is resolved by the imposition code against the
    /// pages being placed, so it is treated as portrait here.
    #[must_use]
    pub fn rect(self, orientation: Orientation) -> Rect {
        let (w, h) = self.size();
        match orientation {
            Orientation::Landscape => Rect::from_size(0.0, 0.0, h, w),
            Orientation::Portrait | Orientation::Auto => Rect::from_size(0.0, 0.0, w, h),
        }
    }

    /// The CUPS/IPP media name, when one exists. Used by the pass-through
    /// path to name the paper without rasterizing.
    #[must_use]
    pub fn ipp_name(self) -> Option<&'static str> {
        Some(match self {
            Self::A3 => "iso_a3_297x420mm",
            Self::A4 => "iso_a4_210x297mm",
            Self::A5 => "iso_a5_148x210mm",
            Self::A6 => "iso_a6_105x148mm",
            Self::B5 => "iso_b5_176x250mm",
            Self::Letter => "na_letter_8.5x11in",
            Self::Legal => "na_legal_8.5x14in",
            Self::Tabloid => "na_ledger_11x17in",
            Self::Executive => "na_executive_7.25x10.5in",
            Self::Custom { .. } => return None,
        })
    }

    /// Parse a paper name, case-insensitively. Also accepts explicit sizes as
    /// `210x297mm`, `8.5x11in`, or `612x792pt`.
    pub fn parse(text: &str) -> Result<Self, PrintError> {
        let trimmed = text.trim();
        let lower = trimmed.to_ascii_lowercase();
        let named = match lower.as_str() {
            "a3" => Some(Self::A3),
            "a4" => Some(Self::A4),
            "a5" => Some(Self::A5),
            "a6" => Some(Self::A6),
            "b5" => Some(Self::B5),
            "letter" | "us-letter" | "na_letter" => Some(Self::Letter),
            "legal" | "us-legal" => Some(Self::Legal),
            "tabloid" | "ledger" => Some(Self::Tabloid),
            "executive" => Some(Self::Executive),
            _ => None,
        };
        if let Some(named) = named {
            return Ok(named);
        }

        let (body, scale) = if let Some(rest) = lower.strip_suffix("mm") {
            (rest, POINTS_PER_MM)
        } else if let Some(rest) = lower.strip_suffix("in") {
            (rest, POINTS_PER_INCH)
        } else if let Some(rest) = lower.strip_suffix("pt") {
            (rest, 1.0)
        } else {
            (lower.as_str(), 1.0)
        };
        let (w, h) = body
            .split_once('x')
            .ok_or_else(|| PrintError::InvalidOptions(format!("unknown paper size {text:?}")))?;
        let parse = |s: &str| {
            s.trim()
                .parse::<f64>()
                .map_err(|_| PrintError::InvalidOptions(format!("unknown paper size {text:?}")))
        };
        let paper = Self::Custom {
            width: parse(w)? * scale,
            height: parse(h)? * scale,
        };
        paper.validate()?;
        Ok(paper)
    }

    pub fn validate(&self) -> Result<(), PrintError> {
        let (w, h) = self.size();
        if !w.is_finite() || !h.is_finite() || w <= 0.0 || h <= 0.0 {
            return Err(PrintError::InvalidOptions(format!(
                "paper size {w}x{h}pt is not positive and finite"
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;

    #[test]
    fn a4_is_210_by_297_millimetres() {
        let (w, h) = PaperSize::A4.size();
        assert!((w - 595.276).abs() < 0.01, "{w}");
        assert!((h - 841.89).abs() < 0.01, "{h}");
    }

    #[test]
    fn landscape_swaps_the_axes() {
        let portrait = PaperSize::A4.rect(Orientation::Portrait);
        let landscape = PaperSize::A4.rect(Orientation::Landscape);
        assert_eq!(portrait.width(), landscape.height());
        assert_eq!(portrait.height(), landscape.width());
    }

    #[test]
    fn parse_accepts_names_and_explicit_sizes() {
        assert_eq!(PaperSize::parse("A4").unwrap(), PaperSize::A4);
        assert_eq!(PaperSize::parse(" letter ").unwrap(), PaperSize::Letter);
        let custom = PaperSize::parse("100x200pt").unwrap();
        assert_eq!(custom.size(), (100.0, 200.0));
        let inches = PaperSize::parse("8.5x11in").unwrap();
        assert_eq!(inches.size(), (612.0, 792.0));
        assert!(PaperSize::parse("nonsense").is_err());
        assert!(PaperSize::parse("0x10pt").is_err());
    }

    #[test]
    fn margins_take_the_wider_of_user_and_hardware() {
        let user = Margins::uniform(4.0);
        let hardware = Margins {
            left: 10.0,
            right: 2.0,
            top: 0.0,
            bottom: 18.0,
        };
        let combined = user.max_with(hardware);
        assert_eq!(combined.left, 10.0);
        assert_eq!(combined.right, 4.0);
        assert_eq!(combined.top, 4.0);
        assert_eq!(combined.bottom, 18.0);
    }

    #[test]
    fn quarter_turn_moves_top_margin_to_the_left_edge() {
        let m = Margins {
            left: 1.0,
            right: 2.0,
            top: 3.0,
            bottom: 4.0,
        };
        let turned = m.rotated_quarter_turns(1);
        assert_eq!(turned.left, 3.0);
        assert_eq!(turned.top, 2.0);
        assert_eq!(turned.right, 4.0);
        assert_eq!(turned.bottom, 1.0);
        assert_eq!(turned.rotated_quarter_turns(3), m);
    }
}
