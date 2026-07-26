#!/usr/bin/env bash
set -euo pipefail

if ! command -v cargo-flamegraph >/dev/null; then
  echo "cargo-flamegraph is required for this wrapper" >&2
  exit 127
fi
if [[ $# -lt 2 ]]; then
  echo "usage: $0 <scale> <file.pdf> [page] [runs] [out.jsonl] [--mode MODE]" >&2
  echo "       $0 pipeline-profile <scale> <file.pdf> [runs] [out.jsonl] [compile-workers] [render-workers]" >&2
  exit 2
fi

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
perf_event=${PDF_RENDERER_PERF_EVENT:-cpu_core/cycles/P}
sample_frequency=${PDF_RENDERER_PERF_FREQUENCY:-199}
flamegraph_output=${PDF_RENDERER_FLAMEGRAPH_OUTPUT:-flamegraph.svg}
profile_command=profile
if [[ $1 == pipeline-profile ]]; then
  profile_command=pipeline-profile
  shift
fi

# Frame-pointer unwinding is both more reliable and dramatically smaller than
# DWARF call graphs for these long render loops. The event defaults to the
# performance-core PMU on hybrid Intel CPUs; override it for other machines.
export CARGO_PROFILE_RELEASE_DEBUG=${CARGO_PROFILE_RELEASE_DEBUG:-line-tables-only}
export RUSTFLAGS="${RUSTFLAGS:+${RUSTFLAGS} }-C force-frame-pointers=yes"

cargo flamegraph \
  -c "record -e ${perf_event} -F ${sample_frequency} --call-graph fp -g" \
  -o "$flamegraph_output" \
  --manifest-path "$script_dir/Cargo.toml" \
  -- "$profile_command" "$@"
