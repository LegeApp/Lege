use chrono::{Local, Timelike};
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::sync::RwLock;

pub type Rgb = (u8, u8, u8);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AppearanceMode {
    Default,
    AutoWarm,
    Evening,
    Night,
    LateNight,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WarmStage {
    Day,
    Evening,
    Night,
    LateNight,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppearanceSettings {
    pub mode: AppearanceMode,
}

impl Default for AppearanceSettings {
    fn default() -> Self {
        Self {
            mode: AppearanceMode::AutoWarm,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    pub app_bg: Rgb,
    pub panel_bg: Rgb,
    pub card_bg: Rgb,
    pub control_bg: Rgb,
    pub selected_bg: Rgb,
    pub hover_bg: Rgb,
    pub focus_bg: Rgb,
    pub active_bg: Rgb,
    pub info_bg: Rgb,
    pub text: Rgb,
    pub muted: Rgb,
    pub border: Rgb,
    pub border_focus: Rgb,
    pub check_mark: Rgb,
    pub inverse_surface: Rgb,
    pub inverse_surface_secondary: Rgb,
    pub inverse_surface_tertiary: Rgb,
    pub progress_track_bg: Rgb,
}

const BASE_APP_BG: Rgb = (226, 232, 235);

#[cfg(target_os = "linux")]
const BASE_PANEL_BG: Rgb = (253, 249, 244);
#[cfg(not(target_os = "linux"))]
const BASE_PANEL_BG: Rgb = (255, 250, 245);

#[cfg(target_os = "linux")]
const BASE_SELECTED_BG: Rgb = (255, 241, 241);
#[cfg(not(target_os = "linux"))]
const BASE_SELECTED_BG: Rgb = (255, 243, 243);

#[cfg(target_os = "linux")]
const BASE_CARD_BG: Rgb = (253, 249, 244);
#[cfg(not(target_os = "linux"))]
const BASE_CARD_BG: Rgb = (255, 250, 245);

const BASE_DAY: Palette = Palette {
    app_bg: BASE_APP_BG,
    panel_bg: BASE_PANEL_BG,
    card_bg: BASE_CARD_BG,
    control_bg: (255, 250, 250),
    selected_bg: BASE_SELECTED_BG,
    hover_bg: (236, 236, 236),
    focus_bg: (232, 232, 232),
    active_bg: (210, 210, 210),
    info_bg: (255, 255, 225),
    text: (20, 20, 20),
    muted: (90, 90, 90),
    border: (128, 128, 128),
    border_focus: (64, 64, 64),
    check_mark: (30, 30, 30),
    inverse_surface: (128, 128, 128),
    inverse_surface_secondary: (96, 96, 96),
    inverse_surface_tertiary: (64, 64, 64),
    progress_track_bg: (236, 236, 236),
};

static ACTIVE_STAGE: Lazy<RwLock<WarmStage>> = Lazy::new(|| RwLock::new(current_warm_stage()));
static SETTINGS: Lazy<RwLock<AppearanceSettings>> =
    Lazy::new(|| RwLock::new(load_appearance_settings().unwrap_or_default()));
static RUNTIME_MODE_OVERRIDE: Lazy<RwLock<Option<AppearanceMode>>> =
    Lazy::new(|| RwLock::new(None));

pub fn initialize_runtime_appearance() {
    drop(SETTINGS.read().unwrap_or_else(|e| e.into_inner()));
    let _ = refresh_runtime_checkpoint();
}

pub fn refresh_runtime_checkpoint() -> bool {
    let new_stage = current_warm_stage();
    let mut stage = ACTIVE_STAGE.write().unwrap_or_else(|e| e.into_inner());
    if *stage == new_stage {
        false
    } else {
        *stage = new_stage;
        true
    }
}

pub fn active_palette() -> Palette {
    palette_for_stage(active_warm_stage())
}

fn active_warm_stage() -> WarmStage {
    match active_mode() {
        AppearanceMode::Default => WarmStage::Day,
        AppearanceMode::AutoWarm => *ACTIVE_STAGE.read().unwrap_or_else(|e| e.into_inner()),
        AppearanceMode::Evening => WarmStage::Evening,
        AppearanceMode::Night => WarmStage::Night,
        AppearanceMode::LateNight => WarmStage::LateNight,
    }
}

pub fn active_mode() -> AppearanceMode {
    RUNTIME_MODE_OVERRIDE
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .unwrap_or_else(|| {
            mode_from_env()
                .unwrap_or_else(|| SETTINGS.read().unwrap_or_else(|e| e.into_inner()).mode)
        })
}

#[allow(dead_code)]
pub fn set_appearance_mode(mode: AppearanceMode) -> anyhow::Result<()> {
    *RUNTIME_MODE_OVERRIDE
        .write()
        .unwrap_or_else(|e| e.into_inner()) = Some(mode);
    {
        let mut settings = SETTINGS.write().unwrap_or_else(|e| e.into_inner());
        settings.mode = mode;
        save_appearance_settings(&settings)?;
    }
    Ok(())
}

pub fn current_warm_stage() -> WarmStage {
    warm_stage_for_hour(Local::now().hour())
}

fn warm_stage_for_hour(hour: u32) -> WarmStage {
    match hour {
        6..=18 => WarmStage::Day,
        19..=20 => WarmStage::Evening,
        21..=22 => WarmStage::Night,
        _ => WarmStage::LateNight,
    }
}

fn palette_for_stage(stage: WarmStage) -> Palette {
    Palette {
        app_bg: warm_surface(BASE_DAY.app_bg, stage),
        panel_bg: warm_surface(BASE_DAY.panel_bg, stage),
        card_bg: warm_surface(BASE_DAY.card_bg, stage),
        control_bg: warm_control_surface(BASE_DAY.control_bg, stage),
        selected_bg: selected_surface(BASE_DAY.selected_bg, stage),
        hover_bg: warm_surface(BASE_DAY.hover_bg, stage),
        focus_bg: warm_surface(BASE_DAY.focus_bg, stage),
        active_bg: warm_surface(BASE_DAY.active_bg, stage),
        info_bg: warm_info_surface(BASE_DAY.info_bg, stage),
        text: warm_text(BASE_DAY.text, stage),
        muted: warm_muted(BASE_DAY.muted, stage),
        border: warm_border(BASE_DAY.border, stage),
        border_focus: warm_border_focus(BASE_DAY.border_focus, stage),
        check_mark: warm_text(BASE_DAY.check_mark, stage),
        inverse_surface: warm_inverse(BASE_DAY.inverse_surface, stage),
        inverse_surface_secondary: warm_inverse(BASE_DAY.inverse_surface_secondary, stage),
        inverse_surface_tertiary: warm_inverse(BASE_DAY.inverse_surface_tertiary, stage),
        progress_track_bg: warm_surface(BASE_DAY.progress_track_bg, stage),
    }
}

fn stage_profile(stage: WarmStage) -> (f32, f32) {
    match stage {
        WarmStage::Day => (0.0, 0.0),
        WarmStage::Evening => (0.18, 0.035),
        WarmStage::Night => (0.34, 0.07),
        WarmStage::LateNight => (0.46, 0.1),
    }
}

fn blend(a: Rgb, b: Rgb, t: f32) -> Rgb {
    let mix = |x: u8, y: u8| -> u8 {
        ((x as f32 * (1.0 - t)) + (y as f32 * t))
            .round()
            .clamp(0.0, 255.0) as u8
    };
    (mix(a.0, b.0), mix(a.1, b.1), mix(a.2, b.2))
}

fn dim(rgb: Rgb, amount: f32) -> Rgb {
    let scale = 1.0 - amount;
    (
        (rgb.0 as f32 * scale).round().clamp(0.0, 255.0) as u8,
        (rgb.1 as f32 * scale).round().clamp(0.0, 255.0) as u8,
        (rgb.2 as f32 * scale).round().clamp(0.0, 255.0) as u8,
    )
}

fn warm_surface(rgb: Rgb, stage: WarmStage) -> Rgb {
    let (tint, darkness) = stage_profile(stage);
    // Age the light UI from fresh paper into warmer parchment without
    // becoming a true inverted/dark theme.
    blend(dim(rgb, darkness), (226, 190, 132), tint)
}

fn warm_control_surface(rgb: Rgb, stage: WarmStage) -> Rgb {
    let (tint, darkness) = stage_profile(stage);
    // Buttons and controls get a slightly darker red-brown cast than the
    // page/card surfaces so evening mode reads as intentionally different.
    blend(dim(rgb, darkness * 0.85), (211, 147, 112), tint * 0.95)
}

fn warm_info_surface(rgb: Rgb, stage: WarmStage) -> Rgb {
    let (tint, darkness) = stage_profile(stage);
    blend(dim(rgb, darkness * 0.65), (235, 199, 128), tint * 0.85)
}

fn selected_surface(rgb: Rgb, stage: WarmStage) -> Rgb {
    let t = match stage {
        WarmStage::Day => 0.0,
        WarmStage::Evening => 0.32,
        WarmStage::Night => 0.58,
        WarmStage::LateNight => 0.74,
    };
    blend(rgb, (198, 103, 79), t)
}

fn warm_text(rgb: Rgb, stage: WarmStage) -> Rgb {
    let t = match stage {
        WarmStage::Day => 0.0,
        WarmStage::Evening => 0.2,
        WarmStage::Night => 0.38,
        WarmStage::LateNight => 0.55,
    };
    blend(rgb, (42, 30, 24), t)
}

fn warm_muted(rgb: Rgb, stage: WarmStage) -> Rgb {
    let t = match stage {
        WarmStage::Day => 0.0,
        WarmStage::Evening => 0.25,
        WarmStage::Night => 0.45,
        WarmStage::LateNight => 0.65,
    };
    blend(rgb, (68, 52, 43), t)
}

fn warm_border(rgb: Rgb, stage: WarmStage) -> Rgb {
    let t = match stage {
        WarmStage::Day => 0.0,
        WarmStage::Evening => 0.2,
        WarmStage::Night => 0.4,
        WarmStage::LateNight => 0.6,
    };
    blend(rgb, (118, 94, 82), t)
}

fn warm_border_focus(rgb: Rgb, stage: WarmStage) -> Rgb {
    let t = match stage {
        WarmStage::Day => 0.0,
        WarmStage::Evening => 0.18,
        WarmStage::Night => 0.35,
        WarmStage::LateNight => 0.5,
    };
    blend(rgb, (84, 58, 46), t)
}

fn warm_inverse(rgb: Rgb, stage: WarmStage) -> Rgb {
    let (tint, darkness) = stage_profile(stage);
    blend(dim(rgb, darkness * 0.5), (139, 106, 83), tint * 0.8)
}

fn settings_directory() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Lege")
}

pub fn get_appearance_settings_file_path() -> PathBuf {
    settings_directory().join("appearance.json")
}

pub fn load_appearance_settings() -> anyhow::Result<AppearanceSettings> {
    let settings_path = get_appearance_settings_file_path();
    if !settings_path.exists() {
        return Ok(AppearanceSettings::default());
    }

    let content = fs::read_to_string(&settings_path)?;
    Ok(serde_json::from_str(&content)?)
}

pub fn save_appearance_settings(settings: &AppearanceSettings) -> anyhow::Result<()> {
    let settings_path = get_appearance_settings_file_path();
    if let Some(parent) = settings_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(settings)?;
    fs::write(&settings_path, content)?;
    Ok(())
}

fn mode_from_env() -> Option<AppearanceMode> {
    let value = std::env::var("LEGE_GUI_APPEARANCE").ok()?;
    match value.trim().to_ascii_lowercase().replace('-', "_").as_str() {
        "default" | "day" | "light" => Some(AppearanceMode::Default),
        "auto" | "auto_warm" | "warm" => Some(AppearanceMode::AutoWarm),
        "evening" => Some(AppearanceMode::Evening),
        "night" => Some(AppearanceMode::Night),
        "late_night" | "latenight" => Some(AppearanceMode::LateNight),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_appearance_is_time_of_day() {
        assert_eq!(AppearanceSettings::default().mode, AppearanceMode::AutoWarm);
    }

    #[test]
    fn warm_stage_uses_whole_hour_boundaries() {
        assert_eq!(warm_stage_for_hour(5), WarmStage::LateNight);
        assert_eq!(warm_stage_for_hour(6), WarmStage::Day);
        assert_eq!(warm_stage_for_hour(18), WarmStage::Day);
        assert_eq!(warm_stage_for_hour(19), WarmStage::Evening);
        assert_eq!(warm_stage_for_hour(21), WarmStage::Night);
        assert_eq!(warm_stage_for_hour(23), WarmStage::LateNight);
    }

    #[test]
    fn warm_palette_keeps_text_dark_and_surfaces_light() {
        let day = palette_for_stage(WarmStage::Day);
        let late = palette_for_stage(WarmStage::LateNight);

        assert_eq!(day, BASE_DAY);
        assert!(late.app_bg.0 > 190);
        assert!(late.panel_bg.0 > 220);
        assert!(late.text.0 < 50);
        assert!(late.selected_bg.1 < day.selected_bg.1);
    }

    #[test]
    fn night_palette_is_aged_paper_not_inverted() {
        let day = palette_for_stage(WarmStage::Day);
        let night = palette_for_stage(WarmStage::Night);

        assert_ne!(
            night.app_bg,
            (255 - day.app_bg.0, 255 - day.app_bg.1, 255 - day.app_bg.2)
        );
        assert_ne!(
            night.panel_bg,
            (
                255 - day.panel_bg.0,
                255 - day.panel_bg.1,
                255 - day.panel_bg.2
            )
        );
        assert!(night.app_bg.0 > 190);
        assert!(night.panel_bg.0 > 220);
        assert!(night.text.0 < 50);
        assert!(night.selected_bg.0 < day.selected_bg.0);
    }
}
