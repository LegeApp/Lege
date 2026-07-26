#!/usr/bin/env bash
# Sharded multi-renderer sweep.
#
# `pdfium-diff sweep` processes files serially (multi.rs: `for (index, pdf) in
# files.iter()`), unlike the legacy PDFium-only controller which had a worker
# pool. On the full corpus that is ~9.5 s/file, i.e. ~51 h. This script gets the
# parallelism back without touching the tool: it splits the corpus into N
# disjoint file lists and runs one sweep per shard, each with its own --out.
#
# Safe because resume keys are `file|page|reference` and each shard owns its
# output directory — shards never read or write each other's CSV. Re-running
# with the same OUT resumes: the file list is sorted and split deterministically,
# so a shard picks up exactly where it stopped.
#
#   ./run-sweep-sharded.sh <out-dir> [jobs] [-- <extra pdfium-diff flags>]
#
# Environment:
#   REFERENCES  comma list (default: mupdf — the primary oracle; add
#               pdfium,hayro only when triaging a specific disagreement)
#   SCALE       device scale (default: 2 — matches sweeps 6-10; do not use the
#               README's 1 or the numbers will not line up with the baseline)
#   CORPUS      space-free root list is not possible, so edit ROOTS below
#   PDF_RENDERER_PDFIUM_LIB  path to libpdfium.so (required)
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# Overridable so the failure paths can be exercised with a stub binary.
BIN="${BIN:-$HERE/target/release/pdfium-diff}"

