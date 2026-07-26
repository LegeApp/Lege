//! Geometry primitives. `f64` throughout — narrowing is a backend decision
//! (roadmap §5.4). Device-space integer types for output surfaces.
//!
//! Leaf crate with no dependencies: shared between the renderer (via
//! `pdf-page-ir`, which re-exports everything here) and, at migration time,
//! the lege-pdf writer, whose `types.rs` mirrors `Matrix`/`Rect`
//! field-for-field until it can depend on this crate directly.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

/// A point in user or text space.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

/// Axis-aligned rectangle, `x0 <= x1`, `y0 <= y1` once normalized.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Rect {
    pub x0: f64,
    pub y0: f64,
    pub x1: f64,
    pub y1: f64,
}

impl Rect {
    pub fn width(&self) -> f64 {
        self.x1 - self.x0
    }
    pub fn height(&self) -> f64 {
        self.y1 - self.y0
    }
    /// Normalize so x0<=x1, y0<=y1 (PDF rectangles may be in any corner
    /// order).
    pub fn normalized(self) -> Rect {
        Rect {
            x0: self.x0.min(self.x1),
            y0: self.y0.min(self.y1),
            x1: self.x0.max(self.x1),
            y1: self.y0.max(self.y1),
        }
    }
    pub fn union(self, other: Rect) -> Rect {
        Rect {
            x0: self.x0.min(other.x0),
            y0: self.y0.min(other.y0),
            x1: self.x1.max(other.x1),
            y1: self.y1.max(other.y1),
        }
    }
    pub fn intersect(self, other: Rect) -> Option<Rect> {
        let r = Rect {
            x0: self.x0.max(other.x0),
            y0: self.y0.max(other.y0),
            x1: self.x1.min(other.x1),
            y1: self.y1.min(other.y1),
        };
        (r.x0 <= r.x1 && r.y0 <= r.y1).then_some(r)
    }
}

/// 2D affine transform, PDF/PostScript layout: `[a b c d e f]` mapping
/// `x' = a·x + c·y + e`, `y' = b·x + d·y + f`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Matrix {
    pub a: f64,
    pub b: f64,
    pub c: f64,
    pub d: f64,
    pub e: f64,
    pub f: f64,
}

impl Matrix {
    pub const IDENTITY: Matrix = Matrix {
        a: 1.0,
        b: 0.0,
        c: 0.0,
        d: 1.0,
        e: 0.0,
        f: 0.0,
    };

    pub fn translate(tx: f64, ty: f64) -> Matrix {
        Matrix {
            e: tx,
            f: ty,
            ..Matrix::IDENTITY
        }
    }

    pub fn scale(sx: f64, sy: f64) -> Matrix {
        Matrix {
            a: sx,
            d: sy,
            ..Matrix::IDENTITY
        }
    }

    /// `self` then `other` (i.e. `other ∘ self`, PDF `cm` semantics where
    /// the new matrix is premultiplied onto the CTM).
    pub fn then(self, other: Matrix) -> Matrix {
        Matrix {
            a: self.a * other.a + self.b * other.c,
            b: self.a * other.b + self.b * other.d,
            c: self.c * other.a + self.d * other.c,
            d: self.c * other.b + self.d * other.d,
            e: self.e * other.a + self.f * other.c + other.e,
            f: self.e * other.b + self.f * other.d + other.f,
        }
    }

    pub fn apply(&self, p: Point) -> Point {
        Point {
            x: self.a * p.x + self.c * p.y + self.e,
            y: self.b * p.x + self.d * p.y + self.f,
        }
    }

    pub fn determinant(&self) -> f64 {
        self.a * self.d - self.b * self.c
    }

    /// Every coefficient is finite. PDF numbers are untrusted: a content
    /// stream may hand us a `cm` built from NaN or overflowed literals, and a
    /// non-finite matrix poisons every coordinate downstream of it.
    pub fn is_finite(&self) -> bool {
        self.a.is_finite()
            && self.b.is_finite()
            && self.c.is_finite()
            && self.d.is_finite()
            && self.e.is_finite()
            && self.f.is_finite()
    }

