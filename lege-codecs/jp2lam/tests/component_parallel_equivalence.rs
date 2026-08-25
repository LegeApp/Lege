//! Component-level parallelism must not change the codestream.
//!
//! `visit_tier1_encoded_mct_components_rect` encodes the three ICT components
//! concurrently when the plan declares a working-memory budget large enough to
//! hold every component's coefficient plane at once, and sequentially otherwise.
//! Only the transform-and-encode work is parallel; the visitor that appends to
//! the codestream still runs in component order. So both paths must emit
//! byte-identical output, and that is what these tests pin.
//!
//! Without this the parallel branch is dormant in the suite: nothing else sets
//! `max_working_memory`, so every other test takes the sequential path.

use jp2lam::{ColorSpace, Component, EncodeOptions, Image, ResourceLimits, encode};

/// Big enough to clear `component_parallel_working_memory_floor` for the small
/// fixtures here, which is what switches the encoder onto the parallel branch.
const AMPLE_WORKING_MEMORY: usize = 512 * 1024 * 1024;

fn rgb_fixture(width: u32, height: u32, mut sample: impl FnMut(u32, u32) -> [u8; 3]) -> Image {
    let pixels = width as usize * height as usize;
    let (mut r, mut g, mut b) = (
        Vec::with_capacity(pixels),
        Vec::with_capacity(pixels),
        Vec::with_capacity(pixels),
    );
    for y in 0..height {
        for x in 0..width {
            let [rr, gg, bb] = sample(x, y);
            r.push(i32::from(rr));
            g.push(i32::from(gg));
            b.push(i32::from(bb));
        }
    }
    let component = |data: Vec<i32>| Component {
        data,
        width,
        height,
        precision: 8,
        signed: false,
        dx: 1,
        dy: 1,
    };
    Image {
        width,
        height,
        components: vec![component(r), component(g), component(b)],
        colorspace: ColorSpace::Srgb,
    }
}

/// Lossy quality, so the plan selects the irreversible 9/7 transform and MCT —
/// the only configuration that reaches the component loop under test.
fn options(max_working_memory: Option<usize>) -> EncodeOptions {
    EncodeOptions {
        quality: 80,
        resource_limits: ResourceLimits {
            max_working_memory,
            ..Default::default()
        },
        ..EncodeOptions::default()
    }
}

fn encode_both_ways(label: &str, image: &Image) {
    let sequential = encode(image, &options(None))
        .unwrap_or_else(|e| panic!("{label}: sequential encode failed: {e}"));
    let parallel = encode(image, &options(Some(AMPLE_WORKING_MEMORY)))
        .unwrap_or_else(|e| panic!("{label}: parallel encode failed: {e}"));

    assert_eq!(
        sequential.len(),
        parallel.len(),
        "{label}: codestream length differs between the sequential and \
         component-parallel paths ({} vs {} bytes)",
        sequential.len(),
        parallel.len()
    );
    if sequential != parallel {
        let at = sequential
            .iter()
            .zip(&parallel)
            .position(|(a, b)| a != b)
            .unwrap_or(0);
        panic!(
            "{label}: codestreams diverge at byte {at} \
             (sequential={:#04x}, parallel={:#04x}); component-level parallelism \
             must not change the output",
            sequential[at], parallel[at]
        );
    }
}

#[test]
fn parallel_and_sequential_agree_on_a_smooth_gradient() {
    let image = rgb_fixture(64, 48, |x, y| {
        [(x * 4) as u8, (y * 5) as u8, ((x + y) * 3) as u8]
    });
    encode_both_ways("smooth gradient", &image);
}

#[test]
fn parallel_and_sequential_agree_on_saturated_extrema() {
    // Saturated corners exercise the ICT's clamping edges, where a data race
    // between components would show up as a single wrong byte rather than a
    // wholesale difference.
    let image = rgb_fixture(37, 29, |x, y| match ((x & 1) << 1) | (y & 1) {
        0 => [0, 0, 0],
        1 => [255, 255, 255],
        2 => [255, 0, 0],
        _ => [0, 255, 255],
    });
    encode_both_ways("saturated extrema", &image);
}

#[test]
fn parallel_and_sequential_agree_on_deterministic_noise() {
    // High-entropy input maximises the number of coding passes, so the two
    // paths have the most opportunity to disagree.
    let mut state = 0x4a50_324cu32;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        state
    };
    let image = rgb_fixture(53, 41, |_x, _y| {
        [
            (next() & 0xff) as u8,
            (next() & 0xff) as u8,
            (next() & 0xff) as u8,
        ]
    });
    encode_both_ways("deterministic noise", &image);
}
