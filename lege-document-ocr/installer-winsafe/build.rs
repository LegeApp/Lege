use image::{ImageBuffer, Rgba};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("manifest dir"));
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("out dir"));

    println!("cargo:rerun-if-env-changed=LEGE_OCR_INSTALLER_PAYLOAD");
    println!("cargo:rerun-if-env-changed=LEGE_OCR_UNINSTALLER_PATH");
    println!("cargo:rerun-if-env-changed=LEGE_OCR_BUILD_INSTALLER");

    let payload_path = embedded_input(
        "LEGE_OCR_INSTALLER_PAYLOAD",
        &manifest_dir.join("lege-document-ocr.7z"),
        &out_dir.join("empty-payload.7z"),
    );
    let uninstaller_path = embedded_input(
        "LEGE_OCR_UNINSTALLER_PATH",
        &manifest_dir.join("lege-document-ocr-uninstaller.exe"),
        &out_dir.join("empty-uninstaller.exe"),
    );

    let official_build = env::var_os("LEGE_OCR_BUILD_INSTALLER").is_some();
    if official_build && payload_path.metadata().map(|m| m.len()).unwrap_or(0) == 0 {
        panic!("official installer build has no payload; run scripts/build_windows_installer.ps1");
    }
    if official_build && uninstaller_path.metadata().map(|m| m.len()).unwrap_or(0) == 0 {
        panic!("official installer build has no embedded uninstaller");
    }

    println!(
        "cargo:rustc-env=LEGE_OCR_PAYLOAD_PATH={}",
        payload_path.display()
    );
    println!(
        "cargo:rustc-env=LEGE_OCR_UNINSTALLER_PATH={}",
        uninstaller_path.display()
    );
    println!("cargo:rerun-if-changed={}", payload_path.display());
    println!("cargo:rerun-if-changed={}", uninstaller_path.display());

    let workspace_cargo = manifest_dir.join("../../Cargo.toml");
    let version = read_workspace_version(&workspace_cargo);
    println!("cargo:rustc-env=EXPECTED_LEGE_OCR_VERSION={version}");
    println!("cargo:rerun-if-changed={}", workspace_cargo.display());

    let png_path = out_dir.join("lege-document-ocr-icon.png");
    let ico_path = out_dir.join("LegeDocumentOCR.ico");
    write_icon_png(&png_path);
    write_icon_ico(&png_path, &ico_path);
    println!("cargo:rustc-env=LEGE_OCR_ICON_PNG={}", png_path.display());
    println!("cargo:rustc-env=LEGE_OCR_ICON_ICO={}", ico_path.display());
}

fn embedded_input(variable: &str, local_path: &Path, empty_path: &Path) -> PathBuf {
    if let Some(path) = env::var_os(variable).map(PathBuf::from) {
        if !path.is_file() {
            panic!("{variable} does not name a file: {}", path.display());
        }
        return path;
    }
    if local_path.is_file() {
        return local_path.to_path_buf();
    }
    fs::write(empty_path, []).expect("write empty development embed");
    println!(
        "cargo:warning={variable} is unset; building a development shell without distributable payload"
    );
    empty_path.to_path_buf()
}

fn read_workspace_version(path: &Path) -> String {
    let text = fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("reading {}: {error}", path.display()));
    let mut in_workspace_package = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_workspace_package = trimmed == "[workspace.package]";
            continue;
        }
        if in_workspace_package {
            if let Some(rest) = trimmed.strip_prefix("version") {
                if let Some((_, value)) = rest.split_once('=') {
                    return value.trim().trim_matches('"').to_string();
                }
            }
        }
    }
    panic!("no [workspace.package] version found in {}", path.display());
}

fn write_icon_png(path: &Path) {
    let mut image = ImageBuffer::from_pixel(128, 128, Rgba([18, 50, 53, 255]));
    for y in 0..128 {
        for x in 0..128 {
            let tint = ((x + y) / 14) as u8;
            image.put_pixel(
                x,
                y,
                Rgba([18 + tint.min(16), 50 + tint.min(22), 53 + tint.min(20), 255]),
            );
        }
    }
    for y in 103..112 {
        for x in 13..115 {
            image.put_pixel(x, y, Rgba([154, 73, 55, 255]));
        }
    }
    for y in 113..118 {
        for x in 13..83 {
            image.put_pixel(x, y, Rgba([197, 154, 61, 255]));
        }
    }
    draw_text(&mut image, 10, 32, "LEGE", 4, Rgba([247, 244, 234, 255]));
    draw_text(&mut image, 22, 73, "OCR", 4, Rgba([247, 244, 234, 255]));
    image.save(path).expect("write png icon");
}

fn write_icon_ico(png_path: &Path, ico_path: &Path) {
    let image = ico::IconImage::read_png(fs::File::open(png_path).expect("open png"))
        .expect("read png for ico");
    let entry = ico::IconDirEntry::encode(&image).expect("encode ico entry");
    let mut directory = ico::IconDir::new(ico::ResourceType::Icon);
    directory.add_entry(entry);
    directory
        .write(fs::File::create(ico_path).expect("create ico"))
        .expect("write ico");
}

fn draw_text(
    image: &mut ImageBuffer<Rgba<u8>, Vec<u8>>,
    x: u32,
    y: u32,
    text: &str,
    scale: u32,
    color: Rgba<u8>,
) {
    let mut cursor = x;
    for character in text.chars() {
        draw_char(image, cursor, y, character, scale, color);
        cursor += 6 * scale;
    }
}

fn draw_char(
    image: &mut ImageBuffer<Rgba<u8>, Vec<u8>>,
    x: u32,
    y: u32,
    character: char,
    scale: u32,
    color: Rgba<u8>,
) {
    let rows = match character {
        'C' => [
            "01111", "10000", "10000", "10000", "10000", "10000", "01111",
        ],
        'E' => [
            "11111", "10000", "10000", "11110", "10000", "10000", "11111",
        ],
        'G' => [
            "01111", "10000", "10000", "10111", "10001", "10001", "01111",
        ],
        'L' => [
            "10000", "10000", "10000", "10000", "10000", "10000", "11111",
        ],
        'O' => [
            "01110", "10001", "10001", "10001", "10001", "10001", "01110",
        ],
        'R' => [
            "11110", "10001", "10001", "11110", "10100", "10010", "10001",
        ],
        _ => [
            "00000", "00000", "00000", "00000", "00000", "00000", "00000",
        ],
    };
    for (row_index, row) in rows.iter().enumerate() {
        for (column_index, bit) in row.as_bytes().iter().enumerate() {
            if *bit == b'1' {
                for dy in 0..scale {
                    for dx in 0..scale {
                        image.put_pixel(
                            x + column_index as u32 * scale + dx,
                            y + row_index as u32 * scale + dy,
                            color,
                        );
                    }
                }
            }
        }
    }
}