    /// Whether the inverse is *numerically* trustworthy, not merely defined.
    ///
    /// Separate from [`Matrix::invert`] on purpose. A determinant threshold is
    /// meaningless on its own because the determinant scales as the square of
    /// the matrix: `[0.001 0 0 0.001]` — an ordinary Type 3 font matrix — has
    /// a determinant of 1e-6 while being perfectly invertible, and an
    /// ill-conditioned matrix built from 1e8 coefficients can have a
    /// determinant of 1e7 while its inverse is pure noise. Comparing against
    /// the matrix's own scale is what makes the test mean anything.
    pub fn is_stably_invertible(&self) -> bool {
        if !self.is_finite() {
            return false;
        }
        let scale = self
            .a
            .abs()
            .max(self.b.abs())
            .max(self.c.abs())
            .max(self.d.abs());
        if scale == 0.0 || !scale.is_finite() {
            return false;
        }
        self.determinant().abs() > 64.0 * f64::EPSILON * scale * scale
    }

    /// The inverse transform, or `None` when the matrix is singular. Used to
    /// map device points back into a paint's own coordinate space (e.g. a
    /// shading's axis parameter).
    ///
    /// The singularity test is literal — exactly zero, or non-finite. It is
    /// deliberately *not* `det.abs() < 1e-12`: that threshold is not
    /// scale-independent, so it silently discards valid transforms merely for
    /// being small. Callers needing a numerically reliable inverse should ask
    /// [`Matrix::is_stably_invertible`] first.
    pub fn invert(&self) -> Option<Matrix> {
        if !self.is_finite() {
            return None;
        }
        let det = self.determinant();
        if det == 0.0 || !det.is_finite() {
            return None;
        }
        let inv = 1.0 / det;
        // Inverse of the 2x2 linear part, then the translation it implies.
        let a = self.d * inv;
        let b = -self.b * inv;
        let c = -self.c * inv;
        let d = self.a * inv;
        Some(Matrix {
            a,
            b,
            c,
            d,
            e: -(self.e * a + self.f * c),
            f: -(self.e * b + self.f * d),
        })
    }
}

impl Default for Matrix {
    fn default() -> Self {
        Matrix::IDENTITY
    }
}

/// Integer pixel size of an output surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceSize {
    pub width: u32,
    pub height: u32,
}

