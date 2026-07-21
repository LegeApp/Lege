// System-font loading below is retained (behind dead_code) in case a future
// mode wants a real embedded font again; the OCR text layer now uses the
// generated glyphless font instead (see `build_glyphless_font`).
#[allow(unused_imports)]
use anyhow::{Context, Result, anyhow};
use once_cell::sync::Lazy;
#[allow(unused_imports)]
use std::fs;
#[allow(unused_imports)]
use std::path::{Path, PathBuf};
use std::sync::Arc;
#[allow(unused_imports)]
use ttf_parser::{Face, name_id};

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

static CACHED_FONT: Lazy<Option<UnicodeFontData>> = Lazy::new(build_glyphless_font);

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

#[allow(dead_code)]
fn try_load_system_font() -> Result<UnicodeFontData> {
    for candidate in candidate_paths() {
        if let Some(font) = load_font_from_path(&candidate)? {
            return Ok(font);
        }
    }

    Err(anyhow!(
        "No suitable Unicode font found in standard system locations"
    ))
}

#[allow(dead_code)]
fn load_font_from_path(path: &Path) -> Result<Option<UnicodeFontData>> {
    if !path.exists() {
        return Ok(None);
    }

    let data =
        fs::read(path).with_context(|| format!("Failed to read font at {}", path.display()))?;
    let face = match Face::parse(&data, 0) {
        Ok(face) => face,
        Err(err) => {
            log::warn!("Skipping font {} (ttf-parser error: {err})", path.display());
            return Ok(None);
        }
    };

    let mut post_script_name: Option<String> = None;
    let mut family_name: Option<String> = None;

    for name in face.names() {
        if post_script_name.is_none() && name.name_id == name_id::POST_SCRIPT_NAME {
            post_script_name = name.to_string();
        }

        if family_name.is_none() && name.name_id == name_id::FAMILY {
            family_name = name.to_string();
        }

        if post_script_name.is_some() && family_name.is_some() {
            break;
        }
    }

    let post_script_name = post_script_name
        .or_else(|| family_name.clone().map(|s| s.replace(' ', "")))
        .unwrap_or_else(|| "EmbeddedUnicode".to_string());
    let family_name = family_name.unwrap_or_else(|| post_script_name.clone());

    let os2_table = face.tables().os2;
    let cap_height = os2_table
        .and_then(|os2| os2.capital_height())
        .unwrap_or(face.ascender());
    let italic_angle = face.italic_angle();
    let bbox = face.global_bounding_box();

    let metrics = FontMetrics {
        units_per_em: face.units_per_em(),
        ascent: face.ascender(),
        descent: face.descender(),
        cap_height,
        italic_angle,
        bbox: FontBBox {
            x_min: bbox.x_min,
            y_min: bbox.y_min,
            x_max: bbox.x_max,
            y_max: bbox.y_max,
        },
    };

    Ok(Some(UnicodeFontData {
        post_script_name,
        family_name,
        data: Arc::from(data.into_boxed_slice()),
        metrics,
    }))
}

#[cfg(target_os = "windows")]
#[allow(dead_code)]
fn candidate_paths() -> Vec<PathBuf> {
    let base = std::env::var_os("WINDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("C:/Windows"));
    let fonts_dir = base.join("Fonts");

    [
        "arialuni.ttf",
        "arial.ttf",
        "segoeui.ttf",
        "calibri.ttf",
        "times.ttf",
    ]
    .into_iter()
    .map(|file| fonts_dir.join(file))
    .collect()
}

#[cfg(target_os = "macos")]
#[allow(dead_code)]
fn candidate_paths() -> Vec<PathBuf> {
    [
        "/System/Library/Fonts/Supplemental/Arial Unicode.ttf",
        "/System/Library/Fonts/Supplemental/Helvetica.ttc",
        "/System/Library/Fonts/Supplemental/Times New Roman.ttf",
        "/Library/Fonts/Arial Unicode.ttf",
    ]
    .iter()
    .map(PathBuf::from)
    .collect()
}

#[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
#[allow(dead_code)]
fn candidate_paths() -> Vec<PathBuf> {
    [
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
        "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf",
        "/usr/share/fonts/truetype/freefont/FreeSans.ttf",
    ]
    .iter()
    .map(PathBuf::from)
    .collect()
}
