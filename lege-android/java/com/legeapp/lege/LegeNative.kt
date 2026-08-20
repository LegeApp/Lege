package com.legeapp.lege

/**
 * Native entry points into the Lege pipeline.
 *
 * These declarations must match `lege-android/src/api.rs` exactly. JNI symbol
 * names encode the package, so moving this class to a different package means
 * renaming the `Java_…` symbols in `api.rs` and `JAVA_PACKAGE_PATH` in
 * `bridge.rs` to match. Nothing detects a mismatch until the host hits an
 * `UnsatisfiedLinkError` at runtime.
 */
object LegeNative {
    init {
        System.loadLibrary("lege_android")
    }

    /**
     * Install the runtime environment. Call once, before any job.
     *
     * Pass [android.app.ActivityManager.getLargeMemoryClass] as
     * [memoryBudgetMb] (or `memoryClass` when the manifest does not request
     * `largeHeap`); 0 falls back to a conservative share of total device RAM.
     * Do not pass total device RAM — the pipeline sizes its worker pool from
     * this number and will schedule more concurrent pages than the app is
     * allowed to hold.
     *
     * @throws IllegalStateException if the environment cannot be established.
     */
    @JvmStatic
    external fun nativeInit(filesDir: String, cacheDir: String, memoryBudgetMb: Int)

    /**
     * Queue a document. Returns the task id for [nativePollProgress] and
     * [nativeCancel].
     *
     * Returns immediately; processing runs on the pipeline's own threads.
     *
     * @throws IllegalStateException if [nativeInit] has not run, the paths are
     *   unreadable, or [params] is not a valid configuration.
     */
    @JvmStatic
    external fun nativeStartJob(inputPath: String, outputPath: String, params: LegeParams): Long

    /**
     * Wait up to [timeoutMs] for the next update.
     *
     * Returns null when the interval passed with no news — an ordinary result,
     * not an error. Keep polling until a [LegeProgress.KIND_COMPLETED] or
     * [LegeProgress.KIND_ERROR] arrives.
     *
     * Blocks the calling thread, so call it off the main thread.
     */
    @JvmStatic
    external fun nativePollProgress(taskId: Long, timeoutMs: Long): LegeProgress?

    /**
     * Request cancellation. True when a running task was signalled or a queued
     * one was dropped; false when the id is unknown or already finished.
     *
     * Cancellation is cooperative — expect a short delay before the terminal
     * update arrives.
     */
    @JvmStatic
    external fun nativeCancel(taskId: Long): Boolean
}
