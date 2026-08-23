#!/usr/bin/env bash
# Build "Lege Sheet Music Edition.app" (+ dmg/zip) for macOS.
#
# Runs either on a real Mac or inside the macos-cross-compiler container
# (ghcr.io/shepherdjerred/macos-cross-compiler). On Linux, a normal invocation
# automatically starts that container when the Darwin compiler is not installed.
# The whole Lege ecosystem workspace root (the directory that holds the virtual
# Cargo.toml, lege-process/, lege-misc/, lege-codecs/, …) is mounted at
# /workspace so cargo can see every workspace member. The equivalent manual
# container invocation is:
#
#   sudo docker run --platform=linux/amd64 \
#     -v /mnt/Samsung980_1TB/Rust-projects/Lege-ecosystem:/workspace \
#     --rm ghcr.io/shepherdjerred/macos-cross-compiler:latest \
#     /bin/bash -lc 'rustup toolchain install 1.97.0 --profile minimal \
#       --target x86_64-apple-darwin --no-self-update && \
#       RUSTUP_TOOLCHAIN=1.97.0 bash -lc "cd /workspace && lege-process/scripts/build-macos-app.sh"'
#
# Inputs / env:
#   TARGET        rust target triple        (default: x86_64-apple-darwin)
#   SAUVOLA_ONNX  path to sauvola.onnx      (default:
#                 lege-process/packaging/macos/sauvola.onnx; required — the
#                 --binarization heavy mode loads it at runtime)
#   OUT_DIR       output directory          (default: <workspace>/target/macos)
#   CODESIGN_ID   codesign identity — macOS only (default: "-" = ad-hoc)
#   SKIP_BUILD    set to 1 to repackage already-built target artifacts
#   AUTO_CONTAINER set to 0 to disable automatic Docker/Podman use on Linux
#   CONTAINER_RUNTIME Docker/Podman executable (auto-detected by default)
#   MACOS_CROSS_IMAGE cross-compiler image override
#   MACOS_RUST_TOOLCHAIN Rust toolchain installed inside the cross container
#
# Notes:
#   * The Sheet Music Edition GUI is its own crate (GUI/musicsheet, binary
#     lege-music-gui) and is built with --no-default-features: the preset runs
#     OCR off anyway. The bundled `lege` CLI, however, now builds with the
#     pure-Rust PP-OCR backend (paddle-ocr), so OCR works on macOS without any
#     Tesseract native dependency. sauvola.onnx IS bundled in Resources because the
#     --music-sheet preset now supports every --binarization mode and
#     "heavy" loads the Sauvola model at runtime. yolo-layout.onnx is NOT
#     bundled — the preset always runs with layout detection off.
#   * DjVu encoding is linked directly into the AGPL application; no helper
#     executable is built or bundled.
#   * Gatekeeper: an ad-hoc signature seals the bundle but does not establish a
#     trusted developer identity. On current macOS, first try to launch the app,
#     then approve it in System Settings → Privacy & Security → Open Anyway.
#     True one-click distribution needs a Developer ID certificate and Apple
#     notarization; neither is possible without Apple Developer Program access.

set -euo pipefail

