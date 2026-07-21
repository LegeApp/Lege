use std::sync::atomic::{AtomicU64, Ordering};

pub static TOTAL_BLOCKS: AtomicU64 = AtomicU64::new(0);
pub static EMPTY_BLOCKS: AtomicU64 = AtomicU64::new(0);
pub static MQ_SYMBOLS: AtomicU64 = AtomicU64::new(0);
pub static CLEANUP_PASSES: AtomicU64 = AtomicU64::new(0);
pub static SP_PASSES: AtomicU64 = AtomicU64::new(0);
pub static MR_PASSES: AtomicU64 = AtomicU64::new(0);
pub static TOTAL_PASS_BYTES: AtomicU64 = AtomicU64::new(0);
pub static TILE_SAMPLE_BYTES_PEAK: AtomicU64 = AtomicU64::new(0);
pub static DWT_COEFFICIENT_BYTES_PEAK: AtomicU64 = AtomicU64::new(0);
pub static DWT_SCRATCH_BYTES_PEAK: AtomicU64 = AtomicU64::new(0);
pub static CODEBLOCK_WORKER_BYTES_PEAK: AtomicU64 = AtomicU64::new(0);
pub static ENCODED_STORE_MEMORY_BYTES_PEAK: AtomicU64 = AtomicU64::new(0);
pub static ENCODED_STORE_SPILLED_BYTES: AtomicU64 = AtomicU64::new(0);
pub static RD_METADATA_BYTES_PEAK: AtomicU64 = AtomicU64::new(0);
pub static PACKET_HEADER_BYTES_PEAK: AtomicU64 = AtomicU64::new(0);
pub static OUTPUT_BUFFER_BYTES_PEAK: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MemoryCounterSnapshot {
    pub tile_sample_bytes_peak: u64,
    pub dwt_coefficient_bytes_peak: u64,
    pub dwt_scratch_bytes_peak: u64,
    pub codeblock_worker_bytes_peak: u64,
    pub encoded_store_memory_bytes_peak: u64,
    pub encoded_store_spilled_bytes: u64,
    pub rd_metadata_bytes_peak: u64,
    pub packet_header_bytes_peak: u64,
    pub output_buffer_bytes_peak: u64,
}

fn record_peak(counter: &AtomicU64, bytes: usize) {
    counter.fetch_max(bytes as u64, Ordering::Relaxed);
}

pub(crate) fn record_tile_samples(bytes: usize) {
    record_peak(&TILE_SAMPLE_BYTES_PEAK, bytes);
}

pub(crate) fn record_dwt_coefficients(bytes: usize) {
    record_peak(&DWT_COEFFICIENT_BYTES_PEAK, bytes);
}

pub(crate) fn record_dwt_scratch(bytes: usize) {
    record_peak(&DWT_SCRATCH_BYTES_PEAK, bytes);
}

pub(crate) fn record_codeblock_worker(bytes: usize) {
    record_peak(&CODEBLOCK_WORKER_BYTES_PEAK, bytes);
}

pub(crate) fn record_encoded_store(memory_bytes: usize, spilled_bytes: u64) {
    record_peak(&ENCODED_STORE_MEMORY_BYTES_PEAK, memory_bytes);
    ENCODED_STORE_SPILLED_BYTES.store(spilled_bytes, Ordering::Relaxed);
}

pub(crate) fn record_rd_metadata(bytes: usize) {
    record_peak(&RD_METADATA_BYTES_PEAK, bytes);
}

pub(crate) fn record_packet_header(bytes: usize) {
    record_peak(&PACKET_HEADER_BYTES_PEAK, bytes);
}

pub(crate) fn record_output_buffer(bytes: usize) {
    record_peak(&OUTPUT_BUFFER_BYTES_PEAK, bytes);
}

