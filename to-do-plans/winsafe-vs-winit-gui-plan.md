# Winsafe vs. winit for the Lege viewer shell — assessment and plan

*Written 2026-08-05, after the input-freeze / resize-jank debugging session on `lege-viewer`
(`lege-gui`). Companion to the fixes landed that day.*

## 1. Question being answered

Should the native viewer GUI move from its current stack (winit event loop + fully
homegrown chrome) to [winsafe](https://github.com/rodrigocfd/winsafe) (safe, idiomatic
Win32 bindings with native controls)? Secondary questions:

- Is the `GDIP*/GDI+` window that hangs system shutdown tied to the current stack?
- Is a winit-vs-winsafe split *inside one GUI codebase* feasible, given winsafe is
  Windows-only?

## 2. What the current stack actually consists of

| Layer | Provided by | Notes |
|---|---|---|
| Window, event loop, DPI, IME | `winit` 0.30 | `ApplicationHandler` in `app.rs` |
| Presentation | `lege-gpu::presentation` (wgpu/DX12) with `softbuffer` (GDI) fallback | atlas compositor, 512 px image slots |
| All widgets (toolbar, panel, dropdowns, status bar, outline) | homegrown | pixels drawn via `cosmic-text` into `SceneSurface`s; hit-testing by hand-maintained rectangles in `app.rs` |
| File dialogs | vendored `rfd` (IFileDialog, modal, blocks the loop) | |
| Clipboard | `copypasta` | |
| Document canvas | homegrown (tiles, previews, selection, links) | this part is custom under *any* toolkit |

Two important observations from the code:

1. **~80 % of `lege-viewer` is shell-agnostic.** `document/*`, `scroll/*`, `text/*`,
   `scene.rs`, `paint.rs`, `processing.rs`, the presenters, and the layout math have no
   winit types in their interfaces beyond geometry structs. The winit coupling is
   concentrated in `app.rs` (~5.5 k lines) and `main.rs`.
2. **The chrome is not DPI-scaled.** Toolbar buttons are laid out in raw pixel constants
   (`OPEN_GROUP_X = 0..64` etc.) while the toolbar *bar* height multiplies by
   `scale_factor`. At 150 % the buttons render visibly undersized. Any "Windows look and
   feel" effort has to fix this regardless of toolkit.

## 3. Evidence from the 2026-08-05 session — which layer do the bugs live in?

| Bug | Root cause | Layer |
|---|---|---|
| All buttons dead after pressing Process | Processing panel surface (540 px) exceeded the GPU atlas's 512 px slot → present error → fallback/exit; plus a redraw-scheduler flag that wedged after a dropped RedrawRequested | **our compositor + our scheduler**, not winit |
| Resize black gutters / accordion | frame presented for the old size while the modal resize loop runs | winit-adjacent (fixed by presenting synchronously inside `Resized`) |
| Synthetic clicks landed 1.5× off; window sometimes refuses activation | process not per-monitor-DPI-aware at the OS level → Windows virtualizes coordinates, winit rescales | **DPI/awareness configuration** — winit is supposed to opt in at runtime; on this machine it demonstrably ran virtualized (posted client coords arrived ×1.5) |
| Tiny chrome on 150 % displays | homegrown chrome ignores scale factor | ours |

Honest conclusion: **winsafe would not have prevented the two worst bugs** (they were in
our own compositor/scheduler). What winsafe *would* have changed is the debugging
experience — with a plain Win32 wndproc you see exactly which messages arrive with which
coordinates, there is no hidden rescaling layer, and DPI awareness is declared in a
manifest you control instead of a runtime call another crate makes on your behalf.

## 4. What winsafe is (state as of v0.0.28)

- Safe, idiomatic hand-written Win32 bindings: ~914 functions, 98 COM interfaces,
  684 window messages; modular features (`kernel`, `user`, `gdi`, `ole`, `shell`, …). MIT.
- A small GUI layer: `WindowMain` / `WindowModal` / `WindowControl`, native controls
  (button, edit, combo, list view, tree view, status bar, tab, trackbar…), events as Rust
  closures, DPI helpers (`gui::dpi`).
- Explicitly incomplete ("Win32 is gigantic"), version still 0.0.x, effectively a
  single-maintainer project (active: ~2 k commits). Anything missing can be reached by
  dropping to `windows-sys`/raw FFI alongside it — they coexist fine.
