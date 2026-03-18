//! PDF variant test harness
//!
//! Generates 5 JBIG2-embedded PDF variants for viewer compatibility testing.
//! Run with:
//!   cargo test --lib --test pdf_variants --features symboldict -- --nocapture
//!
//! Output PDFs are written to `test_output_pdfs/` in the crate root.

use jbig2enc_rust::jbig2enc::Jbig2Encoder;
use jbig2enc_rust::jbig2structs::Jbig2Config;
use jbig2enc_rust::jbig2sym::BitImage;
use lopdf::dictionary;
use lopdf::{Document, Object, Stream};
use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::time::SystemTime;

/// Load a P4 (raw) PBM file into a BitImage.
fn load_pbm(path: &str) -> BitImage {
    let mut file = std::fs::File::open(path).expect(&format!("Failed to open PBM: {}", path));
    let mut reader = BufReader::new(&mut file);

    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert_eq!(line.trim(), "P4", "Only raw PBM (P4) supported");

    loop {
        line.clear();
        reader.read_line(&mut line).unwrap();
        if !line.starts_with('#') && !line.trim().is_empty() {
            break;
        }
    }

    let dims: Vec<usize> = line
        .split_whitespace()
        .map(|s| s.parse().unwrap())
        .collect();
    let (width, height) = (dims[0], dims[1]);

    let bytes_per_row = (width + 7) / 8;
    let expected = bytes_per_row * height;
    let mut data = vec![0u8; expected];
    reader.read_exact(&mut data).unwrap();

    let mut img = BitImage::new(width as u32, height as u32).unwrap();
    for y in 0..height {
        for x in 0..width {
            let byte = data[y * bytes_per_row + x / 8];
            let bit = 7 - (x % 8);
            let is_black = (byte & (1 << bit)) != 0;
            img.set(x as u32, y as u32, is_black);
        }
    }
    img
}

/// Convert a BitImage to an ndarray Array2<u8> for the encoder.
fn bitimage_to_array(img: &BitImage) -> ndarray::Array2<u8> {
    ndarray::Array2::from_shape_fn((img.height, img.width), |(y, x)| {
        if img.get(x as u32, y as u32) {
            1u8
        } else {
            0u8
        }
    })
}

/// Build a minimal PDF containing a single JBIG2-encoded page.
///
/// - `page_data`: raw JBIG2 segment bytes for the page
/// - `global_data`: optional JBIG2Globals stream bytes
/// - `width`, `height`: image dimensions in pixels
/// - `decode_inverted`: if true, uses Decode [0 1]; if false, uses Decode [1 0]
fn build_single_page_pdf(
    page_data: &[u8],
    global_data: Option<&[u8]>,
    width: u32,
    height: u32,
    decode_inverted: bool,
) -> Document {
    let mut doc = Document::with_version("1.7");

    // -- Optional JBIG2Globals stream (bare dict per ISO 32000-1 §8.9.5.6)
    let globals_id = global_data.map(|gd| {
        let stream = Stream::new(lopdf::Dictionary::new(), gd.to_vec());
        doc.add_object(stream)
    });

    // -- Image XObject
    let mut img_dict = lopdf::Dictionary::new();
    img_dict.set("Type", Object::Name(b"XObject".to_vec()));
    img_dict.set("Subtype", Object::Name(b"Image".to_vec()));
    img_dict.set("Width", Object::Integer(width as i64));
    img_dict.set("Height", Object::Integer(height as i64));
    img_dict.set("ColorSpace", Object::Name(b"DeviceGray".to_vec()));
    img_dict.set("BitsPerComponent", Object::Integer(1));
    img_dict.set("Filter", Object::Name(b"JBIG2Decode".to_vec()));

    if decode_inverted {
        img_dict.set("Decode", vec![Object::Integer(1), Object::Integer(0)]);
    } else {
        img_dict.set("Decode", vec![Object::Integer(0), Object::Integer(1)]);
    }

    if let Some(gid) = globals_id {
        let mut dp = lopdf::Dictionary::new();
        dp.set("JBIG2Globals", Object::Reference(gid));
        img_dict.set("DecodeParms", Object::Dictionary(dp));
    }

    let img_stream = Stream::new(img_dict, page_data.to_vec());
    let img_id = doc.add_object(img_stream);

    // -- Resources dict
    let xobject_dict = dictionary! {
        "Im0" => Object::Reference(img_id),
    };
    let resources = dictionary! {
        "XObject" => Object::Dictionary(xobject_dict),
    };

    // -- Page content stream: draw the image full-page
    // Scale to points (assuming 300 DPI)
    let pt_w = ((width as f64 / 300.0) * 72.0) as f32;
    let pt_h = ((height as f64 / 300.0) * 72.0) as f32;
    let content_str = format!("q {:.2} 0 0 {:.2} 0 0 cm /Im0 Do Q", pt_w, pt_h);
    let content_id = doc.add_object(Stream::new(
        lopdf::Dictionary::new(),
        content_str.into_bytes(),
    ));

    // -- Page object
    let page_dict = dictionary! {
        "Type" => "Page",
        "MediaBox" => vec![0.into(), 0.into(), Object::Real(pt_w), Object::Real(pt_h)],
        "Contents" => Object::Reference(content_id),
        "Resources" => Object::Dictionary(resources),
    };
    let page_id = doc.add_object(page_dict);

    // -- Pages node
    let pages_dict = dictionary! {
        "Type" => "Pages",
        "Kids" => vec![Object::Reference(page_id)],
        "Count" => Object::Integer(1),
    };
    let pages_id = doc.add_object(pages_dict);

    // Patch the page's Parent
    if let Ok(Object::Dictionary(d)) = doc.get_object_mut(page_id) {
        d.set("Parent", Object::Reference(pages_id));
    }

    // -- Catalog
    let catalog = dictionary! {
        "Type" => "Catalog",
        "Pages" => Object::Reference(pages_id),
    };
    let catalog_id = doc.add_object(catalog);
    doc.trailer.set("Root", Object::Reference(catalog_id));

    doc
}

