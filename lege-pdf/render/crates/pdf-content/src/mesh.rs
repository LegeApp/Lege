//! Mesh shading stream decoding (ISO 32000-1 §8.7.4.5.5–8.7.4.5.8; PDFium
//! `core/fpdfapi/page/cpdf_meshstream.cpp` +
//! `core/fpdfapi/render/cpdf_rendershading.cpp` parity).
//!
//! Types 4/5 decode to a flat Gouraud triangle list; types 6/7 decode to
//! tensor patches (a Coons patch is upgraded to tensor form here via the
//! §8.7.4.5.8 interior-point formulas). All bit unpacking, `/Decode`
//! mapping, edge-sharing flags, and lattice row pairing happen here; the
//! caller supplies a color converter (function evaluation + color space).
//!
//! Untrusted input: nothing here panics; malformed streams yield whatever
//! whole primitives decoded before the data ran out (PDFium behavior).

use crate::semantic::{SemMeshPatch, SemMeshVertex};

/// Hard cap on primitives decoded from one mesh stream: a hostile stream
/// must not balloon the IR (each triangle is ~100 bytes; each patch ~200).
const MAX_PRIMITIVES: usize = 1 << 18;

/// MSB-first bit reader over the decoded stream data (PDFium `CFX_BitStream`).
struct BitReader<'a> {
    data: &'a [u8],
    bit: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, bit: 0 }
    }

    fn remaining(&self) -> usize {
        (self.data.len() * 8).saturating_sub(self.bit)
    }

    /// Read `n` bits MSB-first (`n` ≤ 32). `None` at end of data.
    fn get(&mut self, n: u32) -> Option<u32> {
        if n == 0 || n > 32 || self.remaining() < n as usize {
            return None;
        }
        let mut v: u64 = 0;
        for _ in 0..n {
            let byte = self.bit / 8;
            let shift = 7 - (self.bit % 8);
            let b = (self.data[byte] >> shift) & 1;
            v = (v << 1) | b as u64;
            self.bit += 1;
        }
        Some(v as u32)
    }

    fn byte_align(&mut self) {
        self.bit = self.bit.div_ceil(8) * 8;
    }
}

/// Decode parameters shared by all mesh types (`/BitsPer*`, `/Decode`).
pub(crate) struct MeshParams {
    pub coord_bits: u32,
    pub component_bits: u32,
    /// 0 for type 5 (no flags in a lattice stream).
    pub flag_bits: u32,
    /// Components carried per color entry: 1 with `/Function`, else the
    /// color-space component count.
    pub components: usize,
    /// `/Decode` pairs: `[xmin, xmax]`, `[ymin, ymax]`, then one per component.
    pub x: [f32; 2],
    pub y: [f32; 2],
    pub comp: Vec<[f32; 2]>,
}

impl MeshParams {
    /// Validate the bit widths exactly as PDFium does (tables 4.32–4.34).
    pub(crate) fn valid(&self, needs_flag: bool) -> bool {
        matches!(self.coord_bits, 1 | 2 | 4 | 8 | 12 | 16 | 24 | 32)
            && matches!(self.component_bits, 1 | 2 | 4 | 8 | 12 | 16)
            && (!needs_flag || matches!(self.flag_bits, 2 | 4 | 8))
            && self.components > 0
            && self.comp.len() >= self.components
    }
}

/// `min + raw · (max − min) / (2^bits − 1)` (PDFium `ReadCoords`/`ReadColor`).
fn dequant(raw: u32, bits: u32, range: [f32; 2]) -> f32 {
    let max = if bits >= 32 {
        u32::MAX as f64
    } else {
        ((1u64 << bits) - 1) as f64
    };
    range[0] + (raw as f64 * (range[1] - range[0]) as f64 / max) as f32
}

struct Reader<'a, 'c> {
    bits: BitReader<'a>,
    params: &'a MeshParams,
    convert: &'c dyn Fn(&[f32]) -> [f32; 4],
}

impl Reader<'_, '_> {
    fn read_flag(&mut self) -> Option<u32> {
        self.bits.get(self.params.flag_bits).map(|v| v & 0x03)
    }

    fn read_point(&mut self) -> Option<[f64; 2]> {
        let bits = self.params.coord_bits;
        let x = dequant(self.bits.get(bits)?, bits, self.params.x);
        let y = dequant(self.bits.get(bits)?, bits, self.params.y);
        Some([x as f64, y as f64])
    }

    fn read_color(&mut self) -> Option<[f32; 4]> {
        let mut comps = Vec::with_capacity(self.params.components);
        for i in 0..self.params.components {
            let raw = self.bits.get(self.params.component_bits)?;
            comps.push(dequant(
                raw,
                self.params.component_bits,
                self.params.comp[i],
            ));
        }
        Some((self.convert)(&comps))
    }

