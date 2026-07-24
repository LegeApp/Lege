# Linux packaging

Lege currently has first-pass Linux packaging for `.deb` and AppImage builds.
Both formats read the same runtime asset staging directory:

```bash
<staging>/
  sauvola.onnx
```

Those binary inputs are not committed to the repository. Download the existing
Linux `.tar.gz` from the
[GitHub releases](https://github.com/LegeApp/Lege/releases), extract it to a
folder of your choice, and point packaging at that folder:

```bash
export LEGE_PACKAGE_INPUT_DIR=/path/to/extracted/linux64
```

PaddleOCR and layout detection are compiled by default, and their models are
embedded in the executables. Only the optional heavy-binarization model
remains a packaging input.

## Debian package

The `.deb` package metadata lives in `Cargo.toml` under
`[package.metadata.deb]`. Its asset paths mirror the AppImage inputs; update
them to match wherever you extracted the release assets.

```bash
cargo build --release --bin lege --bin lege-gui
cargo deb
```

The Debian package installs the CLI and GUI into `usr/bin` and
`sauvola.onnx` into `usr/share/lege/models`.

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
- `usr/share/lege/models/sauvola.onnx`
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
