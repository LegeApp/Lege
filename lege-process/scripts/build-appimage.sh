#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
VERSION="$(awk -F '"' '/^version = / { print $2; exit }' "$ROOT/lege-process/Cargo.toml")"
ARCH="${ARCH:-x86_64}"
APPDIR="${APPDIR:-$ROOT/target/appimage/Lege.AppDir}"
OUT_DIR="${OUT_DIR:-$ROOT/target/appimage}"
APPIMAGE_APPDATA_PATH="$ROOT/lege-process/packaging/appimage/com.legeapp.Lege.appdata.xml"
APPIMAGETOOL_PATH=""
LOCAL_APPIMAGETOOL="$ROOT/lege-process/packaging/appimage/tools/appimagetool-x86_64.AppImage"
APPIMAGE_GUI="${APPIMAGE_GUI:-${1:-freya}}"
PROFILE="${PROFILE:-release}"

if [[ "$PROFILE" != "release" ]]; then
  echo "Only PROFILE=release is wired for first-pass AppImage packaging." >&2
  exit 1
fi

case "$APPIMAGE_GUI" in
  freya)
    ;;
  viewer)
    ;;
  *)
    echo "Unsupported GUI variant '$APPIMAGE_GUI'. Use 'freya' (default) or 'viewer'." >&2
    exit 1
    ;;
esac

required_inputs=(
  "$ROOT/lege-process/lege.desktop"
  "$ROOT/lege-misc/assets/icon.png"
  "$APPIMAGE_APPDATA_PATH"
)

for input in "${required_inputs[@]}"; do
  if [[ ! -f "$input" ]]; then
    echo "Missing packaging input: $input" >&2
    exit 1
  fi
done

if [[ -n "${APPIMAGETOOL:-}" ]]; then
  if [[ -x "$APPIMAGETOOL" ]]; then
    APPIMAGETOOL_PATH="$APPIMAGETOOL"
  elif command -v "$APPIMAGETOOL" >/dev/null 2>&1; then
    APPIMAGETOOL_PATH="$(command -v "$APPIMAGETOOL")"
  fi
fi

if [[ -z "$APPIMAGETOOL_PATH" && -x "$LOCAL_APPIMAGETOOL" ]]; then
  APPIMAGETOOL_PATH="$LOCAL_APPIMAGETOOL"
fi

if [[ -z "$APPIMAGETOOL_PATH" ]] && command -v appimagetool >/dev/null 2>&1; then
  APPIMAGETOOL_PATH="$(command -v appimagetool)"
fi

if [[ -z "$APPIMAGETOOL_PATH" ]]; then
  echo "Missing appimagetool. Set APPIMAGETOOL=/path/to/appimagetool (exported), place it on PATH as 'appimagetool', or ensure"
  echo "  $LOCAL_APPIMAGETOOL"
  echo "is present and executable." >&2
  exit 1
fi

features_args=()
if [[ -n "${LEGE_CARGO_FEATURES:-}" ]]; then
  features_args+=(--features "$LEGE_CARGO_FEATURES")
fi

cargo build \
  --release \
  --package lege \
  "${features_args[@]}"

# Each frontend owns a uniquely named binary; the AppDir always installs it as
# `lege-gui`, which is what lege.desktop and AppRun exec.
if [[ "$APPIMAGE_GUI" == "viewer" ]]; then
  GUI_BINARY="lege-viewer"
  cargo build --release \
    --manifest-path "$ROOT/Cargo.toml" \
    --package lege-viewer
else
  GUI_BINARY="lege-gui"
  cargo build --release \
    --manifest-path "$ROOT/Cargo.toml" \
    --package lege-gui-freya \
    "${features_args[@]}"
fi

if [[ -d "$APPDIR" ]]; then
  find "$APPDIR" -mindepth 1 -delete
else
  mkdir -p "$APPDIR"
fi
mkdir -p \
  "$APPDIR/usr/bin" \
  "$APPDIR/usr/share/applications" \
  "$APPDIR/usr/share/icons/hicolor/256x256/apps" \
  "$APPDIR/usr/share/metainfo"

install -Dm755 "$ROOT/target/release/lege" "$APPDIR/usr/bin/lege"
install -Dm755 "$ROOT/target/release/$GUI_BINARY" "$APPDIR/usr/bin/lege-gui"

install -Dm644 "$ROOT/lege-process/lege.desktop" "$APPDIR/usr/share/applications/lege.desktop"
install -Dm644 "$ROOT/lege-process/lege.desktop" "$APPDIR/lege.desktop"
install -Dm644 "$ROOT/lege-misc/assets/icon.png" "$APPDIR/usr/share/icons/hicolor/256x256/apps/lege.png"
install -Dm644 "$ROOT/lege-misc/assets/icon.png" "$APPDIR/lege.png"
install -Dm644 "$ROOT/lege-misc/assets/icon.png" "$APPDIR/.DirIcon"
install -Dm644 "$APPIMAGE_APPDATA_PATH" \
  "$APPDIR/usr/share/metainfo/com.legeapp.Lege.appdata.xml"

cat >"$APPDIR/AppRun" <<'APPRUN'
#!/usr/bin/env bash
set -euo pipefail

HERE="$(dirname "$(readlink -f "${0}")")"
export LD_LIBRARY_PATH="${HERE}/usr/lib/lege:${HERE}/usr/lib:${LD_LIBRARY_PATH:-}"
export LEGE_DATA_DIR="${HERE}/usr/share/lege"

exec "${HERE}/usr/bin/lege-gui" "$@"
APPRUN
chmod 755 "$APPDIR/AppRun"

mkdir -p "$OUT_DIR"
appimage_args=()
if [[ -n "${APPIMAGE_UPDATE_INFORMATION:-}" ]]; then
  appimage_args+=("-u" "$APPIMAGE_UPDATE_INFORMATION")
fi

"$APPIMAGETOOL_PATH" "${appimage_args[@]}" "$APPDIR" "$OUT_DIR/Lege-${VERSION}-${ARCH}.AppImage"