OUT="${1:?usage: run-sweep-sharded.sh <out-dir> [jobs] [-- extra flags]}"
shift
# `run-sweep-sharded.sh out -- --flag` must not read `--` as the job count.
JOBS=6
if [ $# -gt 0 ] && [ "$1" != "--" ]; then
  JOBS="$1"
  shift
fi
[ "${1:-}" = "--" ] && shift
EXTRA=("$@")

# MuPDF is the oracle: it has the broadest rendering coverage of the three, and
# one control instead of three cuts a sweep's reference-render work (and the
# heat it makes) to roughly a third. pdfium/hayro stay available for triage —
# `REFERENCES=mupdf,pdfium,hayro` — where a disagreement needs adjudicating.
REFERENCES="${REFERENCES:-mupdf}"
SCALE="${SCALE:-2}"

# Files that OOM-killed a shard in sweep 11. Each is a control-engine problem
# (huge page count or a pathological image), not ours; keeping them in costs a
# whole shard tail for nothing. Matched as substrings.
EXCLUDE=(
  "pdfium/467170761.pdf"
  "pdfium/373764900.pdf"
  "08-29-2021-mbta-system-brochure.pdf"
  "designated_basinmap"
  "bug1019475_1"
  "to-sort/Getting started.pdf"
)

ROOTS=(
  "/mnt/Samsung980_1TB/Pol was right again"
  "/mnt/Samsung980_1TB/to-sort"
  "/mnt/Samsung980_1TB/Rust-projects/pdfium-port-plan/renderer-corpus"
)

if [ ! -x "$BIN" ]; then
  echo "missing $BIN — build first:" >&2
  echo "  cargo build --release --features all-renderers" >&2
  exit 1
fi
if [ -z "${PDF_RENDERER_PDFIUM_LIB:-}" ]; then
  echo "set PDF_RENDERER_PDFIUM_LIB=/path/to/libpdfium.so" >&2
  exit 1
fi
# A rebuild mid-sweep would swap the binary under running shards; a concurrent
# sweep would skew everything. Both have cost us a full run before.
if pgrep -f "$BIN sweep" >/dev/null 2>&1; then
  echo "a sweep is already running; refusing to start another" >&2
  exit 1
fi

mkdir -p "$OUT"
LIST="$OUT/files.txt"

if [ ! -s "$LIST" ]; then
  echo "enumerating corpus…"
  : > "$LIST"
  for root in "${ROOTS[@]}"; do
    find "$root" -type d \( -name node_modules -o -name .git -o -name target \) -prune \
      -o -type f -iname '*.pdf' -print
  done | LC_ALL=C sort -u | grep -v -F -f <(printf '%s\n' "${EXCLUDE[@]}") > "$LIST"
fi
TOTAL=$(wc -l < "$LIST")
echo "$TOTAL files, $JOBS shards, references=$REFERENCES scale=$SCALE"

# Round-robin rather than contiguous: directories cluster by size and type, so
# contiguous blocks would give wildly uneven shards.
for ((k = 0; k < JOBS; k++)); do
  awk -v k="$k" -v n="$JOBS" 'NR % n == k' "$LIST" > "$OUT/shard-$k.txt"
done

# Sweep 11 ran each shard under a single `xargs`. When the OOM killer SIGKILLed
# a worker, xargs stopped immediately and abandoned everything after it in that
# shard — 1,430 files silently never processed. So: drive batches from bash,
# which keeps going regardless of how a child died, and on a failed batch retry
# its files one at a time so a single bad file costs one file, not a batch.
# Resume keys make the retry nearly free: pages already in the CSV are skipped.
BATCH="${BATCH:-20}"
# Sweep 11 also lost a shard to a *hang*, not an OOM: `bug1019475_1.pdf` spun at
# 97% CPU for 8.5 h and the shard never advanced past it. The tool's per-page
# timeout did not catch it, so bound each invocation from outside. A hung file
# then costs FILE_TIMEOUT once instead of the rest of the shard.
FILE_TIMEOUT="${FILE_TIMEOUT:-600}"

run_shard() {
  local k="$1"
  local list="$OUT/shard-$k.txt"
  local dest="$OUT/shard-$k"
  local -a batch=()
  local file rc

  flush_batch() {
    [ "${#batch[@]}" -gt 0 ] || return 0
    # Scale the allowance to the batch actually being run, not the nominal
    # BATCH: the final partial batch should not get a 20-file budget.
    timeout -k 10 "$(( ${#batch[@]} * FILE_TIMEOUT ))" \
      "$BIN" sweep --scale "$SCALE" --reference "$REFERENCES" \
      --out "$dest" "${EXTRA[@]}" "${batch[@]}" < /dev/null
    rc=$?
    if [ "$rc" -ne 0 ]; then
      echo "!! batch exited $rc; retrying ${#batch[@]} files singly" >&2
      for file in "${batch[@]}"; do
        timeout -k 10 "$FILE_TIMEOUT" \
          "$BIN" sweep --scale "$SCALE" --reference "$REFERENCES" \
          --out "$dest" "${EXTRA[@]}" "$file" < /dev/null \
          || echo "!! DIED: $file (exit $?)" >&2
      done
    fi
    batch=()
  }

  while IFS= read -r file; do
    [ -n "$file" ] || continue
    batch+=("$file")
    [ "${#batch[@]}" -ge "$BATCH" ] && flush_batch
  done < "$list"
  flush_batch
}

pids=()

# `timeout` runs each batch in its OWN process group (verified: uutils timeout
# setpgids itself + the batch), so neither Ctrl-C on this script nor killing it
# reaches in-flight batches — that is how sweep 11 left orphans running for
# 8.5 h. On INT/TERM: stop the shard drivers first (no new batches), then kill
# every in-flight batch by its process group, which takes the pdfium-diff AND
# its renderer children (mupdf/gs/...) down together.
cleanup() {
  trap - INT TERM
  echo "interrupted — stopping shards and in-flight batches" >&2
  kill "${pids[@]}" 2>/dev/null
  local pid pgid
  for pid in $(pgrep -f "$BIN sweep" 2>/dev/null); do
    pgid=$(ps -o pgid= -p "$pid" 2>/dev/null | tr -d ' ')
    [ -n "$pgid" ] && kill -- "-$pgid" 2>/dev/null
  done
  # Belt and braces for anything that escaped its group.
  pkill -f "$BIN sweep" 2>/dev/null
  exit 130
}
trap cleanup INT TERM

START=$(date +%s)
for ((k = 0; k < JOBS; k++)); do
  count=$(wc -l < "$OUT/shard-$k.txt")
  echo "  shard $k: $count files -> $OUT/shard-$k"
  # A subshell rather than `setsid bash -c` so the shard inherits EXTRA, ROOTS
  # and the rest as real arrays instead of re-quoted strings. Launch the script
  # itself under nohup if you want it to survive the terminal.
  ( run_shard "$k" ) > "$OUT/shard-$k.log" 2>&1 < /dev/null &
  pids+=($!)
done

echo "waiting for $JOBS shards (logs: $OUT/shard-*.log)"
fail=0
for pid in "${pids[@]}"; do
  wait "$pid" || fail=$((fail + 1))
done
ELAPSED=$(( $(date +%s) - START ))
printf 'shards finished in %dh%02dm (%d failed)\n' $((ELAPSED/3600)) $(((ELAPSED%3600)/60)) "$fail"

# Merge: one header, then every shard's rows.
merge() {
  local name="$1"
  local dest="$OUT/$name"
  local first=1
  : > "$dest"
  for ((k = 0; k < JOBS; k++)); do
    local src="$OUT/shard-$k/$name"
    [ -f "$src" ] || continue
    if [ "$first" = 1 ]; then head -1 "$src" >> "$dest"; first=0; fi
    tail -n +2 "$src" >> "$dest"
  done
  [ -s "$dest" ] && echo "  $dest: $(( $(wc -l < "$dest") - 1 )) rows"
}
echo "merging:"
merge results.csv
merge attribution.csv
