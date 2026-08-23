package com.legeapp.lege

/**
 * Job configuration, read field-by-field by `lege-android/src/config.rs`.
 *
 * Every property is `@JvmField` on purpose: the native side looks these up by
 * name with `GetFieldID`, so they must be real fields under exactly these
 * names rather than Kotlin properties behind accessors.
 *
 * This exposes the Android-safe processing controls. Worker and buffer sizing
 * intentionally stay automatic: native code derives them from the app memory
 * class supplied to `LegeNative.nativeInit`.
 */
class LegeParams {
    /** Output page height in pixels. 0 keeps the pipeline default. */
    @JvmField
    var targetHeight: Int = 0

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

    @JvmField
    var highQualityOutput: Boolean = false

    /** Treat the source as inverted (light text on dark). */
    @JvmField
    var invertInput: Boolean = false

    /** `"5"` or `"1-20"`, one-based inclusive. Null processes everything. */
    @JvmField
    var pageRange: String? = null

    /** `"none"`, `"center"`, `"crop"`, or `"reflow"`. */
    @JvmField
    var marginMode: String = "none"

    @JvmField
    var slowOcr: Boolean = false

    @JvmField
    var jpegCompat: Boolean = false

    /** Dither image regions instead of retaining their original raster crops. */
    @JvmField
    var ditherImages: Boolean = false

    /** `"default"`, `"adaptive"`, `"threshold"`, or `"sauvola"`. */
    @JvmField
    var binarizationMode: String = "default"

    /** Adaptive Sauvola sensitivity; valid range is 0.0–1.0. */
    @JvmField
    var sauvolaK: Float = 0.05f

    /** Fixed global threshold; valid range is 0–255. */
    @JvmField
    var fixedThreshold: Int = 180

    /** Filesystem destination for the optional EPUB companion file. */
    @JvmField
    var epubSidecarPath: String? = null
}
