#!/usr/bin/env bash
set -euo pipefail

if ! command -v perf >/dev/null; then
  echo "perf is required for this wrapper" >&2
  exit 127
fi
if [[ $# -lt 2 ]]; then
  echo "usage: $0 <scale> <file.pdf> [page] [runs] [out.jsonl]" >&2
  exit 2
fi

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
out=${5:-perf-profile.jsonl}
perf stat -d -d -d -- "$script_dir/run-profile.sh" "$@" 2>"${out%.jsonl}.perf.txt"
