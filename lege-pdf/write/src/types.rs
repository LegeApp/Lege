//! Core small types: ObjectId (u32 num + gen), Affine, PdfRect, ResourceName.
//! Mirrors pdf-page-ir geometry (f64); swap to the shared crate once render/ lands.

use std::io;

/// An indirect object reference. In a write-only emitter the generation is
/// always 0, but the field is kept so xref/reference syntax stays honest.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ObjectId {
    pub num: u32,
    pub generation: u16,
}

impl ObjectId {
    pub const fn new(num: u32) -> Self {
        Self { num, generation: 0 }
    }
}

/// A 2-D affine transform, `[a b c d e f]` in PDF/PostScript layout:
/// `x' = a·x + c·y + e`, `y' = b·x + d·y + f`. Field names and f64 type mirror
/// `pdf-page-ir::geom::Matrix` exactly so the future swap is mechanical.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Affine {
    pub a: f64,
    pub b: f64,
    pub c: f64,
    pub d: f64,
    pub e: f64,
    pub f: f64,
}

impl Affine {
    pub const IDENTITY: Affine = Affine {
        a: 1.0,
        b: 0.0,
        c: 0.0,
        d: 1.0,
        e: 0.0,
        f: 0.0,
    };

    pub const fn new(a: f64, b: f64, c: f64, d: f64, e: f64, f: f64) -> Self {
        Self { a, b, c, d, e, f }
    }

    /// The common image placement: scale a 1×1 unit XObject to `(sx, sy)` and
    /// translate to `(tx, ty)` — exactly the `cm` the pipeline builds today.
    pub const fn scale_translate(sx: f64, sy: f64, tx: f64, ty: f64) -> Self {
        Self {
            a: sx,
            b: 0.0,
            c: 0.0,
            d: sy,
            e: tx,
            f: ty,
        }
    }
}

impl Default for Affine {
    fn default() -> Self {
        Affine::IDENTITY
    }
}

/// Axis-aligned rectangle in PDF user space (points), `x0 <= x1`, `y0 <= y1`.
/// Field names/type mirror `pdf-page-ir::geom::Rect`.
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct PdfRect {
    pub x0: f64,
    pub y0: f64,
    pub x1: f64,
    pub y1: f64,
}

impl PdfRect {
    pub const fn new(x0: f64, y0: f64, x1: f64, y1: f64) -> Self {
        Self { x0, y0, x1, y1 }
    }

    /// A media box anchored at the origin with the given size.
    pub const fn from_size(width: f64, height: f64) -> Self {
        Self {
            x0: 0.0,
            y0: 0.0,
            x1: width,
            y1: height,
        }
    }

    pub fn width(&self) -> f64 {
        self.x1 - self.x0
    }

    pub fn height(&self) -> f64 {
        self.y1 - self.y0
    }
}

/// A page resource name, formatted into bytes with no per-name heap allocation.
/// The vocabulary is closed: image XObjects (`/Im{n}`) and fonts (`/F{n}`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ResourceName {
    Image(u32),
    Font(u16),
}

impl ResourceName {
    /// Write the bare name without the leading slash, e.g. `Im12` or `F0`.
    /// Every character produced is ASCII alphanumeric, so no `#` escaping is
    /// ever required.
    pub fn write_bare(&self, out: &mut Vec<u8>) {
        match *self {
            ResourceName::Image(n) => {
                out.extend_from_slice(b"Im");
                push_u32(out, n);
            }
            ResourceName::Font(n) => {
                out.push(b'F');
                push_u32(out, n as u32);
            }
        }
    }

    /// Write the name as a PDF name object, e.g. `/Im12`.
    pub fn write_name(&self, out: &mut Vec<u8>) {
        out.push(b'/');
        self.write_bare(out);
    }
}

/// Append the decimal digits of `n` to `out` without allocating.
fn push_u32(out: &mut Vec<u8>, n: u32) {
    let mut buf = [0u8; 10];
    let mut i = buf.len();
    let mut v = n;
    loop {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    out.extend_from_slice(&buf[i..]);
}

/// Errors from the writer. The domain surface is deliberately small.
#[derive(thiserror::Error, Debug)]
pub enum WriteError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    /// A page index was submitted more than once (the old BTreeMap silently
    /// overwrote; here it is a hard error).
    #[error("duplicate page index {0}")]
    DuplicatePage(u32),

    /// A page index outside `0..total_pages` was submitted.
    #[error("page index {index} out of range (total {total})")]
    PageIndexOutOfRange { index: u32, total: u32 },

    /// finalize() called before every logical page arrived.
    #[error("incomplete document: {missing} of {total} pages missing")]
    IncompletePages { missing: usize, total: usize },

    /// An artifact carried a resource shape the writer cannot emit. Should be
    /// unreachable given the closed input vocabulary; kept as a guard.
    #[error("invalid artifact: {0}")]
    InvalidArtifact(String),
}

pub type Result<T> = std::result::Result<T, WriteError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_names_format_without_slash() {
        let mut out = Vec::new();
        ResourceName::Image(12).write_bare(&mut out);
        assert_eq!(out, b"Im12");

        out.clear();
        ResourceName::Font(0).write_bare(&mut out);
        assert_eq!(out, b"F0");

        out.clear();
        ResourceName::Image(0).write_name(&mut out);
        assert_eq!(out, b"/Im0");
    }

    #[test]
    fn push_u32_handles_boundaries() {
        let mut out = Vec::new();
        push_u32(&mut out, 0);
        assert_eq!(out, b"0");
        out.clear();
        push_u32(&mut out, u32::MAX);
        assert_eq!(out, b"4294967295");
    }

    #[test]
    fn affine_scale_translate() {
        let m = Affine::scale_translate(2.0, 3.0, 10.0, 20.0);
        assert_eq!(m, Affine::new(2.0, 0.0, 0.0, 3.0, 10.0, 20.0));
    }

    #[test]
    fn rect_dims() {
        let r = PdfRect::from_size(612.0, 792.0);
        assert_eq!(r.width(), 612.0);
        assert_eq!(r.height(), 792.0);
    }
}
