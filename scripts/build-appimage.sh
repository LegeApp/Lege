#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VERSION="$(awk -F '"' '/^version = / { print $2; exit }' "$ROOT/Cargo.toml")"
ARCH="${ARCH:-x86_64}"
APPDIR="${APPDIR:-$ROOT/target/appimage/Lege.AppDir}"
OUT_DIR="${OUT_DIR:-$ROOT/target/appimage}"
INPUT_DIR="${LEGE_PACKAGE_INPUT_DIR:-}"
APPIMAGETOOL="${APPIMAGETOOL:-appimagetool}"
PROFILE="${PROFILE:-release}"

if [[ "$PROFILE" != "release" ]]; then
  echo "Only PROFILE=release is wired for first-pass AppImage packaging." >&2
  exit 1
fi

if [[ -z "$INPUT_DIR" ]]; then
  echo "Set LEGE_PACKAGE_INPUT_DIR to the folder containing libpdfium.so, yolo-layout.onnx, sauvola.onnx, and eng.traineddata." >&2
  echo "Tip: extract the existing Linux .tar.gz from GitHub releases to a folder of your choice." >&2
  exit 1
fi

required_inputs=(
  "$INPUT_DIR/libpdfium.so"
  "$INPUT_DIR/yolo-layout.onnx"
  "$INPUT_DIR/sauvola.onnx"
  "$INPUT_DIR/eng.traineddata"
  "$ROOT/lege.desktop"
  "$ROOT/assets/icon.png"
)

for input in "${required_inputs[@]}"; do
  if [[ ! -f "$input" ]]; then
    echo "Missing packaging input: $input" >&2
    echo "Extract the Linux release .tar.gz and set LEGE_PACKAGE_INPUT_DIR to that folder." >&2
    exit 1
  fi
done

if ! command -v "$APPIMAGETOOL" >/dev/null 2>&1; then
  echo "Missing appimagetool. Install it or set APPIMAGETOOL=/path/to/appimagetool." >&2
  exit 1
fi

features_args=()
if [[ -n "${LEGE_CARGO_FEATURES:-}" ]]; then
  features_args+=(--features "$LEGE_CARGO_FEATURES")
fi

cargo build \
  --release \
  --bin lege \
  --bin lege-gui \
  --no-default-features \
  "${features_args[@]}"

rm -rf "$APPDIR"
mkdir -p \
  "$APPDIR/usr/bin" \
  "$APPDIR/usr/lib/lege" \
  "$APPDIR/usr/share/lege/models" \
  "$APPDIR/usr/share/lege/tessdata" \
  "$APPDIR/usr/share/applications" \
  "$APPDIR/usr/share/icons/hicolor/256x256/apps" \
  "$APPDIR/usr/share/metainfo"

install -Dm755 "$ROOT/target/release/lege" "$APPDIR/usr/bin/lege"
install -Dm755 "$ROOT/target/release/lege-gui" "$APPDIR/usr/bin/lege-gui"
install -Dm644 "$INPUT_DIR/libpdfium.so" "$APPDIR/usr/lib/lege/libpdfium.so"
install -Dm644 "$INPUT_DIR/yolo-layout.onnx" "$APPDIR/usr/share/lege/models/yolo-layout.onnx"
install -Dm644 "$INPUT_DIR/sauvola.onnx" "$APPDIR/usr/share/lege/models/sauvola.onnx"
install -Dm644 "$INPUT_DIR/eng.traineddata" "$APPDIR/usr/share/lege/tessdata/eng.traineddata"
install -Dm644 "$ROOT/lege.desktop" "$APPDIR/usr/share/applications/lege.desktop"
install -Dm644 "$ROOT/lege.desktop" "$APPDIR/lege.desktop"
install -Dm644 "$ROOT/assets/icon.png" "$APPDIR/usr/share/icons/hicolor/256x256/apps/lege.png"
install -Dm644 "$ROOT/assets/icon.png" "$APPDIR/lege.png"
install -Dm644 "$ROOT/assets/icon.png" "$APPDIR/.DirIcon"
install -Dm644 "$ROOT/packaging/appimage/com.legeapp.Lege.appdata.xml" \
  "$APPDIR/usr/share/metainfo/com.legeapp.Lege.appdata.xml"

cat >"$APPDIR/AppRun" <<'APPRUN'
#!/usr/bin/env bash
set -euo pipefail

HERE="$(dirname "$(readlink -f "${0}")")"
export LD_LIBRARY_PATH="${HERE}/usr/lib/lege:${HERE}/usr/lib:${LD_LIBRARY_PATH:-}"
export LEGE_DATA_DIR="${HERE}/usr/share/lege"
export TESSDATA_PREFIX="${HERE}/usr/share/lege/tessdata"

exec "${HERE}/usr/bin/lege-gui" "$@"
APPRUN
chmod 755 "$APPDIR/AppRun"

mkdir -p "$OUT_DIR"
appimage_args=()
if [[ -n "${APPIMAGE_UPDATE_INFORMATION:-}" ]]; then
  appimage_args+=("-u" "$APPIMAGE_UPDATE_INFORMATION")
fi

"$APPIMAGETOOL" "${appimage_args[@]}" "$APPDIR" "$OUT_DIR/Lege-${VERSION}-${ARCH}.AppImage"
