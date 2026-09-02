## Release note

Lege now exposes its native PDF renderer through a stable C ABI for C/C++ and generated bindings in languages such as C#, Go, Swift, Zig, and Java. The interface provides opaque document and cancellation handles, page geometry, PNG/JPEG rendering, text extraction, thread-local errors, and library-owned output buffers.

JBIG2 now defaults to symbol mode, which can substantially reduce repeated-text size by substituting similar glyphs with shared bitmaps. This mode is **lossy** and may alter individual pixels or substitute near-identical glyphs. For archival documents, line art, or any material requiring bit-exact preservation, enable **No symbol mode** in the GUI or pass `--no-symbol` on the CLI. The Rust API equivalent is `Jbig2Config::generic()`.

## Reviewer checklist

ABI boundary:

* [ ] Public ABI contains only opaque handles, fixed-width/plain C data, and `#[repr(C)]` structures.
* [ ] `LegeRenderOptions` layout remains unchanged; new options use new functions.
* [ ] Every exported entry point catches panics; release builds retain `panic = "unwind"`.
* [ ] Rust-owned buffers are released only with `lege_buffer_free`.
* [ ] Failed calls clear output buffers and expose a thread-local error message.
* [ ] Documents are not closed while another thread is using them.
* [ ] Cancellation works safely from a second thread.
* [ ] Generated `lege_render.h` matches the Rust ABI.
* [ ] C smoke test builds with warnings treated as errors and renders a valid image on all supported platforms.
* [ ] AGPL implications are documented for downstream consumers.

JBIG2 behavior:

* [ ] Default JBIG2 encoding selects symbol mode consistently across the codec, CLI, Python binding, and GUI.
* [ ] **No symbol mode** is shown only when JBIG2 is selected.
* [ ] Enabling it reaches the encoder as generic-region mode and survives GUI state/worker serialization.
* [ ] CLI `--no-symbol` and Rust `Jbig2Config::generic()` produce the same opt-out behavior.
* [ ] Conflicting legacy mode flags have deterministic precedence.
* [ ] Help text clearly states: symbol mode is smaller but lossy; no-symbol mode is bit-exact but generally larger.
* [ ] Round-trip tests distinguish intentionally lossy symbol output from exact generic output.
* [ ] Release notes do not describe JBIG2 generally as lossless now that symbol substitution is the default.

Repository reference: [Lege](https://github.com/LegeApp/Lege).
