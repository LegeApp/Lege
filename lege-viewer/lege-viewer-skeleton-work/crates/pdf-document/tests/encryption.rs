#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panics are the assertion mechanism

//! End-to-end encryption wiring: build an RC4-encrypted document with
//! `pdf-security`'s own handler (RC4 is symmetric, so the handler *encrypts*
//! when fed plaintext), then open it through `DocumentSnapshot` and assert the
//! decrypted strings and stream bodies come back as the original plaintext.
//!
//! This exercises the wiring — key derivation from `/Encrypt`, string
//! decryption, stream-body decryption, and the two exemptions the spec
//! requires (the `/Encrypt` dict itself, and objects inside an object stream) —
//! not the ciphers themselves, which `pdf-security`'s own tests pin against
//! published vectors.

use std::sync::Arc;

use pdf_document::{DocumentLimits, DocumentSnapshot, ParseContext};
use pdf_object::{ObjectId, PdfObject};
use pdf_security::{Cipher, EncryptDict, StandardHandler};
use pdf_source::{OwnedBytesSource, PdfSource};
use pdf_test_support::builder::PdfBuilder;

const FILE_ID: [u8; 16] = [
    0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC, 0xDE, 0xF0, 0x0F, 0xED, 0xCB, 0xA9, 0x87, 0x65, 0x43, 0x21,
];
const O_ENTRY: [u8; 32] = [0xAB; 32];
const P: i32 = -44;