/// Build a multi-page PDF where all pages share the same JBIG2Globals.
fn build_multi_page_pdf(
    page_streams: &[Vec<u8>],
    global_data: Option<&[u8]>,
    width: u32,
    height: u32,
) -> Document {
    let mut doc = Document::with_version("1.7");

    let globals_id = global_data.map(|gd| {
        let stream = Stream::new(lopdf::Dictionary::new(), gd.to_vec());
        doc.add_object(stream)
    });

    let pt_w = ((width as f64 / 300.0) * 72.0) as f32;
    let pt_h = ((height as f64 / 300.0) * 72.0) as f32;

    let mut page_ids = Vec::new();

    for (_i, page_data) in page_streams.iter().enumerate() {
        let mut img_dict = lopdf::Dictionary::new();
        img_dict.set("Type", Object::Name(b"XObject".to_vec()));
        img_dict.set("Subtype", Object::Name(b"Image".to_vec()));
        img_dict.set("Width", Object::Integer(width as i64));
        img_dict.set("Height", Object::Integer(height as i64));
        img_dict.set("ColorSpace", Object::Name(b"DeviceGray".to_vec()));
        img_dict.set("BitsPerComponent", Object::Integer(1));
        img_dict.set("Filter", Object::Name(b"JBIG2Decode".to_vec()));
        img_dict.set("Decode", vec![Object::Integer(0), Object::Integer(1)]);

        if let Some(gid) = globals_id {
            let mut dp = lopdf::Dictionary::new();
            dp.set("JBIG2Globals", Object::Reference(gid));
            img_dict.set("DecodeParms", Object::Dictionary(dp));
        }

        let img_stream = Stream::new(img_dict, page_data.clone());
        let img_id = doc.add_object(img_stream);

        let xobject_dict = dictionary! {
            "Im0" => Object::Reference(img_id),
        };
        let resources = dictionary! {
            "XObject" => Object::Dictionary(xobject_dict),
        };

        let content_str = format!("q {:.2} 0 0 {:.2} 0 0 cm /Im0 Do Q", pt_w, pt_h);
        let content_id = doc.add_object(Stream::new(
            lopdf::Dictionary::new(),
            content_str.into_bytes(),
        ));

        let page_dict = dictionary! {
            "Type" => "Page",
            "MediaBox" => vec![0.into(), 0.into(), Object::Real(pt_w as f32), Object::Real(pt_h as f32)],
            "Contents" => Object::Reference(content_id),
            "Resources" => Object::Dictionary(resources),
        };
        let page_id = doc.add_object(page_dict);
        page_ids.push(page_id);
    }

    let kids: Vec<Object> = page_ids.iter().map(|id| Object::Reference(*id)).collect();
    let pages_dict = dictionary! {
        "Type" => "Pages",
        "Kids" => kids,
        "Count" => Object::Integer(page_ids.len() as i64),
    };
    let pages_id = doc.add_object(pages_dict);

    for &pid in &page_ids {
        if let Ok(Object::Dictionary(d)) = doc.get_object_mut(pid) {
            d.set("Parent", Object::Reference(pages_id));
        }
    }

    let catalog = dictionary! {
        "Type" => "Catalog",
        "Pages" => Object::Reference(pages_id),
    };
    let catalog_id = doc.add_object(catalog);
    doc.trailer.set("Root", Object::Reference(catalog_id));

    doc
}

