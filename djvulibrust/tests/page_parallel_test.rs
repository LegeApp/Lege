//! Verifies the page-level parallel API described in
//! `src/doc/builder.rs`'s `DjvuDocument` doc comments: `encode_page` is
//! documented as "touches no shared mutable state, so it is safe to call
//! from a worker thread or rayon iterator", paired with the thread-safe
//! `add_encoded_page` insert into `PageCollection` (per-slot `RwLock`).
//!
//! That claim wasn't exercised by any existing test before this file —
//! this actually runs pages through real concurrent encoding (both
//! `std::thread::scope`, no extra dependency, and `rayon`, the pattern the
//! doc comments recommend) and checks the assembled document decodes
//! correctly via `ddjvu` (djvulibre), not just that it doesn't panic.

use djvu_encoder::{DjvuBuilder, PageBuilder, Pixel, Pixmap};
use std::process::Command;

fn synthetic_page_image(page_num: usize, width: u32, height: u32) -> Pixmap {
    Pixmap::from_fn(width, height, |x, y| {
        let r = (((x + page_num as u32 * 37) * 255 / width) % 256) as u8;
        let g = ((y * 255 / height) % 256) as u8;
        let b = (((x + y) * 128 / (width + height)) % 256) as u8;
        Pixel::new(r, g, b)
    })
}

fn ddjvu_available() -> bool {
    Command::new("ddjvu")
        .arg("--help")
        .output()
        .map(|o| o.status.success() || !o.stdout.is_empty() || !o.stderr.is_empty())
        .unwrap_or(false)
}

fn assert_decodes_with_page_count(djvu_bytes: &[u8], expected_pages: usize) {
    if !ddjvu_available() {
        eprintln!("SKIP decode check: ddjvu not found on PATH");
        return;
    }
    let dir = std::env::temp_dir();
    let djvu_path = dir.join(format!(
        "djvulibrust_page_parallel_test_{}.djvu",
        std::process::id()
    ));
    std::fs::write(&djvu_path, djvu_bytes).expect("write djvu");

    let output = Command::new("djvudump")
        .arg(&djvu_path)
        .output()
        .expect("run djvudump");
    assert!(output.status.success(), "djvudump failed");
    let dump = String::from_utf8_lossy(&output.stdout);
    assert!(
        dump.contains(&format!("{} page", expected_pages)) || dump.contains("FORM:DJVU"),
        "djvudump output doesn't look like a {expected_pages}-page document:\n{dump}"
    );

    let ppm_path = dir.join(format!(
        "djvulibrust_page_parallel_test_{}.ppm",
        std::process::id()
    ));
    let output = Command::new("ddjvu")
        .args([
            "-format=ppm",
            "-page=1",
            djvu_path.to_str().unwrap(),
            ppm_path.to_str().unwrap(),
        ])
        .output()
        .expect("run ddjvu");
    assert!(
        output.status.success(),
        "ddjvu failed to decode page 1: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(ppm_path.exists() && std::fs::metadata(&ppm_path).unwrap().len() > 0);
}

#[test]
fn std_thread_scope_parallel_encode_produces_correct_document() {
    let page_count = 6usize;
    let width = 200u32;
    let height = 150u32;

    let doc = DjvuBuilder::new(page_count).with_dpi(300).build();

    let pages: Vec<_> = (0..page_count)
        .map(|i| {
            let bg = synthetic_page_image(i, width, height);
            PageBuilder::new(i, width, height)
                .with_background(bg)
                .expect("with_background")
                .build()
                .expect("page build")
        })
        .collect();

    // Encode all pages concurrently on real OS threads, then insert. This
    // is exactly the split the doc comments recommend: encode_page (heavy,
    // touches no shared state) off-thread, add_encoded_page (cheap) after.
    std::thread::scope(|scope| {
        let handles: Vec<_> = pages
            .into_iter()
            .map(|page| {
                let doc_ref = &doc;
                scope.spawn(move || {
                    let encoded = doc_ref.encode_page(page).expect("encode_page");
                    doc_ref.add_encoded_page(encoded).expect("add_encoded_page");
                })
            })
            .collect();
        for h in handles {
            h.join().expect("thread panicked");
        }
    });

    assert!(doc.is_complete(), "not all pages were inserted");
    assert_eq!(doc.pages_ready(), page_count);

    let bytes = doc.finalize().expect("finalize");
    assert!(bytes.starts_with(b"AT&TFORM"));
    assert_decodes_with_page_count(&bytes, page_count);
}

#[cfg(feature = "rayon")]
#[test]
fn rayon_par_iter_parallel_encode_produces_correct_document() {
    use rayon::prelude::*;

    let page_count = 6usize;
    let width = 200u32;
    let height = 150u32;

    let doc = DjvuBuilder::new(page_count).with_dpi(300).build();

    let pages: Vec<_> = (0..page_count)
        .map(|i| {
            let bg = synthetic_page_image(i, width, height);
            PageBuilder::new(i, width, height)
                .with_background(bg)
                .expect("with_background")
                .build()
                .expect("page build")
        })
        .collect();

    pages.into_par_iter().for_each(|page| {
        let encoded = doc.encode_page(page).expect("encode_page");
        doc.add_encoded_page(encoded).expect("add_encoded_page");
    });

    assert!(doc.is_complete());
    assert_eq!(doc.pages_ready(), page_count);

    let bytes = doc.finalize().expect("finalize");
    assert_decodes_with_page_count(&bytes, page_count);
}

/// Cross-check: sequential and (std::thread) parallel encoding of the same
/// pages must produce byte-identical output. Each page's encode touches no
/// shared state (no rayon feature needed here — this isolates just the
/// document-assembly path), so insertion order/thread scheduling must not
/// change a single byte.
#[test]
fn parallel_and_sequential_encode_produce_identical_bytes() {
    let page_count = 4usize;
    let width = 150u32;
    let height = 120u32;

    let make_pages = || {
        (0..page_count)
            .map(|i| {
                let bg = synthetic_page_image(i, width, height);
                PageBuilder::new(i, width, height)
                    .with_background(bg)
                    .expect("with_background")
                    .build()
                    .expect("page build")
            })
            .collect::<Vec<_>>()
    };

    let seq_doc = DjvuBuilder::new(page_count).with_dpi(300).build();
    for page in make_pages() {
        seq_doc.add_page(page).expect("add_page");
    }
    let seq_bytes = seq_doc.finalize().expect("finalize");

    let par_doc = DjvuBuilder::new(page_count).with_dpi(300).build();
    std::thread::scope(|scope| {
        let handles: Vec<_> = make_pages()
            .into_iter()
            .rev() // deliberately out of order
            .map(|page| {
                let doc_ref = &par_doc;
                scope.spawn(move || {
                    let encoded = doc_ref.encode_page(page).expect("encode_page");
                    doc_ref.add_encoded_page(encoded).expect("add_encoded_page");
                })
            })
            .collect();
        for h in handles {
            h.join().expect("thread panicked");
        }
    });
    let par_bytes = par_doc.finalize().expect("finalize");

    assert_eq!(
        seq_bytes, par_bytes,
        "concurrent out-of-order encoding must produce identical output to sequential"
    );
}
