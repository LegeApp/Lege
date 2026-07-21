//! Phase 5f / Gap D: steady-state allocation behaviour of the zero-alloc
//! renderer API (`decode_embedded_into` + a reused `DecoderContext`).
//!
//! A counting global allocator measures heap allocations per decode. The
//! guarantee asserted here is that the per-decode allocation count is *stable*
//! across iterations of the same page — the context and target buffers are
//! reused, so steady state does not grow. Every allocation that scales with the
//! page size (the page bitmap, each region bitmap) is now pooled in the
//! `DecoderContext`, so the steady-state count is down to a single small,
//! fixed-size allocation: the parser's `Vec<ParsedSegment>`. Eliminating that
//! last one would need either `unsafe` to pool a lifetime-bearing `Vec` across
//! calls or an offset-based rework of the public parse types — not worth it for
//! a buffer that does not scale with the image. Stability is the shipped
//! guarantee.

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use common::writer::{nominal_at, single_generic_page, TestBitmap};
use jbig2enc_rust::decode::{decode_embedded_into, DecodeOptions, DecoderContext};
use jbig2enc_rust::shared::bitmap::MonoBitmap;

static ALLOCS: AtomicUsize = AtomicUsize::new(0);

struct Counting;
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn random_bitmap(w: u32, h: u32, seed: u32) -> TestBitmap {
    let mut bm = TestBitmap::new(w, h);
    let mut s = seed.wrapping_add(1);
    for y in 0..h {
        for x in 0..w {
            s = s.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            if (s >> 16) & 1 == 1 {
                bm.set(x, y, true);
            }
        }
    }
    bm
}

#[test]
fn steady_state_allocations_are_stable() {
    let bm = random_bitmap(64, 48, 1);
    let stream = single_generic_page(&bm, 0, &nominal_at(0), false);
    let opts = DecodeOptions::default();
    let mut ctx = DecoderContext::new();
    let mut target = MonoBitmap::new(1, 1, false, &opts.limits).unwrap();

    // Warm up: grow the context banks and the target buffer to steady state.
    for _ in 0..4 {
        decode_embedded_into(&mut target, None, &stream, &opts, &mut ctx).unwrap();
    }

    // Measure per-decode allocation counts.
    let mut counts = Vec::new();
    for _ in 0..5 {
        let before = ALLOCS.load(Ordering::Relaxed);
        decode_embedded_into(&mut target, None, &stream, &opts, &mut ctx).unwrap();
        counts.push(ALLOCS.load(Ordering::Relaxed) - before);
    }

    // The decode must be correct.
    assert_eq!(target.width(), bm.width);
    for y in 0..bm.height {
        for x in 0..bm.width {
            assert_eq!(target.get(x, y), bm.get(x, y), "pixel ({x},{y})");
        }
    }

    // Steady state: the allocation count does not grow across iterations.
    let first = counts[0];
    for &c in &counts {
        assert_eq!(c, first, "allocation count grew across iterations: {counts:?}");
    }
    eprintln!("steady-state heap allocations per decode: {first}");
}
