# lege-render-ffi

C ABI over the Lege PDF renderer: open a document, ask about its pages,
render one to PNG or JPEG, pull its text layer, cancel a slow render from
another thread.

## Licensing — read this first

This links the Lege renderer crates, which are **AGPL-3.0-only**. Anything
you distribute against this library is a combined AGPL work, and AGPL
section 13 extends that to users who reach it over a network. That is a
deliberate choice, not an oversight, but it makes this the wrong starting
point for a proprietary application without a separate arrangement.

## Why a C ABI

It is the multiplier. C and C++ use this header directly, and C#, Go, Swift,
Zig and Java 22+ (via `jextract`) all generate their own bindings from it
with no further Rust. Only Python, Node and Wasm justify a separate Rust
crate, because their idioms need more than a C ABI can express.

The whole surface binds `lege-pdf-read`, which is already the narrow owned
facade over the renderer's twenty-four crates.

## Build

```sh
cargo build -p lege-render-ffi --release
```

Produces `liblege_render.so` / `.dylib` / `.dll` plus a static
`liblege_render.a`. The header is checked in at `include/lege_render.h`;
regenerate it after any ABI change:

```sh
cbindgen --config cbindgen.toml --output include/lege_render.h
```

## Use

```c
#include "lege_render.h"

LegeDocument *doc = lege_document_open(pdf_bytes, pdf_len, NULL);
if (!doc) { fprintf(stderr, "%s\n", lege_last_error_message()); return 1; }

LegeRenderOptions options = lege_render_options_default();
options.dpi = 300.0;

LegeBuffer image = {0};
if (lege_document_render_page(doc, 0, &options, NULL, &image) == LEGE_OK) {
    fwrite(image.data, 1, image.len, out);
    lege_buffer_free(&image);
}
lege_document_close(doc);
```

`examples/render_page.c` is the complete version:

```sh
cc -Iinclude examples/render_page.c -o render_page \
   -L../../../target/debug -llege_render -lm -lpthread -ldl
LD_LIBRARY_PATH=../../../target/debug ./render_page in.pdf 0 out.png 300
```

## Conventions

**Errors.** Every fallible call returns a status code; `LEGE_OK` is 0 and
failures are negative. `lege_last_error_message()` returns the reason as a
NUL-terminated string, or `NULL` if the last call succeeded. The message is
**thread-local**, so two threads failing concurrently do not overwrite each
other, and it is owned by the library — copy it, do not free it.

**Buffers.** Anything the library fills into a `LegeBuffer` must be released
with `lege_buffer_free`. It was allocated by Rust; calling C `free()` on it
is undefined behaviour. Freeing clears the struct, so a second free is inert.
A failed call always leaves the buffer empty, never stale.

**Handles are opaque and not reference counted.** `LegeDocument` is
`Send + Sync`, so several threads may render from one document at once, but
the caller must not close it while another thread is still using it.

**Cancellation is the useful part.** `LegeCancellation` can be cancelled from
a different thread than the one rendering, so a slow page does not have to be
waited out. Almost no PDF library binding offers this.

**`LegeRenderOptions` is frozen.** Adding a field breaks the ABI for every
compiled caller, so new knobs get new functions. Build it from
`lege_render_options_default()` rather than zeroing it, so you inherit sane
values rather than a 0 DPI request.

## Panics

Every entry point runs inside `catch_unwind` and maps a panic to
`LEGE_ERR_PANIC`. This is load-bearing: the renderer parses untrusted input,
and a panic crossing an `extern "C"` frame is undefined behaviour.

It only works while the release profile is `panic = "unwind"`. If that is
ever set back to `abort`, every consumer of this library turns a malformed
PDF into a killed host process — with no warning, because the code still
compiles. The workspace manifest carries a comment to that effect; do not
change it without reading this paragraph.
