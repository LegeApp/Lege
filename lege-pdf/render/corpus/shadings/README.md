# Mesh-shading fixtures (types 4–7)

Self-authored (2026-07-21, this project; no external content — license =
repository license). Each file draws one `sh` mesh on a 200×200 page,
16-bit coordinates with `/Decode [0 200 …]`, DeviceRGB corner colors:

| file | exercises |
|---|---|
| `mesh-type4-triangles.pdf` | free-form Gouraud triangles (two, flag 0) |
| `mesh-type5-lattice.pdf`   | 3×3 lattice-form Gouraud mesh |
| `mesh-type6-coons.pdf`     | one Coons patch, curved edges |
| `mesh-type7-tensor.pdf`    | one tensor patch (16 control points) |

Purpose: put mesh shadings in the pdfium-diff corpus (DEFERRED.md noted
mesh fixtures were absent from the diff corpus). Run them through
`tools/pdfium-diff` after the current sweep; expected: low inkΔ on all
four (see POST-SWEEP-VERIFY.md).
