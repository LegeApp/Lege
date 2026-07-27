#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
VERSION="$(awk -F '"' '/^version = / { print $2; exit }' "$ROOT/lege-process/Cargo.toml")"
ARCH="${ARCH:-x86_64}"
APPDIR="${APPDIR:-$ROOT/target/appimage/Lege.AppDir}"
OUT_DIR="${OUT_DIR:-$ROOT/target/appimage}"
REPO_APPIMAGETOOL="$ROOT/lege-misc/packaging/appimage/tools/appimagetool-x86_64.AppImage"
if [[ -n "${APPIMAGETOOL:-}" ]]; then
  APPIMAGETOOL="$APPIMAGETOOL"
elif [[ -x "$REPO_APPIMAGETOOL" ]]; then
  APPIMAGETOOL="$REPO_APPIMAGETOOL"
else
  APPIMAGETOOL="appimagetool"
fi
PROFILE="${PROFILE:-release}"

if [[ "$PROFILE" != "release" ]]; then
  echo "Only PROFILE=release is wired for first-pass AppImage packaging." >&2
  exit 1
fi

required_inputs=(
  "$ROOT/lege-process/lege.desktop"
  "$ROOT/lege-misc/assets/icon.png"
)

for input in "${required_inputs[@]}"; do
  if [[ ! -f "$input" ]]; then
    echo "Missing packaging input: $input" >&2
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
  --package lege \
  --package lege-gui \
  "${features_args[@]}"

rm -rf "$APPDIR"
mkdir -p \
  "$APPDIR/usr/bin" \
  "$APPDIR/usr/lib/lege" \
  "$APPDIR/usr/share/applications" \
  "$APPDIR/usr/share/icons/hicolor/256x256/apps" \
  "$APPDIR/usr/share/metainfo"

install -Dm755 "$ROOT/target/release/lege" "$APPDIR/usr/bin/lege"
install -Dm755 "$ROOT/target/release/lege-gui" "$APPDIR/usr/bin/lege-gui"
install -Dm644 "$ROOT/lege-process/lege.desktop" "$APPDIR/usr/share/applications/lege.desktop"
install -Dm644 "$ROOT/lege-process/lege.desktop" "$APPDIR/lege.desktop"
install -Dm644 "$ROOT/lege-misc/assets/icon.png" "$APPDIR/usr/share/icons/hicolor/256x256/apps/lege.png"
install -Dm644 "$ROOT/lege-misc/assets/icon.png" "$APPDIR/lege.png"
install -Dm644 "$ROOT/lege-misc/assets/icon.png" "$APPDIR/.DirIcon"
install -Dm644 "$ROOT/lege-misc/packaging/appimage/com.legeapp.Lege.appdata.xml" \
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

"$APPIMAGETOOL" "${appimage_args[@]}" "$APPDIR" "$OUT_DIR/Lege-${VERSION}-${ARCH}.AppImage"
