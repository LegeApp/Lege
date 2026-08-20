# Android APK — handoff for finishing on Linux

The Rust side of the Android port is committed and cross-compiles. What remains
needs an NDK, which lives on the Linux side of this dual-boot box. This picks up
from there.

Reference commit: `7ea7f11 feat(android): feature-gated Android port with a JNI cdylib`.

---

## State

**Done and verified (on Windows, `cargo check` only):**

| Check | Result |
|---|---|
| `cargo check -p lege` (desktop, default features) | pass |
| `cargo test -p lege` (desktop) | pass |
| `cargo check --workspace` (desktop) | pass |
| `cargo check -p lege --target aarch64-linux-android --features android` | pass |
| `cargo check -p lege-android --target aarch64-linux-android` | pass, no warnings |
| `cargo clippy -p lege-android --no-deps` | clean under the workspace deny lints |
| Android target *without* `--features android` | fails with one `compile_error!`, as designed |
| `android` feature on a desktop target | fails with the converse `compile_error!`, as designed |

**Not done — everything below needs the NDK:**

- Linking. **Nothing here has ever been through a linker.** `cargo check` stops
  before that, so undefined symbols, missing runtime libs, and `cdylib`
  export problems are all still unproven. Treat the first `cargo ndk build` as
  a real step that can fail, not a formality.
- On-device run: layout detection, PP-OCR, and a full PDF through the pipeline.
- Numerics comparison between the wgpu path and the CPU reference on real
  Adreno/Mali drivers.

---

## Prerequisites on Linux

```bash
rustup target add aarch64-linux-android
cargo install cargo-ndk
```

Toolchain needs Rust **1.97+** (`rust-version` in the workspace root). Verified
against 1.97.1 and cargo-ndk 4.1.2.

Point cargo-ndk at whichever NDK Android Studio has:

```bash
export ANDROID_NDK_HOME="$HOME/Android/Sdk/ndk/<version>"
```

**Any NDK from r23 onward is fine.** The old `-lgcc` breakage (NDK r23 dropped
`libgcc.a` while Rust still linked it) was fixed in Rust 1.68, and this
workspace requires 1.97, so it cannot bite. r27+ additionally gives 16 KB
page-size alignment — irrelevant for sideloading onto E-Ink readers, and only
needed for Play Store submission.

Add the emulator target too if you want to test without hardware — the port is
not aarch64-specific, and `x86_64-linux-android` is still `target_os = "android"`,
so every feature gate and the blake3 `pure` pin apply there as well:

```bash
rustup target add x86_64-linux-android
```

---

## Build

```bash
cargo ndk -t arm64-v8a -p 26 -o <android-project>/app/src/main/jniLibs build --package lege-android --profile android
```

Or via the alias already in `.cargo/config.toml` (add `-o` yourself):

```bash
cargo android-build
```

Three flags that are not cosmetic:

- **`--profile android`** — the workspace `release` profile sets
  `panic = "abort"`, which would make every `catch_unwind` guard at the JNI
  boundary inert and turn any panic into a whole-app abort. The `android`
  profile restores `panic = "unwind"`. It also uses thin rather than fat LTO;
  the cdylib carries ~23 MB of embedded ONNX and fat LTO on it is punishing.
- **`-p 26`** — minSdk for the native build. Devices need Android 9+ in
  practice for dependable Vulkan 1.1 compute; Boox/Kobo readers run 10–13, well
  inside that.
- **`-t arm64-v8a`** — add `-t x86_64` for an emulator build.

Output is `liblege_android.so`.

### If linking fails

Most likely causes, roughly in order:

1. A build script reaching for a C compiler. Only blake3 did this
   (`lege-ocr/Cargo.toml` pins it to its `pure` feature on Android to avoid a
   NEON C kernel). If another dependency does the same, cargo-ndk normally
   supplies `CC_aarch64-linux-android` automatically — check it is exported.
2. `ANDROID_NDK_HOME` pointing at an SDK root instead of a specific version
   directory. It must be the versioned path.
3. Vulkan. `libvulkan.so` is `dlopen`ed at runtime by wgpu, not linked, so it
   should not appear as a link error — if it does, something is configured to
   link it statically and should not be.

---

## Wiring into the app

Kotlin sources are in `lege-android/java/` — see the README there. Copy them to
`app/src/main/java/com/legeapp/lege/` or point a source set at the directory.

The package name is load-bearing: JNI symbol names encode it. To use a
different package, change `JAVA_PACKAGE_PATH` in `lege-android/src/bridge.rs`
**and** every `Java_com_legeapp_lege_…` symbol in `lege-android/src/api.rs`.
Nothing detects a mismatch until runtime `UnsatisfiedLinkError`.

Call once at startup, before any job:

```kotlin
val am = getSystemService(Context.ACTIVITY_SERVICE) as ActivityManager
LegeNative.nativeInit(filesDir.absolutePath, cacheDir.absolutePath, am.largeMemoryClass)
```

