#!/usr/bin/env bash
#
# Full differential sweep (day-plan "sweep 3") over ALL corpus roots.
#
#   1. Pol was right again      (the main archive.org corpus)
#   2. to-sort                  (loose intake)
#   3. renderer-corpus          (pdfbox + pdf.js + hayro hand-picked regression
#                                PDFs — small but curated: ~2.8k files)
#
# ~14k PDFs total; the run takes hours. It does NOT start automatically — run it
# by hand once the known-error fixes are in. Results append to
# `<out>/results.csv` and the run is resumable (re-running skips file|page keys
# already recorded), so the existing 2026-07-18 baseline keys stay byte-identical
# and the new renderer-corpus rows are purely additive.
#
# Usage:
#   ./run-sweep.sh [libpdfium] [scale] [workdir]
# `workdir` (default: cwd) is where `pdfium-diff-out/results.csv` is written.
#
# Env:
#   CORPUS_ROOT  Drive/mount holding the corpus. Default /mnt/Samsung980_1TB
#                (Linux). On Windows set CORPUS_ROOT=D: (the same physical
#                disk). The three roots below are resolved under it.
set -euo pipefail

ROOT="${CORPUS_ROOT:-/mnt/Samsung980_1TB}"
LIB="${1:-$ROOT/Rust-projects/pdfium-port-plan/libpdfium.so}"
SCALE="${2:-2.0}"
WORKDIR="${3:-$(pwd)}"

CORPORA=(
    "$ROOT/Pol was right again"
    "$ROOT/to-sort"
    "$ROOT/Rust-projects/pdfium-port-plan/renderer-corpus"
)

# Fail loudly if a corpus root is missing rather than silently sweeping less.
missing=0
for c in "${CORPORA[@]}"; do
    if [[ ! -d "$c" ]]; then
        echo "MISSING corpus root: $c" >&2
        missing=1
    fi
done
[[ "$missing" == 0 ]] || { echo "aborting: fix the roots above (CORPUS_ROOT=$ROOT)" >&2; exit 1; }
[[ -f "$LIB" ]] || { echo "MISSING libpdfium: $LIB" >&2; exit 1; }

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
echo "building release pdfium-diff…" >&2
(cd "$SCRIPT_DIR" && cargo build --release)
BIN="$SCRIPT_DIR/target/release/pdfium-diff"
[[ -f "$BIN" ]] || BIN="$SCRIPT_DIR/target/release/pdfium-diff.exe"

# The tool always writes ./pdfium-diff-out in its CWD (resumable), so just run
# from the chosen workdir.
mkdir -p "$WORKDIR"
cd "$WORKDIR"
echo "sweep → $WORKDIR/pdfium-diff-out/results.csv   lib=$LIB scale=$SCALE" >&2
echo "roots:" >&2; printf '  %s\n' "${CORPORA[@]}" >&2
exec "$BIN" "$LIB" "$SCALE" "${CORPORA[@]}"
