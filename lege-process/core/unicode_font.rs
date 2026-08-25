use std::sync::{Arc, LazyLock};

#[derive(Clone)]
pub struct UnicodeFontData {
    pub post_script_name: String,
    pub family_name: String,
    pub data: Arc<[u8]>,
    pub metrics: FontMetrics,
}

#[derive(Clone, Copy, Debug)]
pub struct FontMetrics {
    pub units_per_em: u16,
    pub ascent: i16,
    pub descent: i16,
    pub cap_height: i16,
    pub italic_angle: f32,
    pub bbox: FontBBox,
}

#[derive(Clone, Copy, Debug)]
pub struct FontBBox {
    pub x_min: i16,
    pub y_min: i16,
    pub x_max: i16,
    pub y_max: i16,
}

static CACHED_FONT: LazyLock<Option<UnicodeFontData>> = LazyLock::new(build_glyphless_font);

/// The invisible OCR/searchable text layer never renders glyphs and carries its
/// Unicode via a ToUnicode CMap, so we embed a ~1 KB glyphless font instead of
/// a ~1 MB system font. Everything downstream (Type0/CIDFontType2, Identity-H,
/// UTF-16 encoding, ToUnicode) is unchanged; only the font program shrinks.
fn build_glyphless_font() -> Option<UnicodeFontData> {
    use crate::glyphless_font as gf;
    let data = gf::build_glyphless_ttf();
    Some(UnicodeFontData {
        post_script_name: "Glyphless".to_string(),
        family_name: "Glyphless".to_string(),
        data: Arc::from(data.into_boxed_slice()),
        metrics: FontMetrics {
            units_per_em: gf::UNITS_PER_EM,
            ascent: gf::ASCENT,
            descent: gf::DESCENT,
            cap_height: gf::CAP_HEIGHT,
            italic_angle: 0.0,
            bbox: FontBBox {
                x_min: 0,
                y_min: gf::DESCENT,
                x_max: 1000,
                y_max: gf::ASCENT,
            },
        },
    })
}

pub fn get_unicode_font() -> Option<UnicodeFontData> {
    CACHED_FONT.clone()
}
