#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

cd "$ROOT"
export LEGE_RUN_GPU_OCR_TESTS=1
export WGPU_REQUIRE_REAL_GPU=1

cargo test -p lege-ocr --features paddle-ocr paddle_engine_ -- --nocapture --test-threads=1
