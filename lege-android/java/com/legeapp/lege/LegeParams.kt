package com.legeapp.lege

/**
 * Job configuration, read field-by-field by `lege-android/src/config.rs`.
 *
 * Every property is `@JvmField` on purpose: the native side looks these up by
 * name with `GetFieldID`, so they must be real fields under exactly these
 * names rather than Kotlin properties behind accessors.
 *
 * This is a deliberate subset of the pipeline's ~45 settings — the rest keep
 * their defaults. Adding one means a field here and a line in `config.rs`.
 */
class LegeParams {
    /** Output page height in pixels. 0 keeps the pipeline default. */
    @JvmField
    var targetHeight: Int = 0

    /** `"jbig2"`, `"ccitt4"`, …; null keeps the default. */
    @JvmField
    var textFormat: String? = null

    /**
     * Run YOLO layout detection. Requires a working Vulkan device; if the GPU
     * is unusable the pipeline logs a warning, turns this off for the run, and
     * continues rather than failing.
     */
    @JvmField
    var enableLayoutDetection: Boolean = true

    /** Add an OCR text layer using the embedded PP-OCR models. */
    @JvmField
    var enableOcr: Boolean = false

    /** OCR language code; null keeps the default. */
    @JvmField
    var ocrLanguage: String? = null

    @JvmField
    var highQualityOutput: Boolean = false

    @JvmField
    var enableCoverPage: Boolean = true

    /** Treat the source as inverted (light text on dark). */
    @JvmField
    var invertInput: Boolean = false

    /** `"5"` or `"1-20"`, one-based inclusive. Null processes everything. */
    @JvmField
    var pageRange: String? = null

    /**
     * Pages in flight. 0 lets the pipeline size the pool from the device's
     * core count and the memory budget given to [LegeNative.nativeInit], which
     * is almost always the better answer than a fixed value.
     */
    @JvmField
    var maxParallelPages: Int = 0
}
