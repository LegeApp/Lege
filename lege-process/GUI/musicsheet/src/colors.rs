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

/// The pastel palette the progress bar draws from (and which other accents may
/// reuse). The progress bar no longer cycles through it per job; instead it
/// derives a single color from the active theme (see [`progress_bar_color`]),
/// but the palette is kept so edits here still drive that derivation.
///
/// Only consumed by [`tests::default_progress_bar_color_is_reddest_job_accent`],
/// which checks that [`DEFAULT_PROGRESS_BAR_COLOR`] still tracks the reddest
/// entry here.
#[cfg(test)]
pub const JOB_ACCENT_COLORS: [Rgb; 15] = [
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

/// The progress-bar color on the Default (non–Sanzo Wada) theme: the reddest of
/// [`JOB_ACCENT_COLORS`] (warm sand), where redness = `r - (g + b) / 2` (how much
/// red dominates the other channels). A unit test asserts this const stays equal
/// to that computation over the palette, so palette edits can't silently drift it.
pub const DEFAULT_PROGRESS_BAR_COLOR: Rgb = (235, 205, 175);

/// The single progress-bar color for the active theme. Standard (Default) theme:
/// [`DEFAULT_PROGRESS_BAR_COLOR`], the reddest pastel in [`JOB_ACCENT_COLORS`].
/// Sanzo Wada themes: the color the theme uses for its mouseover/hover accent
/// (`hover_bg`), so the bar matches the theme's own hover styling instead of
/// cycling through pastels.
pub fn progress_bar_color() -> Rgb {
    match crate::appearance::active_theme_choice() {
        crate::appearance::ThemeChoice::Default => DEFAULT_PROGRESS_BAR_COLOR,
        crate::appearance::ThemeChoice::Sanzo { .. } => hover_bg(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Guards [`DEFAULT_PROGRESS_BAR_COLOR`] against palette drift: it must always
    /// equal the reddest entry of [`JOB_ACCENT_COLORS`] by `r - (g + b) / 2`. If
    /// this fails, the palette was edited — update the const to the new maximum.
    #[test]
    fn default_progress_bar_color_is_reddest_job_accent() {
        let reddest = JOB_ACCENT_COLORS
            .iter()
            .copied()
            .max_by_key(|c| c.0 as i32 - (c.1 as i32 + c.2 as i32) / 2)
            .expect("palette is non-empty");
        assert_eq!(
            DEFAULT_PROGRESS_BAR_COLOR, reddest,
            "DEFAULT_PROGRESS_BAR_COLOR must track the reddest JOB_ACCENT_COLORS entry"
        );
    }
}
