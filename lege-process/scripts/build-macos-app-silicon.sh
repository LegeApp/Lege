#!/usr/bin/env bash
# Build "Lege.app" (the MAIN program, +dmg/zip) for macOS.
#
# This is the sibling of scripts/build-macos-app.sh. That script builds the
# Sheet Music Edition for Intel (x86_64). THIS script builds the standard Lege
# application (CLI `lege` + GUI `lege-gui`). It targets Apple Silicon by
# default; pass --intel to build the Intel version of the main application.
#
# Differences from the Sheet Music / Intel script:
#   * Apple Silicon is the default; --intel selects x86_64-apple-darwin.
#   * The GUI is the main crate GUI/Freya (binary `lege-gui`), NOT the Sheet
#     Music crate (`lege-music-gui`).
#   * The `lege` CLI is built with the full default feature set, including
#     layout-detection. The current PP-DocLayout-M model is embedded in the
#     binary (its source filename is models/doclayout.onnx), so no external
#     layout model is bundled.
#   * App name "Lege", bundle id com.legeapp.lege, docs/documentation.html.
#
# Usage:
#   scripts/build-macos-app-silicon.sh           # Apple Silicon (default)
#   scripts/build-macos-app-silicon.sh --intel   # Intel
#   scripts/build-macos-app-silicon.sh --help
#
# Runs either on a real Mac or inside the macos-cross-compiler
# container (ghcr.io/shepherdjerred/macos-cross-compiler). On Linux, a normal
# invocation automatically starts that container when the Darwin compiler is not
# installed. The equivalent manual container invocation is:
#
#   sudo docker run --platform=linux/amd64 \
#     -v /mnt/Samsung980_1TB/Rust-projects/Lege-ecosystem:/workspace \
#     --rm ghcr.io/shepherdjerred/macos-cross-compiler:latest \
#     /bin/bash -lc 'rustup toolchain install 1.97.0 --profile minimal \
#       --target aarch64-apple-darwin --no-self-update && \
#       RUSTUP_TOOLCHAIN=1.97.0 bash -lc "cd /workspace && lege-process/scripts/build-macos-app-silicon.sh"'
#
# Inputs / env:
#   PDFIUM_DYLIB  path to libpdfium.dylib matching the selected architecture
#                 (Apple Silicon default:
#                    lege-process/packaging/macos/silicon/libpdfium.dylib
#                  Intel default:
#                    lege-process/packaging/macos/pdfium-mac-x64/lib/libpdfium.dylib)
#                 Download mac-arm64 or mac-x64 from
#                 https://github.com/bblanchon/pdfium-binaries/releases
#   SAUVOLA_ONNX  path to sauvola.onnx      (default:
#                 lege-process/packaging/macos/sauvola.onnx; required — the
#                 --binarization heavy mode loads it at runtime)
#   OUT_DIR       output directory (defaults to <workspace>/target/macos-silicon
#                 or <workspace>/target/macos-intel according to architecture)
#   CODESIGN_ID   codesign identity — macOS only (default: "-" = ad-hoc)
#   SKIP_BUILD    set to 1 to repackage already-built target artifacts
#   SKIP_CLI_BUILD set to 1 to reuse an existing lege CLI binary
#   DJVU_ENCODER_MANIFEST path to the standalone AGPL encoder's Cargo.toml
#                 (default: <workspace>/lege-codecs/djvulibrust/Cargo.toml)
#   DJVU_ENCODER_LICENSE path to its AGPLv3 license text
#                 (default: LICENSE next to that Cargo.toml)
#   AUTO_CONTAINER set to 0 to disable automatic Docker/Podman use on Linux
#   CONTAINER_RUNTIME Docker/Podman executable (auto-detected by default)
#   MACOS_CROSS_IMAGE cross-compiler image override
#   MACOS_RUST_TOOLCHAIN Rust toolchain installed inside the cross container
#
# Notes:
#   * The main GUI (GUI/Freya, binary lege-gui) drives the bundled `lege` CLI as
#     a subprocess; the CLI is where OCR, layout detection and binarization run.
#     The CLI is built with the pure-Rust PP-OCR backend (paddle-ocr) so OCR
#     works on macOS without any Tesseract native dependency. sauvola.onnx IS
#     bundled in Resources because "heavy" binarization loads it at runtime.
#     PP-DocLayout-M is embedded by layout-detection and is NOT copied into the
#     application bundle as a separate model file.
#   * DjVu encoding remains a separate AGPL executable, built from the sibling
#     djvulibrust project and bundled under Contents/Helpers; Lege does not link
#     it as a library.
#   * Gatekeeper: an ad-hoc signature seals the bundle but does not establish a
#     trusted developer identity. On current macOS, first try to launch the app,
#     then approve it in System Settings -> Privacy & Security -> Open Anyway.
#     True one-click distribution needs a Developer ID certificate and Apple
#     notarization; neither is possible without Apple Developer Program access.

