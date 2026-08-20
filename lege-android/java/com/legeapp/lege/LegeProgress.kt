package com.legeapp.lege

/**
 * One progress update.
 *
 * Constructed from native code — the primary constructor's signature must stay
 * in step with `new_progress` in `lege-android/src/bridge.rs`.
 *
 * The pipeline has around eighteen internal status variants, but they are not
 * mirrored here. Their prose is already rendered into [headline]/[detail]/[hint]
 * by the same function the desktop GUI uses, and their numbers are already
 * unified into [LegeMetrics]. A new variant therefore needs no change on
 * either side of the boundary.
 */
class LegeProgress(
    /** One of [KIND_STATUS], [KIND_COMPLETED], [KIND_ERROR]. */
    @JvmField val kind: Int,
    @JvmField val taskId: Long,
    /** Short stage label, e.g. `"[Finalizing]"`. */
    @JvmField val headline: String,
    /** Main line, e.g. `"Assembling output file..."`. */
    @JvmField val detail: String,
    /** Supplementary line; may be empty. */
    @JvmField val hint: String,
    /** Numeric snapshot, or null when this update carries none. */
    @JvmField val metrics: LegeMetrics?,
) {
    val isTerminal: Boolean
        get() = kind == KIND_COMPLETED || kind == KIND_ERROR

    companion object {
        const val KIND_STATUS = 0
        const val KIND_COMPLETED = 1
        const val KIND_ERROR = 2
    }
}

/**
 * Numeric progress snapshot.
 *
 * Constructor signature must match `new_metrics` in
 * `lege-android/src/bridge.rs`.
 */
class LegeMetrics(
    @JvmField val pagesTotal: Int,
    @JvmField val rendered: Int,
    @JvmField val detected: Int,
    @JvmField val encoded: Int,
    /** One of the `MODE_*` constants. */
    @JvmField val mode: Int,
    @JvmField val isDjvu: Boolean,
    @JvmField val enableLayoutDetection: Boolean,
    /** Seconds remaining, or [NO_ETA] when not yet estimable. */
    @JvmField val etaSeconds: Int,
) {
    val eta: Int?
        get() = etaSeconds.takeIf { it != NO_ETA }

    /**
     * Fraction complete in 0..1, or null when the page count is not yet known.
     *
     * Encoding is the last stage a page passes through, so it is the honest
     * measure of finished work.
     */
    val fraction: Float?
        get() = if (pagesTotal > 0) encoded.toFloat() / pagesTotal else null

    companion object {
        /** Sentinel for "no estimate", kept primitive to avoid a boxed Integer. */
        const val NO_ETA = -1

        const val MODE_UNKNOWN = 0
        const val MODE_NO_LAYOUT = 1
        const val MODE_LAYOUT = 2
        const val MODE_MARGIN = 3
        const val MODE_HEAVY_SEQUENTIAL = 4
        const val MODE_REFLOW = 5
        const val MODE_EPUB = 6
    }
}
