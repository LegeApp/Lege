#!/usr/bin/env bash
# Warning gate for the Lege workspace.
#
# A single `cargo check` only ever inspects one point in a large feature space, and
# dead-or-live status moves with the configuration. `djvu_encoder` went months without
# compiling under `--features debug-logging` because a binding was renamed to `_e` to
# silence a default-build warning while the cfg-gated log still referenced `e`; no
# default-feature check would ever have reported it. This script checks every
# configuration that ships, so that class of defect fails loudly instead.
#
#   ./lege-misc/dev-misc/warncheck.sh            # check every configuration
#   ./lege-misc/dev-misc/warncheck.sh --baseline # record counts instead of failing
#   ./lege-misc/dev-misc/warncheck.sh --forced   # also see through #![allow(...)]
#
# `--all-targets` is not optional: every jbig2enc-rust warning lives in a test target
# and is invisible without it.

set -uo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/../.." || exit 1

BASELINE=0
FORCED=0
for arg in "$@"; do
    case "$arg" in
        --baseline) BASELINE=1 ;;
        --forced)   FORCED=1 ;;
        *) echo "unknown flag: $arg" >&2; exit 2 ;;
    esac
done

if [ "$FORCED" = 1 ]; then
    # --force-warn overrides #![allow(...)] in the source, revealing what those
    # attributes mask. Needs a separate target dir or cargo replays cached, unforced
    # diagnostics and silently under-reports.
    export RUSTFLAGS="--force-warn dead_code --force-warn unused_variables --force-warn unused_mut --force-warn unused_imports"
    export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-target}/warncheck-forced"
fi

# Vendored trees are reported but never gate the build.
VENDORED_RE='^(freya-main/|lege-process/GUI/rfd/)'

fail=0
total=0

# run <label> <cargo args...>
run() {
    local label="$1"; shift
    local out rc n vendored
    out="$("$@" --message-format=short 2>&1)"
    rc=$?
    if [ "$rc" -ne 0 ]; then
        printf '  %-58s BUILD FAILED\n' "$label"
        echo "$out" | grep -E '^error|: error' | head -5 | sed 's/^/      /'
        fail=1
        return
    fi
    n="$(echo "$out" | grep -E ':[0-9]+:[0-9]+: warning' \
         | sed "s|^$PWD/||" | grep -Ecv "$VENDORED_RE")"
    vendored="$(echo "$out" | grep -E ':[0-9]+:[0-9]+: warning' \
         | sed "s|^$PWD/||" | grep -Ec "$VENDORED_RE")"
    total=$((total + n))
    if [ "$n" -eq 0 ]; then
        printf '  %-58s ok%s\n' "$label" \
            "$([ "$vendored" -gt 0 ] && echo " ($vendored vendored)")"
    else
        printf '  %-58s %d warning(s)%s\n' "$label" "$n" \
            "$([ "$vendored" -gt 0 ] && echo " (+$vendored vendored)")"
        [ "$BASELINE" = 1 ] || fail=1
        echo "$out" | grep -E ':[0-9]+:[0-9]+: warning' | sed "s|^$PWD/||" \
            | grep -Ev "$VENDORED_RE" | sed -E 's/: help:.*//' | sort -u | head -40 | sed 's/^/      /'
    fi
}

ws()  { run "$1" cargo check "${@:2}" --all-targets; }
mp()  { local m="$1"; run "$2" cargo check --manifest-path "$m" "${@:3}" --all-targets; }

echo "== lege (CLI + core library) =="
ws "default"                      -p lege
ws "debug-logging"                -p lege --features debug-logging
ws "no-default-features"          -p lege --no-default-features
ws "music-sheet"                  -p lege --no-default-features --features music-sheet
ws "tesseract-ocr"                -p lege --features tesseract-ocr
ws "profiling"                    -p lege --features profiling
ws "german"                       -p lege --features german
ws "djvu_debug"                   -p lege --features djvu_debug
ws "no-include-shaders"           -p lege --features no-include-shaders
run "android (aarch64)"           cargo check -p lege --target aarch64-linux-android --features android --all-targets

echo "== lege-gpu =="
ws "default"                      -p lege-gpu
ws "debug-logging"                -p lege-gpu --features debug-logging
ws "layout-detection"             -p lege-gpu --features layout-detection
ws "presentation"                 -p lege-gpu --features presentation
ws "debug-layers"                 -p lege-gpu --features debug-layers
# Not `--all-features`: `android` is guarded by a deliberate `compile_error!` in
# lege-gpu/src/lib.rs ("only valid for Android targets"), so --all-features can
# never pass on a desktop host. This is every feature except that one.
ws "all desktop features"         -p lege-gpu --features layout-detection,debug-logging,presentation,debug-layers

echo "== lege-ocr =="
ws "default"                      -p lege-ocr
ws "paddle-ocr"                   -p lege-ocr --features paddle-ocr
ws "tesseract-ocr"                -p lege-ocr --features tesseract-ocr
ws "debug-bin"                    -p lege-ocr --features debug-bin
ws "all-features"                 -p lege-ocr --all-features

echo "== frontends =="
ws "lege-gui-freya"               -p lege-gui-freya
ws "lege-music-gui"               -p lege-music-gui
ws "lege-viewer"                  -p lege-viewer
ws "rfd (vendored, in-workspace)" -p rfd

echo "== PDF crates =="
ws "workspace (pdf + document-ocr + rest)" --workspace

echo "== codecs (own build graphs) =="
mp lege-codecs/jp2lam/Cargo.toml         "jp2lam default"
mp lege-codecs/jp2lam/Cargo.toml         "jp2lam no-default"      --no-default-features
mp lege-codecs/jp2lam/Cargo.toml         "jp2lam cli,profile,counters" --features cli,profile,counters
mp lege-codecs/jbig2enc-rust/Cargo.toml  "jbig2enc default"
mp lege-codecs/jbig2enc-rust/Cargo.toml  "jbig2enc no-default"    --no-default-features
mp lege-codecs/jbig2enc-rust/Cargo.toml  "jbig2enc decode only"   --no-default-features --features decode
mp lege-codecs/jbig2enc-rust/Cargo.toml  "jbig2enc cli"           --features cli
# asm_zp is excluded on purpose: its own manifest calls it "bit-incorrect and
# experimental" and it does not compile, for reasons unrelated to warnings.
mp lege-codecs/djvulibrust/Cargo.toml    "djvu default"
mp lege-codecs/djvulibrust/Cargo.toml    "djvu debug-logging"     --features debug-logging
mp lege-codecs/djvulibrust/Cargo.toml    "djvu cli,rayon,simd"    --features cli,rayon,simd

echo "== nested workspace =="
mp lege-pdf/pdf-integrity/Cargo.toml     "pdf-integrity"          --workspace

echo
if [ "$BASELINE" = 1 ]; then
    echo "baseline: $total first-party warning(s) across the matrix"
    exit 0
fi
if [ "$fail" -ne 0 ]; then
    echo "FAIL: $total first-party warning(s) across the matrix"
    exit 1
fi
echo "PASS: no first-party warnings across the matrix"
