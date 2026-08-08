// On Linux, Freya/Vulkan renders raw RGB without OS color management, so the
// subtle warm tint needs a slightly stronger base. The base colors live in
// `appearance.rs`; this module stays as the app-facing color accessor layer.

use crate::appearance::{Rgb, active_palette};

pub fn app_bg() -> Rgb {
    active_palette().app_bg
}

pub fn panel_bg() -> Rgb {
    active_palette().panel_bg
}

pub fn selected_bg() -> Rgb {
    active_palette().selected_bg
}

pub fn card_bg() -> Rgb {
    active_palette().card_bg
}

pub fn text_fg() -> Rgb {
    active_palette().text
}

pub fn muted_fg() -> Rgb {
    active_palette().muted
}

pub fn border() -> Rgb {
    active_palette().border
}

pub fn info_bg() -> Rgb {
    active_palette().info_bg
}

/// Readable foreground for notification cards, whose background deliberately
/// uses a theme's lightest color even while the rest of a dark theme uses white.
pub fn info_fg() -> Rgb {
    let (r, g, b) = info_bg();
    let luma = 0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32;
    if luma >= 150.0 {
        (20, 20, 20)
    } else {
        (255, 255, 255)
    }
}

pub fn control_bg() -> Rgb {
    active_palette().control_bg
}

pub fn hover_bg() -> Rgb {
    active_palette().hover_bg
}

/// Mouseover shade for interactive surfaces, derived from the surface's own
/// base color: light surfaces darken slightly, dark surfaces lighten by the
/// same amount, so the effect reads identically in light and dark themes.
/// General rule — use this for any hand-rolled hover effect instead of a
/// fixed darken/lighten.
pub fn hover_shade(base: Rgb) -> Rgb {
    const STEP: u8 = 8;
    let (r, g, b) = base;
    let luma = 0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32;
    if luma < 128.0 {
        (
            r.saturating_add(STEP),
            g.saturating_add(STEP),
            b.saturating_add(STEP),
        )
    } else {
        (
            r.saturating_sub(STEP),
            g.saturating_sub(STEP),
            b.saturating_sub(STEP),
        )
    }
}

pub fn focus_bg() -> Rgb {
    active_palette().focus_bg
}

pub fn active_bg() -> Rgb {
    active_palette().active_bg
}

pub fn inverse_surface() -> Rgb {
    active_palette().inverse_surface
}

pub fn inverse_surface_secondary() -> Rgb {
    active_palette().inverse_surface_secondary
}

pub fn inverse_surface_tertiary() -> Rgb {
    active_palette().inverse_surface_tertiary
}

pub fn border_focus() -> Rgb {
    active_palette().border_focus
}

pub fn check_mark() -> Rgb {
    active_palette().check_mark
}

pub fn progress_track_bg() -> Rgb {
    active_palette().progress_track_bg
}