# Layout of the Lege ecosystem workspace:
#   ROOT         — the workspace root (virtual Cargo.toml; also the cargo target/)
#   PROCESS_DIR  — the `lege` crate (packaging/, per-crate Cargo.toml version)
#   MISC_DIR     — shared docs/ + assets/ used by the packaged bundle
# This script lives in $PROCESS_DIR/scripts, so ROOT is two levels up.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROCESS_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
ROOT="$(cd "$PROCESS_DIR/.." && pwd)"
MISC_DIR="${MISC_DIR:-$ROOT/lege-misc}"
TARGET="${TARGET:-x86_64-apple-darwin}"
OUT_DIR="${OUT_DIR:-$ROOT/target/macos}"
SAUVOLA_ONNX="${SAUVOLA_ONNX:-$PROCESS_DIR/packaging/macos/sauvola.onnx}"
CODESIGN_ID="${CODESIGN_ID:--}"
SKIP_BUILD="${SKIP_BUILD:-0}"
SKIP_CLI_BUILD="${SKIP_CLI_BUILD:-0}"
AUTO_CONTAINER="${AUTO_CONTAINER:-1}"
CONTAINER_RUNTIME="${CONTAINER_RUNTIME:-}"
MACOS_CROSS_IMAGE="${MACOS_CROSS_IMAGE:-ghcr.io/shepherdjerred/macos-cross-compiler:latest}"
MACOS_RUST_TOOLCHAIN="${MACOS_RUST_TOOLCHAIN:-1.97.0}"
VERSION="$(awk -F '"' '/^version = / { print $2; exit }' "$PROCESS_DIR/Cargo.toml")"
APP_NAME="Lege Sheet Music Edition"
BUNDLE_ID="com.legeapp.lege-music"
ARCH_PREFIX="${TARGET%%-*}"

case "$TARGET" in
  x86_64-apple-darwin | aarch64-apple-darwin) ;;
  *)
    echo "Unsupported macOS target: $TARGET" >&2
    exit 1
    ;;
esac

# Make the documented Linux cross-build the default behavior. The marker keeps
# the script from trying to start a nested container after it re-enters itself.
if [[ "$(uname -s)" != "Darwin" \
      && "$SKIP_BUILD" != "1" \
      && "$AUTO_CONTAINER" == "1" \
      && "${LEGE_MACOS_CROSS_CONTAINER:-0}" != "1" \
      && -z "$(command -v "${ARCH_PREFIX}-apple-darwin24-clang" || true)" ]]; then
  if [[ -z "$CONTAINER_RUNTIME" ]]; then
    if command -v docker >/dev/null 2>&1; then
      CONTAINER_RUNTIME="docker"
    elif command -v podman >/dev/null 2>&1; then
      CONTAINER_RUNTIME="podman"
    else
      echo "No Darwin compiler, Docker, or Podman found." >&2
      echo "Install Docker/Podman, or set AUTO_CONTAINER=0 and provide the cross toolchain." >&2
      exit 1
    fi
  fi

  mkdir -p "$ROOT/target/macos-cross-cache/cargo" \
    "$ROOT/target/macos-cross-cache/rustup"

  echo "== Starting macOS cross-compiler container ($MACOS_CROSS_IMAGE)"
  "$CONTAINER_RUNTIME" run --platform=linux/amd64 --rm \
    -e LEGE_MACOS_CROSS_CONTAINER=1 \
    -e TARGET="$TARGET" \
    -e CODESIGN_ID="$CODESIGN_ID" \
    -e SKIP_CLI_BUILD="$SKIP_CLI_BUILD" \
    -e MACOS_RUST_TOOLCHAIN="$MACOS_RUST_TOOLCHAIN" \
    -e CARGO_HOME=/workspace/target/macos-cross-cache/cargo \
    -e RUSTUP_HOME=/workspace/target/macos-cross-cache/rustup \
    -v "$ROOT:/workspace" \
    "$MACOS_CROSS_IMAGE" \
    /bin/bash -lc 'rustup toolchain install "$MACOS_RUST_TOOLCHAIN" --profile minimal --target "$TARGET" --no-self-update && export RUSTUP_TOOLCHAIN="$MACOS_RUST_TOOLCHAIN" && cd /workspace && lege-process/scripts/build-macos-app.sh'

  # The cross image does not currently ship rcodesign. Once it returns, use a
  # host installation when available and replace the container's deliberately
  # unsigned archive with a signed one.
  HOST_APP="$OUT_DIR/$APP_NAME.app"
  ARCHIVE_STEM="Lege-Sheet-Music-Edition-$VERSION-$ARCH_PREFIX"
  if command -v rcodesign >/dev/null 2>&1 && [[ -d "$HOST_APP" ]]; then
    echo "== Ad-hoc signing the macOS bundle with host rcodesign"
    # A host signing pass replaces the deliberately unsigned cross-build.
    rcodesign sign "$HOST_APP"
    for signed_path in \
      Contents/MacOS/lege-music-gui \
      Contents/MacOS/lege; do
      # rcodesign's `verify` currently rejects valid ad-hoc signatures because
      # their CMS slot is intentionally empty. Parsing the signature confirms
      # that each nested Mach-O was signed successfully.
      rcodesign print-signature-info "$HOST_APP/$signed_path" >/dev/null
    done

    rm -f "$OUT_DIR/$ARCHIVE_STEM-UNSIGNED.zip" \
      "$OUT_DIR/$ARCHIVE_STEM-UNSIGNED.tar.gz"
    if command -v zip >/dev/null 2>&1; then
      rm -f "$OUT_DIR/$ARCHIVE_STEM.zip"
      (cd "$OUT_DIR" && zip -qry -y "$ARCHIVE_STEM.zip" "$APP_NAME.app")
      echo "Built: $OUT_DIR/$ARCHIVE_STEM.zip"
    else
      tar -C "$OUT_DIR" -czf "$OUT_DIR/$ARCHIVE_STEM.tar.gz" "$APP_NAME.app"
      echo "Built: $OUT_DIR/$ARCHIVE_STEM.tar.gz"
    fi
  elif [[ -d "$HOST_APP" ]]; then
    echo "Host rcodesign not found; the container's unsigned archive was retained." >&2
    echo "Install it with: cargo install apple-codesign" >&2
  fi
  exit 0