- No rendering abstraction beyond GDI wrappers and no GPU story: the document canvas
  would still be our wgpu/softbuffer compositor hosted in a child `HWND` we create.

## 5. Pros of a winsafe shell (Windows)

1. **Real Windows look and feel for the chrome.** Native toolbar/rebar or plain themed
   buttons, native menus and accelerators, native status bar, native tooltips — visual
   consistency and per-monitor DPI scaling handled by the OS control library instead of
   the ~1,500 lines of hand-painted button/dropdown code in `app.rs`.
2. **Native dialogs stay native and stop blocking the pump incorrectly.** `IFileDialog`
   driven from our own message loop, with correct owner/disable semantics.
3. **Accessibility and IME for free** on the chrome (UIA exposure of native controls;
   our painted buttons are invisible to screen readers today).
4. **Deterministic input and DPI.** Manifest-declared PerMonitorV2, physical coordinates
   end to end, no virtualization layer to reverse-engineer. Today's "clicks land at
   1.5×" class of confusion becomes impossible.
5. **Deterministic teardown.** We own `WM_DESTROY`/`PostQuitMessage` ordering, can
   explicitly `GdiplusShutdown`, unregister classes, and join worker threads — the right
   footing for chasing the shutdown hang (see §7).
6. **Less code we must maintain.** The chrome (toolbar, dropdowns, tabs, status bar,
   appearance popup) shrinks dramatically; hit-testing rectangles disappear.
7. **Smaller dependency surface on Windows.** winit + softbuffer + copypasta + parts of
   rfd overlap with what winsafe/Win32 gives directly (clipboard is ~20 lines of Win32).

## 6. Cons and risks

1. **Windows-only — the big one.** The workspace ships Linux packages (`cargo deb`,
   Tesseract path, Vulkan backend). A winsafe-only viewer abandons that, so realistically
   we keep a second shell (see §8), which is *two* shells to maintain.
2. **Rewrite cost is concentrated but real.** `app.rs` must be split into
   shell-independent state + per-shell drivers. Estimate: the split refactor ~1–2 weeks;
   the winsafe shell (window, input translation, menus, toolbar, panel as a modeless
   dialog or control window, status bar) another ~2–3 weeks including DPI/theming polish;
   plus a long tail (dark mode for native controls is genuinely fiddly on Win32 —
   undocumented `uxtheme` ordinals — and our five color themes don't map onto native
   controls at all; we'd likely keep owner-drawn chrome for theming or accept
   light/dark only).
3. **Pre-1.0, one maintainer.** API churn between 0.0.x releases; coverage gaps mean
   some `unsafe`/`windows-sys` escape hatches anyway. Lower bus-factor than
   winit's org-maintained ecosystem.
4. **The custom canvas remains custom.** Page rendering, tiles, selection, link hover,
   search overlays — none of that gets simpler; it just gets re-hosted in a child HWND.
   The recent bugs lived mostly *there*, so the stability win for the worst bug class is
   modest.
5. **Two input paths to keep semantically identical** (keyboard shortcuts, wheel
   behavior, selection drag) if we keep the winit shell for Linux/macOS.

## 7. The GDI+ / shutdown-hang question

The hidden top-level window named "GDI+ Window (…)" is created by `GdiplusStartup` — a
background thread GDI+ spins up for its notification hook. If the process never calls
`GdiplusShutdown` (or a thread holding GDI+ objects is still alive at logoff), that
window can stall shutdown with the classic "GDI+ Window: this app is preventing
shutdown" dialog.

Notable: **nothing in the viewer's Rust stack initializes GDI+ on purpose.** winit,
softbuffer (plain GDI), wgpu (DXGI), cosmic-text, copypasta — none use GDI+. Likely
sources to check before blaming the toolkit:

- the vendored `rfd`/shell dialog path (shell extensions loaded into the process —
  thumbnail handlers and third-party shell extensions frequently start GDI+ in-process;
  the Open dialog in this app loads the full shell namespace),
- `open::that` (ShellExecute pulls in shell DLLs likewise),
- WinRT OCR / WIC codecs in the *processing* binary rather than the viewer.

Suggested diagnostic (cheap, no migration needed): reproduce the hang, then in WinDbg
attach and run `!handle`/`lm` to see which module called `GdiplusStartup`
(`x gdiplus!GdiplusStartup`, `bp` on a fresh launch, or ETW `Microsoft-Windows-Win32k`).
If it is shell-extension fallout from the file dialog, winsafe would inherit the same
behavior — the fix is calling the dialog on an STA helper thread and tearing it down, or
`SetProcessShutdownParameters` + explicit `GdiplusShutdown` at exit, both doable today
without winsafe. **Conclusion: treat the shutdown hang as its own bug; do not count it
as an automatic winsafe win.**

