# Linux packaging

Lege currently has first-pass Linux packaging for `.deb` and AppImage builds.
PaddleOCR, layout detection, and heavy Sauvola binarization are compiled by
default, and their models are embedded in the executables. No external model
staging directory is required.

## Debian package

The `.deb` package metadata lives in `Cargo.toml` under
`[package.metadata.deb]`.

```bash
cargo build --release --bin lege --bin lege-gui
cargo deb
```

The Debian package installs the CLI and GUI into `usr/bin`.

## AppImage

`cargo appimage` requires `appimagetool`, which is **not** in the Debian/Ubuntu
repositories. Download the official AppImage from
[AppImageKit releases](https://github.com/AppImage/AppImageKit/releases), make
it executable, and either put it on your `PATH` as `appimagetool` or pass its
location explicitly:

```bash
chmod +x appimagetool-x86_64.AppImage
# option A: place on PATH as `appimagetool`, then:
cargo appimage
# option B: point at it directly:
APPIMAGETOOL=/path/to/appimagetool-x86_64.AppImage cargo appimage
```

The `cargo appimage` task builds `lege` and `lege-gui` in release mode with the
normal default features, creates `target/appimage/Lege.AppDir`, and emits:

```bash
target/appimage/Lege-<version>-x86_64.AppImage
```

The AppImage bundles:

- `usr/bin/lege`
- `usr/bin/lege-gui`
- `lege.desktop`
- `lege.png`
- AppStream metadata

`AppRun` launches the GUI and exports `LD_LIBRARY_PATH` and `LEGE_DATA_DIR` so
Lege resolves bundled data from inside the mounted AppImage.

To include AppImage update metadata, set the update string accepted by
`appimagetool`:

```bash
APPIMAGE_UPDATE_INFORMATION='gh-releases-zsync|LegeApp|Lege|latest|Lege-*-x86_64.AppImage.zsync' \
  cargo appimage
```

To build with additional Cargo features, set `LEGE_CARGO_FEATURES`:

```bash
LEGE_CARGO_FEATURES='debug-logging' cargo appimage
```

## PaddleOCR real-GPU release gate

Before publishing a Linux package, run:

```bash
scripts/test-paddle-ocr-gpu.sh
```

This uses the embedded detector, recognizer, and dictionary against the
checked-in page fixture. It checks known output text and rejects CPU/software
wgpu adapters.
