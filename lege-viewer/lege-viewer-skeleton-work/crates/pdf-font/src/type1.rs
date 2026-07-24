//! Native Type 1 font support (fonts.md Font Phase 5).
//!
//! Type 1 is the original PostScript outline format and is still embedded in
//! PDFs as a bare `/FontFile` (as opposed to `/FontFile2` TrueType and
//! `/FontFile3` CFF). Skrifa does not read it — it is not an SFNT — so
//! without this a common class of documents falls back to substituted faces
//! or boxes.
//!
//! The format (Adobe's *Type 1 Font Format*, "the black book"):
//!
//! ```text
//! %!PS-AdobeFont-1.0 ...        cleartext: /FontMatrix, /Encoding
//! ... currentfile eexec
//! <eexec-encrypted binary>      /Private: /lenIV, /Subrs, /CharStrings
//! 0000000...                    512 zeros + cleartomark
//! ```
//!
//! Two layers of the same weak cipher guard it: the section after `eexec`
//! (key 55665) and each individual charstring (key 4330). Neither is a
//! security measure — Adobe documented both — so this decrypts them and
//! interprets the charstrings directly.
//!
//! Scope: outlines and metrics, unhinted (Type 1 stem hints are parsed and
//! discarded, which is what fonts.md asks for at this phase). Multiple
//! Master interpolation is not implemented.

use std::collections::HashMap;
use std::sync::Arc;

use crate::engine::{Outline, OutlineVerb};

/// A parsed Type 1 font: glyph outlines by name, plus the font's built-in
/// encoding.
#[derive(Debug)]
pub struct Type1Font {
    /// Charstrings in a stable order; the index is the glyph id we expose.
    glyphs: Vec<Type1Glyph>,
    /// Glyph name → index into `glyphs`.
    by_name: HashMap<Vec<u8>, u32>,
    /// The font's built-in encoding: code → index into `glyphs`.
    encoding: Box<[Option<u32>; 256]>,
    /// Local subroutines, referenced by `callsubr`.
    subrs: Vec<Vec<u8>>,
    /// `/FontMatrix`'s scale, expressed as units per em (usually 1000).
    units_per_em: u16,
}

#[derive(Debug)]
struct Type1Glyph {
    name: Vec<u8>,
    /// Decrypted charstring bytes.
    code: Vec<u8>,
}

impl Type1Font {
    /// Parse a bare Type 1 program (PFA or PFB). Returns `None` if this is
    /// not Type 1 or is too damaged to yield charstrings.
    pub fn parse(data: &[u8]) -> Option<Type1Font> {
        let data = unwrap_pfb(data);
        if !looks_like_type1(&data) {
            return None;
        }
        let eexec_at = find(&data, b"eexec")?;
        let clear = &data[..eexec_at];

        // The encrypted section starts after `eexec` and any following
        // whitespace, and may be hex (PFA) or raw binary (PFB).
        let mut p = eexec_at + 5;
        while p < data.len() && matches!(data[p], b'\r' | b'\n' | b' ' | b'\t') {
            p += 1;
        }
        let enc = &data[p..];
        let binary = decode_eexec_body(enc);
        let private = decrypt(&binary, 55665, 4);

        // `/lenIV` is the count of random plaintext bytes prefixed to every
        // charstring — but a **negative** value is the convention for "these
        // charstrings are not encrypted at all" (Founder's FzBookMaker writes
        // `/lenIV -1`). Clamping that to 0 still ran the 4330 cipher over
        // plaintext, scrambling every outline into a few stray segments while
        // the font, its encoding and its glyph ids all resolved perfectly —
        // pdfbox/3874.pdf rendered as confetti.
        let len_iv = read_int_after(&private, b"/lenIV").unwrap_or(4).clamp(-1, 16) as i32;
        let subrs = parse_subrs(&private, len_iv);
        let (glyphs, by_name) = parse_charstrings(&private, len_iv)?;
        if glyphs.is_empty() {
            return None;
        }
        let encoding = parse_encoding(clear, &by_name);
        let units_per_em = font_matrix_upem(clear);

        Some(Type1Font { glyphs, by_name, encoding, subrs, units_per_em })
    }

    pub fn num_glyphs(&self) -> u16 {
        self.glyphs.len().min(u16::MAX as usize) as u16
    }

    pub fn units_per_em(&self) -> u16 {
        self.units_per_em
    }

