# Lege open-source monorepo relaunch plan

Status: implementation in progress  
Prepared: 2026-08-22  
Target repository: `LegeApp/Lege`  
Canonical codec paths: `lege-codecs/{djvulibrust,jbig2enc-rust,jp2lam}`

## Outcome

Publish one authoritative Lege ecosystem repository containing the three Lege
codec implementations, retain the completed in-process DJVULibRust integration,
keep required embedded application models in Git, and make optional/large
Document OCR models reproducibly downloadable. The old codec repositories stay
online as archived history and signposts, not writable mirrors.

This is a narrowly scoped relaunch. Apart from the DjVu integration seam,
repository hygiene, asset bootstrap, licensing/metadata, and CI needed to make
the public tree reproducible, application behavior should not be rolled back.

## Facts established from the current checkout

### DjVu baseline

- The last direct-library implementation in reachable history is commit
  `b47958d5e168e0c48bc30311d2a9bbaacf639889`, Lege v1.4.61. Its
  `src/djvu.rs` imports `djvu_encoder` and builds/encodes pages in process.
- Commit `838256892493bb0899a31683cc5f46b0354e114f` (v1.4.64) introduced the
  neutral JSON/filesystem manifest and the separate `djvu-encoder` subprocess.
- The reachable version sequence later goes through v1.4.7 and v1.4.71, then
  v1.4.74; there is no exact v1.4.72 revision or tag in this checkout. “About
  1.4.72” should therefore mean restoring the pre-`8382568` integration seam,
  not resetting the application to an assumed version.
- The pending public application release is v1.4.76. Its DjVu path has behavior added after the split,
  including full-color cover preservation, cleaned grayscale backgrounds,
  cancellation, temporary-output publication, current progress accounting, and
  renderer/pipeline changes. Those behaviors must be retained.

### Repository topology

- All intended codecs are already present in the monorepo:
  `lege-codecs/djvulibrust`, `lege-codecs/jbig2enc-rust`, and
  `lege-codecs/jp2lam`.
- The codec crates intentionally retain independent Cargo build graphs and
  lockfiles while being consumed through path dependencies. That arrangement is
  compatible with a monorepo and need not be collapsed into the root workspace.
- The old public repositories are currently live, not archived:
  `LegeApp/DJVULibRust`, `LegeApp/jbig2enc-rust`, and `LegeApp/jp2lam`.
- The former public Lege tree had a GitHub Action that used `git subtree split`
  and pushed copies to the standalone repositories. That solves compatibility
  but still creates three copies and should not be restored as the steady state.
- Local `main` and `public/main` have unrelated rewritten histories (no merge
  base), so publication is a controlled replacement/cutover, not a normal
  fast-forward push.

### Size and asset inventory

Required, currently tracked runtime models total about 22.9 MB and are directly
embedded or used by the application:

| Asset | Bytes | SHA-256 | Disposition |
|---|---:|---|---|
| `lege-process/models/doclayout.onnx` | 11,637,620 | `7d990ee5e3ea1c2d3956a276e4f1de956652a5169ce10f03aba8aaca52bce40e` | Keep tracked |
| `lege-process/models/sauvola.onnx` | 439,690 | `88274fab8bd8a201b4f67e89b58d774cb9176bd87e8687e3a4eb68dc80e44f2a` | Keep tracked |
| `lege-ocr/assets/ppocr-det.onnx` | 4,943,118 | `1398b2fb1178e3d82337dc886363a634c31a464be6770aec5a3e8f827a25e988` | Keep tracked |
| `lege-ocr/assets/ppocr-rec.onnx` | 10,566,898 | `f39a0c630afdfc27706e00dc90313e0ef125a41ed4db4cda0ea3796e074ed91c` | Keep tracked |

Large Document OCR/TurboOCR assets are already beneath the ignored
`lege-document-ocr/turboocr/` clone. The production tiny text bundle is about
6.25 MB; optional small and medium bundles are about 31 MB and 139 MB. These
should be fetched from the checksum-pinned TurboOCR model release rather than
committed to Lege.

The current HEAD also contains development output that should not ship:

- about 107 MB under `lege-pdf/oracle-sweep-15-2026-07-26`;
- about 15 MB under `lege-pdf/render/corpus/perf/results`;
- about 3.3 MB of tracked `lege-codecs/jp2lam/.agent/scratch` flamegraphs;
- a 50 MB oracle result as the largest current blob.

Ignored local output is much larger (for example the 728 MB installer in
`dist/`, codec `target/` directories, TurboOCR build directories, and the nested
TurboOCR `.git`). It is not currently tracked, but ignore and clean-clone checks
must ensure it remains absent.

