#!/usr/bin/env bash
set -euo pipefail

ALLOWLIST_FILE="${1:-.sync-allowlist}"

if ! git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
  echo "Error: run inside a git repository." >&2
  exit 1
fi

if [ ! -f "$ALLOWLIST_FILE" ]; then
  echo "Error: allowlist file not found: $ALLOWLIST_FILE" >&2
  exit 1
fi

tmp_file="$(mktemp)"
trap 'rm -f "$tmp_file"' EXIT

# Remove comments/blank lines.
sed -e 's/[[:space:]]*#.*$//' -e 's/[[:space:]]*$//' "$ALLOWLIST_FILE" | awk 'NF > 0 { print }' > "$tmp_file"

if [ ! -s "$tmp_file" ]; then
  echo "Error: allowlist has no active entries: $ALLOWLIST_FILE" >&2
  exit 1
fi

# Start from a clean index so only approved paths are staged.
git restore --staged :/

# Stage only approved paths.
git add -A --pathspec-from-file="$tmp_file"

echo "Staged files are restricted to $ALLOWLIST_FILE"
git diff --name-only --cached