    pub fn gid_for_name(&self, name: &[u8]) -> Option<u32> {
        self.by_name.get(name).copied()
    }

    /// The glyph the font's own `/Encoding` assigns to `code`.
    pub fn gid_for_code(&self, code: u8) -> Option<u32> {
        self.encoding[code as usize]
    }

    /// Resolve a Unicode scalar by matching it against the glyph names.
    pub fn gid_for_char(&self, c: char) -> Option<u32> {
        self.glyphs
            .iter()
            .position(|g| crate::encoding::glyph_name_to_char(&g.name) == Some(c))
            .map(|i| i as u32)
    }

    pub fn glyph_name(&self, gid: u32) -> Option<&[u8]> {
        self.glyphs.get(gid as usize).map(|g| g.name.as_slice())
    }

    /// The outline of `gid` in font units, or `None` for an empty glyph.
    pub fn outline(&self, gid: u32) -> Option<Outline> {
        let mut ctx = Interp::new(self);
        ctx.run_glyph(gid, 0)?;
        let outline = ctx.finish();
        (!outline.is_empty()).then_some(outline)
    }

    /// The advance width of `gid` in font units (from the charstring's
    /// `hsbw`/`sbw`, which is where Type 1 keeps its metrics).
    pub fn advance(&self, gid: u32) -> Option<f32> {
        let mut ctx = Interp::new(self);
        ctx.run_glyph(gid, 0)?;
        Some(ctx.advance)
    }
}

/// Strip PFB segment headers (`0x80 0x01/0x02 <len32le>`), yielding the raw
/// PostScript+binary stream. A PFA is returned unchanged.
fn unwrap_pfb(data: &[u8]) -> Vec<u8> {
    if data.first() != Some(&0x80) {
        return data.to_vec();
    }
    let mut out = Vec::with_capacity(data.len());
    let mut p = 0usize;
    while p + 6 <= data.len() && data[p] == 0x80 {
        let kind = data[p + 1];
        if kind == 3 {
            break; // EOF segment
        }
        let len = u32::from_le_bytes([data[p + 2], data[p + 3], data[p + 4], data[p + 5]]) as usize;
        let start = p + 6;
        let end = start.saturating_add(len).min(data.len());
        out.extend_from_slice(&data[start..end]);
        p = end;
    }
    out
}

fn looks_like_type1(data: &[u8]) -> bool {
    let head = &data[..data.len().min(1024)];
    find(head, b"%!PS-AdobeFont").is_some()
        || find(head, b"%!FontType1").is_some()
        || find(head, b"%!PS-Adobe").is_some() && find(head, b"eexec").is_some()
}

