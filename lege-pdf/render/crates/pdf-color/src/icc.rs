//! Matrix/TRC ICC profiles (`/ICCBased`) → sRGB.
//!
//! An `/ICCBased` space was treated as its `/Alternate` (DeviceRGB for `N 3`),
//! i.e. as a pass-through. That is right for the *common* profile — an sRGB one,
//! which is what most producers embed — and wrong for any other, in a way that
//! shifts the whole page: a scanned cover carrying the classic Apple/ColorSync
//! **gamma 1.8** monitor profile renders ~10/255 too dark per channel, because
//! its encoding is being read as if it were sRGB's ≈2.2. Both oracles apply the
//! profile (PDFium via LittleCMS, MuPDF natively), so this was a real gap.
//!
//! Scope is deliberately the *matrix/TRC* class — three colorant tags, three
//! tone-reproduction curves, a media white point — which covers sRGB, Apple
//! RGB, Adobe RGB, and the generic gamma profiles that make up almost all
//! embedded `/ICCBased` RGB streams. Anything else (`A2B0` lookup-table
//! profiles, non-RGB data spaces) returns `None` and keeps the alternate-space
//! behaviour, so this only ever adds fidelity where the maths is exact.
//!
//! [`IccRgb::from_profile`] also returns `None` when the parsed profile is
//! *already* sRGB within tolerance. That keeps the overwhelmingly common case
//! byte-identical rather than pushing it through a numerically-equivalent but
//! not bit-equivalent round trip.

use crate::cie;

/// A parsed matrix/TRC RGB profile, reduced to what a conversion needs.
#[derive(Debug, Clone, PartialEq)]
pub struct IccRgb {
    /// Per-channel tone reproduction curve, sampled to 256 points: encoded
    /// value → linear. Sampling is exact for the `curv` gamma form and is a
    /// resampling of a `curv` table (which is itself a sampled curve).
    trc: [[f32; 256]; 3],
    /// Columns are the profile's R/G/B colorants in PCS XYZ (D50), already
    /// chromatically adapted to D65 so the result feeds `xyz_to_srgb`
    /// directly. Row-major.
    to_xyz_d65: [f32; 9],
}

impl IccRgb {
    /// Parse an ICC profile, returning a converter only for a matrix/TRC RGB
    /// profile that is *not* already sRGB.
    pub fn from_profile(bytes: &[u8]) -> Option<IccRgb> {
        if bytes.len() < 132 || &bytes[36..40] != b"acsp" {
            return None;
        }
        // Data space must be RGB; the PCS must be XYZ (a Lab PCS would need the
        // A2B path, which this does not implement).
        if &bytes[16..20] != b"RGB " || &bytes[20..24] != b"XYZ " {
            return None;
        }
        let tag_count = u32::from_be_bytes(bytes[128..132].try_into().ok()?) as usize;
        // A sane profile has a handful of tags; a huge count is a malformed
        // length field, not a reason to scan gigabytes.
        if tag_count > 256 {
            return None;
        }
        let mut find = |want: &[u8; 4]| -> Option<(usize, usize)> {
            for i in 0..tag_count {
                let o = 132 + i * 12;
                let sig = bytes.get(o..o + 4)?;
                if sig == want {
                    let off =
                        u32::from_be_bytes(bytes.get(o + 4..o + 8)?.try_into().ok()?) as usize;
                    let len =
                        u32::from_be_bytes(bytes.get(o + 8..o + 12)?.try_into().ok()?) as usize;
                    return Some((off, len));
                }
            }
            None
        };

        let r = read_xyz(bytes, find(b"rXYZ")?)?;
        let g = read_xyz(bytes, find(b"gXYZ")?)?;
        let b = read_xyz(bytes, find(b"bXYZ")?)?;
        let trc = [
            read_trc(bytes, find(b"rTRC")?)?,
            read_trc(bytes, find(b"gTRC")?)?,
            read_trc(bytes, find(b"bTRC")?)?,
        ];

        // Colorants are PCS-relative (D50). Adapt to D65 so the result can go
        // straight through the sRGB matrix.
        let m_d50 = [r[0], g[0], b[0], r[1], g[1], b[1], r[2], g[2], b[2]];
        let to_xyz_d65 = mat3_mul(BRADFORD_D50_TO_D65, m_d50);

        let icc = IccRgb { trc, to_xyz_d65 };
        if icc.is_srgb() {
            return None;
        }
        Some(icc)
    }

    /// Convert one encoded RGB triple (each `0..=1`) to sRGB (each `0..=1`).
    pub fn to_srgb(&self, rgb: [f32; 3]) -> [f32; 3] {
        to_srgb_with(self.trc_flat().as_ref(), &self.to_xyz_d65, rgb)
    }