fn output_dir() -> PathBuf {
    let ts = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("test_output_pdfs")
        .join(format!("run_{ts}"));
    std::fs::create_dir_all(&dir).expect("Failed to create output dir");
    dir
}

/// Find the best test image (prefer the larger one for better symbol extraction).
fn find_test_image() -> (BitImage, String) {
    let base = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    // Try test_image1.pbm first (larger, better for symbol mode)
    for name in &[
        "test_image1.pbm",
        "tests/fixtures/test_image1.pbm",
        "test_image2.pbm",
    ] {
        let path = base.join(name);
        if path.exists() {
            let img = load_pbm(path.to_str().unwrap());
            return (img, name.to_string());
        }
    }
    // Fallback: small test image
    let path = base.join("tests/fixtures/test_image1.pbm");
    (
        load_pbm(path.to_str().unwrap()),
        "tests/fixtures/test_image1.pbm".into(),
    )
}

#[test]
fn generate_pdf_variants() {
    let out = output_dir();
    let (test_img, img_name) = find_test_image();
    let width = test_img.width as u32;
    let height = test_img.height as u32;
    let array = bitimage_to_array(&test_img);

    println!("╔══════════════════════════════════════════════════════╗");
    println!("║         JBIG2 PDF Variant Test Harness              ║");
    println!("╠══════════════════════════════════════════════════════╣");
    println!("║ Source image: {} ({}×{})", img_name, width, height);
    println!("║ Output dir:   {}", out.display());
    println!("╚══════════════════════════════════════════════════════╝");
    println!();

    // ═══════════════════════════════════════════════════════════
    // Variant A: Lossless generic region (no symbol dictionary)
    // ═══════════════════════════════════════════════════════════
    {
        let cfg = Jbig2Config::lossless();
        let mut enc = Jbig2Encoder::new(&cfg);
        enc.add_page(&array).expect("A: add_page failed");
        let split = enc.flush_pdf_split().expect("A: flush_pdf_split failed");

        assert!(
            split.global_segments.is_none(),
            "A: lossless should have no globals"
        );
        assert_eq!(split.page_streams.len(), 1, "A: expected 1 page stream");

        let mut pdf = build_single_page_pdf(
            &split.page_streams[0],
            None,
            width,
            height,
            false, // Decode [0 1]
        );
        let path = out.join("variant_A_lossless_generic.pdf");
        pdf.save(&path).expect("A: save failed");
        println!(
            "  [A] Lossless generic region (no symbols)     → {} ({} bytes page stream)",
            path.file_name().unwrap().to_str().unwrap(),
            split.page_streams[0].len()
        );
    }

    // ═══════════════════════════════════════════════════════════
    // Variant B: Symbol mode, single page (embedded dict, Decode [0 1])
    // ═══════════════════════════════════════════════════════════
    {
        let mut cfg = Jbig2Config::text();
        cfg.want_full_headers = false;
        let mut enc = Jbig2Encoder::new(&cfg);
        enc.add_page(&array).expect("B: add_page failed");
        let split = enc.flush_pdf_split().expect("B: flush_pdf_split failed");

        assert_eq!(split.page_streams.len(), 1, "B: expected 1 page stream");

        let mut pdf = build_single_page_pdf(
            &split.page_streams[0],
            split.global_segments.as_deref(),
            width,
            height,
            false, // Decode [1 0]
        );
        let path = out.join("variant_B_symbol_single_decode01.pdf");
        pdf.save(&path).expect("B: save failed");
        println!(
            "  [B] Symbol mode, single page, Decode [0 1]   → {} (globals: {} bytes, page: {} bytes)",
            path.file_name().unwrap().to_str().unwrap(),
            split.global_segments.as_ref().map_or(0, |g| g.len()),
            split.page_streams[0].len()
        );
    }

    // ═══════════════════════════════════════════════════════════
    // Variant C: Symbol mode with globals, Decode [0 1]
    //   Same encoder, two pages of the same image → shared globals
    // ═══════════════════════════════════════════════════════════
    {
        let mut cfg = Jbig2Config::text();
        cfg.want_full_headers = false;
        let mut enc = Jbig2Encoder::new(&cfg);
        enc.add_page(&array).expect("C: add_page 1 failed");
        enc.add_page(&array).expect("C: add_page 2 failed");
        let split = enc.flush_pdf_split().expect("C: flush_pdf_split failed");

        assert_eq!(split.page_streams.len(), 2, "C: expected 2 page streams");

        // Single-page PDF using page 0 with shared globals
        let mut pdf = build_single_page_pdf(
            &split.page_streams[0],
            split.global_segments.as_deref(),
            width,
            height,
            false,
        );
        let path = out.join("variant_C_symbol_globals_decode01.pdf");
        pdf.save(&path).expect("C: save failed");
        println!(
            "  [C] Symbol mode + globals, Decode [0 1]      → {} (globals: {} bytes, page: {} bytes)",
            path.file_name().unwrap().to_str().unwrap(),
            split.global_segments.as_ref().map_or(0, |g| g.len()),
            split.page_streams[0].len()
        );
    }

    // ═══════════════════════════════════════════════════════════
    // Variant D: Symbol mode with globals, Decode [1 0] (inverted polarity)
    // ═══════════════════════════════════════════════════════════
    {
        let mut cfg = Jbig2Config::text();
        cfg.want_full_headers = false;
        let mut enc = Jbig2Encoder::new(&cfg);
        enc.add_page(&array).expect("D: add_page 1 failed");
        enc.add_page(&array).expect("D: add_page 2 failed");
        let split = enc.flush_pdf_split().expect("D: flush_pdf_split failed");

        let mut pdf = build_single_page_pdf(
            &split.page_streams[0],
            split.global_segments.as_deref(),
            width,
            height,
            true, // Decode [1 0] — inverted polarity
        );
        let path = out.join("variant_D_symbol_globals_decode10.pdf");
        pdf.save(&path).expect("D: save failed");
        println!(
            "  [D] Symbol mode + globals, Decode [1 0]      → {} (globals: {} bytes, page: {} bytes)",
            path.file_name().unwrap().to_str().unwrap(),
            split.global_segments.as_ref().map_or(0, |g| g.len()),
            split.page_streams[0].len()
        );
    }

    // ═══════════════════════════════════════════════════════════
    // Variant E: Multi-page PDF with shared globals
    // ═══════════════════════════════════════════════════════════
    {
        let mut cfg = Jbig2Config::text();
        cfg.want_full_headers = false;
        let mut enc = Jbig2Encoder::new(&cfg);
        enc.add_page(&array).expect("E: add_page 1 failed");
        enc.add_page(&array).expect("E: add_page 2 failed");
        let split = enc.flush_pdf_split().expect("E: flush_pdf_split failed");

        let mut pdf = build_multi_page_pdf(
            &split.page_streams,
            split.global_segments.as_deref(),
            width,
            height,
        );
        let path = out.join("variant_E_multipage_shared_globals.pdf");
        pdf.save(&path).expect("E: save failed");
        println!(
            "  [E] Multi-page, shared globals               → {} ({} pages, globals: {} bytes)",
            path.file_name().unwrap().to_str().unwrap(),
            split.page_streams.len(),
            split.global_segments.as_ref().map_or(0, |g| g.len()),
        );
    }

    println!();
    println!("All 5 variants written to: {}", out.display());
    println!("Open in Adobe Acrobat, Chrome/Edge, and test with jbig2dec.");
}
