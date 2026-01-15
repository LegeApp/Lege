/// Predefined portrait target resolutions for popular e-ink readers released since 2020.
/// 
/// These presets let the pipeline render directly to the device's native canvas
/// instead of using proportional scaling. Heights are always stored as the
/// longer edge so portrait rendering stays consistent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TargetDeviceProfile {
    pub name: &'static str,
    pub width: u32,
    pub height: u32,
}

pub const TARGET_DEVICE_PROFILES: &[TargetDeviceProfile] = &[
    TargetDeviceProfile { name: "Amazon Kindle (11th Gen, 2022)", width: 1072, height: 1448 },
    TargetDeviceProfile { name: "Amazon Kindle Paperwhite (11th Gen, 2021)", width: 1236, height: 1648 },
    TargetDeviceProfile { name: "Amazon Kindle Scribe (2022)", width: 1860, height: 2480 },
    TargetDeviceProfile { name: "B&N Nook GlowLight 4 (2021)", width: 1072, height: 1448 },
    TargetDeviceProfile { name: "B&N Nook GlowLight 4e (2022)", width: 758, height: 1024 },
    TargetDeviceProfile { name: "Bigme InkNote Color (2022)", width: 1404, height: 1872 },
    TargetDeviceProfile { name: "Boyue Likebook P10 (2021)", width: 1404, height: 1872 },
    TargetDeviceProfile { name: "Huawei MatePad Paper (2022)", width: 1404, height: 1872 },
    TargetDeviceProfile { name: "Kobo Clara 2E (2022)", width: 1072, height: 1448 },
    TargetDeviceProfile { name: "Kobo Elipsa 2E (2023)", width: 1404, height: 1872 },
    TargetDeviceProfile { name: "Kobo Libra 2 (2021)", width: 1264, height: 1680 },
    TargetDeviceProfile { name: "Kobo Sage (2021)", width: 1440, height: 1920 },
    TargetDeviceProfile { name: "Onyx Boox Leaf 2 (2022)", width: 1264, height: 1680 },
    TargetDeviceProfile { name: "Onyx Boox Note Air (2020)", width: 1404, height: 1872 },
    TargetDeviceProfile { name: "Onyx Boox Nova Air 2 (2023)", width: 1404, height: 1872 },
    TargetDeviceProfile { name: "Onyx Boox Nova3 Color (2021)", width: 1404, height: 1872 },
    TargetDeviceProfile { name: "Onyx Boox Palma (2023)", width: 824, height: 1648 },
    TargetDeviceProfile { name: "Onyx Boox Tab Ultra (2022)", width: 1404, height: 1872 },
    TargetDeviceProfile { name: "Onyx Boox Tab X (2023)", width: 1650, height: 2200 },
    TargetDeviceProfile { name: "PocketBook Color (2020)", width: 1072, height: 1448 },
    TargetDeviceProfile { name: "PocketBook Era (2022)", width: 1264, height: 1680 },
    TargetDeviceProfile { name: "PocketBook InkPad Color 2 (2023)", width: 1404, height: 1872 },
    TargetDeviceProfile { name: "Ratta Supernote A5 X (2020)", width: 1404, height: 1872 },
    TargetDeviceProfile { name: "Ratta Supernote A6 X (2020)", width: 1404, height: 1872 },
    TargetDeviceProfile { name: "reMarkable 2 (2020)", width: 1404, height: 1872 },
    TargetDeviceProfile { name: "Tolino Epos 3 (2021)", width: 1404, height: 1872 },
    TargetDeviceProfile { name: "Tolino Vision 6 (2021)", width: 1264, height: 1680 },
    TargetDeviceProfile { name: "Xiaomi Mi Reader Pro (2020)", width: 1404, height: 1872 },
];

/// Label used by the UI/CLI when users want to fall back to proportional scaling.
pub const PROPORTIONAL_OPTION_LABEL: &str = "Set height, width proportional";

/// Locate a device profile by its display label.
pub fn find_profile(name: &str) -> Option<TargetDeviceProfile> {
    let normalized = normalize_label(name);
    TARGET_DEVICE_PROFILES
        .iter()
        .copied()
        .find(|profile| {
            profile.name.eq_ignore_ascii_case(name)
                || normalize_label(profile.name) == normalized
        })
        .map(|profile| {
            debug_assert!(
                profile.width <= profile.height,
                "Target profile '{}' has width greater than height ({}x{})",
                profile.name,
                profile.width,
                profile.height
            );
            profile
        })
}

fn normalize_label(label: &str) -> String {
    label
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .map(|ch| ch.to_ascii_lowercase())
        .collect()
}
