#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panics are the assertion mechanism

//! AES-256 (V5, revision 6) decryption end to end.
//!
//! `aes256_r6_empty_user_password.pdf` is pdf.js's `empty_protected.pdf` — a
//! real AES-256/R6 file with an empty user password (owner-protected). Opening
//! it exercises the whole crypto path: Algorithm 2.B password hashing, `/U`
//! validation, and AES-256 decryption of `/UE` to recover the file key. Before
//! AES-256 landed this file was declined with `UnsupportedScheme`. It renders
//! blank in both PDFium and our engine (it is genuinely empty); the content-
//! decryption path is additionally verified against PDFium on `issue7665.pdf`
//! (inkΔ 0.0008) in the differential oracle.

use std::sync::Arc;

use pdf_document::{DocumentLimits, DocumentSnapshot};
use pdf_source::{OwnedBytesSource, PdfSource};

fn fixture(name: &str) -> Vec<u8> {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(&path).unwrap_or_else(|e| panic!("read {path}: {e}"))
}

#[test]
fn aes256_r6_empty_user_password_opens() {
    let bytes = fixture("aes256_r6_empty_user_password.pdf");
    let source: Arc<dyn PdfSource> = Arc::new(OwnedBytesSource::new(bytes));
    let snap = DocumentSnapshot::open(source, DocumentLimits::default())
        .expect("AES-256/R6 with empty user password must open (was UnsupportedScheme)");
    // The file is a single (empty) page; the point is that the standard handler
    // derived the file key and did not decline the V5 scheme.
    assert_eq!(snap.page_count(), 1);
}
