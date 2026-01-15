use anyhow::{Context, Result, anyhow};
use once_cell::sync::Lazy;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
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

static CACHED_FONT: Lazy<Option<UnicodeFontData>> = Lazy::new(|| match try_load_system_font() {
    Ok(font) => Some(font),
    Err(err) => {
        log::warn!(
            "Falling back to built-in Helvetica for OCR text (Unicode font load failed: {err})"
        );
        None
    }
});

pub fn get_unicode_font() -> Option<UnicodeFontData> {
    CACHED_FONT.clone()
}

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
    let italic_angle = face.italic_angle().unwrap_or(0.0);
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
