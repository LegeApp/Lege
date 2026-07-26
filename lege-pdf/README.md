# lege-pdf

Conjoined PDF I/O for Lege: native reading/rendering plus a write-side emitter.
Three subfolders:

```text
lege-pdf/
├── read/     ← lege-pdf-read: Lege-owned intake, geometry, outline, and
│             render-session types
├── render/   ← native PDF document engine and CPU renderer (`pdf-*` crates)
└── write/    ← lege-pdf-write: typed, append-only, image-oriented PDF emitter
              (new code; replaces all lopdf usage in src/accumulator.rs)
```

See `PLAN.md` in this directory for the implementation plan of the write side
and the shared-type contract between the two halves.

The renderer crates are first-class members of the Lege root workspace. The
viewer consumes their internal APIs directly so the viewer/document engine
can evolve together without a bitmap-server boundary.
