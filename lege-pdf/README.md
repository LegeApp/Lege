# lege-pdf

Conjoined PDF I/O for Lege: a read seam over the external renderer and a
write-side emitter. Three subfolders:

```text
lege-pdf/
├── read/     ← lege-pdf-read: Lege-owned intake, geometry, outline, and
│             render-session types; renderer types do not escape this crate
├── render/   ← intentionally empty placeholder; the renderer stays at
│             /mnt/Samsung980_1TB/Rust-projects/pdfium-port-plan/pdf-renderer
└── write/    ← lege-pdf-write: typed, append-only, image-oriented PDF emitter
              (new code; replaces all lopdf usage in src/accumulator.rs)
```

See `PLAN.md` in this directory for the implementation plan of the write side
and the shared-type contract between the two halves.

The renderer keeps its own workspace and lock file. The Lege root names its
required crates once through relative `[workspace.dependencies]` paths.
