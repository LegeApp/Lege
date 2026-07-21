use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::sync::RwLock;

pub type Rgb = (u8, u8, u8);

/// The app's active theme: either the built-in Default (day) palette or one of the
/// curated Sanzo Wada combos in light or dark mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThemeChoice {
    Default,
    /// `index` is the 0-based position in `sanzowada::SANZO_THEMES`.
    Sanzo {
        index: usize,
        dark: bool,
    },
}

impl Default for ThemeChoice {
    fn default() -> Self {
        ThemeChoice::Sanzo {
            index: 0,
            dark: true,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AppearanceSettings {
    pub theme: ThemeChoice,
}

impl Default for AppearanceSettings {
    fn default() -> Self {
        Self {
            theme: ThemeChoice::default(),
        }
    }
}

/// On-disk form of [`ThemeChoice`]. Sanzo themes are identified by their
/// source color names, which are stable across reorders or removals in
/// `SANZO_THEMES`; `index` is written too so older builds can still read the
/// file, and doubles as a fallback when the names fail to resolve.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum StoredThemeChoice {
    Default,
    Sanzo {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        colors: Option<Vec<String>>,
        #[serde(default)]
        index: Option<usize>,
        dark: bool,
    },
}

impl Default for StoredThemeChoice {
    fn default() -> Self {
        StoredThemeChoice::Default
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct StoredAppearanceSettings {
    theme: StoredThemeChoice,
}

fn to_stored(choice: ThemeChoice) -> StoredThemeChoice {
    match choice {
        ThemeChoice::Default => StoredThemeChoice::Default,
        ThemeChoice::Sanzo { index, dark } => StoredThemeChoice::Sanzo {
            colors: crate::sanzowada::theme_at(index)
                .map(|t| t.colors.iter().map(|c| c.to_string()).collect()),
            index: Some(index),
            dark,
        },
    }
}

fn from_stored(stored: StoredThemeChoice) -> ThemeChoice {
    match stored {
        StoredThemeChoice::Default => ThemeChoice::Default,
        StoredThemeChoice::Sanzo {
            colors,
            index,
            dark,
        } => {
            let resolved = colors
                .as_deref()
                .and_then(crate::sanzowada::theme_index_by_colors)
                .or(index)
                .filter(|&i| i < crate::sanzowada::THEME_COUNT);
            match resolved {
                Some(index) => ThemeChoice::Sanzo { index, dark },
                None => ThemeChoice::Default,
            }
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

// On Linux, Freya/Vulkan renders raw RGB without OS color management, so the light
// surfaces use a slightly warmer base than on other platforms.
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

/// The built-in "Default" theme (the original daytime look).
pub(crate) const BASE_DAY: Palette = Palette {
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

static SETTINGS: Lazy<RwLock<AppearanceSettings>> =
    Lazy::new(|| RwLock::new(load_appearance_settings().unwrap_or_default()));
// A non-persisted override used by the theme picker to preview a choice live while
// browsing; committing writes it to SETTINGS and clears this.
static PREVIEW: Lazy<RwLock<Option<ThemeChoice>>> = Lazy::new(|| RwLock::new(None));

/// Force the persisted settings to load at startup.
pub fn initialize_runtime_appearance() {
    drop(SETTINGS.read().unwrap_or_else(|e| e.into_inner()));
}

/// The committed (persisted) theme choice.
pub fn committed_theme_choice() -> ThemeChoice {
    SETTINGS.read().unwrap_or_else(|e| e.into_inner()).theme
}

/// The choice currently driving the palette: the live preview if one is set, else the
/// committed choice.
pub fn active_theme_choice() -> ThemeChoice {
    PREVIEW
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .unwrap_or_else(committed_theme_choice)
}

/// Set a live, non-persisted preview (used while browsing the picker). `None` clears it
/// and falls back to the committed theme.
pub fn set_theme_preview(choice: Option<ThemeChoice>) {
    *PREVIEW.write().unwrap_or_else(|e| e.into_inner()) = choice;
}

/// Commit a theme choice: persist it and clear any active preview.
pub fn commit_theme_choice(choice: ThemeChoice) -> anyhow::Result<()> {
    {
        let mut settings = SETTINGS.write().unwrap_or_else(|e| e.into_inner());
        settings.theme = choice;
        save_appearance_settings(&settings)?;
    }
    *PREVIEW.write().unwrap_or_else(|e| e.into_inner()) = None;
    Ok(())
}

pub fn active_palette() -> Palette {
    match active_theme_choice() {
        ThemeChoice::Default => BASE_DAY,
        ThemeChoice::Sanzo { index, dark } => match crate::sanzowada::theme_at(index) {
            Some(t) => {
                if dark {
                    t.dark
                } else {
                    colorize_light_surfaces(t.light)
                }
            }
            None => BASE_DAY,
        },
    }
}

/// Many Sanzo combos have a near-white lightest color (Cream/Sulphur/Pale Lemon
/// Yellow, etc.), so mapping "lightest -> background" makes those light themes look
/// like the default cream UI instead of their own combo. This pulls near-white
/// surfaces toward the theme's own accent (a Sanzo color) so each light theme reads
/// as itself. The tint is proportional to how close the background is to white, so
/// themes whose lightest color is already colored (mint, orange, etc.) are untouched.
pub(crate) fn colorize_light_surfaces(mut p: Palette) -> Palette {
    let luma = |c: Rgb| 0.299 * c.0 as f32 + 0.587 * c.1 as f32 + 0.114 * c.2 as f32;
    // 0 below luma 200, ramping to the cap by luma 255.
    let t = (((luma(p.app_bg) - 200.0) / 55.0).clamp(0.0, 1.0)) * 0.34;
    if t <= 0.0 {
        return p;
    }
    let accent = p.selected_bg;
    p.app_bg = blend(p.app_bg, accent, t);
    p.panel_bg = blend(p.panel_bg, accent, t);
    p.card_bg = blend(p.card_bg, accent, t);
    p.control_bg = blend(p.control_bg, accent, t);
    p.info_bg = blend(p.info_bg, accent, t);
    p
}

fn blend(a: Rgb, b: Rgb, t: f32) -> Rgb {
    let mix = |x: u8, y: u8| -> u8 {
        ((x as f32 * (1.0 - t)) + (y as f32 * t))
            .round()
            .clamp(0.0, 255.0) as u8
    };
    (mix(a.0, b.0), mix(a.1, b.1), mix(a.2, b.2))
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
    let stored: StoredAppearanceSettings = serde_json::from_str(&content)?;
    Ok(AppearanceSettings {
        theme: from_stored(stored.theme),
    })
}

pub fn save_appearance_settings(settings: &AppearanceSettings) -> anyhow::Result<()> {
    let settings_path = get_appearance_settings_file_path();
    if let Some(parent) = settings_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let stored = StoredAppearanceSettings {
        theme: to_stored(settings.theme),
    };
    let content = serde_json::to_string_pretty(&stored)?;
    fs::write(&settings_path, content)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn music_sheet_defaults_to_sanzo_dark_one() {
        assert_eq!(
            AppearanceSettings::default().theme,
            ThemeChoice::Sanzo {
                index: 0,
                dark: true
            }
        );
        set_theme_preview(Some(ThemeChoice::Default));
        assert_eq!(active_palette(), BASE_DAY);
        set_theme_preview(None);
    }

    #[test]
    fn preview_drives_active_palette_light_and_dark() {
        let t = crate::sanzowada::theme_at(0).unwrap();

        set_theme_preview(Some(ThemeChoice::Sanzo {
            index: 0,
            dark: false,
        }));
        assert_eq!(active_palette(), colorize_light_surfaces(t.light));

        set_theme_preview(Some(ThemeChoice::Sanzo {
            index: 0,
            dark: true,
        }));
        assert_eq!(active_palette(), t.dark);

        // Out-of-range index falls back to the default palette rather than panicking.
        set_theme_preview(Some(ThemeChoice::Sanzo {
            index: usize::MAX,
            dark: false,
        }));
        assert_eq!(active_palette(), BASE_DAY);

        set_theme_preview(None);
    }

    #[test]
    fn colorize_tints_near_white_light_surfaces_only() {
        // A near-white cream background is pulled toward the accent so it no longer
        // reads as the default cream UI.
        let mut cream = BASE_DAY;
        cream.app_bg = (251, 245, 205);
        cream.panel_bg = (251, 245, 205);
        cream.selected_bg = (224, 28, 21); // red accent
        let out = colorize_light_surfaces(cream);
        assert_ne!(out.app_bg, cream.app_bg, "near-white bg should be tinted");
        assert!(out.panel_bg != cream.panel_bg);

        // An already-colored (non-white) background is left untouched.
        let mut colored = BASE_DAY;
        colored.app_bg = (155, 212, 189); // mint, luma < 200
        colored.selected_bg = (227, 91, 30);
        let out2 = colorize_light_surfaces(colored);
        assert_eq!(out2.app_bg, colored.app_bg, "colored bg must not be tinted");
    }

    #[test]
    fn theme_choice_round_trips_through_stored_form() {
        let choice = ThemeChoice::Sanzo {
            index: 7,
            dark: true,
        };
        let stored = StoredAppearanceSettings {
            theme: to_stored(choice),
        };
        let json = serde_json::to_string(&stored).unwrap();
        assert!(
            json.contains("colors"),
            "stable color names must be persisted: {json}"
        );
        let back: StoredAppearanceSettings = serde_json::from_str(&json).unwrap();
        assert_eq!(from_stored(back.theme), choice);
    }

    #[test]
    fn legacy_index_only_settings_still_load() {
        let json = r#"{"theme":{"kind":"sanzo","index":3,"dark":false}}"#;
        let stored: StoredAppearanceSettings = serde_json::from_str(json).unwrap();
        assert_eq!(
            from_stored(stored.theme),
            ThemeChoice::Sanzo {
                index: 3,
                dark: false
            }
        );
    }

    #[test]
    fn color_names_win_over_a_stale_index() {
        // Simulates SANZO_THEMES having been reordered since the file was
        // written: the persisted names identify theme 2, the stale index says 0.
        let names: Vec<String> = crate::sanzowada::theme_at(2)
            .unwrap()
            .colors
            .iter()
            .map(|c| c.to_string())
            .collect();
        let stored = StoredThemeChoice::Sanzo {
            colors: Some(names),
            index: Some(0),
            dark: true,
        };
        assert_eq!(
            from_stored(stored),
            ThemeChoice::Sanzo {
                index: 2,
                dark: true
            }
        );
    }

    #[test]
    fn unresolvable_stored_theme_falls_back_to_default() {
        let stored = StoredThemeChoice::Sanzo {
            colors: Some(vec!["Not A Real Color".to_string()]),
            index: Some(usize::MAX),
            dark: false,
        };
        assert_eq!(from_stored(stored), ThemeChoice::Default);
    }
}
