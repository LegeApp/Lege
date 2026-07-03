# Linux packaging

Lege currently has first-pass Linux packaging for `.deb` and AppImage builds.
Both formats read the same runtime asset staging directory:

```bash
<staging>/
  libpdfium.so
  yolo-layout.onnx
  sauvola.onnx
  eng.traineddata
```

Those binary inputs are not committed to the repository. Download the existing
Linux `.tar.gz` from the
[GitHub releases](https://github.com/LegeApp/Lege/releases), extract it to a
folder of your choice, and point packaging at that folder:

```bash
export LEGE_PACKAGE_INPUT_DIR=/path/to/extracted/linux64
```

## Bundled `eng.traineddata`

Lege ships an `eng.traineddata` file even though Tesseract installs its own.
This is the improved English model from the University of Freiburg, which
delivers better OCR accuracy on English text than the stock Tesseract data.
Swap in the stock file from your Tesseract installation if you prefer it.

## Debian package

The `.deb` package metadata lives in `Cargo.toml` under
`[package.metadata.deb]`. Its asset paths mirror the AppImage inputs; update
them to match wherever you extracted the release assets.

```bash
cargo build --release --bin lege --bin lege-gui --no-default-features
cargo deb
```

The Debian package installs the CLI and GUI into `usr/bin`, Pdfium into
`usr/lib/lege`, the ONNX models into `usr/share/lege/models`, and bundled
English tessdata into `usr/share/lege/tessdata`.

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

The `cargo appimage` task builds `lege` and `lege-gui` in release mode with
`--no-default-features`, creates `target/appimage/Lege.AppDir`, and emits:

```bash
target/appimage/Lege-<version>-x86_64.AppImage
```

The AppImage bundles:

- `usr/bin/lege`
- `usr/bin/lege-gui`
- `usr/lib/lege/libpdfium.so`
- `usr/share/lege/models/yolo-layout.onnx`
- `usr/share/lege/models/sauvola.onnx`
- `usr/share/lege/tessdata/eng.traineddata`
- `lege.desktop`
- `lege.png`
- AppStream metadata

`AppRun` launches the GUI and exports `LD_LIBRARY_PATH`, `LEGE_DATA_DIR`, and
`TESSDATA_PREFIX` so Lege resolves the bundled libraries and data from inside
the mounted AppImage.

To include AppImage update metadata, set the update string accepted by
`appimagetool`:

```bash
APPIMAGE_UPDATE_INFORMATION='gh-releases-zsync|LegeApp|Lege|latest|Lege-*-x86_64.AppImage.zsync' \
  cargo appimage
```

To build with additional Cargo features, set `LEGE_CARGO_FEATURES`:

```bash
LEGE_CARGO_FEATURES='tesseract-ocr,jp2-lam' cargo appimage
```

## Remaining external dependency

The first-pass AppImage is not fully self-contained for Linux OCR when built
with the `tesseract-ocr` feature. It can bundle `eng.traineddata`, but Lege's
Linux OCR path still expects a host-accessible `tesseract` binary and compatible
Tesseract/leptonica shared libraries. The default AppImage script matches the
current `.deb` build settings and disables default Cargo features; in that mode,
OCR is not compiled in.
