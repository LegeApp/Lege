//! PBM (P4 binary) read/write plus a first-differing-pixel report, for the
//! decoder interoperability harness (jbig2decplan.md §21.5, Gap C).

#![allow(dead_code)]

/// A one-bit image as one byte per pixel (`1` = black), row-major.
#[derive(Clone, PartialEq, Eq)]
pub struct Pbm {
    pub width: u32,
    pub height: u32,
    /// `width * height` bytes, each 0 or 1.
    pub pixels: Vec<u8>,
}

impl Pbm {
    pub fn new(width: u32, height: u32, pixels: Vec<u8>) -> Self {
        assert_eq!(pixels.len(), (width as usize) * (height as usize));
        Self { width, height, pixels }
    }

    #[inline]
    pub fn get(&self, x: u32, y: u32) -> u8 {
        self.pixels[(y * self.width + x) as usize]
    }

    /// Serialise as a P4 (raw) PBM. In PBM, a set bit is black.
    pub fn to_p4(&self) -> Vec<u8> {
        let mut out = format!("P4\n{} {}\n", self.width, self.height).into_bytes();
        let row_bytes = (self.width as usize).div_ceil(8);
        for y in 0..self.height {
            let mut row = vec![0u8; row_bytes];
            for x in 0..self.width {
                if self.get(x, y) != 0 {
                    row[(x / 8) as usize] |= 0x80 >> (x % 8);
                }
            }
            out.extend_from_slice(&row);
        }
        out
    }

    /// Parse a P4 (raw) PBM. Tolerates comments and arbitrary whitespace in the
    /// header, per the PBM format.
    pub fn from_p4(bytes: &[u8]) -> Result<Pbm, String> {
        let mut i = 0usize;
        if bytes.len() < 2 || &bytes[0..2] != b"P4" {
            return Err("not a P4 PBM".into());
        }
        i += 2;

        fn skip_ws_comments(b: &[u8], i: &mut usize) {
            loop {
                while *i < b.len() && b[*i].is_ascii_whitespace() {
                    *i += 1;
                }
                if *i < b.len() && b[*i] == b'#' {
                    while *i < b.len() && b[*i] != b'\n' {
                        *i += 1;
                    }
                } else {
                    break;
                }
            }
        }
        fn read_uint(b: &[u8], i: &mut usize) -> Result<u32, String> {
            let start = *i;
            while *i < b.len() && b[*i].is_ascii_digit() {
                *i += 1;
            }
            if *i == start {
                return Err("expected integer in PBM header".into());
            }
            std::str::from_utf8(&b[start..*i])
                .map_err(|e| e.to_string())?
                .parse()
                .map_err(|_| "bad integer".to_string())
        }

        skip_ws_comments(bytes, &mut i);
        let width = read_uint(bytes, &mut i)?;
        skip_ws_comments(bytes, &mut i);
        let height = read_uint(bytes, &mut i)?;
        // Exactly one whitespace byte separates the header from the raster.
        if i >= bytes.len() {
            return Err("truncated PBM before raster".into());
        }
        i += 1;

        let row_bytes = (width as usize).div_ceil(8);
        let need = row_bytes
            .checked_mul(height as usize)
            .ok_or("PBM size overflow")?;
        if bytes.len() < i + need {
            return Err(format!(
                "truncated PBM raster: have {}, need {}",
                bytes.len() - i,
                need
            ));
        }
        let mut pixels = Vec::with_capacity((width * height) as usize);
        for y in 0..height as usize {
            let row = &bytes[i + y * row_bytes..i + (y + 1) * row_bytes];
            for x in 0..width as usize {
                let bit = (row[x / 8] >> (7 - (x % 8))) & 1;
                pixels.push(bit);
            }
        }
        Ok(Pbm { width, height, pixels })
    }
}

/// The first differing pixel between two images: `(x, y, expected, got)`.
pub fn first_diff(expected: &Pbm, got: &Pbm) -> Option<(u32, u32, u8, u8)> {
    if expected.width != got.width || expected.height != got.height {
        return Some((0, 0, 255, 255)); // dimension mismatch sentinel
    }
    for y in 0..expected.height {
        for x in 0..expected.width {
            let e = expected.get(x, y);
            let g = got.get(x, y);
            if e != g {
                return Some((x, y, e, g));
            }
        }
    }
    None
}

/// Assert two images are pixel-identical, with a precise first-difference
/// message on failure.
pub fn assert_pixels_eq(expected: &Pbm, got: &Pbm, context: &str) {
    if let Some((x, y, e, g)) = first_diff(expected, got) {
        panic!(
            "{context}: pixel mismatch at ({x},{y}) expected={e} got={g} \
             (dims exp {}x{} got {}x{})",
            expected.width, expected.height, got.width, got.height
        );
    }
}