    /// One type 4/5 vertex; byte-aligned afterwards (PDFium `ReadVertex`).
    fn read_vertex(&mut self, with_flag: bool) -> Option<(u32, SemMeshVertex)> {
        let flag = if with_flag { self.read_flag()? } else { 0 };
        let [x, y] = self.read_point()?;
        let color = self.read_color()?;
        self.bits.byte_align();
        Some((flag, SemMeshVertex { x, y, color }))
    }
}

/// Type 4: free-form Gouraud triangles. Flag 0 starts a fresh triangle (two
/// more vertices follow); flags 1/2 share an edge with the previous triangle
/// (PDFium `DrawFreeGouraudShading`).
pub(crate) fn read_free_triangles(
    data: &[u8],
    params: &MeshParams,
    convert: &dyn Fn(&[f32]) -> [f32; 4],
) -> Vec<[SemMeshVertex; 3]> {
    let mut r = Reader {
        bits: BitReader::new(data),
        params,
        convert,
    };
    let mut out = Vec::new();
    let mut tri: [SemMeshVertex; 3] = [SemMeshVertex {
        x: 0.0,
        y: 0.0,
        color: [0.0; 4],
    }; 3];
    let mut have = false;
    while r.bits.remaining() > 0 && out.len() < MAX_PRIMITIVES {
        let Some((flag, vertex)) = r.read_vertex(true) else {
            break;
        };
        if flag == 0 {
            tri[0] = vertex;
            let mut ok = true;
            for slot in tri.iter_mut().skip(1) {
                match r.read_vertex(true) {
                    Some((_, v)) => *slot = v,
                    None => {
                        ok = false;
                        break;
                    }
                }
            }
            if !ok {
                break;
            }
            have = true;
        } else {
            if !have {
                break; // Edge-share with no previous triangle: malformed.
            }
            if flag == 1 {
                tri[0] = tri[1];
            }
            tri[1] = tri[2];
            tri[2] = vertex;
        }
        out.push(tri);
    }
    out
}

/// Type 5: lattice-form Gouraud triangles — rows of `row_verts` vertices
/// (no flags); consecutive rows pair into two triangles per lattice cell
/// (PDFium `DrawLatticeGouraudShading`).
pub(crate) fn read_lattice_triangles(
    data: &[u8],
    params: &MeshParams,
    row_verts: usize,
    convert: &dyn Fn(&[f32]) -> [f32; 4],
) -> Vec<[SemMeshVertex; 3]> {
    // A row longer than the stream could possibly hold (each vertex needs at
    // least one bit) is hostile input — reject before allocating for it.
    if row_verts < 2 || row_verts > data.len().saturating_mul(8) {
        return Vec::new();
    }
    let mut r = Reader {
        bits: BitReader::new(data),
        params,
        convert,
    };
    let read_row = |r: &mut Reader| -> Option<Vec<SemMeshVertex>> {
        let mut row = Vec::with_capacity(row_verts);
        for _ in 0..row_verts {
            row.push(r.read_vertex(false)?.1);
        }
        Some(row)
    };
    let Some(mut prev) = read_row(&mut r) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    while out.len() < MAX_PRIMITIVES {
        let Some(cur) = read_row(&mut r) else { break };
        for i in 1..row_verts {
            out.push([prev[i - 1], prev[i], cur[i - 1]]);
            out.push([cur[i - 1], prev[i], cur[i]]);
        }
        prev = cur;
    }
    out
}