    /// The three 256-point curves, concatenated — the form the page IR carries
    /// (it depends on no colour crate, exactly like `Lab`'s white point).
    pub fn trc_flat(&self) -> Vec<f32> {
        let mut out = Vec::with_capacity(768);
        for c in &self.trc {
            out.extend_from_slice(c);
        }
        out
    }

    /// The profile→XYZ(D65) matrix, row-major.
    pub fn matrix(&self) -> [f32; 9] {
        self.to_xyz_d65
    }
}

/// Convert an encoded RGB triple to sRGB from the raw parts a page IR carries:
/// `trc` is three concatenated 256-point curves, `matrix` is row-major
/// profile→XYZ(D65). Out-of-shape input falls back to a pass-through rather
/// than panicking.
pub fn to_srgb_with(trc: &[f32], matrix: &[f32; 9], rgb: [f32; 3]) -> [f32; 3] {
    if trc.len() < 768 {
        return rgb;
    }
    let lin = [
        sample_slice(&trc[0..256], rgb[0]),
        sample_slice(&trc[256..512], rgb[1]),
        sample_slice(&trc[512..768], rgb[2]),
    ];
    let m = matrix;
    let x = m[0] * lin[0] + m[1] * lin[1] + m[2] * lin[2];
    let y = m[3] * lin[0] + m[4] * lin[1] + m[5] * lin[2];
    let z = m[6] * lin[0] + m[7] * lin[1] + m[8] * lin[2];
    cie::xyz_to_srgb(x, y, z)
}

fn sample_slice(trc: &[f32], v: f32) -> f32 {
    let t = v.clamp(0.0, 1.0) * 255.0;
    let lo = t.floor() as usize;
    let hi = (lo + 1).min(255);
    let f = t - lo as f32;
    trc[lo] * (1.0 - f) + trc[hi] * f
}

impl IccRgb {
    /// Whether this profile is sRGB to within a tolerance that cannot show up
    /// in 8-bit output. Checked on the actual conversion rather than on tag
    /// values, so an sRGB profile written with a curve table, a gamma
    /// approximation, or slightly different rounding all compare equal.
    fn is_srgb(&self) -> bool {
        // A diagonal sweep plus the primaries: enough to separate sRGB from
        // Apple RGB (different primaries *and* gamma) and from a pure-gamma
        // variant of sRGB's own primaries.
        const PROBES: [[f32; 3]; 7] = [
            [0.25, 0.25, 0.25],
            [0.5, 0.5, 0.5],
            [0.75, 0.75, 0.75],
            [1.0, 0.0, 0.0],
            [0.0, 1.0, 0.0],
            [0.0, 0.0, 1.0],
            [0.5, 0.25, 0.75],
        ];
        PROBES.iter().all(|p| {
            let out = self.to_srgb(*p);
            (0..3).all(|k| ((out[k] - p[k]) * 255.0).abs() < 0.5)
        })
    }
}

/// A parsed CMYK `A2B0` lookup-table profile (`mft2`/`mft1`), reduced to a
/// CMYK→sRGB evaluator.
///
/// This is the device-CMYK counterpart of [`IccRgb`]: a 4-input, 3-output LUT
/// with a Lab PCS, the shape almost every embedded `/DefaultCMYK` and ICCBased
/// CMYK press profile uses (a `prtr` profile whose `A2B0` is an `mft2`). The
/// evaluation mirrors what PDFium gets from LittleCMS
/// (`IccTransform::CreateTransformSRGB`, `INTENT_PERCEPTUAL`, no black-point
/// compensation): input curves → n-linear CLUT → output curves → v2-encoded Lab
/// (D50) → XYZ → Bradford D50→D65 → sRGB (the same tail [`IccRgb`] uses).
///
/// All three A2B tags of a press profile are typically identical, so the
/// perceptual `A2B0` alone covers PDFium's fixed intent. Non-CMYK inputs, a
/// non-Lab PCS, or a non-lut A2B0 return `None` and keep the arity
/// approximation.
#[derive(Debug, Clone, PartialEq)]
pub struct IccCmyk {
    /// Grid points per input dimension (the CLUT is `grid.pow(4)` cells).
    grid: usize,
    /// One input curve per CMYK channel, normalised `0..=1`.
    in_tables: [Vec<f32>; 4],
    /// `grid^4` cells, each three normalised `0..=1` PCS-Lab-encoded outputs,
    /// first input channel most significant (ICC.1 §10.8).
    clut: Vec<[f32; 3]>,
    /// One output curve per PCS channel (L, a, b), normalised `0..=1`.
    out_tables: [Vec<f32>; 3],
}

