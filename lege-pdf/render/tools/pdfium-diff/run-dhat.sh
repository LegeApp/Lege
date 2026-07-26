#!/usr/bin/env bash
set -euo pipefail

if [[ $# -lt 3 ]]; then
  echo "usage: $0 <dhat-output.json> <scale> <file.pdf> [page] [runs] [out.jsonl] [--mode cold|warm|compiled|warm-decoded|decode-only|prepared]" >&2
  exit 2
fi

dhat_output=$1
shift
script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)

PDF_RENDERER_DHAT_OUT="$dhat_output" \
  cargo run --release \
    --features dhat-heap \
    --manifest-path "$script_dir/Cargo.toml" \
    -- profile "$@"
