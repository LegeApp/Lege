//! Typed content-stream writer for the fixed operator set:
//! q Q g cm Do BT ET Tr Tc Tw Tz Tf Tm Tj. No generic Operation type.
//!
//! Each method appends one operator (operands first, operator token last, then
//! a newline) to an owned byte buffer. This is the whole content vocabulary
//! Lege emits — images (`q`/`Q`/`g`/`cm`/`Do`) and the invisible OCR text layer
//! (`BT`…`ET`). Extending it is a plan change, not a drop-in.

use crate::serialize::{write_hex_string, write_real};
use crate::types::{Affine, ResourceName};

/// Accumulates a page content stream.
#[derive(Debug, Default)]
pub struct ContentWriter {
    bytes: Vec<u8>,
}

impl ContentWriter {
    pub fn new() -> Self {
        Self { bytes: Vec::new() }
    }

    pub fn with_capacity(cap: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(cap),
        }
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.bytes
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    // --- graphics state -------------------------------------------------

    /// `q` — save graphics state.
    pub fn save(&mut self) {
        self.bytes.extend_from_slice(b"q\n");
    }

    /// `Q` — restore graphics state.
    pub fn restore(&mut self) {
        self.bytes.extend_from_slice(b"Q\n");
    }

    /// `g` — set the nonstroking gray level (0 = black, 1 = white). Used to
    /// paint stencil-mask ink.
    pub fn set_gray_fill(&mut self, gray: f64) {
        write_real(&mut self.bytes, gray);
        self.bytes.extend_from_slice(b" g\n");
    }

    /// `cm` — concatenate a matrix onto the CTM.
    pub fn concat_matrix(&mut self, m: Affine) {
        self.write_affine(m);
        self.bytes.extend_from_slice(b" cm\n");
    }

    /// `Do` — paint a named XObject.
    pub fn draw_xobject(&mut self, name: ResourceName) {
        name.write_name(&mut self.bytes);
        self.bytes.extend_from_slice(b" Do\n");
    }

    // --- text -----------------------------------------------------------

    /// `BT` — begin a text object.
    pub fn begin_text(&mut self) {
        self.bytes.extend_from_slice(b"BT\n");
    }

    /// `ET` — end a text object.
    pub fn end_text(&mut self) {
        self.bytes.extend_from_slice(b"ET\n");
    }

    /// `Tr` — text rendering mode (3 = invisible, the OCR layer mode).
    pub fn set_text_render_mode(&mut self, mode: i32) {
        crate::serialize::write_i64(&mut self.bytes, mode as i64);
        self.bytes.extend_from_slice(b" Tr\n");
    }

    /// `Tc` — character spacing.
    pub fn set_char_spacing(&mut self, spacing: f64) {
        write_real(&mut self.bytes, spacing);
        self.bytes.extend_from_slice(b" Tc\n");
    }

    /// `Tw` — word spacing.
    pub fn set_word_spacing(&mut self, spacing: f64) {
        write_real(&mut self.bytes, spacing);
        self.bytes.extend_from_slice(b" Tw\n");
    }

    /// `Tz` — horizontal scaling, in percent (100 = no stretch).
    pub fn set_horizontal_scale(&mut self, percent: f64) {
        write_real(&mut self.bytes, percent);
        self.bytes.extend_from_slice(b" Tz\n");
    }

    /// `Tf` — select a font resource and size.
    pub fn set_font(&mut self, font: ResourceName, size: f64) {
        font.write_name(&mut self.bytes);
        self.bytes.push(b' ');
        write_real(&mut self.bytes, size);
        self.bytes.extend_from_slice(b" Tf\n");
    }

    /// `Tm` — set the text matrix.
    pub fn set_text_matrix(&mut self, m: Affine) {
        self.write_affine(m);
        self.bytes.extend_from_slice(b" Tm\n");
    }

    /// `Tj` — show a string, given as raw bytes emitted as a PDF hex string
    /// (the OCR layer encodes text as UTF-16BE code units).
    pub fn show_text_hex(&mut self, bytes: &[u8]) {
        write_hex_string(&mut self.bytes, bytes);
        self.bytes.extend_from_slice(b" Tj\n");
    }

    fn write_affine(&mut self, m: Affine) {
        for (i, v) in [m.a, m.b, m.c, m.d, m.e, m.f].into_iter().enumerate() {
            if i > 0 {
                self.bytes.push(b' ');
            }
            write_real(&mut self.bytes, v);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_draw_sequence() {
        let mut c = ContentWriter::new();
        c.save();
        c.concat_matrix(Affine::scale_translate(200.0, 300.0, 10.0, 20.0));
        c.draw_xobject(ResourceName::Image(1));
        c.restore();
        assert_eq!(c.as_slice(), &b"q\n200 0 0 300 10 20 cm\n/Im1 Do\nQ\n"[..]);
    }

    #[test]
    fn masked_image_uses_black_fill() {
        let mut c = ContentWriter::new();
        c.save();
        c.set_gray_fill(0.0);
        c.restore();
        assert_eq!(c.as_slice(), &b"q\n0 g\nQ\n"[..]);
    }

    #[test]
    fn invisible_text_run() {
        let mut c = ContentWriter::new();
        c.begin_text();
        c.set_text_render_mode(3);
        c.set_char_spacing(0.0);
        c.set_word_spacing(0.0);
        c.set_horizontal_scale(100.0);
        c.set_font(ResourceName::Font(0), 1.0);
        c.set_text_matrix(Affine::scale_translate(12.0, 12.0, 72.0, 700.0));
        c.show_text_hex(&[0x00, 0x41]);
        c.end_text();
        assert_eq!(
            c.as_slice(),
            &b"BT\n3 Tr\n0 Tc\n0 Tw\n100 Tz\n/F0 1 Tf\n12 0 0 12 72 700 Tm\n<0041> Tj\nET\n"[..]
        );
    }
}