fi

if [[ -z "$SAUVOLA_ONNX" || ! -f "$SAUVOLA_ONNX" ]]; then
  echo "Set SAUVOLA_ONNX to the sauvola.onnx model (or place it at lege-process/packaging/macos/sauvola.onnx)." >&2
  echo "The --binarization heavy mode loads it at runtime from Contents/Resources." >&2
  exit 1
fi

if [[ "$SKIP_BUILD" == "1" ]]; then
  for binary in lege lege-music-gui; do
    if [[ ! -x "$ROOT/target/$TARGET/release/$binary" ]]; then
      echo "SKIP_BUILD=1 but target/$TARGET/release/$binary is missing." >&2
      exit 1
    fi
  done
  echo "== Repackaging existing $TARGET release binaries"
else
  # Cross-compile toolchain env (not needed when merely repackaging).
  if [[ "$(uname -s)" != "Darwin" ]]; then
    DARWIN_CLANG="$(command -v "${ARCH_PREFIX}-apple-darwin24-clang" || true)"
    if [[ -z "$DARWIN_CLANG" ]]; then
      echo "No ${ARCH_PREFIX}-apple-darwin24-clang on PATH — run inside the macos-cross-compiler container." >&2
      exit 1
    fi
    ENV_TRIPLE="${TARGET//-/_}"
    export "CC_${ENV_TRIPLE}=${ARCH_PREFIX}-apple-darwin24-clang"
    export "CXX_${ENV_TRIPLE}=${ARCH_PREFIX}-apple-darwin24-clang++"
    export "AR_${ENV_TRIPLE}=${ARCH_PREFIX}-apple-darwin24-ar"
    TRIPLE_UPPER="$(echo "$ENV_TRIPLE" | tr '[:lower:]' '[:upper:]')"
    export "CARGO_TARGET_${TRIPLE_UPPER}_LINKER=${ARCH_PREFIX}-apple-darwin24-clang"
    export MACOSX_DEPLOYMENT_TARGET="${MACOSX_DEPLOYMENT_TARGET:-13.3}"
  fi

  if [[ "$SKIP_CLI_BUILD" == "1" ]]; then
    if [[ ! -x "$ROOT/target/$TARGET/release/lege" ]]; then
      echo "SKIP_CLI_BUILD=1 but target/$TARGET/release/lege is missing." >&2
      exit 1
    fi
    echo "== Reusing existing lege CLI for $TARGET"
  else
    echo "== Building lege (CLI) for $TARGET"
    # paddle-ocr gives the CLI working OCR on macOS: it is pure-Rust + wgpu (Metal),
    # unlike Tesseract whose native cross-compile was broken. tesseract-ocr is left
    # off so nothing links libtesseract.
    cargo build --release --target "$TARGET" -p lege --bin lege \
      --no-default-features --features jp2-lam,paddle-ocr
  fi

  echo "== Building lege-music-gui (Sheet Music Edition) for $TARGET"
  cargo build --release --target "$TARGET" -p lege-music-gui --bin lege-music-gui \
    --no-default-features