impl IccCmyk {
    /// Parse a CMYK→Lab lut ICC profile, or `None` if it is not the
    /// 4-input/3-output Lab-PCS lut shape this evaluates.
    pub fn from_cmyk_profile(bytes: &[u8]) -> Option<IccCmyk> {
        if bytes.len() < 132 || &bytes[36..40] != b"acsp" {
            return None;
        }
        // Data space CMYK, PCS Lab — the only combination this path handles.
        if &bytes[16..20] != b"CMYK" || &bytes[20..24] != b"Lab " {
            return None;
        }
        let tag_count = u32::from_be_bytes(bytes[128..132].try_into().ok()?) as usize;
        if tag_count > 256 {
            return None;
        }
        let mut a2b0: Option<(usize, usize)> = None;
        for i in 0..tag_count {
            let o = 132 + i * 12;
            if bytes.get(o..o + 4)? == b"A2B0" {
                let off = u32::from_be_bytes(bytes.get(o + 4..o + 8)?.try_into().ok()?) as usize;
                let len = u32::from_be_bytes(bytes.get(o + 8..o + 12)?.try_into().ok()?) as usize;
                a2b0 = Some((off, len));
                break;
            }
        }
        parse_lut_cmyk(bytes, a2b0?)
    }

    /// Convert one CMYK tuple (each `0..=1`) to sRGB (each `0..=1`).
    pub fn to_srgb(&self, cmyk: [f32; 4]) -> [f32; 3] {
        cmyk_to_srgb_with(
            self.grid,
            [
                &self.in_tables[0],
                &self.in_tables[1],
                &self.in_tables[2],
                &self.in_tables[3],
            ],
            &self.clut,
            [
                &self.out_tables[0],
                &self.out_tables[1],
                &self.out_tables[2],
            ],
            cmyk,
        )
    }

    /// Export the parsed tables into backend-neutral image IR. Callers should
    /// cache this allocation per profile object.
    pub fn ir_tables(
        &self,
    ) -> (
        u8,
        [std::sync::Arc<[f32]>; 4],
        std::sync::Arc<[[f32; 3]]>,
        [std::sync::Arc<[f32]>; 3],
    ) {
        (
            self.grid as u8,
            std::array::from_fn(|i| std::sync::Arc::from(self.in_tables[i].clone())),
            std::sync::Arc::from(self.clut.clone()),
            std::array::from_fn(|i| std::sync::Arc::from(self.out_tables[i].clone())),
        )
    }
}

/// Evaluate a backend-neutral CMYK ICC lookup transform carried by image IR.
///
/// Invalid geometry falls back to the renderer's frozen DeviceCMYK policy
/// rather than indexing malformed IR.
pub fn cmyk_to_srgb_with(
    grid: usize,
    input_tables: [&[f32]; 4],
    clut: &[[f32; 3]],
    output_tables: [&[f32]; 3],
    cmyk: [f32; 4],
) -> [f32; 3] {
    let Some(cells) = grid.checked_pow(4) else {
        return crate::cmyk_to_rgb(cmyk[0], cmyk[1], cmyk[2], cmyk[3]);
    };
    if grid < 2
        || clut.len() != cells
        || input_tables.iter().any(|table| table.len() < 2)
        || output_tables.iter().any(|table| table.len() < 2)
    {
        return crate::cmyk_to_rgb(cmyk[0], cmyk[1], cmyk[2], cmyk[3]);
    }
    let t = std::array::from_fn(|i| sample_table(input_tables[i], cmyk[i]));
    let enc = clut_interp(grid, clut, t);
    let l = sample_table(output_tables[0], enc[0]) * (65535.0 / 652.8);
    let a = sample_table(output_tables[1], enc[1]) * (65535.0 / 256.0) - 128.0;
    let b = sample_table(output_tables[2], enc[2]) * (65535.0 / 256.0) - 128.0;
    lab_d50_to_srgb(l, a, b)
}

fn clut_interp(grid: usize, clut: &[[f32; 3]], t: [f32; 4]) -> [f32; 3] {
    let last = (grid - 1) as f32;
    let mut base = [0usize; 4];
    let mut frac = [0f32; 4];
    for c in 0..4 {
        let p = t[c].clamp(0.0, 1.0) * last;
        let lo = (p.floor() as usize).min(grid - 1);
        base[c] = lo;
        frac[c] = p - lo as f32;
    }
    let mut out = [0f32; 3];
    for corner in 0..16u32 {
        let mut weight = 1.0f32;
        let mut idx = 0usize;
        for c in 0..4 {
            let hi = (corner >> c) & 1 == 1;
            let coord = if hi {
                (base[c] + 1).min(grid - 1)
            } else {
                base[c]
            };
            weight *= if hi { frac[c] } else { 1.0 - frac[c] };
            idx = idx * grid + coord;
        }
        if weight != 0.0 {
            let cell = clut[idx];
            out[0] += weight * cell[0];
            out[1] += weight * cell[1];
            out[2] += weight * cell[2];
        }
    }
    out
}