set -euo pipefail

# Layout of the Lege ecosystem workspace:
#   ROOT         — workspace root (virtual Cargo.toml and cargo target/)
#   PROCESS_DIR  — the `lege` crate (packaging/ and package Cargo.toml)
#   MISC_DIR     — shared docs/ and assets/ used by the app bundle
# This script lives in $PROCESS_DIR/scripts, so ROOT is two levels up.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROCESS_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
ROOT="$(cd "$PROCESS_DIR/.." && pwd)"
MISC_DIR="${MISC_DIR:-$ROOT/lege-misc}"

usage() {
  cat <<EOF
Usage: $(basename "$0") [--silicon | --intel]

Build the main Lege macOS application.

Options:
  --silicon  Build for Apple Silicon / arm64 (default).
  --intel    Build for Intel / x86_64.
  -h, --help Show this help.

The selected architecture also controls the default output directory and
PDFium dylib path. Environment variables such as PDFIUM_DYLIB, OUT_DIR,
CODESIGN_ID, and SKIP_BUILD can still override their documented defaults.
EOF
}

# Keep TARGET as an environment-variable override for automated builds, while
# providing readable command-line flags for normal use.
TARGET="${TARGET:-aarch64-apple-darwin}"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --silicon)
      TARGET="aarch64-apple-darwin"
      ;;
    --intel)
      TARGET="x86_64-apple-darwin"
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    --)
      shift
      if [[ $# -gt 0 ]]; then
        echo "Unexpected positional argument: $1" >&2
        usage >&2
        exit 2
      fi
      break
      ;;
    -*)
      echo "Unknown option: $1" >&2
      usage >&2
      exit 2
      ;;
    *)
      echo "Unexpected positional argument: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
  shift
done

case "$TARGET" in
  aarch64-apple-darwin)
    EXPECTED_PDFIUM_ARCH="arm64"
    ARCH_LABEL="arm64"
    DEFAULT_OUT_DIR="$ROOT/target/macos-silicon"
    DEFAULT_PDFIUM_DYLIB="$PROCESS_DIR/packaging/macos/silicon/libpdfium.dylib"
    ;;
  x86_64-apple-darwin)
    EXPECTED_PDFIUM_ARCH="x86_64"
    ARCH_LABEL="x86_64"
    DEFAULT_OUT_DIR="$ROOT/target/macos-intel"
    DEFAULT_PDFIUM_DYLIB="$PROCESS_DIR/packaging/macos/pdfium-mac-x64/lib/libpdfium.dylib"
    ;;
  *)
    echo "Unsupported TARGET: $TARGET" >&2
    echo "Use --silicon or --intel." >&2
    exit 1
    ;;
esac

