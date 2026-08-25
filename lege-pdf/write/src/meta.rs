//! Document metadata + conformance profiles: Info dict, OutputIntent,
//! PdfProfile enum (Pdf14/Pdf17/PdfA1b/...) deciding version/xref form/
//! object-stream permission/required metadata in ONE place.
//!
//! One seam decides everything profile-dependent, so nothing scatters across
//! `finalize`. NOTE (honesty gap, M4): the PdfA1b path here reproduces the
//! current writer's *claims* (OutputIntent + MarkInfo + Info) but is not truly
//! conformant — no XMP metadata stream and no embedded ICC profile behind the
//! OutputIntent (`/DestOutputProfile`). M4 either adds those or stops claiming
//! PDF/A. The time source is injected (`pdf_date`) so this crate stays free of
//! a clock dependency.

use std::io::Write;

use crate::serialize::{write_literal_string, write_name, write_text_string};
use crate::sink::PdfSink;
use crate::types::{ObjectId, Result};

/// Output conformance profile. Selected once; drives version and which
/// metadata/xref machinery finalize emits.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum PdfProfile {
    /// Plain PDF 1.4.
    Pdf14,
    /// Plain PDF 1.7 (default for image-only output).
    #[default]
    Pdf17,
    /// PDF/A-1b claims (version 1.4, OutputIntent, MarkInfo, Info).
    PdfA1b,
}

impl PdfProfile {
    /// The `%PDF-x.y` version string this profile pins.
    pub fn version(&self) -> &'static str {
        match self {
            PdfProfile::Pdf14 | PdfProfile::PdfA1b => "1.4",
            PdfProfile::Pdf17 => "1.7",
        }
    }

    /// Whether finalize should emit the PDF/A OutputIntent + MarkInfo + Info.
    pub fn wants_pdfa_metadata(&self) -> bool {
        matches!(self, PdfProfile::PdfA1b)
    }

    /// Object streams are never allowed under PDF/A-1 (a 1.5 feature); the
    /// image-only profiles do not use them either (classic xref, M1–M3).
    pub fn allows_object_streams(&self) -> bool {
        false
    }
}

/// Document information, all fields optional-with-defaults. `pdf_date` is a
/// preformatted PDF date string (`D:YYYYMMDDHHmmSS±HH'mm'`) or None.
#[derive(Debug)]
pub struct DocumentMeta {
    pub profile: PdfProfile,
    pub pdf_date: Option<String>,
    pub title: String,
    pub author: String,
    pub producer: String,
    pub creator: String,
    pub subject: String,
    pub keywords: String,
}

impl Default for DocumentMeta {
    fn default() -> Self {
        Self {
            profile: PdfProfile::default(),
            pdf_date: None,
            title: "Document".to_string(),
            author: "Lege".to_string(),
            producer: "Lege PDF Generator".to_string(),
            creator: "Lege".to_string(),
            subject: "PDF/A Document".to_string(),
            keywords: "PDF/A-1b".to_string(),
        }
    }
}

/// Write the sRGB OutputIntent object and return its id.
pub fn write_output_intent<W: Write>(sink: &mut PdfSink<W>) -> Result<ObjectId> {
    let id = sink.alloc_id();
    let mut d = Vec::new();
    d.extend_from_slice(b"<<");
    key(&mut d, b"Type");
    write_name(&mut d, b"OutputIntent");
    key(&mut d, b"S");
    write_name(&mut d, b"GTS_PDFA1");
    key(&mut d, b"OutputConditionIdentifier");
    write_literal_string(&mut d, b"sRGB IEC61966-2.1");
    key(&mut d, b"Info");
    write_literal_string(&mut d, b"sRGB IEC61966-2.1");
    key(&mut d, b"OutputCondition");
    write_literal_string(&mut d, b"sRGB IEC61966-2.1");
    key(&mut d, b"RegistryName");
    write_literal_string(&mut d, b"http://www.color.org");
    d.extend_from_slice(b">>");
    sink.write_indirect(id, &d)?;
    Ok(id)
}

/// Write the document information dictionary and return its id.
pub fn write_info<W: Write>(sink: &mut PdfSink<W>, meta: &DocumentMeta) -> Result<ObjectId> {
    let id = sink.alloc_id();
    let mut d = Vec::new();
    d.extend_from_slice(b"<<");
    kv_text(&mut d, b"Producer", &meta.producer);
    kv_text(&mut d, b"Creator", &meta.creator);
    if let Some(date) = &meta.pdf_date {
        kv_text(&mut d, b"CreationDate", date);
        kv_text(&mut d, b"ModDate", date);
    }
    kv_text(&mut d, b"Title", &meta.title);
    kv_text(&mut d, b"Author", &meta.author);
    kv_text(&mut d, b"Subject", &meta.subject);
    kv_text(&mut d, b"Keywords", &meta.keywords);
    key(&mut d, b"Trapped");
    write_name(&mut d, b"False");
    d.extend_from_slice(b">>");
    sink.write_indirect(id, &d)?;
    Ok(id)
}

fn key(d: &mut Vec<u8>, k: &[u8]) {
    write_name(d, k);
    d.push(b' ');
}

fn kv_text(d: &mut Vec<u8>, key_name: &[u8], value: &str) {
    if value.is_empty() {
        return;
    }
    key(d, key_name);
    write_text_string(d, value);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_versions() {
        assert_eq!(PdfProfile::Pdf17.version(), "1.7");
        assert_eq!(PdfProfile::Pdf14.version(), "1.4");
        assert_eq!(PdfProfile::PdfA1b.version(), "1.4");
        assert!(PdfProfile::PdfA1b.wants_pdfa_metadata());
        assert!(!PdfProfile::Pdf17.wants_pdfa_metadata());
        assert!(!PdfProfile::PdfA1b.allows_object_streams());
    }

    #[test]
    fn output_intent_shape() {
        let mut sink = PdfSink::new(Vec::new(), "1.4").unwrap();
        write_output_intent(&mut sink).unwrap();
        let t = String::from_utf8_lossy(&sink.finish().unwrap()).into_owned();
        assert!(t.contains("/Type /OutputIntent"));
        assert!(t.contains("/S /GTS_PDFA1"));
        assert!(t.contains("(sRGB IEC61966-2.1)"));
    }

    #[test]
    fn info_includes_dates_when_present() {
        let meta = DocumentMeta {
            pdf_date: Some("D:20260718090000+00'00'".to_string()),
            ..Default::default()
        };
        let mut sink = PdfSink::new(Vec::new(), "1.4").unwrap();
        write_info(&mut sink, &meta).unwrap();
        let t = String::from_utf8_lossy(&sink.finish().unwrap()).into_owned();
        assert!(t.contains("/Producer (Lege PDF Generator)"));
        assert!(t.contains("/CreationDate (D:20260718090000+00'00')"));
        assert!(t.contains("/Trapped /False"));
    }
}