/// Parse an `mft2` (16-bit) or `mft1` (8-bit) lut A2B tag with 4 inputs and 3
/// outputs into an [`IccCmyk`]. The 3×3 matrix is ignored: it applies only to an
/// XYZ-input (3-channel) profile, not a CMYK one (ICC.1 §10.8).
fn parse_lut_cmyk(bytes: &[u8], (off, len): (usize, usize)) -> Option<IccCmyk> {
    let tag = bytes.get(off..off + 4)?;
    let end = off.checked_add(len)?;
    if end > bytes.len() {
        return None;
    }
    let body = &bytes[off..end];
    let n_in = *body.get(8)? as usize;
    let n_out = *body.get(9)? as usize;
    let grid = *body.get(10)? as usize;
    if n_in != 4 || n_out != 3 || !(2..=64).contains(&grid) {
        return None;
    }
    let cells = grid.checked_pow(4)?;

    match tag {
        b"mft2" => {
            let n_in_entries = u16::from_be_bytes(body.get(48..50)?.try_into().ok()?) as usize;
            let n_out_entries = u16::from_be_bytes(body.get(50..52)?.try_into().ok()?) as usize;
            if n_in_entries < 2 || n_out_entries < 2 {
                return None;
            }
            let mut pos = 52usize;
            let read_tbl = |data: &[u8], start: usize, entries: usize| -> Option<Vec<f32>> {
                let span = data.get(start..start + entries * 2)?;
                Some(
                    (0..entries)
                        .map(|i| {
                            u16::from_be_bytes([span[i * 2], span[i * 2 + 1]]) as f32 / 65535.0
                        })
                        .collect(),
                )
            };
            let mut in_tables: [Vec<f32>; 4] = Default::default();
            for slot in &mut in_tables {
                *slot = read_tbl(body, pos, n_in_entries)?;
                pos += n_in_entries * 2;
            }
            let clut_span = body.get(pos..pos + cells * 3 * 2)?;
            let clut: Vec<[f32; 3]> = (0..cells)
                .map(|i| {
                    let b = i * 6;
                    [
                        u16::from_be_bytes([clut_span[b], clut_span[b + 1]]) as f32 / 65535.0,
                        u16::from_be_bytes([clut_span[b + 2], clut_span[b + 3]]) as f32 / 65535.0,
                        u16::from_be_bytes([clut_span[b + 4], clut_span[b + 5]]) as f32 / 65535.0,
                    ]
                })
                .collect();
            pos += cells * 3 * 2;
            let mut out_tables: [Vec<f32>; 3] = Default::default();
            for slot in &mut out_tables {
                *slot = read_tbl(body, pos, n_out_entries)?;
                pos += n_out_entries * 2;
            }
            Some(IccCmyk {
                grid,
                in_tables,
                clut,
                out_tables,
            })
        }
        b"mft1" => {
            // 8-bit lut: fixed 256-entry input/output tables, no entry counts.
            let mut pos = 48usize;
            let read_tbl = |data: &[u8], start: usize| -> Option<Vec<f32>> {
                let span = data.get(start..start + 256)?;
                Some(span.iter().map(|&v| v as f32 / 255.0).collect())
            };
            let mut in_tables: [Vec<f32>; 4] = Default::default();
            for slot in &mut in_tables {
                *slot = read_tbl(body, pos)?;
                pos += 256;
            }
            let clut_span = body.get(pos..pos + cells * 3)?;
            let clut: Vec<[f32; 3]> = (0..cells)
                .map(|i| {
                    [
                        clut_span[i * 3] as f32 / 255.0,
                        clut_span[i * 3 + 1] as f32 / 255.0,
                        clut_span[i * 3 + 2] as f32 / 255.0,
                    ]
                })
                .collect();
            pos += cells * 3;
            let mut out_tables: [Vec<f32>; 3] = Default::default();
            for slot in &mut out_tables {
                *slot = read_tbl(body, pos)?;
                pos += 256;
            }
            Some(IccCmyk {
                grid,
                in_tables,
                clut,
                out_tables,
            })
        }
        _ => None,
    }
}

/// Sample a normalised curve table (any length ≥ 2) with linear interpolation.
fn sample_table(table: &[f32], v: f32) -> f32 {
    if table.len() < 2 {
        return v.clamp(0.0, 1.0);
    }
    let t = v.clamp(0.0, 1.0) * (table.len() - 1) as f32;
    let lo = t.floor() as usize;
    let hi = (lo + 1).min(table.len() - 1);
    let f = t - lo as f32;
    table[lo] * (1.0 - f) + table[hi] * f
}

