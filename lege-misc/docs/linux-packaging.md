# Linux packaging

Lege currently has first-pass Linux packaging for `.deb` and AppImage builds.
PaddleOCR, layout detection, and heavy Sauvola binarization are compiled by
default, and their models are embedded in the executables. No external model
staging directory is required.

## Debian package

The `.deb` package metadata lives in `Cargo.toml` under
`[package.metadata.deb]`.

```bash
cargo build --release --package lege --package lege-gui-freya
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
# default Freya GUI
cargo appimage
# optional viewer GUI
cargo appimage viewer
# option B: point at it directly (same shell command):
APPIMAGETOOL=/path/to/appimagetool-x86_64.AppImage cargo appimage
# or keep it for this session:
export APPIMAGETOOL=/path/to/appimagetool-x86_64.AppImage
cargo appimage viewer
cargo appimage
```

`cargo appimage` also falls back to the bundled tool at
`lege-process/packaging/appimage/tools/appimagetool-x86_64.AppImage` if neither
`APPIMAGETOOL` nor PATH-provided `appimagetool` is available.

You can also force variant through env:
```bash
APPIMAGE_GUI=viewer cargo appimage
```

By default, `cargo appimage` builds the core CLI plus the Freya GUI (`lege-process/GUI/Freya`).
Pass `viewer` (or `APPIMAGE_GUI=viewer`) to build the legacy viewer GUI instead.
The task builds in release mode with the requested GUI target, creates
`target/appimage/Lege.AppDir`, and emits:

```bash
target/appimage/Lege-<version>-x86_64.AppImage
```

The AppImage bundles:

- `usr/bin/lege`
- `usr/bin/lege-gui`
- `lege.desktop`
- `usr/share/icons/hicolor/256x256/apps/lege.png`
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