pub fn memory_snapshot() -> MemoryCounterSnapshot {
    MemoryCounterSnapshot {
        tile_sample_bytes_peak: TILE_SAMPLE_BYTES_PEAK.load(Ordering::Relaxed),
        dwt_coefficient_bytes_peak: DWT_COEFFICIENT_BYTES_PEAK.load(Ordering::Relaxed),
        dwt_scratch_bytes_peak: DWT_SCRATCH_BYTES_PEAK.load(Ordering::Relaxed),
        codeblock_worker_bytes_peak: CODEBLOCK_WORKER_BYTES_PEAK.load(Ordering::Relaxed),
        encoded_store_memory_bytes_peak: ENCODED_STORE_MEMORY_BYTES_PEAK.load(Ordering::Relaxed),
        encoded_store_spilled_bytes: ENCODED_STORE_SPILLED_BYTES.load(Ordering::Relaxed),
        rd_metadata_bytes_peak: RD_METADATA_BYTES_PEAK.load(Ordering::Relaxed),
        packet_header_bytes_peak: PACKET_HEADER_BYTES_PEAK.load(Ordering::Relaxed),
        output_buffer_bytes_peak: OUTPUT_BUFFER_BYTES_PEAK.load(Ordering::Relaxed),
    }
}

pub fn reset() {
    TOTAL_BLOCKS.store(0, Ordering::Relaxed);
    EMPTY_BLOCKS.store(0, Ordering::Relaxed);
    MQ_SYMBOLS.store(0, Ordering::Relaxed);
    CLEANUP_PASSES.store(0, Ordering::Relaxed);
    SP_PASSES.store(0, Ordering::Relaxed);
    MR_PASSES.store(0, Ordering::Relaxed);
    TOTAL_PASS_BYTES.store(0, Ordering::Relaxed);
    TILE_SAMPLE_BYTES_PEAK.store(0, Ordering::Relaxed);
    DWT_COEFFICIENT_BYTES_PEAK.store(0, Ordering::Relaxed);
    DWT_SCRATCH_BYTES_PEAK.store(0, Ordering::Relaxed);
    CODEBLOCK_WORKER_BYTES_PEAK.store(0, Ordering::Relaxed);
    ENCODED_STORE_MEMORY_BYTES_PEAK.store(0, Ordering::Relaxed);
    ENCODED_STORE_SPILLED_BYTES.store(0, Ordering::Relaxed);
    RD_METADATA_BYTES_PEAK.store(0, Ordering::Relaxed);
    PACKET_HEADER_BYTES_PEAK.store(0, Ordering::Relaxed);
    OUTPUT_BUFFER_BYTES_PEAK.store(0, Ordering::Relaxed);
}

pub fn print() {
    let total = TOTAL_BLOCKS.load(Ordering::Relaxed);
    let empty = EMPTY_BLOCKS.load(Ordering::Relaxed);
    let mq = MQ_SYMBOLS.load(Ordering::Relaxed);
    let cl = CLEANUP_PASSES.load(Ordering::Relaxed);
    let sp = SP_PASSES.load(Ordering::Relaxed);
    let mr = MR_PASSES.load(Ordering::Relaxed);
    let bytes = TOTAL_PASS_BYTES.load(Ordering::Relaxed);

    let total_passes = cl.saturating_add(sp).saturating_add(mr);

    println!("\n=== Tier-1 Counters ===");
    println!(
        "  Blocks: total={} empty={} ({:.1}%)",
        total,
        empty,
        if total > 0 {
            100.0 * empty as f64 / total as f64
        } else {
            0.0
        }
    );
    println!(
        "  Passes: cleanup={} SP={} MR={} (total={})",
        cl, sp, mr, total_passes
    );
    println!(
        "  MQ symbols: {} ({:.1} per block, {:.1} per pass)",
        mq,
        if total > 0 {
            mq as f64 / total as f64
        } else {
            0.0
        },
        if total_passes > 0 {
            mq as f64 / total_passes as f64
        } else {
            0.0
        }
    );
    println!(
        "  Bytes: {} (avg {:.1} per pass)",
        bytes,
        if total_passes > 0 {
            bytes as f64 / total_passes as f64
        } else {
            0.0
        }
    );
    println!("  Memory stages: {:?}", memory_snapshot());
}