The full history contains old archives, binaries, benchmark images, and vendored
trees, including blobs around 15–18 MB in addition to the current oracle data.
Publishing only a clean HEAD does not remove those from a full-history clone.

## Decisions to make before implementation

### D1 — License of the integrated application (blocking)

DJVULibRust declares `AGPL-3.0-only`; Lege currently declares MIT. The existing
subprocess boundary was deliberately documented as an arms-length license
boundary. Restoring direct linking must not leave contradictory license claims.

Choose and document one of these before merging the integration change:

1. **Recommended for a literal historical restoration:** license the combined
   Lege application under `AGPL-3.0-only`, while keeping independently usable
   MIT/Apache codec crates under their existing licenses.
2. Keep Lege MIT only if every relevant DJVULibRust copyright holder has validly
   relicensed that code under an MIT-compatible license. Record the relicensing
   basis and update the crate manifest/license files in the same change.
3. If neither is acceptable, retain the subprocess boundary; this does not meet
   the requested integrated-API outcome.

Have the final choice reviewed as a licensing decision. The relevant primary
text is the [GNU AGPL v3](https://www.gnu.org/licenses/agpl-3.0.html); this plan
is not legal advice.

### D2 — Public-history strategy (blocking)

**Selected:** audit the comprehensive local history, then force-replace the
public default branch with the approved local monorepo history. Existing GitHub
releases remain unchanged. `Lege_internal` is not a publication source and will
be deleted separately after the public cutover.

Do not push the private branch blindly. Before choosing between a filtered
history and a clean snapshot, run the secret, license, and blob audits below.
Because `main` and `public/main` are unrelated rewritten lines, prepare the
candidate in a temporary clone and compare its tree to the approved source tree.

### D3 — Legacy repository behavior

GitHub cannot make a repository URL transparently resolve to a subdirectory of
another repository. Repository rename/transfer redirects apply to whole
repositories, not monorepo folders. Therefore:

- make `LegeApp/Lege` the only writable source of truth;
- force-replace each old repository's default branch with a prominent moved
  notice and a direct link to its exact monorepo directory;
- update its description and website URL to the monorepo directory;
- leave existing GitHub releases unchanged;
- archive the repository after the notice commit;
- disable or delete subtree-sync credentials/workflows.

This preserves discoverability and history without pretending that old Git
dependency URLs continue to track current code. If compatibility with those Git
URLs is later judged mandatory, an automated subtree mirror is the only simple
option, but it is explicitly a copy and should be treated as a time-limited
compatibility exception.

## Implementation phases

### Phase 0 — Freeze and audit the publication candidate

1. Create a private staging branch/repository from a clean clone. Do not work
   from the current dirty checkout and do not include `.filter-tmp`.
2. Record the exact source commit and generate inventories for:
   - tracked files over 1 MB and 10 MB;
   - every blob over 10 MB in the proposed public history;
   - ignored files that would become tracked after ignore changes;
   - nested Git repositories and submodules;
   - executable/archive/database/model file types;
   - secrets using `gitleaks` or an equivalent full-history scanner.
3. Classify every large file as runtime-required, small deterministic fixture,
   redistributable third-party runtime, downloadable asset, generated evidence,
   or local development output. Unclassified large files fail the gate.
4. Remove development outputs from HEAD and strengthen ignores for:
   `.agent/scratch`, `target`, `fuzz/target`, `build-*`, `dist`, local corpora,
   oracle sweeps/results, package staging, downloaded SDK/runtime files, and
   nested-repository metadata.
5. Move useful benchmark conclusions into compact Markdown/CSV summaries or AKR
   evidence. Put reproducible bulky results in release assets or a separately
   documented benchmark-data store; do not keep raw local runs in source Git.
6. Decide whether large history requires `git filter-repo`. If history is
   rewritten, produce old-to-new tag/commit mapping and announce the one-time
   force cutover before making the repository public.

Acceptance gate:

- a fresh clone contains no unapproved tracked file over 10 MB;
- the four allowlisted embedded models match the table above;
- no secret scanner findings remain unresolved;
- `git status --ignored` confirms build/model downloads stay ignored;
- source builds and tests do not read from the original developer machine.

### Phase 1 — Restore the DJVULibRust integrated API only (complete)

Use `b47958d:src/djvu.rs` as the API/reference implementation and current
`lege-process/core/djvu.rs` as the behavioral specification. Do not restore the
old file wholesale.

1. Add the path dependency back to `lege-process/Cargo.toml`, using the current
   crate and its needed acceleration features:

   ```toml
   djvu_encoder = { path = "../lege-codecs/djvulibrust", features = ["simd", "rayon"] }
   ```

2. Replace the manifest/filesystem/subprocess handoff in
   `lege-process/core/djvu.rs` with typed `djvu_encoder` calls:
   `PageBuilder`/`PageEncodeParams`, parallel `encode_page`, ordered
   `add_encoded_page`, and in-process document finalization.
3. Preserve current page composition semantics:
   - full-color cover handling and background subsampling policy;
   - cleaned grayscale/MRC background behavior;
   - JB2 mask polarity and blank-page handling;
   - image-region placement and pre-masking;
   - hidden OCR text coordinates and Unicode normalization;
   - out-of-order page completion followed by deterministic page order;
   - cancellation before publication and atomic final-output replacement;
   - existing progress events and error context.
4. Remove only subprocess-specific surface:
   `resolve_encoder_path`, `LEGE_DJVU_ENCODER`,
   `--djvu-encoder-path`, manifest structs/files, process spawning, and packaging
   of the helper executable. Keep the standalone codec CLI available inside
   `lege-codecs/djvulibrust` for codec users and diagnostics.
5. Do not change PDF, EPUB, OCR, renderer, GUI, quality defaults, or codec
   algorithms as part of this phase.
6. Add/retain regression coverage for single-page, multi-page, full-color cover,
   MRC/JB2, hidden text, cancellation, atomic publication, and reproducible page
   ordering. Compare output validity with `djvudump`/`ddjvu` where available and
   compare rendered pixels or semantic page structure rather than requiring old
   and new container bytes to be identical unless determinism is promised.

Acceptance gate:

- Lege produces valid DjVu without a `djvu-encoder` executable or environment
  override present;
- the integration is entirely in process (no command spawn or manifest staging);
- current v1.4.75 DjVu modes and cancellation tests pass;
- a clean release package contains no helper binary solely for Lege's use;
- license metadata matches D1.

### Phase 2 — Make the monorepo the codec source of truth

1. Keep canonical code at:
   - `lege-codecs/djvulibrust`
   - `lege-codecs/jbig2enc-rust`
   - `lege-codecs/jp2lam`
2. Keep independent codec manifests/lockfiles if their build graphs and release
   profiles require them. “Monorepo” means one source-control authority, not
   necessarily one Cargo workspace graph.
3. Update every codec manifest and README repository/homepage link to its
   `https://github.com/LegeApp/Lege/tree/main/lege-codecs/...` location. Add
   missing `repository`, `readme`, documentation, and `include` metadata.
4. Add actual license texts to every publishable crate package. The current
   jbig2enc-rust and jp2lam manifests declare `MIT OR Apache-2.0` but their
   directories do not contain corresponding license files; fix that before a
   release. Add a root license/NOTICE map explaining mixed-license subtrees.
5. Add a codec CI matrix that runs each crate from its own manifest, including
   formatting, clippy, tests, minimal/default/feature builds, package dry-run,
   and the relevant oracle tests. Root workspace exclusion must not mean CI
   exclusion.
6. Publish crates.io releases from the monorepo if Git-consumer compatibility is
   needed. Cargo consumers should use versioned registry dependencies; internal
   Lege consumers use path dependencies. Document that old repository Git URLs
   are frozen.
7. Remove the old subtree mirror workflow and rotate/delete its fine-grained
   token after the final archived-repository notice commits.

Acceptance gate:

- a code search finds no authoritative codec source outside `lege-codecs`;
- each codec builds/tests/packages from a fresh monorepo clone;
- crate metadata points to the monorepo;
- no workflow pushes codec source into another repository.

### Phase 3 — Add one verified Document OCR asset bootstrap

Add a checked-in, standard-library-only Python entry point such as
`scripts/bootstrap-assets.py` and a data-only lock manifest such as
`assets/model-assets.lock.json`. Keep URL, size, SHA-256, destination, license,
and profile in the manifest rather than duplicating them in shell and
PowerShell scripts.

Required behavior:

1. Commands/profiles:
   - `python scripts/bootstrap-assets.py --profile document-ocr-tiny` downloads
     the production `det_tiny.onnx`, `rec_tiny.onnx`, and `keys_tiny.txt`;
   - `--profile document-ocr-small` and `--profile document-ocr-medium` add the
     optional quality tiers;
   - `--all` fetches every optional model explicitly;
   - `--check` performs no network access and verifies installed assets;
   - `--clean` is not provided initially; deletion remains explicit/manual.
2. Default source is the immutable TurboOCR release
   `models-v3.0.0-ppocrv6`, never a moving `latest` URL. Permit a documented
   mirror base URL for outages without permitting unchecked bytes.
3. Download to a temporary file beside the destination, stream SHA-256 while
   downloading, verify expected size and digest, then atomically rename. A
   failed/interrupted download never replaces a valid asset.
4. Refuse hash mismatches, path traversal, redirects to unsupported schemes,
   and an existing wrong file unless `--force` is explicitly given. Re-running
   with valid files is a no-op.
5. Place downloads only under ignored paths. The packaging script should call
   `--check` (or the same manifest verifier) before copying assets.
6. Emit a concise machine-readable summary for CI and print model provenance and
   license information for humans.
7. Test the downloader with a local HTTP fixture: success, idempotence, corrupt
   bytes, interrupted transfer, wrong size, unknown profile, and offline check.

The root bootstrap may optionally clone TurboOCR source, but only at a pinned
commit. The current ignored TurboOCR checkout has three local modifications
(`scripts/build_windows_trt.ps1`, `src/engine/trt/onnx_to_trt.cpp`, and
`src/recognition/ctc_decode.cpp`). Before publication, either upstream/commit
those changes and pin that commit, or deliberately vendor the reviewed source
without `.git`, models, tests corpora, or build output. A fresh clone must never
depend on these uncommitted local files.

Do not move the four embedded Lege models into the downloader in this phase.
They are small enough for source distribution and are required for the ordinary
application to work without a post-clone network fetch.

Acceptance gate:

- a clean clone plus one documented bootstrap command can build/package Document
  OCR;
- the repository remains clean after download because destinations are ignored;
- offline `--check` verifies all required production assets;
- corrupted or partial assets are rejected and never published into place;
- no optional small/medium model is present in Git history.

### Phase 4 — Cut over public repositories

1. Build the sanitized candidate in a temporary remote/branch and run all gates.
2. Back up branch protection, issue settings, releases, and current ref tips.
3. Put `LegeApp/Lege` into a short maintenance window. Publish the approved
   history using the D2 procedure, then restore branch protection and required
   checks.
4. Verify the public clone by URL on Linux and Windows; run the ordinary build,
   codec CI, downloader `--check`, and a small PDF/DjVu/OCR smoke matrix.
5. For each old codec repository, make a final notice commit containing:
   - “Development moved to LegeApp/Lege”;
   - direct source/docs/issues links;
   - the last standalone commit and corresponding monorepo provenance;
   - crates.io migration instructions;
   - a statement that tags/releases/history remain available.
6. Update repository description/homepage, close or transfer actionable issues,
   disable Actions/secrets, and archive each old repository.
7. Announce the cutover, especially any history rewrite and the loss of moving
   Git dependencies at old URLs.

Rollback: keep the old repositories archived but untouched for at least one
release cycle, preserve all pre-cutover ref tips, and do not delete releases.
If the monorepo cutover fails, revert repository visibility/protection and fix
the staging candidate; do not restart the subtree mirrors unless D3 is formally
reconsidered.

## Release checklist

- [x] D1 combined-work license selected and recorded (AGPL-3.0-only).
- [x] D2 public-history strategy selected: force-replace public main from the audited local monorepo.
- [ ] Full-history secret and large-blob audit passes.
- [ ] Runtime model allowlist and checksums pass.
- [ ] Development output/corpora/scratch removed from the public tree.
- [x] Direct DJVULibRust API path passes functional and cancellation tests.
- [x] No Lege runtime dependency on `djvu-encoder` helper remains.
- [ ] All three codec crates build/test/package independently from the monorepo.
- [ ] Root and per-crate license/NOTICE files match manifest declarations.
- [ ] Asset bootstrap works from a clean clone and fails closed on corruption.
- [ ] TurboOCR source revision is pinned and contains every required local fix.
- [ ] Public Linux and Windows clean-clone smoke tests pass.
- [ ] Legacy repository notices are committed before repositories are archived.
- [ ] Mirror workflows and credentials are removed/rotated.
- [ ] AKR validation and generated project views are clean at the cutover commit.

## Suggested commit sequence

Keep review boundaries narrow:

1. `chore(repo): remove generated development artifacts and harden ignores`
2. `chore(licenses): define monorepo and codec license boundaries`
3. `feat(assets): add checksum-pinned OCR model bootstrap`
4. `refactor(djvu): restore in-process DJVULibRust integration`
5. `ci(codecs): test and package all in-tree codec crates`
6. `docs(repo): declare monorepo ownership and legacy migration`
7. one separately reviewed publication-history/cutover operation

Do not combine the history rewrite, license change, and DjVu code change in one
commit; each needs an independently auditable review and rollback point.
