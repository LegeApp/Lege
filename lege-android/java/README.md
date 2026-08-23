# Kotlin side of the JNI boundary

Reference sources for the host app. Copy them into the Android module's
`src/main/java/com/legeapp/lege/`, or point a source set at this directory.
They are not compiled by Cargo — nothing here is checked against
`lege-android/src/` automatically, so the two must be kept in step by hand:

| Kotlin | Rust |
|---|---|
| `LegeNative` method signatures | `Java_com_legeapp_lege_LegeNative_*` in `src/api.rs` |
| `LegeParams` field names and types | `from_java` in `src/config.rs` |
| `LegeProgress` / `LegeMetrics` constructors | `new_progress` / `new_metrics` in `src/bridge.rs` |
| package `com.legeapp.lege` | `JAVA_PACKAGE_PATH` in `src/bridge.rs`, plus every `Java_…` symbol name |

A mismatch is not a build error on either side. It surfaces at runtime as
`UnsatisfiedLinkError` (wrong symbol name) or `NoSuchMethodError` (wrong
constructor signature).

## Wiring it up

Build the library into the APK:

```bash
cargo ndk -t arm64-v8a --platform 26 -o app/src/main/jniLibs build --package lege-android --profile android
```

The `android` profile is required, not cosmetic: the workspace's `release`
profile sets `panic = "abort"`, which would make the `catch_unwind` guards at
the JNI boundary inert and turn any panic into an app-wide abort.

## Driving a job

`nativePollProgress` blocks, so it belongs off the main thread. As a flow:

```kotlin
fun process(input: String, output: String, params: LegeParams): Flow<LegeProgress> = flow {
    val taskId = LegeNative.nativeStartJob(input, output, params)
    try {
        while (true) {
            val update = LegeNative.nativePollProgress(taskId, 250L) ?: continue
            emit(update)
            if (update.isTerminal) break
        }
    } finally {
        // Covers cancellation of the collecting coroutine. A no-op once the
        // task has already finished.
        LegeNative.nativeCancel(taskId)
    }
}.flowOn(Dispatchers.IO)
```

Call `nativeInit` once at startup, before any job:

```kotlin
val activityManager = getSystemService(Context.ACTIVITY_SERVICE) as ActivityManager
LegeNative.nativeInit(
    filesDir.absolutePath,
    cacheDir.absolutePath,
    activityManager.largeMemoryClass,
)
```

Pass the memory *class*, not total device RAM. The pipeline sizes its worker
pool from this figure — roughly 2 GB per concurrent CPU stage — so handing it
the device total makes it schedule more in-flight pages than the app is
permitted to hold, and the low-memory killer ends the process.

## Input paths

The pipeline takes filesystem paths, not `content://` URIs. Content picked
through the Storage Access Framework has to be copied into `filesDir` or
`cacheDir` first, and the result copied back out.
