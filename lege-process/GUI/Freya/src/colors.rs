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

pub fn job_accent_color(index: u32) -> Rgb {
    const COLORS: [Rgb; 15] = [
        (180, 217, 232), // sky blue
        (232, 218, 166), // warm yellow
        (220, 192, 214), // rose
        (214, 196, 234), // lavender
        (180, 232, 180), // mint green
        (232, 196, 180), // peach
        (180, 232, 220), // seafoam
        (232, 180, 200), // coral pink
        (196, 180, 232), // periwinkle
        (195, 220, 180), // sage green
        (232, 210, 180), // apricot
        (180, 210, 210), // steel teal
        (210, 180, 232), // lilac
        (180, 225, 210), // mint teal
        (235, 205, 175), // warm sand
    ];
    COLORS[(index as usize) % COLORS.len()]
}