/// Integer pixel rectangle in an output surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceRect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn matrix_compose_and_apply() {
        let m = Matrix::scale(2.0, 3.0).then(Matrix::translate(10.0, 20.0));
        let p = m.apply(Point { x: 1.0, y: 1.0 });
        assert_eq!(p, Point { x: 12.0, y: 23.0 });
        assert_eq!(m.determinant(), 6.0);
    }

    #[test]
    fn matrix_invert_roundtrips() {
        let m = Matrix {
            a: 2.0,
            b: 0.5,
            c: -1.0,
            d: 3.0,
            e: 10.0,
            f: -4.0,
        };
        let inv = m.invert().unwrap();
        let p = Point { x: 7.0, y: -2.0 };
        let back = inv.apply(m.apply(p));
        assert!((back.x - p.x).abs() < 1e-9 && (back.y - p.y).abs() < 1e-9);
        // A singular matrix has no inverse.
        assert!(
            Matrix {
                a: 1.0,
                b: 2.0,
                c: 2.0,
                d: 4.0,
                e: 0.0,
                f: 0.0
            }
            .invert()
            .is_none()
        );
    }

    #[test]
    fn small_but_valid_matrices_stay_invertible() {
        // A Type 3 font matrix is [0.001 0 0 0.001]; under a further 0.001
        // downscale the determinant reaches 1e-12. The old `det.abs() < 1e-12`
        // guard sat exactly on that cliff and threw such transforms away, which
        // silently drops the glyph. Smallness is not singularity.
        let m = Matrix::scale(0.001, 0.001).then(Matrix::scale(0.001, 0.001));
        assert_eq!(m.determinant(), 1e-12);
        let inv = m.invert().expect("a pure scale is always invertible");
        let p = inv.apply(m.apply(Point { x: 3.0, y: -5.0 }));
        assert!((p.x - 3.0).abs() < 1e-9 && (p.y + 5.0).abs() < 1e-9);
        assert!(
            m.is_stably_invertible(),
            "a pure scale is well-conditioned at any size"
        );
    }

    #[test]
    fn stability_is_scale_relative_not_absolute() {
        // The mirror failure: huge coefficients with near-parallel columns give
        // a determinant of 10 — thirteen orders of magnitude past a 1e-12
        // epsilon — while retaining barely one good digit, so the inverse is
        // noise. Only a scale-relative test catches it.
        let ill = Matrix {
            a: 1e8,
            b: 1e8,
            c: 1e8,
            d: 1e8 * (1.0 + 1e-15),
            e: 0.0,
            f: 0.0,
        };
        assert!(
            ill.determinant().abs() > 1e-12,
            "sails past a fixed epsilon"
        );
        assert!(
            !ill.is_stably_invertible(),
            "but must be rejected as ill-conditioned"
        );

        // Conversely, "large coefficients" is not itself a defect: the same
        // 1e8-scaled matrix with a healthy relative determinant is fine.
        let big = Matrix {
            a: 1e8,
            b: 0.0,
            c: 0.0,
            d: 1e8,
            e: 0.0,
            f: 0.0,
        };
        assert!(
            big.is_stably_invertible(),
            "a big pure scale is well-conditioned"
        );
    }

    #[test]
    fn non_finite_matrices_are_rejected() {
        // PDF numbers are untrusted; a malformed `cm` must not poison geometry.
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let m = Matrix {
                a: bad,
                ..Matrix::IDENTITY
            };
            assert!(!m.is_finite());
            assert!(m.invert().is_none(), "{bad} matrix must not invert");
            assert!(!m.is_stably_invertible());
        }
        // NaN slips past a magnitude comparison specifically because every
        // comparison against NaN is false — the exact hole `det.abs() < 1e-12`
        // left open.
        let nan_det = Matrix {
            a: f64::NAN,
            d: f64::NAN,
            ..Matrix::IDENTITY
        };
        let old_guard_would_fire = nan_det.determinant().abs() < 1e-12;
        assert!(
            !old_guard_would_fire,
            "every NaN comparison is false, so the old guard passed it"
        );
        assert!(
            nan_det.invert().is_none(),
            "the finiteness check catches what magnitude cannot"
        );
    }

    #[test]
    fn singular_matrices_have_no_inverse() {
        // Exactly-zero determinant: genuinely singular, correctly refused.
        assert!(Matrix::scale(0.0, 1.0).invert().is_none());
        assert!(
            Matrix {
                a: 1.0,
                b: 2.0,
                c: 2.0,
                d: 4.0,
                e: 0.0,
                f: 0.0
            }
            .invert()
            .is_none()
        );
    }

    #[test]
    fn rect_ops() {
        let r = Rect {
            x0: 10.0,
            y0: 10.0,
            x1: 0.0,
            y1: 0.0,
        }
        .normalized();
        assert_eq!(r.width(), 10.0);
        let other = Rect {
            x0: 5.0,
            y0: 5.0,
            x1: 20.0,
            y1: 20.0,
        };
        assert_eq!(
            r.intersect(other),
            Some(Rect {
                x0: 5.0,
                y0: 5.0,
                x1: 10.0,
                y1: 10.0
            })
        );
        assert!(
            r.intersect(Rect {
                x0: 11.0,
                y0: 11.0,
                x1: 12.0,
                y1: 12.0
            })
            .is_none()
        );
    }
}
