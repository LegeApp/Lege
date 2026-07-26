#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // dev example: fail-fast is fine

//! Diagnostic: dump every image codec stream from a PDF with metadata, so a
//! specific real-world stream can be pinned as a regression fixture.
//! Usage: cargo run -p pdf-cli --example codecdump -- <file.pdf> <out-dir>
//! Prints one line per codec image: obj, codec, colorspace, WxH, byte length.

use std::sync::Arc;

use pdf_document::{DocumentLimits, DocumentSnapshot, ParseContext};
use pdf_object::{ObjectId, PdfObject};
use pdf_source::OwnedBytesSource;

fn name_of<'a>(snap: &'a DocumentSnapshot, obj: &'a PdfObject) -> String {
    match obj {
        PdfObject::Name(n) => String::from_utf8_lossy(&snap.names().resolve(*n)).into_owned(),
        PdfObject::Array(a) => a
            .iter()
            .map(|it| name_of(snap, it))
            .collect::<Vec<_>>()
            .join("+"),
        _ => "-".into(),
    }
}

fn ext_for(codec: Option<&str>) -> &'static str {
    match codec {
        Some("DCTDecode") => "jpg",
        Some("JPXDecode") => "jp2",
        Some("JBIG2Decode") => "jbig2",
        Some("CCITTFaxDecode") => "ccitt",
        _ => "bin",
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage: codecdump <file.pdf> <out-dir>");
    let out_dir = args.next().unwrap_or_else(|| ".".to_string());
    let bytes = std::fs::read(&path).expect("read pdf");
    let snap = DocumentSnapshot::open(
        Arc::new(OwnedBytesSource::new(bytes)),
        DocumentLimits::default(),
    )
    .expect("open pdf");

    let names = snap.names();
    let (subtype_k, cs_k, w_k, h_k, filter_k) = (
        names.intern(b"Subtype"),
        names.intern(b"ColorSpace"),
        names.intern(b"Width"),
        names.intern(b"Height"),
        names.intern(b"Filter"),
    );
    let size = snap.structure().xref.size();
    for n in 1..size {
        let mut ctx = ParseContext::new();
        let Ok(obj) = snap.objects().resolve(&snap, ObjectId::new(n, 0), &mut ctx) else {
            continue;
        };
        let PdfObject::Stream(stream) = obj.as_ref() else {
            continue;
        };
        let get = |k| stream.dict.iter().find(|(kk, _)| *kk == k).map(|(_, v)| v);
        // Only image XObjects with a real filter.
        let is_image = get(subtype_k).map(|v| name_of(&snap, v)) == Some("Image".into());
        if !is_image || get(filter_k).is_none() {
            continue;
        }
        let (data, codec) = match snap.decode_stream_data_to_codec(stream, &mut ctx) {
            Ok(v) => v,
            Err(_) => continue,
        };
        // Resolve /ColorSpace one indirection level and describe its shape.
        let cs_raw = get(cs_k).cloned();
        let cs_resolved = match &cs_raw {
            Some(PdfObject::Reference(id)) => snap
                .objects()
                .resolve(&snap, *id, &mut ctx)
                .ok()
                .map(|a| (*a).clone()),
            other => other.clone(),
        };
        let cs = match &cs_resolved {
            Some(PdfObject::Array(items)) => {
                let head = items.first().map(|v| name_of(&snap, v)).unwrap_or_default();
                format!("[{head} n={}]", items.len())
            }
            Some(v) => name_of(&snap, v),
            None => "-".into(),
        };
        let dec_k = names.intern(b"Decode");
        let decode = match get(dec_k) {
            Some(PdfObject::Array(a)) => {
                let vals: Vec<String> = a
                    .iter()
                    .map(|v| {
                        v.as_int()
                            .map(|i| i.to_string())
                            .unwrap_or_else(|| "r".into())
                    })
                    .collect();
                format!(" Decode=[{}]", vals.join(" "))
            }
            _ => String::new(),
        };
        let w = get(w_k).and_then(PdfObject::as_int).unwrap_or(0);
        let h = get(h_k).and_then(PdfObject::as_int).unwrap_or(0);
        let ext = ext_for(codec.as_deref());
        let outpath = format!("{out_dir}/obj{n}.{ext}");
        std::fs::write(&outpath, &data).expect("write codec stream");
        println!(
            "obj {n:<5} codec={:<14} cs={:<22}{decode} {w}x{h} bytes={} -> {outpath}",
            codec.as_deref().unwrap_or("<raw>"),
            cs,
            data.len()
        );
    }
}
