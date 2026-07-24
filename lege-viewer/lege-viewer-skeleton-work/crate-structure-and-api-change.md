**Yes, with an important distinction:** I placed `lege-viewer` as a **sibling application crate in the renderer’s Cargo workspace**:

```text
workspace/
└── crates/
    ├── lege-viewer/
    ├── pdf-content/
    ├── pdf-document/
    ├── pdf-render-api/
    ├── pdf-render-cpu/
    ├── pdf-text/
    └── ...
```

It is **not embedded inside one renderer crate**, and renderer crates do not depend on the viewer. The dependency direction is:

```text
lege-viewer
    ↓
pdf-document / pdf-content / pdf-text / pdf-render-api / pdf-render-cpu
```

I think that is the right initial organization because the architecture calls for joint evolution of the viewer and document engine. Keeping them in one workspace gives you direct internal APIs, shared types, coordinated refactors, and no premature versioning boundary.

### Renderer changes I made

I made two targeted API changes:

1. **One-pass semantic and IR compilation**

Added:

```rust
PageCompiler::compile_artifacts(...)
    -> PageCompilation {
        semantic: Arc<SemanticPage>,
        compiled: Arc<CompiledPage>,
    }
```

Previously, the viewer would have needed to compile once for the render IR and potentially interpret again to obtain text geometry and semantic structure. The new method retains both products from one interpretation pass.

This is the central integration change. It allows the viewer to receive:

```text
SemanticPage
├── text geometry
├── font/run information
├── future link and structure extraction
└── line clustering

CompiledPage
├── tile rasterization
├── zoom rerasterization
├── future text-first filtering
├── future IR recoloring
└── future exact content extents
```

2. **Shared cancellation state**

Added to `pdf-render-api::CancellationToken`:

```rust
CancellationToken::from_shared(...)
CancellationToken::shared_flag()
```

That lets the viewer conductor and CPU renderer observe the same atomic cancellation state. A tile can become irrelevant because the viewport moved, and the renderer can stop at its existing operation boundaries without an adapter thread or duplicated cancellation state.

### What I did not change

I did **not** substantially rewrite the renderer or its raster kernels.

In particular, these are architecturally represented but still unimplemented:

* Text-first display-list filtering.
* Image/shading second-pass upgrades.
* Batched tile-run rendering.
* Exact display-list content extents.
* IR-level night-mode color transformation.
* Link and destination extraction.
* Shared budgeting across viewer tiles and renderer image/font caches.

The current real-PDF bridge deliberately returns `TextFirstUnsupported`, then schedules draft and final tiles through the renderer’s existing API.

One caveat: the supplied renderer archive omitted its original root `Cargo.toml` and external codec repositories, so I reconstructed a workspace manifest. That reconstructed manifest should not blindly replace the real repository manifest. The code was source-checked, but I could not run `cargo check` in that environment.
