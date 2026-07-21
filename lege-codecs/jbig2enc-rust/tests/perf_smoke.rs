use jbig2enc_rust::jbig2comparator::Comparator;
use jbig2enc_rust::jbig2enc::{SymbolInstance, encode_symbol_dict, encode_text_region_mapped};
use jbig2enc_rust::jbig2structs::Jbig2Config;
use jbig2enc_rust::jbig2sym::{BitImage, Rect};
use std::time::Instant;

fn make_checker(width: u32, height: u32, phase: usize) -> BitImage {
    let mut image = BitImage::new(width, height).expect("image");
    let width = width as usize;
    let height = height as usize;
    for y in 0..height {
        for x in 0..width {
            if ((x + y + phase) % 3) == 0 {
                image.as_mut_bits().set(y * width + x, true);
            }
        }
    }
    image
}

#[test]
#[ignore]
fn packed_words_perf_smoke() {
    let image = make_checker(128, 128, 0);
    let start = Instant::now();
    let mut total_words = 0usize;
    for _ in 0..2_000 {
        total_words += image.packed_words().len();
    }
    eprintln!(
        "packed_words: {:?} for {} words",
        start.elapsed(),
        total_words
    );
    assert!(total_words > 0);
}

#[test]
#[ignore]
fn comparator_distance_perf_smoke() {
    let a = make_checker(32, 32, 0);
    let b = make_checker(32, 32, 1);
    let start = Instant::now();
    let mut comparator = Comparator::default();
    let mut hits = 0usize;
    for _ in 0..5_000 {
        if comparator.distance(&a, &b, 64).is_some() {
            hits += 1;
        }
    }
    eprintln!(
        "comparator_distance: {:?} with {} hits",
        start.elapsed(),
        hits
    );
    assert!(hits > 0);
}

#[test]
#[ignore]
fn symbol_dict_encoding_perf_smoke() {
    let symbols: Vec<BitImage> = (0..128).map(|idx| make_checker(16, 16, idx)).collect();
    let refs: Vec<&BitImage> = symbols.iter().collect();
    let cfg = Jbig2Config::text();
    let start = Instant::now();
    let payload = encode_symbol_dict(&refs, &cfg, 0).expect("symbol dict");
    eprintln!(
        "encode_symbol_dict: {:?} for {} bytes",
        start.elapsed(),
        payload.len()
    );
    assert!(!payload.is_empty());
}

#[test]
#[ignore]
fn text_region_encoding_perf_smoke() {
    let cfg = Jbig2Config::text();
    let symbols: Vec<BitImage> = (0..32).map(|idx| make_checker(12, 14, idx)).collect();
    let mut instances = Vec::new();
    for idx in 0..128usize {
        instances.push(SymbolInstance {
            symbol_index: idx % symbols.len(),
            position: Rect {
                x: ((idx % 16) * 14) as u32,
                y: ((idx / 16) * 18) as u32,
                width: symbols[idx % symbols.len()].width as u32,
                height: symbols[idx % symbols.len()].height as u32,
            },
            instance_bitmap: symbols[idx % symbols.len()].clone(),
            needs_refinement: false,
            refinement_dx: 0,
            refinement_dy: 0,
        });
    }

    let mut global_map = vec![u32::MAX; symbols.len()];
    for (idx, slot) in global_map.iter_mut().enumerate() {
        *slot = idx as u32;
    }

    let start = Instant::now();
    let payload = encode_text_region_mapped(
        &instances,
        &cfg,
        &symbols,
        &global_map,
        symbols.len() as u32,
        &vec![u32::MAX; symbols.len()],
        0,
        0,
    )
    .expect("text region");
    eprintln!(
        "encode_text_region_mapped: {:?} for {} bytes",
        start.elapsed(),
        payload.len()
    );
    assert!(!payload.is_empty());
}
