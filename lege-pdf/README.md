# lege-pdf

Conjoined PDF I/O for Lege: a read side (renderer, replacing pdfium) and a
write side (emitter, replacing lopdf). Two subfolders:

```text
lege-pdf/
├── render/   ← destination for the pdf-renderer workspace currently at
│             /mnt/Samsung980_1TB/Rust-projects/pdfium-port-plan/pdf-renderer
│             (crates: pdf-source … pdf-render-scheduler; moved, not rewritten)
└── write/    ← lege-pdf-write: typed, append-only, image-oriented PDF emitter
              (new code; replaces all lopdf usage in src/accumulator.rs)
```

See `PLAN.md` in this directory for the implementation plan of the write side
and the shared-type contract between the two halves.

The render side keeps its own workspace until the move is finalized; the root
Lege workspace excludes `lege-pdf/render` so the two build graphs stay
independent for now.
