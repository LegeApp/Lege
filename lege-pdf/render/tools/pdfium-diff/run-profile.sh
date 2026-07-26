#!/usr/bin/env bash
set -euo pipefail

# Run one manifest-selected page. The corpus stays external; callers provide
# an absolute PDF path, avoiding hidden machine-specific paths in reports.
if [[ $# -lt 2 ]]; then
  echo "usage: $0 <scale> <file.pdf> [page] [runs] [out.jsonl] [--mode cold|warm|compiled|warm-decoded|decode-only|prepared]" >&2
  exit 2
fi

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
cargo run --release --manifest-path "$script_dir/Cargo.toml" -- profile "$@"