/// Decode CIE Lab (D50 PCS) to sRGB, matching the tail LittleCMS applies when
/// connecting a Lab-PCS profile to its built-in sRGB profile: Lab→XYZ(D50),
/// Bradford D50→D65, then the D65 sRGB matrix + transfer ([`cie::xyz_to_srgb`]).
fn lab_d50_to_srgb(l: f32, a: f32, b: f32) -> [f32; 3] {
    // D50 PCS white (ICC.1 §7.2.16).
    const XN: f32 = 0.9642;
    const YN: f32 = 1.0;
    const ZN: f32 = 0.8249;
    let fy = (l + 16.0) / 116.0;
    let fx = fy + a / 500.0;
    let fz = fy - b / 200.0;
    let g = |t: f32| -> f32 {
        if t > 6.0 / 29.0 {
            t * t * t
        } else {
            3.0 * (6.0 / 29.0) * (6.0 / 29.0) * (t - 4.0 / 29.0)
        }
    };
    let (x, y, z) = (XN * g(fx), YN * g(fy), ZN * g(fz));
    let d65 = [
        BRADFORD_D50_TO_D65[0] * x + BRADFORD_D50_TO_D65[1] * y + BRADFORD_D50_TO_D65[2] * z,
        BRADFORD_D50_TO_D65[3] * x + BRADFORD_D50_TO_D65[4] * y + BRADFORD_D50_TO_D65[5] * z,
        BRADFORD_D50_TO_D65[6] * x + BRADFORD_D50_TO_D65[7] * y + BRADFORD_D50_TO_D65[8] * z,
    ];
    cie::xyz_to_srgb(d65[0], d65[1], d65[2])
}

/// Read an `XYZType` tag as a D50 XYZ triple.
fn read_xyz(bytes: &[u8], (off, len): (usize, usize)) -> Option<[f32; 3]> {
    if len < 20 || bytes.get(off..off + 4)? != b"XYZ " {
        return None;
    }
    let at = |i: usize| -> Option<f32> {
        let raw = i32::from_be_bytes(bytes.get(i..i + 4)?.try_into().ok()?);
        Some(raw as f32 / 65536.0)
    };
    Some([at(off + 8)?, at(off + 12)?, at(off + 16)?])
}

/// Read a `curveType` or `parametricCurveType` tag, sampled to 256 points.
fn read_trc(bytes: &[u8], (off, len): (usize, usize)) -> Option<[f32; 256]> {
    let kind = bytes.get(off..off + 4)?;
    let mut out = [0f32; 256];
    match kind {
        b"curv" => {
            if len < 12 {
                return None;
            }
            let count = u32::from_be_bytes(bytes.get(off + 8..off + 12)?.try_into().ok()?) as usize;
            match count {
                // Identity.
                0 => {
                    for (i, v) in out.iter_mut().enumerate() {
                        *v = i as f32 / 255.0;
                    }
                }
                // A single u8.8 gamma.
                1 => {
                    let g = u16::from_be_bytes(bytes.get(off + 12..off + 14)?.try_into().ok()?)
                        as f32
                        / 256.0;
                    if !(0.1..=10.0).contains(&g) {
                        return None;
                    }
                    for (i, v) in out.iter_mut().enumerate() {
                        *v = (i as f32 / 255.0).powf(g);
                    }
                }
                // A sampled curve of u16 points, linearly interpolated.
                _ => {
                    let table = bytes.get(off + 12..off + 12 + count * 2)?;
                    let point = |i: usize| -> f32 {
                        u16::from_be_bytes([table[i * 2], table[i * 2 + 1]]) as f32 / 65535.0
                    };
                    for (i, v) in out.iter_mut().enumerate() {
                        let t = (i as f32 / 255.0) * (count - 1) as f32;
                        let lo = t.floor() as usize;
                        let hi = (lo + 1).min(count - 1);
                        let f = t - lo as f32;
                        *v = point(lo) * (1.0 - f) + point(hi) * f;
                    }
                }
            }
        }
        b"para" => {
            // ICC parametric curves: type 0 is a plain gamma, types 1-4 add the
            // linear toe sRGB uses. Parameters are s15Fixed16.
            let ty = u16::from_be_bytes(bytes.get(off + 8..off + 10)?.try_into().ok()?);
            let p = |i: usize| -> Option<f32> {
                let o = off + 12 + i * 4;
                Some(i32::from_be_bytes(bytes.get(o..o + 4)?.try_into().ok()?) as f32 / 65536.0)
            };
            let g = p(0)?;
            if !(0.1..=10.0).contains(&g) {
                return None;
            }
            for (i, v) in out.iter_mut().enumerate() {
                let x = i as f32 / 255.0;
                *v = match ty {
                    0 => x.powf(g),
                    1 => {
                        let (a, b) = (p(1)?, p(2)?);
                        if x >= -b / a {
                            (a * x + b).powf(g)
                        } else {
                            0.0
                        }
                    }
                    2 => {
                        let (a, b, c) = (p(1)?, p(2)?, p(3)?);
                        if x >= -b / a {
                            (a * x + b).powf(g) + c
                        } else {
                            c
                        }
                    }
                    3 => {
                        let (a, b, c, d) = (p(1)?, p(2)?, p(3)?, p(4)?);
                        if x >= d { (a * x + b).powf(g) } else { c * x }
                    }
                    4 => {
                        let (a, b, c, d, e, f) = (p(1)?, p(2)?, p(3)?, p(4)?, p(5)?, p(6)?);
                        if x >= d {
                            (a * x + b).powf(g) + e
                        } else {
                            c * x + f
                        }
                    }
                    _ => return None,
                };
            }
        }
        _ => return None,
    }
    Some(out)
}