/// A conforming `/U` for the empty user password: open now genuinely
/// validates `/U` (Algorithm 6), so the fixture must store the value a real
/// writer would.
fn u_entry() -> Vec<u8> {
    StandardHandler::compute_user_entry(&encrypt_fields(vec![]), "")
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// The `/Encrypt` fields shared by the fixture and the in-test encryptor, so
/// the key the fixture is built with is the key `open()` re-derives.
fn encrypt_fields(u: Vec<u8>) -> EncryptDict {
    EncryptDict {
        v: 2,
        r: 3,
        o: O_ENTRY.to_vec(),
        u,
        ue: vec![],
        oe: vec![],
        p: P,
        perms: vec![],
        key_bytes: 16,
        encrypt_metadata: true,
        cipher: Cipher::Rc4,
        file_id: FILE_ID.to_vec(),
    }
}

fn encrypt_dict() -> EncryptDict {
    encrypt_fields(u_entry())
}

/// `/Encrypt` dictionary body as written into the fixture — the same fields the
/// handler above derives its key from.
fn encrypt_dict_body() -> String {
    format!(
        "<</Filter/Standard/V 2/R 3/Length 128/P {P}/O<{}>/U<{}>>>",
        hex(&O_ENTRY),
        hex(&u_entry()),
    )
}

fn id_trailer() -> String {
    let id = hex(&FILE_ID);
    format!("/ID[<{id}><{id}>]")
}

fn open_bytes(bytes: Vec<u8>) -> DocumentSnapshot {
    let source: Arc<dyn PdfSource> = Arc::new(OwnedBytesSource::new(bytes));
    DocumentSnapshot::open(source, DocumentLimits::default()).expect("encrypted open failed")
}

#[test]
fn rc4_strings_and_streams_round_trip() {
    let handler = StandardHandler::open(&encrypt_dict()).expect("handler");

    // Object 5: a page content stream. Encrypt the plaintext with obj 5's key.
    let content_plain = b"BT /F1 12 Tf 72 720 Td (Secret) Tj ET".to_vec();
    let mut content_ct = content_plain.clone();
    handler.decrypt(5, 0, &mut content_ct); // RC4 symmetric -> ciphertext

    // Object 6: a dictionary carrying an encrypted /Title string.
    let title_plain = b"Confidential".to_vec();
    let mut title_ct = title_plain.clone();
    handler.decrypt(6, 0, &mut title_ct);

    let mut b = PdfBuilder::new();
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b.add_object(2, "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 612 792]>>");
    b.add_object(3, "<</Type/Page/Parent 2 0 R/Contents 5 0 R/Resources<<>>>>");
    b.add_object(4, &encrypt_dict_body());
    b.add_stream(5, "", &content_ct);
    b.add_object(6, &format!("<</Title<{}>>>", hex(&title_ct)));
    b.finish_classic_xref(&format!("/Root 1 0 R/Encrypt 4 0 R{}", id_trailer()));
    let snapshot = open_bytes(b.into_bytes());

    assert!(snapshot.security().is_some());
    let mut ctx = ParseContext::new();

    // The /Title string decrypts back to plaintext.
    let obj6 = snapshot.objects().resolve(&snapshot, ObjectId::new(6, 0), &mut ctx).unwrap();
    let title_id = snapshot.names().lookup(b"Title").expect("Title interned");
    let title = obj6.as_dict().unwrap().get(title_id).expect("has /Title");
    match title {
        PdfObject::String(s) => assert_eq!(s.as_bytes(), &title_plain[..], "title not decrypted"),
        other => panic!("expected string, got {other:?}"),
    }

    // The content stream body decrypts, then decodes to the plaintext program.
    let obj5 = snapshot.objects().resolve(&snapshot, ObjectId::new(5, 0), &mut ctx).unwrap();
    let PdfObject::Stream(stream) = &*obj5 else {
        panic!("object 5 is not a stream");
    };
    let decoded = snapshot.decode_stream_data(stream, &mut ctx).unwrap();
    assert_eq!(decoded, content_plain, "stream body not decrypted");
}

#[test]
fn encrypt_dict_is_never_decrypted() {
    // The /Encrypt dict holds the very O/U the key derives from; if the
    // resolver decrypted it, the round trip above could not derive the right
    // key. Assert directly that resolving the /Encrypt object leaves O intact.
    let handler = StandardHandler::open(&encrypt_dict()).expect("handler");
    let mut title_ct = b"x".to_vec();
    handler.decrypt(6, 0, &mut title_ct);

    let mut b = PdfBuilder::new();
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b.add_object(2, "<</Type/Pages/Kids[3 0 R]/Count 1>>");
    b.add_object(3, "<</Type/Page/Parent 2 0 R>>");
    b.add_object(4, &encrypt_dict_body());
    b.finish_classic_xref(&format!("/Root 1 0 R/Encrypt 4 0 R{}", id_trailer()));
    let snapshot = open_bytes(b.into_bytes());

    let mut ctx = ParseContext::new();
    let enc = snapshot.objects().resolve(&snapshot, ObjectId::new(4, 0), &mut ctx).unwrap();
    let o_id = snapshot.names().lookup(b"O").expect("O interned");
    match enc.as_dict().unwrap().get(o_id).expect("has /O") {
        PdfObject::String(s) => {
            assert_eq!(s.as_bytes(), &O_ENTRY[..], "/O was wrongly decrypted");
        }
        other => panic!("expected /O string, got {other:?}"),
    }
}

#[test]
fn objstm_members_are_not_double_decrypted() {
    // A member object inside an object stream is *not* individually encrypted:
    // the container stream was decrypted whole. Encrypt only the container
    // body; the member's string must come out as the plaintext embedded in it.
    let handler = StandardHandler::open(&encrypt_dict()).expect("handler");

    let mut b = PdfBuilder::new();
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b.add_object(2, "<</Type/Pages/Kids[3 0 R]/Count 1>>");
    b.add_object(3, "<</Type/Page/Parent 2 0 R>>");
    b.add_object(4, &encrypt_dict_body());
    // Object 7 is the /ObjStm container; object 8 lives inside it (plaintext).
    b.add_object_stream(7, &[(8, "<</Title(Inside)>>")]);
    b.finish_xref_stream(200, &format!("/Root 1 0 R/Encrypt 4 0 R{}", id_trailer()));
    let mut bytes = b.into_bytes();

    // Encrypt the object-stream body in place with obj 7's key. The body is the
    // uncompressed pair table + member bodies the builder wrote (see
    // `add_object_stream`); RC4 preserves its length, so /Length stays valid.
    let body_plain = b"8 0 <</Title(Inside)>>";
    let start = bytes
        .windows(body_plain.len())
        .position(|w| w == body_plain)
        .expect("objstm body present in fixture");
    let mut region = bytes[start..start + body_plain.len()].to_vec();
    handler.decrypt(7, 0, &mut region); // RC4 -> ciphertext, same length
    bytes[start..start + body_plain.len()].copy_from_slice(&region);

    let snapshot = open_bytes(bytes);
    let mut ctx = ParseContext::new();
    let obj8 = snapshot.objects().resolve(&snapshot, ObjectId::new(8, 0), &mut ctx).unwrap();
    let title_id = snapshot.names().lookup(b"Title").expect("Title interned");
    match obj8.as_dict().unwrap().get(title_id).expect("has /Title") {
        PdfObject::String(s) => {
            assert_eq!(s.as_bytes(), b"Inside", "objstm member string was double-decrypted");
        }
        other => panic!("expected string, got {other:?}"),
    }
}
