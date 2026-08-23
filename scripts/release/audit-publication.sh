#!/usr/bin/env bash
# Audit a candidate before force-pushing it to LegeApp/Lege. This script never
# modifies Git state; pass a disposable output directory for its reports.
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: scripts/release/audit-publication.sh --report-dir PATH [--strict]

Creates tracked-file, history, nested-repository, and model-checksum reports.
--strict also requires gitleaks and fails on an unavailable scanner or on any
unallowlisted tracked file over 10 MiB.
EOF
}

report_dir=""
strict=false
while (($#)); do
  case "$1" in
    --report-dir) report_dir=${2:?missing report directory}; shift 2 ;;
    --strict) strict=true; shift ;;
    -h|--help) usage; exit 0 ;;
    *) usage >&2; exit 2 ;;
  esac
done
[[ -n "$report_dir" ]] || { usage >&2; exit 2; }

repo_root=$(git rev-parse --show-toplevel)
cd "$repo_root"
mkdir -p "$report_dir"
report_dir=$(cd "$report_dir" && pwd)

git rev-parse HEAD >"$report_dir/source-commit.txt"
git status --short >"$report_dir/worktree-status.txt"
git ls-files -s | awk '{ print $2 "\t" $4 }' \
  | git cat-file --batch-check='%(objectsize) %(rest)' \
  | awk '$1 >= 1048576 { print }' \
  | sort -n >"$report_dir/tracked-over-1m.txt"
git rev-list --objects HEAD | git cat-file --batch-check='%(objecttype) %(objectname) %(objectsize) %(rest)' \
  | awk '$1 == "blob" && $3 >= 10485760 && $4 !~ /^(lege-process\/models\/(doclayout|sauvola)\.onnx|lege-ocr\/assets\/ppocr-(det|rec)\.onnx)$/ { print }' \
  | sort -k3n >"$report_dir/history-blobs-over-10m.txt"

git ls-files -z | while IFS= read -r -d '' path; do
  case "$path" in
    */.git/*|.git/*) printf '%s\n' "$path" ;;
  esac
done >"$report_dir/tracked-nested-git-paths.txt"

sha256sum \
  lege-process/models/doclayout.onnx \
  lege-process/models/sauvola.onnx \
  lege-ocr/assets/ppocr-det.onnx \
  lege-ocr/assets/ppocr-rec.onnx >"$report_dir/embedded-model-checksums.txt"
sha256sum --check assets/runtime-models.sha256 >"$report_dir/embedded-model-verification.txt"

git check-ignore -v .filter-tmp lege-document-ocr/turboocr lege-codecs/jp2lam/.agent/scratch \
  >"$report_dir/ignore-policy.txt" || true

if command -v gitleaks >/dev/null; then
  gitleaks git --redact --report-format json --report-path "$report_dir/gitleaks.json"
elif "$strict"; then
  echo "gitleaks is required for --strict" >&2
  exit 1
else
  echo "gitleaks unavailable; run again with it installed before publication" >"$report_dir/gitleaks-status.txt"
fi

if "$strict" && [[ -s "$report_dir/history-blobs-over-10m.txt" ]]; then
  echo "large blobs remain in reachable history; review $report_dir/history-blobs-over-10m.txt" >&2
  exit 1
fi
if "$strict" && [[ -s "$report_dir/worktree-status.txt" ]]; then
  echo "the publication candidate must be committed before a strict audit" >&2
  exit 1
fi

echo "Publication audit reports: $report_dir"
