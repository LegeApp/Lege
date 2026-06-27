// On Linux, Freya/Vulkan renders raw RGB without OS color management, so the
// subtle (255,250,250) warm tint is indistinguishable from pure white. Use a
// more distinct cream/parchment value that is visible without ICC profiles.
// On Windows/macOS the OS pipeline adds warmth, making (255,250,250) correctly
// paper-like, so leave it unchanged there.

pub const APP_BG: (u8, u8, u8) = (226, 232, 235);

#[cfg(target_os = "linux")]
pub const PANEL_BG: (u8, u8, u8) = (253, 249, 244);
#[cfg(not(target_os = "linux"))]
pub const PANEL_BG: (u8, u8, u8) = (255, 250, 245);

#[cfg(target_os = "linux")]
pub const SELECTED_BG: (u8, u8, u8) = (251, 247, 242);
#[cfg(not(target_os = "linux"))]
pub const SELECTED_BG: (u8, u8, u8) = (253, 248, 243);

#[cfg(target_os = "linux")]
pub const CARD_BG: (u8, u8, u8) = (253, 249, 244);
#[cfg(not(target_os = "linux"))]
pub const CARD_BG: (u8, u8, u8) = (255, 250, 245);

pub const TEXT: (u8, u8, u8) = (20, 20, 20);
pub const MUTED: (u8, u8, u8) = (90, 90, 90);
pub const BORDER: (u8, u8, u8) = (128, 128, 128);
pub const INFO_BG: (u8, u8, u8) = (255, 255, 225);

pub fn job_accent_color(index: u32) -> (u8, u8, u8) {
    const COLORS: [(u8, u8, u8); 15] = [
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