Pass the memory **class**, not total device RAM. The pipeline sizes its worker
pool from that number at roughly 2 GB per concurrent CPU stage; handing it the
device total makes it schedule more in-flight pages than the app may hold, and
the low-memory killer ends the process.

Then poll — `nativePollProgress` blocks, so keep it off the main thread. Full
flow example is in `lege-android/java/README.md`.

**Input is a filesystem path, not a `content://` URI.** Anything from the
Storage Access Framework has to be copied into `filesDir`/`cacheDir` first and
the result copied back out.

---

## On-device verification

Run in this order; each step isolates a different failure class.

1. **Library loads.** Launch, confirm no `UnsatisfiedLinkError`. Then
   `adb logcat -s lege:*` — `nativeInit` installs `android_logger` and
   redirects fds 1 and 2 into logcat, so the pipeline's many `eprintln!` sites
   (≈34 in `lege-gpu/src/vision/onnx/graph.rs` alone) show up under that tag.
   Silence here means logging did not initialise.
2. **GPU adapter.** Look for `wgpu: found N adapter(s)` and
   `wgpu: selected adapter:` in logcat. Android is pinned to Vulkan
   (`lege-gpu/src/android.rs`) — GLES guarantees only 4 storage buffers per
   stage while the conv bind group needs 5, so GL would fail partway through a
   job rather than at startup.
3. **Graceful degradation.** If no adapter is usable, the run should still
   complete with layout detection disabled and a `GPU Warning` progress
   message, not crash. `initialize_inference_or_fallback` in
   `lege-process/pipeline/helper_functions.rs` handles this. Worth forcing once
   with `WGPU_ADAPTER_SKIP` to confirm the path works on device.
4. **Memory sizing.** Confirm the worker count reflects the real device budget.
   A 2 GB reader should land on 1 CPU worker. If it behaves like an 8 GB
   machine, `android::available_ram_gb` is not being reached and the old
   hardcoded fallback is in play.
5. **A full document.** Process a scanned PDF end to end. Check progress
   objects arrive, `nativeCancel` actually stops work mid-run, and the output
   PDF opens.
6. **Numerics.** Compare PicoDet and PP-OCR outputs between the wgpu path and
   `PreparedGraph::run_cpu` on the same input. The CPU reference is
   rayon-parallel and production-grade — it already backs Sauvola on desktop —
   so it is a valid on-device oracle. **Adreno/Mali driver divergence is the
   real risk in this whole port**, not wgpu itself.

---

## Things that will look like bugs but are not

- **`cargo check -p lege --no-default-features` fails.** Pre-existing, on every
  platform including desktop Windows. `epub_pipeline_disabled.rs` is missing
  `build_epub_from_hocr_pages_with_outline_cancellable`, which its enabled
  counterpart gained; two call sites break. Unrelated to Android, tracked
  separately.
- **`cargo check --workspace --target aarch64-linux-android` fails.**
  `lege-document-ocr` drives an NVIDIA/TensorRT worker binary and is
  deliberately excluded from Android (`lege-ocr/src/lib.rs` gates
  `engine_tensorrt` off the platform). Build `-p lege-android`, not the
  workspace.
- **`lege-android` builds to an empty library on desktop.** Intentional — it
  keeps `cargo check --workspace` green on a dev machine. Everything inside is
  `#[cfg(target_os = "android")]`.
- **DjVu output is unavailable.** `lege-process/core/djvu.rs` spawns a separate
  AGPL encoder binary located via `current_exe()`. Out of scope for the first
  APK; the existing preflight fails fast if selected, so just do not offer it in
  the UI. If wanted later: ship it as `libdjvu-encoder.so` in `jniLibs` and exec
  from `nativeLibraryDir`, which is exec-permitted unlike the app data dir.
- **Three icon files are uncommitted** (`lege-misc/assets/icon.png`,
  `lege-misc/assets/icon.ico`, `lege-process/GUI/icon.ico`). Held back as part
  of the same unfinished logo rebrand as the gitignored `logo-concepts/` and
  `logo-export/` directories.

---

## Where the Android code lives

Everything is behind an `android` cargo feature in `lege-process`, `lege-gpu`
and `lege-ocr`. With the feature off, no new code compiles.

```
lege-android/                  JNI cdylib (empty off-Android)
  src/api.rs                   entry points, catch_unwind guards
  src/bridge.rs                Rust values -> Java objects
  src/config.rs                LegeParams -> PipelineConfig
  src/progress.rs              demultiplexes the shared flume channel by task id
  java/                        Kotlin reference sources
lege-process/android/
  platform.rs                  data dir, memory budget, font paths
  logging.rs                   android_logger + stdio -> logcat
lege-gpu/src/android.rs        Vulkan-only backend policy
```

Shared files carry only short delegations. The one exception worth knowing
about: `build.rs` in `lege-process` now emits a `lege_paddle_ocr` cfg, replacing
`all(any(target_os = "linux", target_os = "macos"), feature = "paddle-ocr")`
which had been spelled out at 16 sites across six files. Adding a platform is a
one-line change there rather than sixteen edits.