OUT_DIR="${OUT_DIR:-$DEFAULT_OUT_DIR}"
PDFIUM_DYLIB="${PDFIUM_DYLIB:-$DEFAULT_PDFIUM_DYLIB}"
SAUVOLA_ONNX="${SAUVOLA_ONNX:-$PROCESS_DIR/packaging/macos/sauvola.onnx}"
CODESIGN_ID="${CODESIGN_ID:--}"
SKIP_BUILD="${SKIP_BUILD:-0}"
SKIP_CLI_BUILD="${SKIP_CLI_BUILD:-0}"
DJVU_ENCODER_MANIFEST="${DJVU_ENCODER_MANIFEST:-$ROOT/lege-codecs/djvulibrust/Cargo.toml}"
DJVU_ENCODER_DIR="$(dirname "$DJVU_ENCODER_MANIFEST")"
DJVU_ENCODER_TARGET_DIR="${DJVU_ENCODER_TARGET_DIR:-$ROOT/target/djvu-encoder}"
DJVU_ENCODER_LICENSE="${DJVU_ENCODER_LICENSE:-$(dirname "$DJVU_ENCODER_MANIFEST")/LICENSE}"
AUTO_CONTAINER="${AUTO_CONTAINER:-1}"
CONTAINER_RUNTIME="${CONTAINER_RUNTIME:-}"
MACOS_CROSS_IMAGE="${MACOS_CROSS_IMAGE:-ghcr.io/shepherdjerred/macos-cross-compiler:latest}"
MACOS_RUST_TOOLCHAIN="${MACOS_RUST_TOOLCHAIN:-1.97.0}"
VERSION="$(awk -F '"' '/^version = / { print $2; exit }' "$PROCESS_DIR/Cargo.toml")"
APP_NAME="Lege"
BUNDLE_ID="com.legeapp.lege"
ARCH_PREFIX="${TARGET%%-*}"
DJVU_ENCODER_BIN="$DJVU_ENCODER_TARGET_DIR/$TARGET/release/djvu-encoder"

ARCHIVE_STEM="Lege-$VERSION-$ARCH_LABEL"

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

  if [[ ! -d "$DJVU_ENCODER_DIR" ]]; then
    echo "DjVu encoder source directory not found: $DJVU_ENCODER_DIR" >&2
    exit 1
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
    /bin/bash -lc 'rustup toolchain install "$MACOS_RUST_TOOLCHAIN" --profile minimal --target "$TARGET" --no-self-update && export RUSTUP_TOOLCHAIN="$MACOS_RUST_TOOLCHAIN" && cd /workspace && lege-process/scripts/build-macos-app-silicon.sh'

  # The cross image does not currently ship rcodesign. Once it returns, use a
  # host installation when available and replace the container's deliberately
  # unsigned archive with a signed one.
  HOST_APP="$OUT_DIR/$APP_NAME.app"
  if command -v rcodesign >/dev/null 2>&1 && [[ -d "$HOST_APP" ]]; then
    echo "== Ad-hoc signing the macOS bundle with host rcodesign"
    # Deliberately NOT hardened runtime: the runtime flag turns on library
    # validation, and an ad-hoc identity has no Team ID, so the lege worker's
    # dlopen of the ad-hoc-signed libpdfium.dylib would be rejected at runtime
    # ("different Team IDs") — the GUI then fails silently on Process. Hardened
    # runtime only matters for notarization, which needs a real Developer ID.
    rcodesign sign "$HOST_APP"
    for signed_path in \
      Contents/MacOS/lege-gui \
      Contents/MacOS/lege \
      Contents/Helpers/djvu-encoder \
      Contents/Frameworks/libpdfium.dylib; do
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

if [[ ! -f "$PDFIUM_DYLIB" ]]; then
  echo "Set PDFIUM_DYLIB to a $EXPECTED_PDFIUM_ARCH libpdfium.dylib matching $TARGET" >&2
  echo "(https://github.com/bblanchon/pdfium-binaries/releases, mac-arm64 or mac-x64)" >&2
  echo "Default expected at: $DEFAULT_PDFIUM_DYLIB" >&2
  exit 1
fi

PDFIUM_FILE_INFO="$(file -b "$PDFIUM_DYLIB")"
if [[ "$PDFIUM_FILE_INFO" != *"$EXPECTED_PDFIUM_ARCH"* ]]; then
  echo "PDFIUM_DYLIB architecture does not match $TARGET." >&2
  echo "Expected $EXPECTED_PDFIUM_ARCH; file reports: $PDFIUM_FILE_INFO" >&2
  exit 1
