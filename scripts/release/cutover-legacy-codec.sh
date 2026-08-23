#!/usr/bin/env bash
# Replaces one legacy codec default branch with a redirect-only README, then
# archives the repository. Existing tags and GitHub releases are untouched.
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: scripts/release/cutover-legacy-codec.sh --codec CODEC --apply

CODEC is one of: djvulibrust, jbig2enc-rust, jp2lam.
Requires an authenticated GitHub CLI account with repository-admin permission.
Without --apply the script only prints the planned target.
EOF
}

codec=""
apply=false
while (($#)); do
  case "$1" in
    --codec) codec=${2:?missing codec}; shift 2 ;;
    --apply) apply=true; shift ;;
    -h|--help) usage; exit 0 ;;
    *) usage >&2; exit 2 ;;
  esac
done

case "$codec" in
  djvulibrust) legacy_repo=LegeApp/DJVULibRust; canonical=djvulibrust; crate=djvu_encoder ;;
  jbig2enc-rust) legacy_repo=LegeApp/jbig2enc-rust; canonical=jbig2enc-rust; crate=jbig2enc-rust ;;
  jp2lam) legacy_repo=LegeApp/jp2lam; canonical=jp2lam; crate=jp2lam ;;
  *) usage >&2; exit 2 ;;
esac

target=https://github.com/LegeApp/Lege/tree/main/lege-codecs/$canonical
echo "Legacy repository: $legacy_repo"
echo "Canonical source:  $target"
"$apply" || exit 0

command -v gh >/dev/null || { echo "GitHub CLI is required" >&2; exit 1; }
gh auth status >/dev/null
worktree=$(mktemp -d)
trap 'rm -rf "$worktree"' EXIT
gh repo clone "$legacy_repo" "$worktree/repo" -- --depth=1
cd "$worktree/repo"
git switch --orphan moved-to-lege
git rm -rf .
cat > README.md <<EOF
# Development moved to Lege

The maintained source for **$crate** is now in the
[Lege monorepo]($target).

Please use the monorepo for source, issues, and development. This repository is
an archived redirect only; its existing releases remain available.
EOF
git add README.md
git commit -m "docs: redirect development to Lege monorepo"
git branch -M main
git push --force origin HEAD:main
gh repo edit "$legacy_repo" --description "Development moved to LegeApp/Lege" --homepage "$target"
gh repo archive "$legacy_repo" --yes
