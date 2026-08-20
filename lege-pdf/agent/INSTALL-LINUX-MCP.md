# Installing lege-pdf as a Claude Code MCP server (Linux)

One-time setup to register `lege-pdf` as a global (user-scope) MCP server
alongside `akr`, `codegraph`, and `fff`. Windows already has this installed;
this is the Linux-side counterpart.

## 1. System dependencies

`lege-ocr` (a `lege-pdf-agent` dependency) needs Tesseract/Leptonica dev
headers to build:

```sh
sudo apt-get update
sudo apt-get install -y build-essential pkg-config libtesseract-dev libleptonica-dev clang libclang-dev
```

Runtime libs (`libtesseract.so.5`, `libleptonica.so.6`) are already present
on this system — confirmed via `/usr/lib/x86_64-linux-gnu/`.

## 2. Build

```sh
cd /mnt/Samsung980_1TB/Rust-projects/Lege-ecosystem   # adjust if the repo moved
cargo build --profile debug-fast -p lege-pdf-agent --bin lege-pdf
```

`debug-fast` = release optimizations, LTO off — the project's standard
day-to-day build profile (see root `CLAUDE.md`); don't use plain `--release`
for this, it drags in full LTO and takes far longer for a dev/CLI binary.

## 3. Install the binary

```sh
mkdir -p ~/.local/bin
cp target/debug-fast/lege-pdf ~/.local/bin/lege-pdf
```

## 4. Register with Claude Code (user/global scope)

```sh
claude mcp add --scope user lege-pdf -- ~/.local/bin/lege-pdf mcp
```

`--scope user` is required — plain `claude mcp add` defaults to **local**
scope (tied to whatever project you're in), which is *not* what the other
three global tools use.

## 5. Verify

```sh
claude mcp list
```

Expect all four connected:

```
codegraph: ... - ✔ Connected
akr:       ... - ✔ Connected
fff:       ... - ✔ Connected
lege-pdf:  ~/.local/bin/lege-pdf mcp - ✔ Connected
```

If `lege-pdf` fails to connect, check `ldd ~/.local/bin/lege-pdf` for
missing `libtesseract`/`libleptonica` symbol versions before anything else.
