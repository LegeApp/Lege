# Rendering corpus

Versioned test PDFs, organized by feature area (roadmap Phase 0).

Layout convention:

```text
corpus/
  paths/          basic fill/stroke geometry
  text/           simple + embedded fonts, CID, Type 3
  images/         JPEG, Flate, masks, inline images
  clipping/
  transparency/   groups, soft masks, blend modes
  patterns/
  shadings/
  color/          ICC, Separation, DeviceN, Lab
  structure/      incremental updates, object streams, xref streams, linearized
  malformed/      broken-but-commonly-accepted files (each with a NOTES.md)
  adversarial/    decompression bombs, deep recursion, huge dimensions
```

Every file gets a short provenance note (where it came from, what it
exercises, license). Reference renders produced by the PDFium comparison tool
in `tools/` are stored out-of-repo and keyed by (file hash, PDFium version,
render flags).