fi

APP="$OUT_DIR/$APP_NAME.app"
MACOS_DIR="$APP/Contents/MacOS"
RES_DIR="$APP/Contents/Resources"
FRAMEWORKS_DIR="$APP/Contents/Frameworks"
rm -rf "$APP"
mkdir -p "$MACOS_DIR" "$RES_DIR/docs" "$FRAMEWORKS_DIR"

install -m755 "$ROOT/target/$TARGET/release/lege-music-gui" "$MACOS_DIR/lege-music-gui"
# The GUI resolves the worker CLI next to its own executable.
install -m755 "$ROOT/target/$TARGET/release/lege" "$MACOS_DIR/lege"
# Store documentation as bundle resources. The relative symlink preserves the
# same executable-adjacent `docs/` lookup used by the Linux installer without
# placing non-executable payloads directly in Contents/MacOS.
install -m644 "$MISC_DIR/docs/documentation-sheetmusic.html" \
  "$RES_DIR/docs/documentation-sheetmusic.html"
install -m644 "$MISC_DIR/docs/licenses.html" "$RES_DIR/docs/licenses.html"
ln -s ../Resources/docs "$MACOS_DIR/docs"
install -m644 "$SAUVOLA_ONNX" "$RES_DIR/sauvola.onnx"

# Icon: on macOS generate an icns from lege-misc/assets/icon.png; elsewhere
# copy the pre-made one from lege-process/packaging/macos/lege.icns.
if [[ "$(uname -s)" == "Darwin" ]] && command -v iconutil >/dev/null; then
  ICONSET="$(mktemp -d)/lege.iconset"
  mkdir -p "$ICONSET"
  # iconutil recognizes these five base slots plus their @2x variants.
  for size in 16 32 128 256 512; do
    sips -z $size $size "$MISC_DIR/assets/icon.png" --out "$ICONSET/icon_${size}x${size}.png" >/dev/null
    sips -z $((size*2)) $((size*2)) "$MISC_DIR/assets/icon.png" --out "$ICONSET/icon_${size}x${size}@2x.png" >/dev/null
  done
  iconutil -c icns "$ICONSET" -o "$RES_DIR/lege.icns"
elif [[ -f "$PROCESS_DIR/packaging/macos/lege.icns" ]]; then
  install -m644 "$PROCESS_DIR/packaging/macos/lege.icns" "$RES_DIR/lege.icns"
else
  echo "(no lege.icns available — bundle ships without an icon; add lege-process/packaging/macos/lege.icns)" >&2
fi

ICON_PLIST_ENTRY=""
if [[ -f "$RES_DIR/lege.icns" ]]; then
  ICON_PLIST_ENTRY="    <key>CFBundleIconFile</key>          <string>lege.icns</string>"
fi

cat >"$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>              <string>$APP_NAME</string>
    <key>CFBundleDisplayName</key>       <string>$APP_NAME</string>
    <key>CFBundleIdentifier</key>        <string>$BUNDLE_ID</string>
    <key>CFBundleVersion</key>           <string>$VERSION</string>
    <key>CFBundleShortVersionString</key><string>$VERSION</string>
    <key>CFBundleExecutable</key>        <string>lege-music-gui</string>
    <key>CFBundlePackageType</key>       <string>APPL</string>
