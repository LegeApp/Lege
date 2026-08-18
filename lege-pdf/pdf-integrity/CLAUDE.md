# pdf-integrity

CLI PDF integrity / forensic-triage checker, built on the lege-pdf crates in
`/mnt/Samsung980_1TB/Rust-projects/Lege-ecosystem/lege-pdf/`.

## Planning docs live in AKR, not Markdown

This project's plan is a typed knowledge ledger in `.akr/`, served by the `akr` MCP
server. `initial-plan-shape.md` is the historical source document; the ledger is
authoritative. Do not write new plan/roadmap/decision Markdown files — write AKR
records.

Protocol (full version in `AGENTS.md`):

- **Before any task**: `knowledge_context` with the milestone or work key you are
  working toward (e.g. `audit.milestone.m2-revision-history`). Read the whole bundle.
- **Lookups**: `knowledge_search` / `knowledge_get`.
- **New durable knowledge**: `knowledge_propose`. Plans and decisions are records
  (`work`, `decision`, `question`, ...), namespaced `audit.*` (the checker) and
  `lege.*` (upgrades needed in the Lege-ecosystem repo).
- **Changing settled records**: `knowledge_revise` / `knowledge_supersede` — never
  edit `.akr/` files by hand.
- **Before handing back**: `knowledge_validate`; then run `akr build` if you wrote
  records, so search and `docs/generated/` views stay current.

Known MCP gap: `knowledge_propose` cannot author `acceptance` blocks, so milestones
(and work items that need their own acceptance) must go through the CLI:
write a full record to a temp file (`akr 0.1` header + `project pdf-integrity` line +
`record ... { ... }`) and run
`akr propose <key> --kind milestone --from <file>`. The `akr` binary is at
`~/.local/bin/akr` (built from `/mnt/Samsung980_1TB/Rust-projects/AKR`).
