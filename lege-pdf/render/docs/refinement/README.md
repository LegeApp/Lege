# Renderer refinement archive

This is the single in-repository home for renderer development context after
the 2026-07-25 move from `pdfium-port-plan/pdf-renderer` to
`Lege-ecosystem/lege-pdf/render`.

Start with [CURRENT-STATE.md](CURRENT-STATE.md). It reconciles the latest
Linux and Windows memories and separates still-relevant work from issues that
later sweeps closed.

The remaining material is grouped by purpose:

- `memories/linux-project/` — the fullest sweep 6–14 history. Its
  `sweep13-windows-closures-and-clip-perf.md` file contains the latest
  sweep-14 follow-up and supersedes earlier tail lists.
- `memories/windows/` — Windows-side notes, including early sweep state and
  codec fixes.
- `memories/linux-root/` — older renderer architecture, corpus, and
  performance notes recorded at the broader Rust-projects level.
- `plans/` — original architecture, performance, text, and sweep plans.
  The active focused GPU postprocess work is tracked in
  [`PLAN-GPU-POSTPROCESS-EXECUTOR.md`](plans/PLAN-GPU-POSTPROCESS-EXECUTOR.md).
  The next renderer-focused ticket is
  [`PLAN-GPU-IMAGE-RENDERING.md`](plans/PLAN-GPU-IMAGE-RENDERING.md).
- `handoffs/` — historical queues and handoffs. These are provenance, not a
  current unchecked task list; later memories close many of their items.
- `performance-history/` — dated benchmark notes formerly under
  `corpus/perf/`.
- `integration/` — the viewer-facing API change record.

Operational documentation remains beside the code or tool it describes:
`../../README.md`, `../../tools/README.md`, and crate-local README files.
The pre-move virtual workspace manifest is retained as
`legacy-workspace-manifest.toml` for dependency and lint provenance only.