fi

if [[ -z "$SAUVOLA_ONNX" || ! -f "$SAUVOLA_ONNX" ]]; then
  echo "Set SAUVOLA_ONNX to the sauvola.onnx model (or place it at lege-process/packaging/macos/sauvola.onnx)." >&2
  echo "The --binarization heavy mode loads it at runtime from Contents/Resources." >&2
  exit 1
fi

if [[ ! -f "$DJVU_ENCODER_MANIFEST" ]]; then
  echo "Standalone DjVu encoder manifest not found: $DJVU_ENCODER_MANIFEST" >&2
  echo "Set DJVU_ENCODER_MANIFEST to the djvulibrust Cargo.toml." >&2
  exit 1
fi

if [[ ! -f "$DJVU_ENCODER_LICENSE" ]]; then
  echo "Standalone DjVu encoder license not found: $DJVU_ENCODER_LICENSE" >&2
  echo "Set DJVU_ENCODER_LICENSE to its AGPLv3 license text." >&2
  exit 1
fi

if [[ "$SKIP_BUILD" == "1" ]]; then
  for binary in lege lege-gui; do
    if [[ ! -x "$ROOT/target/$TARGET/release/$binary" ]]; then
      echo "SKIP_BUILD=1 but target/$TARGET/release/$binary is missing." >&2
      exit 1
    fi
  done
  if [[ ! -x "$DJVU_ENCODER_BIN" ]]; then
    echo "SKIP_BUILD=1 but $DJVU_ENCODER_BIN is missing." >&2
    exit 1
  fi
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
    echo "== Building lege (CLI, full feature set) for $TARGET"
    # The main edition ships the full CLI: PP-OCR (pure-Rust + wgpu/Metal),
    # PP-DocLayout-M layout detection (embedded via include_bytes!, with no
    # separately packaged layout model), and JP2. tesseract-ocr stays off so
    # nothing links libtesseract.
    cargo build --release --target "$TARGET" -p lege --bin lege \
      --no-default-features --features jp2-lam,paddle-ocr,layout-detection
  fi

  echo "== Building standalone djvu-encoder for $TARGET"
  # Build from a temporary source copy. The helper's package version can move
  # ahead of its lockfile between releases; Cargo may refresh that root entry,
  # but the sibling source checkout must remain untouched by this packager.
  DJVU_BUILD_DIR="$(mktemp -d /tmp/lege-djvu-build.XXXXXX)"
  cp "$DJVU_ENCODER_MANIFEST" "$DJVU_BUILD_DIR/Cargo.toml"
  cp "$DJVU_ENCODER_DIR/Cargo.lock" "$DJVU_BUILD_DIR/Cargo.lock"
  cp -R "$DJVU_ENCODER_DIR/src" "$DJVU_BUILD_DIR/src"
  (
    cd "$DJVU_BUILD_DIR"
    cargo build --release --target "$TARGET" \
      --target-dir "$DJVU_ENCODER_TARGET_DIR" \
      --bin djvu-encoder --features cli,simd
  )
  rm -rf "$DJVU_BUILD_DIR"

  echo "== Building lege-gui (main GUI) for $TARGET"
  cargo build --release --target "$TARGET" -p lege-gui --bin lege-gui \
    --no-default-features
fi

APP="$OUT_DIR/$APP_NAME.app"
MACOS_DIR="$APP/Contents/MacOS"
RES_DIR="$APP/Contents/Resources"
FRAMEWORKS_DIR="$APP/Contents/Frameworks"
HELPERS_DIR="$APP/Contents/Helpers"
rm -rf "$APP"
mkdir -p "$MACOS_DIR" "$RES_DIR/docs" "$FRAMEWORKS_DIR" "$HELPERS_DIR"

