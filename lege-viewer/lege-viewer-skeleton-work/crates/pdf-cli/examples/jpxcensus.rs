#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // dev example: fail-fast is fine

//! Diagnostic: census of JPX (`/JPXDecode`) decode outcomes across many PDFs.
//! Reads a newline-delimited list of PDF paths from argv[1], extracts every
//! JPX stream, decodes it with `jp2lam`, and tallies normalized error
//! signatures so remaining codec gaps in the corpus are enumerable.
//!
//! Usage: cargo run -p pdf-cli --example jpxcensus -- <pdf-list.txt> [max_files]

use std::collections::BTreeMap;
use std::sync::Arc;

use pdf_document::{DocumentLimits, DocumentSnapshot, ParseContext};
use pdf_object::{ObjectId, PdfObject};
use pdf_source::OwnedBytesSource;

fn normalize(msg: &str) -> String {
    // Collapse numbers so "tile 3 TPsot 5" and "tile 0 TPsot 2" coalesce.
    let mut out = String::new();
    let mut last_digit = false;
    for ch in msg.chars() {
        if ch.is_ascii_digit() {
            if !last_digit {
                out.push('N');
            }
            last_digit = true;
        } else {
            out.push(ch);
            last_digit = false;
        }
    }
    out
}

fn main() {
    let list_path = std::env::args().nth(1).expect("usage: jpxcensus <list.txt> [max]");
    let max_files: usize = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(usize::MAX);
    let list = std::fs::read_to_string(&list_path).expect("read list");

    let mut tally: BTreeMap<String, u32> = BTreeMap::new();
    let mut examples: BTreeMap<String, String> = BTreeMap::new();
    let mut files_done = 0u32;
    let mut jpx_total = 0u32;
    let mut decoded = 0u32;

    for line in list.lines().take(max_files) {
        let path = line.trim();
        if path.is_empty() {
            continue;
        }
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        let source = Arc::new(OwnedBytesSource::new(bytes));
        let Ok(snap) = DocumentSnapshot::open(source, DocumentLimits::default()) else {
            continue;
        };
        files_done += 1;
        let size = snap.structure().xref.size();
        // Sample up to this many JPX streams per file — enough to catch a
        // file's codec profile without decoding every image in a 500-page book.
        let mut jpx_in_file = 0u32;
        for n in 1..size {
            if jpx_in_file >= 5 {
                break;
            }
            let id = ObjectId::new(n, 0);
            let mut ctx = ParseContext::new();
            let Ok(obj) = snap.objects().resolve(&snap, id, &mut ctx) else {
                continue;
            };
            let PdfObject::Stream(stream) = obj.as_ref() else {
                continue;
            };
            // Cheap pre-filter: only peel filters for streams that name JPX,
            // so we do not decode every Flate/CCITT stream in the document.
            let is_jpx = stream.dict.iter().any(|(_, v)| match v {
                PdfObject::Name(nm) => snap.names().resolve(*nm).as_ref() == b"JPXDecode",
                PdfObject::Array(items) => items.iter().any(|it| {
                    matches!(it, PdfObject::Name(nm) if snap.names().resolve(*nm).as_ref() == b"JPXDecode")
                }),
                _ => false,
            });
            if !is_jpx {
                continue;
            }
            let Ok((data, codec)) = snap.decode_stream_data_to_codec(stream, &mut ctx) else {
                continue;
            };
            if codec.as_deref() != Some("JPXDecode") {
                continue;
            }
            jpx_total += 1;
            jpx_in_file += 1;
            match jp2lam::decode_jp2(&data) {
                Ok(_) => decoded += 1,
                Err(e) => {
                    let sig = normalize(&e.to_string());
                    // Dump one representative failing stream per signature for
                    // offline inspection (opj_dump).
                    if !examples.contains_key(&sig)
                        && let Ok(dir) = std::env::var("JPXDUMPDIR")
                    {
                        let safe: String = sig.chars().filter(|c| c.is_ascii_alphanumeric()).take(40).collect();
                        let _ = std::fs::write(format!("{dir}/failjpx_{safe}.jp2"), &data);
                    }
                    *tally.entry(sig.clone()).or_default() += 1;
                    examples.entry(sig).or_insert_with(|| {
                        format!("{} obj{n}", std::path::Path::new(path).file_name().unwrap().to_string_lossy())
                    });
                }
            }
        }
        if files_done % 20 == 0 {
            eprintln!("... {files_done} files, {jpx_total} jpx, {decoded} ok");
        }
    }

    println!("files={files_done} jpx_streams={jpx_total} decoded={decoded} failed={}", jpx_total - decoded);
    println!("--- error signatures (count) ---");
    let mut sorted: Vec<_> = tally.iter().collect();
    sorted.sort_by(|a, b| b.1.cmp(a.1));
    for (sig, count) in sorted {
        println!("{count:>6}  {sig}");
        if let Some(ex) = examples.get(sig) {
            println!("        e.g. {ex}");
        }
    }
}
