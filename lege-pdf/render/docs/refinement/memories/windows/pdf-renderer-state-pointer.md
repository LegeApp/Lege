---
name: pdf-renderer-state-pointer
description: pdf-renderer production-readiness pass state and repo locations as of 2026-07-20 evening
metadata: 
  node_type: memory
  type: project
  originSessionId: 252c08ea-8418-4fc4-a91a-8c52209a74e0
  modified: 2026-07-21T01:36:35.281Z
---

Authoritative older memories live at `F:\home\dk\.claude\projects\-mnt-Samsung980-1TB-Rust-projects\memory\` (Linux dual-boot; `/mnt/Samsung980_1TB/Rust-projects/` = `D:\Rust-projects\`). Note their "uncommitted perf work" warnings are now STALE — everything was committed.

Production-readiness pass executed 2026-07-20 (plan: `C:\Users\dk\.claude\plans\snazzy-finding-thacker.md`): Phase 0 (66e7511) extracted `crates/pdf-geom`, pinned toolchain 1.97.1, added panic lints + ImageIr.lowering_degraded. Three parallel stream branches then merged to master (C=8b9c881 fuzz/chaos/pdf-read/passwords/catch_unwind, B=daebffb annotations/R1-tint/CID→Unicode/vertical/OCGs, A=eb3ca7c JPEG-pass4/image-cache/never-panic/blend-generalization/knockout+BC/mesh-raster/edge-AA). Full workspace green after merges. Integration agent then handled: annotations default ON, password threading, scheduler Panic variant, mesh shading PARSING (types 1/4-7), /TR transfer function, lint deny flip.

Repo layout after user's restructure: jp2lam + jbig2enc-rust moved to `D:\Rust-projects\Lege-ecosystem\lege-codecs\` (renderer path deps updated as TEMPORARY `../../Lege-ecosystem/lege-codecs/...` — become `../../lege-codecs/...` when renderer migrates to `Lege-ecosystem\lege-pdf\render\`). jp2lam pushed (LegeApp/jp2lam, HTTPS push works on Windows). pdf-renderer and jbig2enc-rust have NO git remotes — user has not authorized creating GitHub repos. Lege/Lege-ecosystem rust-toolchain.toml files vanished mid-refactor; toolchain alignment there pending.

Pass COMPLETE 2026-07-21: master at 952f790, 740 tests green, clippy clean at deny. Post-pass closeout landed postprocess graph, minification weights, DS82/DLIFLC root causes, synthetic bold/italic, cancellation checkpoints, /Perms, page-count placeholders, MacExpert+AGL, mesh fixtures; DEFERRED.md reduced to 10 architectural residuals. pdfium-diff controller rewritten batched-concurrent (was fully serial; ~7.7× faster; PDFIUM_DIFF_WORKERS/CHUNK env). Windows sweep runs via run-sweep-windows.ps1 into sweep-annot-baseline\ (flags=annot baseline). POST-SWEEP-VERIFY.md lists 6 re-grade targets (sweep binary predates items 3-6). Integration plan for Lege move: Lege-ecosystem\lege-misc\docs\renderer-integration-plan.md (amends compute-scheduler-plan.md; page-owned pipeline, K inference sessions, pdfium deleted). jbig2enc-rust: remote histories UNRELATED; local main pushed as origin/local-decode-phase5; reconciliation pending user decision. Outstanding after this pass: full corpus re-sweep must run on the Linux boot (rebuild tools/pdfium-diff first — FPDF_ANNOT baseline shift is stamped in its CSV header); fuzz soak needs nightly+Linux; workstream H second half (minification weights) still open; stream worktrees wt-stream-a/b/c under pdfium-port-plan can be pruned.
