//! JNI entry points.
//!
//! Symbol names encode `com.legeapp.lege.LegeNative`; see the crate docs for
//! how to move the class to another package.
//!
//! Every entry point is wrapped in [`catch_unwind`]. A panic crossing an FFI
//! boundary is undefined behaviour, and panics are genuinely reachable here:
//! `lege-process` does not adopt the workspace's `panic = "deny"` lints, and
//! the pipeline contains reachable `expect` calls (`progress.rs`'s poisoned
//! cancel registry, for one). The guards only function under `panic =
//! "unwind"`, which both `release` and the `android` profile set — the latter
//! restates it so this crate never silently loses it.

// Rust 2024 spells exported-symbol attributes `#[unsafe(no_mangle)]`, which
// trips the workspace's `unsafe_code` lint. Exporting these symbols under
// exactly these names is the entire purpose of the crate, so the lint has
// nothing useful to say here.
#![allow(unsafe_code)]

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::time::Duration;

use jni::JNIEnv;
use jni::objects::{JClass, JObject, JString};
use jni::sys::{jboolean, jint, jlong, jobject};

use crate::bridge;
use crate::config;
use crate::progress;

/// Run `body`, converting a panic into a Java exception.
///
/// `AssertUnwindSafe` is required because `JNIEnv` is not `UnwindSafe`. That is
/// sound here: on the panic path we only throw and return, never observing
/// state the unwound code was midway through mutating.
fn guard<T>(
    env: &mut JNIEnv<'_>,
    what: &str,
    fallback: T,
    body: impl FnOnce(&mut JNIEnv<'_>) -> T,
) -> T {
    match catch_unwind(AssertUnwindSafe(|| body(env))) {
        Ok(value) => value,
        Err(payload) => {
            let detail = payload
                .downcast_ref::<&str>()
                .map(|s| (*s).to_owned())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic".to_owned());
            log::error!("lege: panic in {what}: {detail}");
            bridge::throw(env, &format!("lege panicked in {what}: {detail}"));
            fallback
        }
    }
}

fn read_string(env: &mut JNIEnv<'_>, value: &JString<'_>, what: &str) -> anyhow::Result<String> {
    if value.is_null() {
        anyhow::bail!("{what} must not be null");
    }
    Ok(env.get_string(value)?.into())
}

/// Install the host's runtime environment. Must be called once before any job.
///
/// `memoryBudgetMb` should be `ActivityManager.getLargeMemoryClass()` (or
/// `getMemoryClass()` without `largeHeap`); pass 0 to fall back to a
/// conservative share of total device RAM. Total RAM is deliberately not the
/// default — an app is entitled to its heap class, not the whole device, and
/// oversizing it makes the pipeline schedule work the device cannot hold.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_legeapp_lege_LegeNative_nativeInit(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    files_dir: JString<'_>,
    cache_dir: JString<'_>,
    memory_budget_mb: jint,
) {
    guard(&mut env, "nativeInit", (), |env| {
        let result = (|| -> anyhow::Result<()> {
            let files_dir = read_string(env, &files_dir, "filesDir")?;
            let cache_dir = read_string(env, &cache_dir, "cacheDir")?;

            lege::android::logging::init();

            let android_env = lege::android::AndroidEnv {
                files_dir: PathBuf::from(files_dir),
                cache_dir: PathBuf::from(cache_dir),
                memory_budget_mb: u32::try_from(memory_budget_mb).ok().filter(|mb| *mb > 0),
            };

            // A second call is not an error: hosts re-run init across activity
            // restarts. The first environment stays authoritative, and `init`
            // logs if a later call asked for something different.
            lege::android::init(android_env);

            // Sizes the global rayon pool. `Once`-guarded upstream, so calling
            // it again is harmless.
            lege::configure_runtime_env();

            // Deliberately NOT install_termination_handler(): it registers
            // POSIX SIGINT/SIGTERM handlers, which is wrong inside a JVM.
            Ok(())
        })();

        if let Err(error) = result {
            bridge::throw(env, &format!("lege init failed: {error:#}"));
        }
    })
}

/// Queue a document for processing. Returns the task id used by the other
/// entry points, or 0 after throwing.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_legeapp_lege_LegeNative_nativeStartJob(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    input_path: JString<'_>,
    output_path: JString<'_>,
    params: JObject<'_>,
) -> jlong {
    guard(&mut env, "nativeStartJob", 0, |env| {
        let result = (|| -> anyhow::Result<u64> {
            if lege::android::env().is_none() {
                anyhow::bail!("nativeInit has not been called");
            }

            let input = read_string(env, &input_path, "inputPath")?;
            let output = read_string(env, &output_path, "outputPath")?;
            let config = config::from_java(env, &params)?;

            // Set the subscription up here rather than inside the first poll,
            // so one-time initialisation is not interleaved with a job that is
            // already running.
            progress::prime();

            Ok(lege::progress::spawn_file_processing_task(
                PathBuf::from(input),
                PathBuf::from(output),
                config,
            ))
        })();

        match result {
            Ok(task_id) => task_id as jlong,
            Err(error) => {
                bridge::throw(env, &format!("lege could not start the job: {error:#}"));
                0
            }
        }
    })
}

/// Wait up to `timeoutMs` for the next update for `taskId`.
///
/// Returns a `LegeProgress`, or null when the interval passed with no news —
/// null is an ordinary outcome, not an error. Poll in a loop until a
/// `COMPLETED` or `ERROR` kind arrives.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_legeapp_lege_LegeNative_nativePollProgress(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    task_id: jlong,
    timeout_ms: jlong,
) -> jobject {
    guard(
        &mut env,
        "nativePollProgress",
        JObject::null().into_raw(),
        |env| {
            let task_id = task_id.max(0) as u64;
            let timeout = Duration::from_millis(timeout_ms.max(0) as u64);

            let Some(update) = progress::poll(task_id, timeout) else {
                return JObject::null().into_raw();
            };

            if progress::is_terminal(&update) {
                progress::forget(task_id);
            }

            match bridge::new_progress(env, &update) {
                Ok(object) => object.into_raw(),
                Err(error) => {
                    bridge::throw(env, &format!("lege could not build LegeProgress: {error}"));
                    JObject::null().into_raw()
                }
            }
        },
    )
}

/// Request cancellation. True when a running task was signalled or a queued
/// one was removed; false when the id is unknown or already finished.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_legeapp_lege_LegeNative_nativeCancel(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    task_id: jlong,
) -> jboolean {
    guard(&mut env, "nativeCancel", jboolean::from(false), |_env| {
        let task_id = task_id.max(0) as u64;
        jboolean::from(lege::progress::cancel_task(task_id))
    })
}