## 8. Cross-platform: is a winit/winsafe split inside one GUI possible?

Feature-by-feature *within one window* — no. Two toolkits cannot share one window's
event loop; you cannot compile "this button is winsafe, that panel is winit."

But a **whole-shell split by target** is feasible and is the standard pattern, because
the boundary already nearly exists in this codebase:

```
lege-viewer
├── core (compiled everywhere)
│   document/*  scroll/*  text/*  scene.rs  paint.rs  processing.rs
│   viewer_state.rs   ← extracted from app.rs: zoom/scroll/selection/search/
│                        processing state + pure hit-test math + compose()
│   present/ (wgpu + softbuffer against raw-window-handle)
└── shell (one per target, selected by #[cfg(target_os)] or cargo feature)
    shell_winit.rs    ← today's app.rs event plumbing (Linux/macOS, and Windows fallback)
    shell_winsafe.rs  ← Windows: native menus/toolbar/status bar/dialogs,
                        child HWND hosting the canvas + present layer
```

- Selection is `#[cfg(target_os = "windows")]` (optionally overridable by a
  `--features winit-shell` escape hatch for debugging).
- The canvas hosts identically in both: wgpu and softbuffer only need
  `raw-window-handle`, which a winsafe-created child `HWND` satisfies via a ~30-line
  adapter.
- The processing panel is the best first candidate to go native (it is a form: tabs,
  dropdowns, checkboxes, buttons — exactly what winsafe's control set covers), while the
  toolbar/canvas stay custom initially.

So: *not* per-feature interleaving, but a per-target shell split with a shared core is
realistic. The prerequisite refactor (extracting `viewer_state` from `app.rs`) is
valuable even if winsafe is never adopted — it is also what makes the GUI testable
headlessly.

## 9. Alternatives to a full migration

1. **Stay on winit, fix the Windows papercuts directly** (cheapest):
   - embed a PerMonitorV2 manifest (`embed-manifest` crate) so DPI awareness never
     depends on runtime calls — this removes the coordinate-virtualization class of bugs;
   - scale the chrome by `scale_factor` (buttons currently render at fixed px);
   - keep today's fixes (atlas slicing, ungated redraw, synchronous resize present);
   - run file dialogs on a helper thread; add explicit `GdiplusShutdown`-style teardown.
2. **Hybrid via raw `windows-sys` without winsafe:** add native menu bar + status bar to
   the existing winit HWND (winit exposes it). Gets some native feel without a second
   shell; fragile for anything beyond menus.
3. **Full winsafe shell** per §8.

## 10. Recommendation

- **Short term (now):** Option 1. The five bugs fixed today were in our own layers; the
  remaining winit-specific risk shrinks a lot with a DPI manifest and scaled chrome.
  Separately, run the WinDbg/ETW diagnostic for the GDI+ shutdown hang — it is most
  likely shell-dialog fallout that would survive a toolkit change.
- **Medium term (if Windows-native polish is a product goal):** do the `viewer_state`
  extraction, then build the winsafe shell starting with the processing panel and
  menus/status bar, keeping the winit shell compiled for Linux/macOS. Budget ~4–6 weeks
  end to end, dominated by the `app.rs` split and dark-theme handling of native controls.
- **Not recommended:** replacing winit wholesale and dropping the Linux viewer, or
  attempting to mix the two toolkits inside one window.

## Appendix: session facts worth keeping

- GPU atlas images are capped at 512 px per side (`lege-gpu::presentation::MAX_IMAGE_EXTENT`);
  the viewer now slices oversized chrome surfaces (`present/wgpu.rs::push_image_split`).
- `FrameScheduler::request_redraw` no longer gates `window.request_redraw()` — a dropped
  OS paint message previously wedged all repaints forever.
- `WindowEvent::Resized` now composes+presents synchronously (kills resize black-space).
- `LEGE_INPUT_TRACE=1` prints every mouse press with position, resolved toolbar action,
  and scale factor — leave this in; it is how the DPI virtualization was caught.
- On this machine (144 DPI / 150 %), `PostMessage`-injected clicks must use logical
  (pre-scale) client coordinates; real input arrives correctly via winit's rescaling.