/// The eexec body is either raw binary or ASCII hex. Adobe's rule: if the
/// first four bytes are all hex digits, it is hex.
fn decode_eexec_body(enc: &[u8]) -> Vec<u8> {
    let is_hex = enc.iter().filter(|b| !b.is_ascii_whitespace()).take(4).count() == 4
        && enc
            .iter()
            .filter(|b| !b.is_ascii_whitespace())
            .take(4)
            .all(|b| b.is_ascii_hexdigit());
    if !is_hex {
        return enc.to_vec();
    }
    let mut out = Vec::with_capacity(enc.len() / 2);
    let mut hi: Option<u8> = None;
    for &b in enc {
        let Some(v) = hex_val(b) else { continue };
        match hi {
            None => hi = Some(v),
            Some(h) => {
                out.push((h << 4) | v);
                hi = None;
            }
        }
    }
    out
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Decrypt one charstring (or subroutine) body under `/lenIV`.
///
/// A negative `len_iv` means the body is stored as plaintext; running the
/// cipher over it would destroy the outline.
fn decrypt_charstring(bin: &[u8], len_iv: i32) -> Vec<u8> {
    if len_iv < 0 {
        return bin.to_vec();
    }
    decrypt(bin, 4330, len_iv as usize)
}

/// Adobe's eexec/charstring cipher (Type 1 spec §7.2). `skip` leading
/// plaintext bytes are discarded (4 for eexec, `lenIV` for charstrings).
fn decrypt(data: &[u8], key: u16, skip: usize) -> Vec<u8> {
    const C1: u16 = 52845;
    const C2: u16 = 22719;
    let mut r = key;
    let mut out = Vec::with_capacity(data.len().saturating_sub(skip));
    for (i, &c) in data.iter().enumerate() {
        let p = c ^ (r >> 8) as u8;
        r = (c as u16).wrapping_add(r).wrapping_mul(C1).wrapping_add(C2);
        if i >= skip {
            out.push(p);
        }
    }
    out
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn read_int_after(data: &[u8], key: &[u8]) -> Option<i64> {
    let at = find(data, key)? + key.len();
    let rest = &data[at..];
    let start = rest.iter().position(|b| b.is_ascii_digit() || *b == b'-')?;
    let mut end = start;
    while end < rest.len() && (rest[end].is_ascii_digit() || (end == start && rest[end] == b'-')) {
        end += 1;
    }
    std::str::from_utf8(&rest[start..end]).ok()?.parse().ok()
}

/// `/FontMatrix [0.001 0 0 0.001 0 0]` → 1000 units per em.
fn font_matrix_upem(clear: &[u8]) -> u16 {
    let Some(at) = find(clear, b"/FontMatrix") else { return 1000 };
    let rest = &clear[at..(at + 120).min(clear.len())];
    let Some(open) = rest.iter().position(|b| *b == b'[') else { return 1000 };
    let Some(close) = rest.iter().position(|b| *b == b']') else { return 1000 };
    if close <= open {
        return 1000;
    }
    let body = std::str::from_utf8(&rest[open + 1..close]).unwrap_or("");
    let first: Option<f64> = body.split_whitespace().next().and_then(|s| s.parse().ok());
    match first {
        Some(s) if s > 0.0 => ((1.0 / s).round() as i64).clamp(16, 16384) as u16,
        _ => 1000,
    }
}

/// Parse `/Subrs N array-of RD <binary> NP` entries.
fn parse_subrs(private: &[u8], len_iv: i32) -> Vec<Vec<u8>> {
    let Some(at) = find(private, b"/Subrs") else { return Vec::new() };
    let mut subrs: Vec<Vec<u8>> = Vec::new();
    let mut p = at;
    // Entries look like: `dup <index> <len> RD <bytes> NP`
    while let Some(next) = find(&private[p..], b"dup ") {
        let mut q = p + next + 4;
        let Some((idx, after)) = read_uint(private, q) else { break };
        q = after;
        let Some((len, after)) = read_uint(private, q) else { break };
        q = after;
        let Some(bin) = binary_after_token(private, q, len) else { break };
        if subrs.len() <= idx {
            subrs.resize(idx + 1, Vec::new());
        }
        subrs[idx] = decrypt_charstring(bin, len_iv);
        p = q + len;
        // /CharStrings follows /Subrs; stop before running into it.
        if let Some(cs) = find(private, b"/CharStrings")
            && p > cs
        {
            break;
        }
    }
    subrs
}

/// Parse `/CharStrings N dict dup begin /name <len> RD <binary> ND ... end`.
#[allow(clippy::type_complexity)]
fn parse_charstrings(
    private: &[u8],
    len_iv: i32,
) -> Option<(Vec<Type1Glyph>, HashMap<Vec<u8>, u32>)> {
    let at = find(private, b"/CharStrings")?;
    let mut glyphs: Vec<Type1Glyph> = Vec::new();
    let mut by_name: HashMap<Vec<u8>, u32> = HashMap::new();
    let mut p = at + b"/CharStrings".len();

    while p < private.len() {
        // Next `/name len RD <binary>`.
        let Some(slash) = private[p..].iter().position(|b| *b == b'/') else { break };
        let mut q = p + slash + 1;
        let name_start = q;
        while q < private.len() && !is_ps_delim(private[q]) {
            q += 1;
        }
        let name = private[name_start..q].to_vec();
        if name.is_empty() {
            p = q + 1;
            continue;
        }
        let Some((len, after)) = read_uint(private, q) else {
            p = q;
            continue;
        };
        let Some(bin) = binary_after_token(private, after, len) else {
            p = after;
            continue;
        };
        let code = decrypt_charstring(bin, len_iv);
        let gid = glyphs.len() as u32;
        by_name.entry(name.clone()).or_insert(gid);
        glyphs.push(Type1Glyph { name, code });
        p = (after + len).min(private.len());
    }
    Some((glyphs, by_name))
}

fn is_ps_delim(b: u8) -> bool {
    b.is_ascii_whitespace() || matches!(b, b'/' | b'(' | b')' | b'[' | b']' | b'{' | b'}' | b'<' | b'>')
}

/// Read a non-negative integer starting at/after `p`; returns it and the
/// offset just past it.
fn read_uint(data: &[u8], mut p: usize) -> Option<(usize, usize)> {
    while p < data.len() && data[p].is_ascii_whitespace() {
        p += 1;
    }
    let start = p;
    while p < data.len() && data[p].is_ascii_digit() {
        p += 1;
    }
    if p == start {
        return None;
    }
    let v: usize = std::str::from_utf8(&data[start..p]).ok()?.parse().ok()?;
    Some((v, p))
}

/// After a length comes a token (`RD`, `-|`, ...) then exactly one space,
/// then `len` raw bytes.
fn binary_after_token(data: &[u8], mut p: usize, len: usize) -> Option<&[u8]> {
    while p < data.len() && data[p].is_ascii_whitespace() {
        p += 1;
    }
    // The token itself (RD / -| / ND / |-).
    let tok_start = p;
    while p < data.len() && !data[p].is_ascii_whitespace() {
        p += 1;
    }
    if p == tok_start {
        return None;
    }
    p += 1; // exactly one space separates the token from the binary
    data.get(p..p + len)
}

/// The cleartext `/Encoding`: either `StandardEncoding` or a sequence of
/// `dup <code> /<name> put`.
fn parse_encoding(clear: &[u8], by_name: &HashMap<Vec<u8>, u32>) -> Box<[Option<u32>; 256]> {
    let mut enc: Box<[Option<u32>; 256]> = Box::new([None; 256]);
    let Some(at) = find(clear, b"/Encoding") else { return enc };
    let region = &clear[at..];
    if find(&region[..region.len().min(64)], b"StandardEncoding").is_some() {
        for code in 0..256usize {
            if let Some(c) = crate::encoding::BaseEncoding::Standard.to_char(code as u8) {
                let mut buf = [0u8; 4];
                let s = c.encode_utf8(&mut buf);
                // Standard encoding maps through names; approximate by the
                // single-letter names the AGL subset covers.
                if let Some(&gid) = by_name.get(s.as_bytes()) {
                    enc[code] = Some(gid);
                }
            }
        }
        return enc;
    }
    let mut p = 0usize;
    while let Some(next) = find(&region[p..], b"dup ") {
        let q = p + next + 4;
        let Some((code, after)) = read_uint(region, q) else {
            p = q;
            continue;
        };
        let mut r = after;
        while r < region.len() && region[r].is_ascii_whitespace() {
            r += 1;
        }
        if region.get(r) != Some(&b'/') {
            p = after;
            continue;
        }
        r += 1;
        let ns = r;
        while r < region.len() && !is_ps_delim(region[r]) {
            r += 1;
        }
        if code < 256
            && let Some(&gid) = by_name.get(&region[ns..r])
        {
            enc[code] = Some(gid);
        }
        p = r;
        // `readonly def` closes the encoding vector.
        if let Some(end) = find(region, b"readonly def")
            && p > end
        {
            break;
        }
    }
    enc
}

// --- charstring interpreter --------------------------------------------------

/// Type 1 charstring interpreter (Type 1 spec §6). Stem hints are parsed and
/// discarded — this phase is unhinted by design.
struct Interp<'a> {
    font: &'a Type1Font,
    stack: Vec<f32>,
    /// PostScript operand stack for `callothersubr`/`pop`.
    ps_stack: Vec<f32>,
    outline: Outline,
    x: f32,
    y: f32,
    advance: f32,
    left_side_bearing: f32,
    open: bool,
    /// Flex point collection (othersubrs 0–2).
    flex: Option<Vec<[f32; 2]>>,
}

/// Depth cap for `callsubr`/`seac` recursion (malformed fonts).
const MAX_DEPTH: u32 = 10;

impl<'a> Interp<'a> {
    fn new(font: &'a Type1Font) -> Self {
        Self {
            font,
            stack: Vec::new(),
            ps_stack: Vec::new(),
            outline: Outline::default(),
            x: 0.0,
            y: 0.0,
            advance: 0.0,
            left_side_bearing: 0.0,
            open: false,
            flex: None,
        }
    }

    fn run_glyph(&mut self, gid: u32, depth: u32) -> Option<()> {
        let code = self.font.glyphs.get(gid as usize)?.code.clone();
        self.exec(&code, depth)
    }

    fn finish(mut self) -> Outline {
        self.close_path();
        self.outline
    }

    fn close_path(&mut self) {
        if self.open {
            self.outline.verbs.push(OutlineVerb::Close);
            self.open = false;
        }
    }

    fn move_to(&mut self, x: f32, y: f32) {
        // Inside a flex sequence the rmovetos are control points, not moves.
        if let Some(points) = &mut self.flex {
            points.push([x, y]);
            self.x = x;
            self.y = y;
            return;
        }
        self.close_path();
        self.outline.verbs.push(OutlineVerb::MoveTo);
        self.outline.points.push([x, y]);
        self.open = true;
        self.x = x;
        self.y = y;
    }

    fn line_to(&mut self, x: f32, y: f32) {
        self.outline.verbs.push(OutlineVerb::LineTo);
        self.outline.points.push([x, y]);
        self.x = x;
        self.y = y;
    }

    fn curve_to(&mut self, c1: [f32; 2], c2: [f32; 2], e: [f32; 2]) {
        self.outline.verbs.push(OutlineVerb::CurveTo);
        self.outline.points.push(c1);
        self.outline.points.push(c2);
        self.outline.points.push(e);
        self.x = e[0];
        self.y = e[1];
    }

    fn exec(&mut self, code: &[u8], depth: u32) -> Option<()> {
        if depth > MAX_DEPTH {
            return Some(());
        }
        let mut i = 0usize;
        while i < code.len() {
            let b = code[i];
            i += 1;
            // Operands.
            if b >= 32 || b == 255 {
                let v = match b {
                    32..=246 => {
                        let v = b as i32 - 139;
                        v as f32
                    }
                    247..=250 => {
                        let w = *code.get(i)? as i32;
                        i += 1;
                        ((b as i32 - 247) * 256 + w + 108) as f32
                    }
                    251..=254 => {
                        let w = *code.get(i)? as i32;
                        i += 1;
                        (-((b as i32 - 251) * 256) - w - 108) as f32
                    }
                    255 => {
                        let bytes: [u8; 4] = code.get(i..i + 4)?.try_into().ok()?;
                        i += 4;
                        i32::from_be_bytes(bytes) as f32
                    }
                    // The guard (`b >= 32 || b == 255`) makes the arms above
                    // exhaustive; never-panic policy still forbids
                    // `unreachable!()` in production — bail out of the
                    // charstring instead (glyph renders empty, page survives).
                    _ => return None,
                };
                self.stack.push(v);
                continue;
            }
            match b {
                1 | 3 => self.stack.clear(),  // hstem, vstem — unhinted
                4 => {
                    // vmoveto
                    let dy = self.stack.last().copied().unwrap_or(0.0);
                    self.move_to(self.x, self.y + dy);
                    self.stack.clear();
                }
                5 => {
                    // rlineto
                    let (dx, dy) = self.last2();
                    self.line_to(self.x + dx, self.y + dy);
                    self.stack.clear();
                }
                6 => {
                    // hlineto
                    let dx = self.stack.first().copied().unwrap_or(0.0);
                    self.line_to(self.x + dx, self.y);
                    self.stack.clear();
                }
                7 => {
                    // vlineto
                    let dy = self.stack.first().copied().unwrap_or(0.0);
                    self.line_to(self.x, self.y + dy);
                    self.stack.clear();
                }
                8 => {
                    // rrcurveto
                    if self.stack.len() >= 6 {
                        let a = &self.stack[self.stack.len() - 6..];
                        let (c1, c2, e) = rel_curve(self.x, self.y, a[0], a[1], a[2], a[3], a[4], a[5]);
                        self.curve_to(c1, c2, e);
                    }
                    self.stack.clear();
                }
                9 => {
                    self.close_path();
                    self.stack.clear();
                }
                10 => {
                    // callsubr
                    let n = self.stack.pop().unwrap_or(0.0) as i32;
                    if n >= 0
                        && let Some(sub) = self.font.subrs.get(n as usize)
                    {
                        let sub = sub.clone();
                        self.exec(&sub, depth + 1)?;
                    }
                }
                11 => return Some(()), // return
                13 => {
                    // hsbw: sbx wx — the left sidebearing sets the origin.
                    if self.stack.len() >= 2 {
                        self.left_side_bearing = self.stack[0];
                        self.advance = self.stack[1];
                        self.x = self.left_side_bearing;
                        self.y = 0.0;
                    }
                    self.stack.clear();
                }
                14 => {
                    // endchar
                    self.close_path();
                    return Some(());
                }
                21 => {
                    // rmoveto
                    let (dx, dy) = self.last2();
                    self.move_to(self.x + dx, self.y + dy);
                    self.stack.clear();
                }
                22 => {
                    // hmoveto
                    let dx = self.stack.first().copied().unwrap_or(0.0);
                    self.move_to(self.x + dx, self.y);
                    self.stack.clear();
                }
                30 => {
                    // vhcurveto: dy1 dx2 dy2 dx3
                    if self.stack.len() >= 4 {
                        let a = &self.stack[self.stack.len() - 4..];
                        let (c1, c2, e) = rel_curve(self.x, self.y, 0.0, a[0], a[1], a[2], a[3], 0.0);
                        self.curve_to(c1, c2, e);
                    }
                    self.stack.clear();
                }
                31 => {
                    // hvcurveto: dx1 dx2 dy2 dy3
                    if self.stack.len() >= 4 {
                        let a = &self.stack[self.stack.len() - 4..];
                        let (c1, c2, e) = rel_curve(self.x, self.y, a[0], 0.0, a[1], a[2], 0.0, a[3]);
                        self.curve_to(c1, c2, e);
                    }
                    self.stack.clear();
                }
                12 => {
                    let b2 = *code.get(i)?;
                    i += 1;
                    self.escape(b2, depth)?;
                }
                _ => self.stack.clear(),
            }
        }
        Some(())
    }

    /// Two-byte (`12 x`) operators.
    fn escape(&mut self, op: u8, depth: u32) -> Option<()> {
        match op {
            0 => self.stack.clear(),  // dotsection
            1 | 2 => self.stack.clear(), // vstem3, hstem3 — unhinted
            6 => {
                // seac: accent composition from two StandardEncoding glyphs.
                if self.stack.len() >= 5 {
                    let (asb, adx, ady) = (self.stack[0], self.stack[1], self.stack[2]);
                    let (bchar, achar) = (self.stack[3] as u8, self.stack[4] as u8);
                    self.stack.clear();
                    self.seac(asb, adx, ady, bchar, achar, depth)?;
                }
                self.stack.clear();
                return Some(());
            }
            7 => {
                // sbw: sbx sby wx wy
                if self.stack.len() >= 4 {
                    self.left_side_bearing = self.stack[0];
                    self.advance = self.stack[2];
                    self.x = self.stack[0];
                    self.y = self.stack[1];
                }
                self.stack.clear();
            }
            12 => {
                // div
                let b = self.stack.pop().unwrap_or(1.0);
                let a = self.stack.pop().unwrap_or(0.0);
                self.stack.push(if b != 0.0 { a / b } else { 0.0 });
            }
            16 => self.callothersubr(),
            17 => {
                // pop: take a value the othersubr left behind.
                let v = self.ps_stack.pop().unwrap_or(0.0);
                self.stack.push(v);
            }
            33 => {
                // setcurrentpoint
                if self.stack.len() >= 2 {
                    self.x = self.stack[0];
                    self.y = self.stack[1];
                }
                self.stack.clear();
            }
            _ => self.stack.clear(),
        }
        Some(())
    }

    /// `callothersubr`: the hooks Type 1 uses for flex and hint replacement.
    ///
    /// Only the standard OtherSubrs 0–3 exist in practice, and their real
    /// bodies are PostScript we do not run — the interpreter is expected to
    /// recognize them (Type 1 spec §8), which is what every implementation
    /// does.
    fn callothersubr(&mut self) {
        let idx = self.stack.pop().unwrap_or(0.0) as i32;
        let n = self.stack.pop().unwrap_or(0.0).max(0.0) as usize;
        let at = self.stack.len().saturating_sub(n);
        let args: Vec<f32> = self.stack.split_off(at);
        match idx {
            0 => {
                // End flex: seven collected points became two curves.
                let pts = self.flex.take().unwrap_or_default();
                if pts.len() >= 7 {
                    self.curve_to(pts[1], pts[2], pts[3]);
                    self.curve_to(pts[4], pts[5], pts[6]);
                }
                // The charstring pops the final point back off.
                self.ps_stack.clear();
                self.ps_stack.push(self.y);
                self.ps_stack.push(self.x);
            }
            1 => self.flex = Some(Vec::new()), // begin flex
            2 => {}                            // collect flex point (the rmoveto did it)
            3 => {
                // Hint replacement: hand back subr# 3 for the following `pop`.
                self.ps_stack.clear();
                self.ps_stack.push(3.0);
            }
            _ => {
                // Unknown othersubr: per the spec, arguments come back via pop.
                self.ps_stack.clear();
                self.ps_stack.extend(args.iter().rev());
            }
        }
    }

    /// Compose an accented glyph: draw the base, then the accent offset so
    /// their sidebearings line up (Type 1 spec §6, `seac`).
    fn seac(&mut self, asb: f32, adx: f32, ady: f32, bchar: u8, achar: u8, depth: u32) -> Option<()> {
        let base_lsb = self.left_side_bearing;
        let name_of = |c: u8| crate::encoding::BaseEncoding::Standard.to_char(c);
        let gid_of = |this: &Self, c: u8| -> Option<u32> {
            let ch = name_of(c)?;
            let mut buf = [0u8; 4];
            this.font.gid_for_name(ch.encode_utf8(&mut buf).as_bytes())
        };
        let (bg, ag) = (gid_of(self, bchar), gid_of(self, achar));

        if let Some(bg) = bg {
            let code = self.font.glyphs.get(bg as usize)?.code.clone();
            self.reset_pen();
            self.exec(&code, depth + 1)?;
            self.close_path();
        }
        if let Some(ag) = ag {
            let code = self.font.glyphs.get(ag as usize)?.code.clone();
            let shift_x = base_lsb - asb + adx;
            let before = self.outline.points.len();
            self.reset_pen();
            self.exec(&code, depth + 1)?;
            self.close_path();
            for p in &mut self.outline.points[before..] {
                p[0] += shift_x;
                p[1] += ady;
            }
        }
        Some(())
    }

    fn reset_pen(&mut self) {
        self.close_path();
        self.stack.clear();
        self.x = 0.0;
        self.y = 0.0;
    }

    fn last2(&self) -> (f32, f32) {
        if self.stack.len() >= 2 {
            (self.stack[self.stack.len() - 2], self.stack[self.stack.len() - 1])
        } else {
            (0.0, 0.0)
        }
    }
}

/// Turn six relative deltas into absolute control points.
#[allow(clippy::too_many_arguments)]
fn rel_curve(
    x: f32,
    y: f32,
    dx1: f32,
    dy1: f32,
    dx2: f32,
    dy2: f32,
    dx3: f32,
    dy3: f32,
) -> ([f32; 2], [f32; 2], [f32; 2]) {
    let c1 = [x + dx1, y + dy1];
    let c2 = [c1[0] + dx2, c1[1] + dy2];
    let e = [c2[0] + dx3, c2[1] + dy3];
    (c1, c2, e)
}

/// Shareable parsed Type 1 program.
pub type SharedType1 = Arc<Type1Font>;

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn eexec_cipher_round_trips() {
        // The cipher is symmetric-ish: encrypting then decrypting with the
        // same key recovers the plaintext (Type 1 spec §7.2).
        fn encrypt(plain: &[u8], key: u16, lead: usize) -> Vec<u8> {
            const C1: u16 = 52845;
            const C2: u16 = 22719;
            let mut r = key;
            let mut out = Vec::new();
            for &p in std::iter::repeat_n(&0x55u8, lead).chain(plain.iter()) {
                let c = p ^ (r >> 8) as u8;
                r = (c as u16).wrapping_add(r).wrapping_mul(C1).wrapping_add(C2);
                out.push(c);
            }
            out
        }
        let plain = b"/CharStrings 2 dict dup begin";
        let enc = encrypt(plain, 55665, 4);
        assert_eq!(decrypt(&enc, 55665, 4), plain);
    }

    fn synthetic(code: Vec<u8>) -> Type1Font {
        Type1Font {
            glyphs: vec![Type1Glyph { name: b"x".to_vec(), code }],
            by_name: HashMap::new(),
            encoding: Box::new([None; 256]),
            subrs: Vec::new(),
            units_per_em: 1000,
        }
    }

    /// `/lenIV -1` is the convention for "charstrings are stored as
    /// plaintext". Running the 4330 cipher over them anyway destroys every
    /// outline while leaving the font, its encoding and its glyph ids intact —
    /// pdfbox/3874.pdf rendered as scattered fragments of the right glyphs in
    /// the right places, with no error anywhere.
    #[test]
    fn a_negative_len_iv_means_the_charstring_is_plaintext() {
        let n = |v: i32| (v + 139) as u8;
        // `0 100 hsbw 0 0 rmoveto 50 vlineto 50 hlineto closepath endchar`
        let plain = vec![n(0), n(100), 13, n(0), n(0), 21, n(50), 7, n(50), 6, 9, 14];

        assert_eq!(
            decrypt_charstring(&plain, -1),
            plain,
            "a negative lenIV must pass the bytes through untouched"
        );
        // The same bytes under the normal convention *are* decrypted, so the
        // two paths stay genuinely different.
        assert_ne!(decrypt_charstring(&plain, 4), plain);
        // And a plaintext body still drives the interpreter: this glyph draws.
        let font = synthetic(decrypt_charstring(&plain, -1));
        assert_eq!(font.advance(0), Some(100.0));
        let outline = font.outline(0).expect("plaintext charstring must draw");
        assert!(
            !outline.verbs.is_empty(),
            "a plaintext charstring must produce an outline"
        );
    }

    #[test]
    fn charstring_numbers_and_hsbw_decode() {
        // `20 600 hsbw endchar`: sidebearing 20, advance 600. Small operands
        // encode as v + 139; 600 needs the two-byte 247..250 form,
        // (b - 247) * 256 + next + 108.
        let hi = 247 + ((600 - 108) / 256) as u8;
        let lo = ((600 - 108) % 256) as u8;
        let font = synthetic(vec![20 + 139, hi, lo, 13, 14]);
        assert_eq!(font.advance(0), Some(600.0), "advance is hsbw's second operand");
    }

    #[test]
    fn charstring_draws_a_closed_path() {
        // 0 100 hsbw, 0 0 rmoveto, 50 vlineto, 50 hlineto, closepath, endchar.
        let n = |v: i32| (v + 139) as u8;
        let code = vec![
            n(0), n(100), 13, // hsbw
            n(0), n(0), 21, // rmoveto
            n(50), 7,  // vlineto
            n(50), 6,  // hlineto
            9,  // closepath
            14, // endchar
        ];
        let font = synthetic(code);
        let o = font.outline(0).expect("draws something");
        assert_eq!(
            o.verbs,
            vec![OutlineVerb::MoveTo, OutlineVerb::LineTo, OutlineVerb::LineTo, OutlineVerb::Close]
        );
        assert_eq!(o.points[0], [0.0, 0.0]);
        assert_eq!(o.points[1], [0.0, 50.0]);
        assert_eq!(o.points[2], [50.0, 50.0]);
    }

    #[test]
    fn callsubr_depth_is_bounded() {
        // A subr that calls itself must terminate, not blow the stack.
        let n = |v: i32| (v + 139) as u8;
        let mut font = synthetic(vec![n(0), n(0), 13, n(0), 10, 14]);
        font.subrs = vec![vec![n(0), 10]]; // subr 0: `0 callsubr` — infinite
        assert!(font.advance(0).is_some(), "recursion is capped, not fatal");
    }

    #[test]
    fn pfb_segments_are_unwrapped() {
        let mut pfb = vec![0x80, 0x01, 4, 0, 0, 0];
        pfb.extend_from_slice(b"abcd");
        pfb.extend_from_slice(&[0x80, 0x02, 2, 0, 0, 0]);
        pfb.extend_from_slice(b"ef");
        pfb.extend_from_slice(&[0x80, 0x03]);
        assert_eq!(unwrap_pfb(&pfb), b"abcdef");
        // A PFA passes through untouched.
        assert_eq!(unwrap_pfb(b"%!PS-AdobeFont"), b"%!PS-AdobeFont");
    }

    #[test]
    fn font_matrix_gives_units_per_em() {
        assert_eq!(font_matrix_upem(b"/FontMatrix [0.001 0 0 0.001 0 0] readonly def"), 1000);
        assert_eq!(font_matrix_upem(b"/FontMatrix [0.0005 0 0 0.0005 0 0] def"), 2000);
        assert_eq!(font_matrix_upem(b"no matrix here"), 1000);
    }
}
