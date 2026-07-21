use std::time::Instant;

/// Optional decoder-stage attribution returned by [`super::decode_jp2_with_stats`].
///
/// Timings are wall-clock nanoseconds. Nested fields such as the Tier-1 pass
/// timings are subdivisions of their corresponding total and must not be added
/// to that total a second time.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Jp2DecodeStats {
    pub total_ns: u64,
    pub container_parse_ns: u64,
    pub codestream_parse_ns: u64,
    pub tile_plan_ns: u64,

    pub tier2_setup_ns: u64,
    pub tier2_packet_headers_ns: u64,
    pub tier2_merge_ns: u64,
    pub tier2_concat_ns: u64,

    pub tier1_total_ns: u64,
    pub tier1_mq_ns: u64,
    pub tier1_significance_ns: u64,
    pub tier1_refinement_ns: u64,
    pub tier1_cleanup_ns: u64,
    pub tier1_block_output_ns: u64,

    pub dequantize_ns: u64,
    /// Total inverse-DWT time. Axis and level fields are finer subdivisions.
    pub dwt_total_ns: u64,
    pub dwt_horizontal_ns: u64,
    pub dwt_vertical_ns: u64,
    pub dwt_level_ns: Vec<u64>,
    pub inverse_mct_ns: u64,
    pub finalize_ns: u64,
    pub tile_stitch_ns: u64,
    pub output_pack_ns: u64,

    pub packets: u64,
    pub packet_header_bytes: u64,
    pub codeword_bytes: u64,
    pub codeblocks: u64,
    pub mq_symbols: u64,
    pub significance_passes: u64,
    pub refinement_passes: u64,
    pub cleanup_passes: u64,

    pub coefficient_pixels: u64,
    pub reconstructed_pixels: u64,
    pub output_pixels: u64,

    pub allocated_bytes: u64,
    pub peak_scratch_bytes: u64,
}

pub(crate) struct StatsSink<'a> {
    stats: Option<&'a mut Jp2DecodeStats>,
}

impl<'a> StatsSink<'a> {
    pub(crate) fn disabled() -> Self {
        Self { stats: None }
    }

    pub(crate) fn enabled(stats: &'a mut Jp2DecodeStats) -> Self {
        Self { stats: Some(stats) }
    }

    pub(crate) fn is_enabled(&self) -> bool {
        self.stats.is_some()
    }

    #[inline]
    pub(crate) fn start(&self) -> Option<Instant> {
        self.stats.as_ref().map(|_| Instant::now())
    }

    #[inline]
    pub(crate) fn finish(
        &mut self,
        start: Option<Instant>,
        add: impl FnOnce(&mut Jp2DecodeStats, u64),
    ) {
        if let (Some(stats), Some(start)) = (self.stats.as_deref_mut(), start) {
            add(stats, duration_ns(start));
        }
    }

    #[inline]
    pub(crate) fn update(&mut self, update: impl FnOnce(&mut Jp2DecodeStats)) {
        if let Some(stats) = self.stats.as_deref_mut() {
            update(stats);
        }
    }
}

fn duration_ns(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX)
}
