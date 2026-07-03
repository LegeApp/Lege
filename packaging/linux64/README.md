# Linux packaging inputs

This directory documents the release-time Linux runtime assets used for a
`.deb` or AppImage. Download the existing Linux `.tar.gz` from GitHub releases,
extract it to a folder of your choice, and set `LEGE_PACKAGE_INPUT_DIR` to that
folder before building an AppImage:

```bash
export LEGE_PACKAGE_INPUT_DIR=/path/to/extracted/linux64
```

Expected files:

- `libpdfium.so`
- `yolo-layout.onnx`
- `sauvola.onnx`
- `eng.traineddata`

These files are intentionally not committed because they are large binary
artifacts and may have separate upstream redistribution terms.
