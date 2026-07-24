#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panics are the assertion mechanism

//! Password threading end to end, against PDFium's own encrypted fixtures
//! (`testing/resources/encrypted_hello_world_r{2,3,6}.pdf`; user password
//! "hôtel", owner password "âge" per cpdf_security_handler_embeddertest.cpp).
//!
//! - `open_with_password` opens each revision with the user *and* the owner
//!   password and reports the authenticating [`PasswordRole`].
//! - With no (or a wrong) password the open fails **typed** —
//!   `PasswordRequired` / `IncorrectPassword` — never garbage content.

use std::sync::Arc;

use pdf_document::{DocumentError, DocumentLimits, DocumentSnapshot, PageIndex, ParseContext};
use pdf_security::{PasswordRole, SecurityError};
use pdf_source::{OwnedBytesSource, PdfSource};

const USER: &str = "h\u{f4}tel"; // "hôtel"
const OWNER: &str = "\u{e2}ge"; // "âge"

fn fixture(name: &str) -> Arc<dyn PdfSource> {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    Arc::new(OwnedBytesSource::new(bytes))
}

fn open_pw(name: &str, password: Option<&str>) -> Result<DocumentSnapshot, DocumentError> {
    DocumentSnapshot::open_with_password(fixture(name), DocumentLimits::default(), password)
}

/// The page's content stream must decode to real text ops — proof the file
/// key is right, not merely that structure parsed.
fn assert_content_decrypts(snap: &DocumentSnapshot) {
    let mut ctx = ParseContext::new();
    let page = snap.page(PageIndex(0)).expect("page 0").clone();
    let pdf_object::PdfObject::Reference(id) = page.contents.expect("page has contents") else {
        panic!("contents not a single reference")
    };
    let obj = snap.objects().resolve(snap, id, &mut ctx).expect("resolve contents");
    let pdf_object::PdfObject::Stream(stream) = &*obj else { panic!("contents not a stream") };
    let data = snap.decode_stream_data(stream, &mut ctx).expect("decode contents");
    let text = String::from_utf8_lossy(&data);
    assert!(text.contains("Hello, world!"), "content did not decrypt: {text:?}");
}

#[test]
fn all_revisions_open_with_user_and_owner_passwords() {
    for name in
        ["encrypted_hello_world_r2.pdf", "encrypted_hello_world_r3.pdf", "encrypted_hello_world_r6.pdf"]
    {
        let snap = open_pw(name, Some(USER)).unwrap_or_else(|e| panic!("{name} user open: {e}"));
        let role = snap.security().and_then(|s| s.password_role());
        assert_eq!(role, Some(PasswordRole::User), "{name}");
        assert_content_decrypts(&snap);

        let snap = open_pw(name, Some(OWNER)).unwrap_or_else(|e| panic!("{name} owner open: {e}"));
        let role = snap.security().and_then(|s| s.password_role());
        assert_eq!(role, Some(PasswordRole::Owner), "{name}");
        assert_content_decrypts(&snap);
    }
}

#[test]
fn missing_password_fails_typed() {
    for name in
        ["encrypted_hello_world_r2.pdf", "encrypted_hello_world_r3.pdf", "encrypted_hello_world_r6.pdf"]
    {
        let err = open_pw(name, None).expect_err(name);
        assert!(
            matches!(err, DocumentError::Security(SecurityError::PasswordRequired)),
            "{name}: expected PasswordRequired, got {err:?}"
        );
    }
}

#[test]
fn wrong_password_fails_typed() {
    for name in
        ["encrypted_hello_world_r2.pdf", "encrypted_hello_world_r3.pdf", "encrypted_hello_world_r6.pdf"]
    {
        let err = open_pw(name, Some("wrong")).expect_err(name);
        assert!(
            matches!(err, DocumentError::Security(SecurityError::IncorrectPassword)),
            "{name}: expected IncorrectPassword, got {err:?}"
        );
    }
}

#[test]
fn plain_open_still_delegates() {
    // `open` == `open_with_password(.., None)`.
    let err = DocumentSnapshot::open(
        fixture("encrypted_hello_world_r2.pdf"),
        DocumentLimits::default(),
    )
    .expect_err("no password");
    assert!(matches!(err, DocumentError::Security(SecurityError::PasswordRequired)));
}
