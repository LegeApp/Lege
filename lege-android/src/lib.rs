//! JNI bindings exposing the Lege pipeline to an Android host app.
//!
//! # Java surface
//!
//! JNI symbol names encode the Java package, so the host's class **must** be
//! `com.legeapp.lege.LegeNative`. To use a different package, change
//! [`JAVA_PACKAGE_PATH`] in [`bridge`] and the `Java_…` symbol names in
//! [`api`] to match — those are the only two places the package appears.
//!
//! ```java
//! package com.legeapp.lege;
//!
//! public final class LegeNative {
//!     static { System.loadLibrary("lege_android"); }
//!
//!     public static native void nativeInit(
//!         String filesDir, String cacheDir, int memoryBudgetMb);
//!     public static native long nativeStartJob(
//!         String inputPath, String outputPath, LegeParams params);
//!     public static native LegeProgress nativePollProgress(
//!         long taskId, long timeoutMs);
//!     public static native boolean nativeCancel(long taskId);
//! }
//! ```
//!
//! # Why polling rather than a callback
//!
//! Progress is pulled by the host via `nativePollProgress`, not pushed from a
//! Rust-spawned thread. `FindClass` on a thread the JVM did not create resolves
//! against the *system* ClassLoader and cannot see application classes, so a
//! push design needs a cached ClassLoader or cached global class references and
//! an `AttachCurrentThread` on every emit. Polling sidesteps all of it: object
//! construction always happens on the host's own thread.
//!
//! It also matches how Lege already works. The desktop CLI does exactly this —
//! `receiver.recv_timeout(100ms)` in a loop, filtering by task id — so both
//! front ends drive the pipeline the same way. On the Kotlin side this is a
//! `flow { while (active) emit(poll()) }` on `Dispatchers.IO`.

// Off Android this crate is deliberately empty: `lege`'s `android` feature is
// rejected on desktop targets, and this crate exists only to produce the APK's
// native library. Keeping it buildable (as a no-op) means `cargo check
// --workspace` still succeeds on a developer's desktop.
#[cfg(target_os = "android")]
mod api;
#[cfg(target_os = "android")]
mod bridge;
#[cfg(target_os = "android")]
mod config;
#[cfg(target_os = "android")]
mod progress;
