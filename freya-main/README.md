# Freya (LegeApp fork)

This repository is a **fork of [marc2332/freya](https://github.com/marc2332/freya)** with a small set of changes so **[Lege](https://github.com/LegeApp)** can ship layout and overlay UI (modals, richer tooltips) reliably on top of the same Freya stack.

Upstream docs, community, and project history live on the [original Freya README](https://github.com/marc2332/freya/blob/main/README.md) and [freyaui.dev](https://freyaui.dev/).

## Use this fork from Cargo

```toml
freya = { git = "https://github.com/LegeApp/freya.git", branch = "main" }
```

Match `default-features` and `features` to your app (for example `skia-engine`, `markdown`) as you would with upstream.

---

## What differs from upstream

### 1. `LegeModal` (`freya::components::LegeModal`)

A **modal dialog** component oriented toward app chrome and layout-heavy content:

- **Overlay layer** — `Layer::Overlay` and global positioning so the dialog sits above the main UI.
- **Dimmed backdrop** — Uses existing `PopupBackground`; backdrop opacity is animated.
- **Show / hide** — Driven by a `Readable<bool>` (`.show(...)`); open/close animations (scale + opacity + backdrop) stay in sync with visibility.
- **Structure** — Title bar (title + compact **Close** button), then a **scrollable-style content region** (`rect().expanded().padding(12.)`) for arbitrary `Element` content.
- **Dismissal** — Optional `.on_close_request(...)`; **Escape** closes by default (`close_on_escape_key` is wired internally).
- **Sizing** — `.width`, `.height`, `.min_width`, `.min_height` (default min size ~520×360 px, body ~50%×50% of parent).
- **Accessibility** — Dialog role on the panel.

**Example shape:**

```rust
use freya::prelude::*;

LegeModal::new("Title", your_content_element.into())
    .show(show_flag)
    .on_close_request(move |_| { /* set show to false */ })
    .width(Size::percent(60.))
    .height(Size::percent(70.))
```

Re-exported from `freya::components` like other built-ins (`lege_modal::*` in the components module tree).

### 2. Tooltip layout (`Tooltip`)

Tooltips are adjusted for **longer, multi-line help text** and a stable footprint:

| Upstream (typical) | This fork |
|--------------------|-----------|
| Single line (`max_lines(1)`) | Up to **5 lines** |
| Padding `(4, 10)` | **`(5, 8)`** |
| No fixed width | **Fixed width `236px`** so wrapped text layout is predictable |

Useful when tooltips carry short paragraphs instead of one-word hints.

### 3. Runner event drain (`freya-core`)

`Runner::handle_events` and `handle_events_immediately` drain the internal channel with **`try_next()`** and `Ok(Some(msg))` instead of **`try_recv()`**. That keeps the event loop aligned with the **futures / stream-style** channel API used in this tree, avoiding subtle missed or incompatible receives when driving the GUI.

---

## Files touched (reference)

| Area | Path |
|------|------|
| New component | `crates/freya-components/src/lege_modal.rs` |
| Export | `crates/freya-components/src/lib.rs` |
| Re-export in `freya` crate | `crates/freya/src/lib.rs` |
| Tooltip | `crates/freya-components/src/tooltip.rs` |
| Event loop | `crates/freya-core/src/runner.rs` |

---

## License

Same as upstream: **MIT** — see `LICENSE.md`.
