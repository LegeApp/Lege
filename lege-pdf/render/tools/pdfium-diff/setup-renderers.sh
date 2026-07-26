#!/usr/bin/env bash
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
tools_dir=$(cd -- "$script_dir/.." && pwd)
build_dir="$tools_dir/.renderer-build"
bin_dir="$tools_dir/.renderer-bin"
engines=""

usage() {
  echo "usage: $0 --engines <all|hayro,poppler,mupdf,ghostscript,pdfjs>" >&2
  exit 2
}

while (($#)); do
  case "$1" in
    --engines) engines=${2-}; shift 2 ;;
    *) usage ;;
  esac
done
[[ -n "$engines" ]] || usage
if [[ "$engines" == "all" ]]; then
  engines="hayro,poppler,mupdf,ghostscript,pdfjs"
fi
mkdir -p "$build_dir" "$bin_dir"

has_engine() {
  [[ ",$engines," == *",$1,"* ]]
}

if has_engine hayro; then
  hayro_patch="$script_dir/helpers/hayro-selected-pages.patch"
  if git -C "$tools_dir/hayro" apply --reverse --check "$hayro_patch" >/dev/null 2>&1; then
    : # already applied
  else
    git -C "$tools_dir/hayro" apply --check "$hayro_patch"
    git -C "$tools_dir/hayro" apply "$hayro_patch"
  fi
  cargo build --release --example render -p hayro --manifest-path "$tools_dir/hayro/Cargo.toml"
  mkdir -p "$bin_dir/hayro"
  install -m 0755 "$tools_dir/hayro/target/release/examples/render" "$bin_dir/hayro/hayro-render"
fi

if has_engine poppler; then
  cmake -S "$tools_dir/poppler-26.07.0" -B "$build_dir/poppler" -G Ninja \
    -DCMAKE_BUILD_TYPE=Release -DENABLE_UTILS=ON -DENABLE_CPP=OFF \
    -DENABLE_GLIB=OFF -DENABLE_QT5=OFF -DENABLE_QT6=OFF \
    -DENABLE_BOOST=OFF \
    -DENABLE_NSS3=OFF -DENABLE_GPGME=OFF -DENABLE_LIBCURL=OFF \
    -DBUILD_GTK_TESTS=OFF -DBUILD_QT5_TESTS=OFF -DBUILD_QT6_TESTS=OFF \
    -DBUILD_CPP_TESTS=OFF -DBUILD_MANUAL_TESTS=OFF
  cmake --build "$build_dir/poppler" --target pdftoppm
  mkdir -p "$bin_dir/poppler"
  install -m 0755 "$build_dir/poppler/utils/pdftoppm" "$bin_dir/poppler/pdftoppm"
fi

if has_engine mupdf; then
  git -C "$tools_dir/mupdf" submodule update --init --recursive --depth 1
  make -C "$tools_dir/mupdf" build=release -j"$(nproc)" tools
  mkdir -p "$bin_dir/mupdf"
  install -m 0755 "$tools_dir/mupdf/build/release/mutool" "$bin_dir/mupdf/mutool"
fi

if has_engine ghostscript; then
  # Ghostscript 10.07.1's bundled libtiff configure path is not safe in an
  # out-of-tree build, so configure inside its ignored third-party checkout.
  if [[ ! -f "$tools_dir/ghostscript-10.07.1/Makefile" ]]; then
    (cd "$tools_dir/ghostscript-10.07.1" && ./configure --disable-gtk --without-x)
  fi
  make -C "$tools_dir/ghostscript-10.07.1" -j"$(nproc)"
  mkdir -p "$bin_dir/ghostscript"
  install -m 0755 "$tools_dir/ghostscript-10.07.1/bin/gs" "$bin_dir/ghostscript/gs"
fi

if has_engine pdfjs; then
  node_path=$(command -v node)
  node_major=$(node -p 'process.versions.node.split(".")[0]')
  node_minor=$(node -p 'process.versions.node.split(".")[1]')
  if ((node_major < 22 || (node_major == 22 && node_minor < 13))); then
    echo "PDF.js requires Node >=22.13 (found $(node --version))" >&2
    exit 1
  fi
  (cd "$tools_dir/pdf.js" && npm ci && npx gulp lib-legacy)
  mkdir -p "$bin_dir/pdfjs"
  install -m 0755 "$script_dir/helpers/pdfjs-render.mjs" "$bin_dir/pdfjs/render.mjs"
  printf '#!/bin/sh\nexec "%s" "%s" "$@"\n' "$node_path" "$bin_dir/pdfjs/render.mjs" > "$bin_dir/pdfjs/render"
  chmod 0755 "$bin_dir/pdfjs/render"
fi

echo "renderer helpers installed under $bin_dir"