install -m755 "$ROOT/target/$TARGET/release/lege-gui" "$MACOS_DIR/lege-gui"
# The GUI resolves the worker CLI next to its own executable.
install -m755 "$ROOT/target/$TARGET/release/lege" "$MACOS_DIR/lege"
# Keep the AGPL encoder as a separately launched helper, not a linked library.
install -m755 "$DJVU_ENCODER_BIN" "$HELPERS_DIR/djvu-encoder"
mkdir -p "$RES_DIR/licenses"
install -m644 "$DJVU_ENCODER_LICENSE" "$RES_DIR/licenses/djvu-encoder-AGPL-3.0.txt"
# Store documentation as bundle resources. The relative symlink preserves the
# same executable-adjacent `docs/` lookup used by the Linux installer without
# placing non-executable payloads directly in Contents/MacOS.
install -m644 "$MISC_DIR/docs/documentation.html" \
  "$RES_DIR/docs/documentation.html"
install -m644 "$MISC_DIR/docs/licenses.html" "$RES_DIR/docs/licenses.html"
ln -s ../Resources/docs "$MACOS_DIR/docs"
# Lege's macOS runtime search explicitly checks Contents/Frameworks and
# Contents/Resources, keeping code and data in their conventional locations.
install -m644 "$PDFIUM_DYLIB" "$FRAMEWORKS_DIR/libpdfium.dylib"
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
    <key>CFBundleExecutable</key>        <string>lege-gui</string>
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
  # Hardened runtime (--options runtime) is applied ONLY with a real identity:
  # it enables library validation, which demands that loaded dylibs share the
  # process's Team ID (or be Apple-signed). Ad-hoc signatures carry no Team ID,
  # so a hardened-runtime lege cannot dlopen the ad-hoc-signed libpdfium.dylib
  # and processing fails at pdfium bind time. With a Developer ID every nested
  # binary shares the Team ID, so library validation passes and hardened
  # runtime (required for notarization) is safe. Secure timestamps likewise
  # require a real identity.
  CODESIGN_ARGS=(--force --sign "$CODESIGN_ID")
  if [[ "$CODESIGN_ID" != "-" ]]; then
    CODESIGN_ARGS+=(--options runtime --timestamp)
  fi
  codesign "${CODESIGN_ARGS[@]}" "$FRAMEWORKS_DIR/libpdfium.dylib"
  codesign "${CODESIGN_ARGS[@]}" "$HELPERS_DIR/djvu-encoder"
  codesign "${CODESIGN_ARGS[@]}" "$MACOS_DIR/lege"
  codesign "${CODESIGN_ARGS[@]}" "$MACOS_DIR/lege-gui"
  codesign "${CODESIGN_ARGS[@]}" "$APP"
  codesign --verify --deep --strict --verbose=2 "$APP"
  DMG="$OUT_DIR/$ARCHIVE_STEM.dmg"
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
    echo "Ad-hoc signed: users must approve first launch in System Settings -> Privacy & Security."
  fi
elif command -v rcodesign >/dev/null; then
  # Plain ad-hoc signatures, deliberately WITHOUT the hardened-runtime flag:
  # hardened runtime enables library validation, which requires loaded dylibs
  # to be signed by Apple or by the same Team ID as the process. An ad-hoc
  # identity has no Team ID, so a hardened-runtime lege would be unable to
  # dlopen the bundled libpdfium.dylib and every Process click would fail.
  # Hardened runtime is only needed for notarization (Developer ID path).
  rcodesign sign "$APP"
  echo "Ad-hoc signed with rcodesign. Create the dmg on a Mac (hdiutil) or ship the archive below."
  archive_bundle "$ARCHIVE_STEM"
  echo "Built: $BUNDLE_ARCHIVE"
else
  echo "No signing tool found (install rcodesign: cargo install apple-codesign)." >&2
  echo "Unsigned bundles will not launch on modern macOS — sign before shipping." >&2
  archive_bundle "$ARCHIVE_STEM-UNSIGNED"
  echo "Built (unsigned): $BUNDLE_ARCHIVE"
fi

echo "Done: $APP"
