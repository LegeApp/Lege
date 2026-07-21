# lege-pdf/render — placeholder

The pdf-renderer workspace from
`/mnt/Samsung980_1TB/Rust-projects/pdfium-port-plan/pdf-renderer` moves here
(crates/, corpus/, shaders/, tools/, its own Cargo workspace). Until then this
directory is intentionally empty and excluded from the Lege root workspace.

When the move happens, `pdf-page-ir` (dependency-free geometry + page IR)
becomes the shared-type home for both the renderer and `../write`
(`lege-pdf-write`). See `../PLAN.md` §3.
