#!/usr/bin/env bash
set -euo pipefail

if [[ $# -lt 1 || $# -gt 2 ]]; then
  echo "usage: $0 <structured|remaining|rss|perf|dhat> [output-directory]" >&2
  exit 2
fi

action=$1
script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
renderer_dir=$(cd -- "$script_dir/../.." && pwd)
manifest=${PDF_RENDERER_PERF_MANIFEST:-"$renderer_dir/corpus/perf/pages.tsv"}
corpus_root=${PDF_RENDERER_CORPUS_ROOT:-/mnt/Samsung980_1TB}
output_dir=${2:-"$renderer_dir/corpus/perf/results/$(date +%Y%m%d-%H%M%S)"}
binary="$script_dir/target/release/pdfium-diff"
selected_ids=${PDF_RENDERER_PERF_IDS:-}

mkdir -p "$output_dir"

case "$action" in
  structured|remaining|rss)
    cargo build --release --manifest-path "$script_dir/Cargo.toml"
    ;;
  perf)
    export CARGO_PROFILE_RELEASE_DEBUG=${CARGO_PROFILE_RELEASE_DEBUG:-line-tables-only}
    export RUSTFLAGS="${RUSTFLAGS:+${RUSTFLAGS} }-C force-frame-pointers=yes"
    if perf list | rg -q 'cpu_core/cycles'; then
      perf_events=task-clock,context-switches,cpu-migrations,page-faults,cpu_core/cycles/,cpu_core/instructions/,cpu_core/branches/,cpu_core/branch-misses/,cpu_core/cache-references/,cpu_core/cache-misses/
    else
      perf_events=task-clock,context-switches,cpu-migrations,page-faults,cycles,instructions,branches,branch-misses,cache-references,cache-misses
    fi
    cargo build --release --manifest-path "$script_dir/Cargo.toml"
    ;;
  dhat)
    cargo build --release --features dhat-heap --manifest-path "$script_dir/Cargo.toml"
    ;;
  *)
    echo "unknown action: $action" >&2
    exit 2
    ;;
esac

while IFS=$'\t' read -r id page_class relative_path page scale timing_runs counter_runs; do
  [[ -z "$id" || "$id" == \#* ]] && continue
  if [[ -n "$selected_ids" && ",$selected_ids," != *",$id,"* ]]; then
    continue
  fi
  pdf="$corpus_root/$relative_path"
  if [[ ! -f "$pdf" ]]; then
    echo "missing corpus page for $id: $pdf" >&2
    exit 1
  fi
  echo "$action: $id ($page_class)"
  case "$action" in
    structured)
      "$binary" profile "$scale" "$pdf" "$page" 3 "$output_dir/$id.cold.jsonl" --mode cold
      "$binary" profile "$scale" "$pdf" "$page" 5 "$output_dir/$id.warm.jsonl" --mode warm
      "$binary" profile "$scale" "$pdf" "$page" "$timing_runs" "$output_dir/$id.compiled.jsonl" --mode compiled
      "$binary" profile "$scale" "$pdf" "$page" "$timing_runs" "$output_dir/$id.warm-decoded.jsonl" --mode warm-decoded
      "$binary" profile "$scale" "$pdf" "$page" "$timing_runs" "$output_dir/$id.decode-only.jsonl" --mode decode-only
      "$binary" profile "$scale" "$pdf" "$page" "$timing_runs" "$output_dir/$id.prepared.jsonl" --mode prepared
      ;;
    remaining)
      "$binary" profile "$scale" "$pdf" "$page" "$timing_runs" "$output_dir/$id.compiled.jsonl" --mode compiled
      "$binary" profile "$scale" "$pdf" "$page" "$timing_runs" "$output_dir/$id.warm-decoded.jsonl" --mode warm-decoded
      "$binary" profile "$scale" "$pdf" "$page" "$timing_runs" "$output_dir/$id.decode-only.jsonl" --mode decode-only
      ;;
    rss)
      "$binary" profile "$scale" "$pdf" "$page" 1 "$output_dir/$id.compiled.jsonl" --mode compiled
      "$binary" profile "$scale" "$pdf" "$page" 1 "$output_dir/$id.warm-decoded.jsonl" --mode warm-decoded
      ;;
    perf)
      perf stat -x, \
        -e "$perf_events" \
        -o "$output_dir/$id.perf.csv" \
        -- "$binary" profile "$scale" "$pdf" "$page" "$counter_runs" "$output_dir/$id.perf.jsonl" --mode compiled
      ;;
    dhat)
      PDF_RENDERER_DHAT_OUT="$output_dir/$id.dhat.json" \
        "$binary" profile "$scale" "$pdf" "$page" 1 "$output_dir/$id.dhat-profile.jsonl" --mode compiled
      ;;
  esac
done < "$manifest"

{
  git -C "$renderer_dir" rev-parse HEAD
  git -C "$renderer_dir" status --short
  rustc -Vv
  lscpu
  printf 'action=%s\ncorpus_root=%s\nRUSTFLAGS=%s\n' "$action" "$corpus_root" "${RUSTFLAGS:-}"
} > "$output_dir/metadata.txt"

echo "wrote $action corpus profiles to $output_dir"