/// Map the spec's boundary points `p1..p12` (+ 4 interior for type 7) into
/// the IR's row-major tensor grid `p[i][j] = points[i * 4 + j]` — exactly
/// PDFium's `patch.points[x][y] = coords[k]` assignment.
fn tensor_from_boundary(coords: &[[f64; 2]; 16], tensor: bool) -> [[f64; 2]; 16] {
    const fn ix(i: usize, j: usize) -> usize {
        i * 4 + j
    }
    let mut p = [[0.0f64; 2]; 16];
    p[ix(0, 0)] = coords[0];
    p[ix(0, 1)] = coords[1];
    p[ix(0, 2)] = coords[2];
    p[ix(0, 3)] = coords[3];
    p[ix(1, 3)] = coords[4];
    p[ix(2, 3)] = coords[5];
    p[ix(3, 3)] = coords[6];
    p[ix(3, 2)] = coords[7];
    p[ix(3, 1)] = coords[8];
    p[ix(3, 0)] = coords[9];
    p[ix(2, 0)] = coords[10];
    p[ix(1, 0)] = coords[11];
    if tensor {
        p[ix(1, 1)] = coords[12];
        p[ix(1, 2)] = coords[13];
        p[ix(2, 2)] = coords[14];
        p[ix(2, 1)] = coords[15];
    } else {
        // Coons → tensor interior points, §8.7.4.5.8 (ISO 32000-2 p.267).
        let q = p;
        let g = |i: usize, j: usize| q[ix(i, j)];
        let lin = |terms: [(f64, [f64; 2]); 7]| -> [f64; 2] {
            let mut acc = [0.0f64; 2];
            for (w, pt) in terms {
                acc[0] += w * pt[0];
                acc[1] += w * pt[1];
            }
            [acc[0] / 9.0, acc[1] / 9.0]
        };
        let p11 = lin([
            (-4.0, g(0, 0)),
            (6.0, g(0, 1)),
            (6.0, g(1, 0)),
            (-2.0, g(0, 3)),
            (-2.0, g(3, 0)),
            (3.0, g(3, 1)),
            (3.0, g(1, 3)),
        ]);
        let p11 = [p11[0] - g(3, 3)[0] / 9.0, p11[1] - g(3, 3)[1] / 9.0];
        let p12 = lin([
            (-4.0, g(0, 3)),
            (6.0, g(0, 2)),
            (6.0, g(1, 3)),
            (-2.0, g(0, 0)),
            (-2.0, g(3, 3)),
            (3.0, g(3, 2)),
            (3.0, g(1, 0)),
        ]);
        let p12 = [p12[0] - g(3, 0)[0] / 9.0, p12[1] - g(3, 0)[1] / 9.0];
        let p21 = lin([
            (-4.0, g(3, 0)),
            (6.0, g(3, 1)),
            (6.0, g(2, 0)),
            (-2.0, g(3, 3)),
            (-2.0, g(0, 0)),
            (3.0, g(0, 1)),
            (3.0, g(2, 3)),
        ]);
        let p21 = [p21[0] - g(0, 3)[0] / 9.0, p21[1] - g(0, 3)[1] / 9.0];
        let p22 = lin([
            (-4.0, g(3, 3)),
            (6.0, g(3, 2)),
            (6.0, g(2, 3)),
            (-2.0, g(3, 0)),
            (-2.0, g(0, 3)),
            (3.0, g(0, 2)),
            (3.0, g(2, 0)),
        ]);
        let p22 = [p22[0] - g(0, 0)[0] / 9.0, p22[1] - g(0, 0)[1] / 9.0];
        p[ix(1, 1)] = p11;
        p[ix(1, 2)] = p12;
        p[ix(2, 2)] = p22;
        p[ix(2, 1)] = p21;
    }
    p
}

/// Types 6/7: Coons / tensor-product patch meshes. Flags 1/2/3 share the
/// previous patch's edge — the new `p1..p4` are the previous boundary points
/// `(flag·3 + k) mod 12` and the new first two corner colors are the
/// previous colors `flag` and `(flag+1) mod 4` (PDFium `DrawCoonPatchMeshes`).
pub(crate) fn read_patches(
    data: &[u8],
    params: &MeshParams,
    tensor: bool,
    convert: &dyn Fn(&[f32]) -> [f32; 4],
) -> Vec<SemMeshPatch> {
    let point_count = if tensor { 16 } else { 12 };
    let mut r = Reader {
        bits: BitReader::new(data),
        params,
        convert,
    };
    let mut out = Vec::new();
    // Raw stream-order boundary (+ interior) points and corner colors of the
    // previous patch, kept for edge sharing.
    let mut coords = [[0.0f64; 2]; 16];
    let mut colors = [[0.0f32; 4]; 4];
    let mut have = false;
    while r.bits.remaining() > 0 && out.len() < MAX_PRIMITIVES {
        let Some(flag) = r.read_flag() else { break };
        let (start_point, start_color) = if flag == 0 {
            (0usize, 0usize)
        } else {
            if !have {
                break; // Shared edge with no previous patch: malformed.
            }
            let mut first = [[0.0f64; 2]; 4];
            for (k, slot) in first.iter_mut().enumerate() {
                *slot = coords[(flag as usize * 3 + k) % 12];
            }
            coords[..4].copy_from_slice(&first);
            let c0 = colors[flag as usize];
            let c1 = colors[(flag as usize + 1) % 4];
            colors[0] = c0;
            colors[1] = c1;
            (4usize, 2usize)
        };
        let mut ok = true;
        for slot in coords.iter_mut().take(point_count).skip(start_point) {
            match r.read_point() {
                Some(p) => *slot = p,
                None => {
                    ok = false;
                    break;
                }
            }
        }
        if ok {
            for slot in colors.iter_mut().skip(start_color) {
                match r.read_color() {
                    Some(c) => *slot = c,
                    None => {
                        ok = false;
                        break;
                    }
                }
            }
        }
        if !ok {
            break;
        }
        have = true;
        out.push(SemMeshPatch {
            points: tensor_from_boundary(&coords, tensor),
            colors,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn bit_reader_msb_first_and_align() {
        let mut r = BitReader::new(&[0b1011_0001, 0xFF]);
        assert_eq!(r.get(3), Some(0b101));
        r.byte_align();
        assert_eq!(r.get(8), Some(0xFF));
        assert_eq!(r.get(1), None);
    }

    #[test]
    fn dequant_maps_full_range() {
        assert_eq!(dequant(0, 8, [-1.0, 1.0]), -1.0);
        assert_eq!(dequant(255, 8, [-1.0, 1.0]), 1.0);
        assert!((dequant(u32::MAX, 32, [0.0, 1.0]) - 1.0).abs() < 1e-6);
    }
}