$ICON_PLIST_ENTRY
    <key>LSMinimumSystemVersion</key>    <string>${MACOSX_DEPLOYMENT_TARGET:-13.3}</string>
    <key>NSHighResolutionCapable</key>   <true/>
</dict>
</plist>
PLIST

archive_bundle() {
  local archive_stem="$1"
  if command -v zip >/dev/null; then
    # `zip` updates an existing archive and otherwise leaves stale entries from
    # older bundle layouts in place. A release archive must be a fresh snapshot.
    rm -f "$OUT_DIR/$archive_stem.zip"
    (cd "$OUT_DIR" && zip -qry -y "$archive_stem.zip" "$APP_NAME.app")
    BUNDLE_ARCHIVE="$OUT_DIR/$archive_stem.zip"
  elif command -v tar >/dev/null; then
    # The cross-compiler image has tar but not zip. Finder extracts tar.gz
    # archives without requiring an additional utility.
    tar -C "$OUT_DIR" -czf "$OUT_DIR/$archive_stem.tar.gz" "$APP_NAME.app"
    BUNDLE_ARCHIVE="$OUT_DIR/$archive_stem.tar.gz"
  else
    echo "Neither zip nor tar is available to archive the app bundle." >&2
    return 1
  fi
}

# Signing + disk image.
if [[ "$(uname -s)" == "Darwin" ]]; then
  # Sign nested Mach-O code first, then seal the outer bundle. The default `-`
  # identity is ad-hoc and requires no Apple account or certificate.
  #
  # Hardened runtime and secure timestamps require a real signing identity.
  CODESIGN_ARGS=(--force --sign "$CODESIGN_ID")
  if [[ "$CODESIGN_ID" != "-" ]]; then
    CODESIGN_ARGS+=(--options runtime --timestamp)
  fi
  codesign "${CODESIGN_ARGS[@]}" "$MACOS_DIR/lege"
  codesign "${CODESIGN_ARGS[@]}" "$MACOS_DIR/lege-music-gui"
  codesign "${CODESIGN_ARGS[@]}" "$APP"
  codesign --verify --deep --strict --verbose=2 "$APP"
  DMG="$OUT_DIR/Lege-Sheet-Music-Edition-$VERSION-$ARCH_PREFIX.dmg"
  rm -f "$DMG"
  hdiutil create -volname "$APP_NAME" -srcfolder "$APP" -ov -format UDZO "$DMG"
  DMG_CODESIGN_ARGS=(--force --sign "$CODESIGN_ID")
  if [[ "$CODESIGN_ID" != "-" ]]; then
    DMG_CODESIGN_ARGS+=(--timestamp)
  fi
  codesign "${DMG_CODESIGN_ARGS[@]}" "$DMG"
  codesign --verify --verbose=2 "$DMG"
  echo "Built: $DMG"
  if [[ "$CODESIGN_ID" == "-" ]]; then
    echo "Ad-hoc signed: users must approve first launch in System Settings → Privacy & Security."
  fi
elif command -v rcodesign >/dev/null; then
  # Plain ad-hoc signature; hardened runtime is reserved for Developer ID builds.
  rcodesign sign "$APP"
  echo "Ad-hoc signed with rcodesign. Create the dmg on a Mac (hdiutil) or ship the archive below."
  archive_bundle "Lege-Sheet-Music-Edition-$VERSION-$ARCH_PREFIX"
  echo "Built: $BUNDLE_ARCHIVE"
else
  echo "No signing tool found (install rcodesign: cargo install apple-codesign)." >&2
  echo "Unsigned bundles will not launch on modern macOS — sign before shipping." >&2
  archive_bundle "Lege-Sheet-Music-Edition-$VERSION-$ARCH_PREFIX-UNSIGNED"
  echo "Built (unsigned): $BUNDLE_ARCHIVE"
fi

echo "Done: $APP"