/// Sample a 256-point TRC with linear interpolation.
fn sample_trc(trc: &[f32; 256], v: f32) -> f32 {
    let t = v.clamp(0.0, 1.0) * 255.0;
    let lo = t.floor() as usize;
    let hi = (lo + 1).min(255);
    let f = t - lo as f32;
    trc[lo] * (1.0 - f) + trc[hi] * f
}

fn mat3_mul(a: [f32; 9], b: [f32; 9]) -> [f32; 9] {
    let mut out = [0f32; 9];
    for r in 0..3 {
        for c in 0..3 {
            out[r * 3 + c] = a[r * 3] * b[c] + a[r * 3 + 1] * b[3 + c] + a[r * 3 + 2] * b[6 + c];
        }
    }
    out
}

/// Bradford chromatic adaptation from the ICC PCS white (D50) to D65, which is
/// what the sRGB matrix in [`cie::xyz_to_srgb`] expects.
const BRADFORD_D50_TO_D65: [f32; 9] = [
    0.955_576_6,
    -0.023_039_3,
    0.063_163_6,
    -0.028_289_5,
    1.009_941_6,
    0.021_007_7,
    0.012_298_2,
    -0.020_483_0,
    1.329_909_8,
];

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    /// Build a minimal matrix/TRC profile with the given primaries and a single
    /// gamma, in the layout `from_profile` reads.
    fn profile(primaries: [[f32; 3]; 3], gamma: f32) -> Vec<u8> {
        profile_with(primaries, Some(gamma))
    }

    /// `gamma: None` emits the exact sRGB parametric (type 3) curve instead of
    /// a plain power law — what a real embedded sRGB profile carries.
    fn profile_with(primaries: [[f32; 3]; 3], gamma: Option<f32>) -> Vec<u8> {
        let tags: [&[u8; 4]; 6] = [b"rXYZ", b"gXYZ", b"bXYZ", b"rTRC", b"gTRC", b"bTRC"];
        let mut header = vec![0u8; 128];
        header[16..20].copy_from_slice(b"RGB ");
        header[20..24].copy_from_slice(b"XYZ ");
        header[36..40].copy_from_slice(b"acsp");

        let mut table = Vec::new();
        let mut body = Vec::new();
        let base = 132 + tags.len() * 12;
        for (i, tag) in tags.iter().enumerate() {
            let off = base + body.len();
            let data: Vec<u8> = if i < 3 {
                let mut d = b"XYZ \0\0\0\0".to_vec();
                for c in 0..3 {
                    d.extend(((primaries[i][c] * 65536.0) as i32).to_be_bytes());
                }
                d
            } else if let Some(g) = gamma {
                let mut d = b"curv\0\0\0\0".to_vec();
                d.extend(1u32.to_be_bytes());
                d.extend((((g * 256.0) as u16).to_be_bytes()).iter());
                d
            } else {
                // sRGB: para type 3, g=2.4 a=1/1.055 b=0.055/1.055 c=1/12.92
                // d=0.04045.
                let mut d = b"para\0\0\0\0".to_vec();
                d.extend(3u16.to_be_bytes());
                d.extend(0u16.to_be_bytes());
                for v in [2.4f32, 1.0 / 1.055, 0.055 / 1.055, 1.0 / 12.92, 0.04045] {
                    d.extend(((v * 65536.0) as i32).to_be_bytes());
                }
                d
            };
            table.extend(tag.iter());
            table.extend((off as u32).to_be_bytes());
            table.extend((data.len() as u32).to_be_bytes());
            body.extend(data);
        }
        let mut out = header;
        out.extend((tags.len() as u32).to_be_bytes());
        out.extend(table);
        out.extend(body);
        let len = out.len() as u32;
        out[0..4].copy_from_slice(&len.to_be_bytes());
        out
    }

    /// sRGB's own D50-adapted primaries.
    const SRGB_D50: [[f32; 3]; 3] = [
        [0.436_065, 0.222_493, 0.013_916],
        [0.385_147, 0.716_888, 0.097_076],
        [0.143_066, 0.060_608, 0.714_096],
    ];
    /// Apple RGB, as measured off the corpus profile that exposed this bug.
    const APPLE_D50: [[f32; 3]; 3] = [
        [0.454_3, 0.242_6, 0.014_8],
        [0.353_3, 0.674_4, 0.090_4],
        [0.156_6, 0.083_4, 0.719_5],
    ];

    #[test]
    fn an_srgb_profile_is_declined_so_the_common_case_stays_untouched() {
        // What a real embedded sRGB profile carries: sRGB primaries and the
        // exact parametric curve. The conversion would be the identity, so the
        // profile must be declined and the pass-through kept byte-identical —
        // this is what bounds the blast radius of applying ICC at all.
        assert!(
            IccRgb::from_profile(&profile_with(SRGB_D50, None)).is_none(),
            "an sRGB profile must not produce a converter"
        );
    }

    #[test]
    fn a_pure_gamma_2_2_profile_is_not_mistaken_for_srgb() {
        // sRGB's curve is a 2.4 power law with a linear toe, not a 2.2 power
        // law; the two differ in the shadows. Accepting this one is correct —
        // the point of the sRGB check is to skip *identity* conversions, not to
        // wave through anything vaguely close.
        let icc = IccRgb::from_profile(&profile(SRGB_D50, 2.2))
            .expect("pure gamma 2.2 differs from sRGB and must convert");
        // Midtones stay close (the curves nearly agree there)...
        let mid = icc
            .to_srgb([0.5, 0.5, 0.5])
            .map(|c| (c * 255.0).round() as i32);
        assert!(mid.iter().all(|&c| (c - 128).abs() <= 4), "midtone {mid:?}");
        // ...while the shadows are where the toe shows.
        let dark = icc
            .to_srgb([0.05, 0.05, 0.05])
            .map(|c| (c * 255.0).round() as i32);
        assert!(
            dark.iter().all(|&c| c < 13),
            "shadow should darken: {dark:?}"
        );
    }

    #[test]
    fn a_gamma_1_8_apple_profile_lightens_midtones() {
        let icc = IccRgb::from_profile(&profile(APPLE_D50, 1.8))
            .expect("Apple RGB gamma 1.8 is a matrix/TRC profile and is not sRGB");
        // The corpus case: palette entry (195, 151, 97) on a scanned cover.
        // Reading it as if it were already sRGB renders ~10/255 too dark per
        // channel; through the profile it lands near (207, 167, 116).
        let out = icc.to_srgb([195.0 / 255.0, 151.0 / 255.0, 97.0 / 255.0]);
        let out8 = out.map(|c| (c * 255.0).round() as i32);
        assert!(
            (out8[0] - 207).abs() <= 3 && (out8[1] - 167).abs() <= 3 && (out8[2] - 116).abs() <= 3,
            "expected ~(207,167,116), got {out8:?}"
        );
        // Every channel must move *up* — the whole class is under-brightening.
        assert!(out8[0] > 195 && out8[1] > 151 && out8[2] > 97, "{out8:?}");
    }

    #[test]
    fn black_and_white_are_preserved() {
        let icc = IccRgb::from_profile(&profile(APPLE_D50, 1.8)).unwrap();
        let black = icc
            .to_srgb([0.0, 0.0, 0.0])
            .map(|c| (c * 255.0).round() as i32);
        let white = icc
            .to_srgb([1.0, 1.0, 1.0])
            .map(|c| (c * 255.0).round() as i32);
        assert_eq!(black, [0, 0, 0], "black must stay black");
        assert!(
            white.iter().all(|&c| (c - 255).abs() <= 1),
            "white must stay white, got {white:?}"
        );
    }

    #[test]
    fn junk_and_non_matrix_profiles_are_declined() {
        assert!(IccRgb::from_profile(b"").is_none());
        assert!(IccRgb::from_profile(&[0u8; 200]).is_none());
        // Right header, no colorant tags.
        let mut p = vec![0u8; 132];
        p[16..20].copy_from_slice(b"RGB ");
        p[20..24].copy_from_slice(b"XYZ ");
        p[36..40].copy_from_slice(b"acsp");
        assert!(IccRgb::from_profile(&p).is_none());
    }

    // --- IccCmyk (A2B0 lut) ------------------------------------------------

    /// Build a minimal `mft2` CMYK→Lab profile with a `grid`²-per-axis CLUT
    /// (here grid 2, i.e. the 16 CMYK corners), identity input/output curves,
    /// and the caller-supplied 16 CLUT cells as raw (L16, a16, b16) triples.
    fn cmyk_lut_profile(cells: &[[u16; 3]; 16]) -> Vec<u8> {
        let grid = 2u8;
        // mft2 body.
        let mut body = Vec::new();
        body.extend_from_slice(b"mft2\0\0\0\0");
        body.push(4); // n_in
        body.push(3); // n_out
        body.push(grid);
        body.push(0);
        // identity 3x3 matrix (ignored for CMYK, but present in the layout).
        for v in [1i32, 0, 0, 0, 1, 0, 0, 0, 1] {
            body.extend_from_slice(&(v * 65536).to_be_bytes());
        }
        body.extend_from_slice(&2u16.to_be_bytes()); // n_in_entries
        body.extend_from_slice(&2u16.to_be_bytes()); // n_out_entries
        // 4 input curves, each linear [0, 65535].
        for _ in 0..4 {
            body.extend_from_slice(&0u16.to_be_bytes());
            body.extend_from_slice(&65535u16.to_be_bytes());
        }
        // 16 CLUT cells.
        for c in cells {
            for v in c {
                body.extend_from_slice(&v.to_be_bytes());
            }
        }
        // 3 output curves, each linear [0, 65535].
        for _ in 0..3 {
            body.extend_from_slice(&0u16.to_be_bytes());
            body.extend_from_slice(&65535u16.to_be_bytes());
        }

        // 128-byte header + one A2B0 tag pointing at the body.
        let mut header = vec![0u8; 128];
        header[16..20].copy_from_slice(b"CMYK");
        header[20..24].copy_from_slice(b"Lab ");
        header[36..40].copy_from_slice(b"acsp");
        let tag_off = 132 + 12;
        let mut out = header;
        out.extend_from_slice(&1u32.to_be_bytes()); // one tag
        out.extend_from_slice(b"A2B0");
        out.extend_from_slice(&(tag_off as u32).to_be_bytes());
        out.extend_from_slice(&(body.len() as u32).to_be_bytes());
        out.extend_from_slice(&body);
        out
    }

    /// v2-encoded Lab: L over 0..0xFF00, a/b centred at 0x8000.
    fn lab16(l: f32, a: f32, b: f32) -> [u16; 3] {
        [
            (l / 100.0 * 65280.0).round() as u16,
            ((a + 128.0) * 256.0).round().clamp(0.0, 65535.0) as u16,
            ((b + 128.0) * 256.0).round().clamp(0.0, 65535.0) as u16,
        ]
    }

    #[test]
    fn cmyk_lut_white_and_black_are_preserved() {
        // Every corner maps to Lab white except pure K, which is Lab black.
        let mut cells = [lab16(100.0, 0.0, 0.0); 16];
        // Cell index for (c,m,y,k) is (((c*2+m)*2+y)*2+k); (0,0,0,1) = 1.
        cells[1] = lab16(0.0, 0.0, 0.0);
        let prof = IccCmyk::from_cmyk_profile(&cmyk_lut_profile(&cells)).expect("parse mft2");

        let white = prof
            .to_srgb([0.0, 0.0, 0.0, 0.0])
            .map(|c| (c * 255.0).round() as i32);
        let black = prof
            .to_srgb([0.0, 0.0, 0.0, 1.0])
            .map(|c| (c * 255.0).round() as i32);
        assert!(
            white.iter().all(|&c| (c - 255).abs() <= 1),
            "white {white:?}"
        );
        assert_eq!(black, [0, 0, 0], "black {black:?}");
    }

    #[test]
    fn cmyk_lut_interpolates_along_an_axis() {
        // White at K=0, black at K=1: a mid K must land a mid grey (the CLUT
        // interpolates and L* is roughly linear here).
        let mut cells = [lab16(100.0, 0.0, 0.0); 16];
        cells[1] = lab16(0.0, 0.0, 0.0);
        let prof = IccCmyk::from_cmyk_profile(&cmyk_lut_profile(&cells)).unwrap();
        let mid = prof
            .to_srgb([0.0, 0.0, 0.0, 0.5])
            .map(|c| (c * 255.0).round() as i32);
        assert!(
            mid.iter().all(|&c| (30..=225).contains(&c)),
            "mid grey {mid:?}"
        );
        // Neutral: channels stay close together.
        assert!(
            mid.iter().max().unwrap() - mid.iter().min().unwrap() <= 12,
            "neutral {mid:?}"
        );
    }

    #[test]
    fn non_cmyk_or_non_lut_profiles_are_declined() {
        // An RGB/XYZ matrix profile is not a CMYK lut.
        assert!(IccCmyk::from_cmyk_profile(&profile(SRGB_D50, 2.2)).is_none());
        assert!(IccCmyk::from_cmyk_profile(b"").is_none());
        // CMYK header but no A2B0 tag.
        let mut p = vec![0u8; 132];
        p[16..20].copy_from_slice(b"CMYK");
        p[20..24].copy_from_slice(b"Lab ");
        p[36..40].copy_from_slice(b"acsp");
        assert!(IccCmyk::from_cmyk_profile(&p).is_none());
    }
}
