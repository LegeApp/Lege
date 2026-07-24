#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panics are the assertion mechanism

//! Phase 1 exit gate (roadmap §7 Phase 1): six threads resolve six
//! different pages' structure from one snapshot, deterministically, with
//! zero document-wide locks — plus the malformed-fixture matrix.

use std::sync::Arc;

use pdf_document::{DocumentError, DocumentLimits, DocumentSnapshot, PageIndex, ParseContext};
use pdf_object::{ObjectId, PdfObject};
use pdf_source::{OwnedBytesSource, PdfSource};
use pdf_structure::RecoveryEvent;
use pdf_test_support::builder::{self, layout};

fn open_bytes(bytes: Vec<u8>) -> DocumentSnapshot {
    let source: Arc<dyn PdfSource> = Arc::new(OwnedBytesSource::new(bytes));
    DocumentSnapshot::open(source, DocumentLimits::default()).expect("open failed")
}

/// Everything one worker extracts from one page — the unit of determinism
/// comparison across runs and thread counts.
#[derive(Debug, PartialEq)]
struct PageProbe {
    object: ObjectId,
    media_box: [f64; 4],
    crop_box: [f64; 4],
    rotate: u16,
    content_bytes: Vec<u8>,
    font_base: Vec<u8>,
}

/// Resolve one page's structure through a fresh worker-owned context.
fn probe_page(snapshot: &DocumentSnapshot, index: u32) -> PageProbe {
    let mut ctx = ParseContext::new();
    let page = snapshot.page(PageIndex(index)).expect("page missing").clone();

    // Contents: reference → stream → decoded body.
    let contents_ref = page.contents.clone().expect("page has no /Contents");
    let contents_id = contents_ref.as_reference().expect("contents should be a reference");
    let contents = snapshot
        .objects()
        .resolve(snapshot, contents_id, &mut ctx)
        .expect("contents resolution failed");
    let PdfObject::Stream(stream) = &*contents else {
        panic!("contents is not a stream");
    };
    let content_bytes = snapshot.decode_stream_data(stream, &mut ctx).expect("decode failed");

    // Font: reaches through the /Resources dict — in the exit fixture this
    // crosses into an object stream member.
    let resources = page.resources.clone().expect("page has no /Resources");
    let resources = match resources {
        PdfObject::Reference(id) => {
            snapshot.objects().resolve(snapshot, id, &mut ctx).expect("resources")
        }
        direct => Arc::new(direct),
    };
    let names = snapshot.names();
    let font_dict = resources
        .as_dict()
        .and_then(|d| d.get(names.known.font))
        .and_then(|f| f.as_dict())
        .expect("resources /Font");
    let f1 = font_dict.get(names.lookup(b"F1").expect("F1 interned")).expect("F1 entry");
    let font_id = f1.as_reference().expect("F1 is a reference");
    let font = snapshot.objects().resolve(snapshot, font_id, &mut ctx).expect("font resolution");
    let font_base = font
        .as_dict()
        .and_then(|d| d.get(names.lookup(b"BaseFont")?))
        .and_then(PdfObject::as_name)
        .map(|n| names.resolve(n).to_vec())
        .expect("font /BaseFont");

    PageProbe {
        object: page.object,
        media_box: page.media_box,
        crop_box: page.crop_box,
        rotate: page.rotate,
        content_bytes,
        font_base,
    }
}

/// Probe all pages using `threads` worker threads, each with its own
/// `ParseContext`, one page per worker at a time.
fn probe_all(snapshot: &DocumentSnapshot, threads: usize) -> Vec<PageProbe> {
    let page_count = snapshot.page_count() as usize;
    let mut results: Vec<Option<PageProbe>> = (0..page_count).map(|_| None).collect();
    let next = std::sync::atomic::AtomicUsize::new(0);
    let slots: Vec<std::sync::Mutex<Option<PageProbe>>> =
        (0..page_count).map(|_| std::sync::Mutex::new(None)).collect();
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                loop {
                    let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if i >= page_count {
                        return;
                    }
                    *slots[i].lock().unwrap() = Some(probe_page(snapshot, i as u32));
                }
            });
        }
    });
    for (i, slot) in slots.into_iter().enumerate() {
        results[i] = slot.into_inner().unwrap();
    }
    results.into_iter().map(|p| p.expect("page not probed")).collect()
}

// --- The exit gate itself -------------------------------------------------

#[test]
fn six_threads_resolve_six_pages_deterministically() {
    let snapshot = open_bytes(builder::phase1_exit_fixture(6));
    assert_eq!(snapshot.page_count(), 6);

    // Baseline: single-threaded.
    let baseline = probe_all(&snapshot, 1);

    // Sanity on the baseline itself.
    for (i, probe) in baseline.iter().enumerate() {
        assert_eq!(probe.object, ObjectId::new(layout::page(i as u32), 0));
        assert_eq!(probe.media_box, [0.0, 0.0, 612.0, 792.0]);
        assert_eq!(probe.font_base, b"Helvetica".to_vec());
        let expected = if i == 0 {
            layout::updated_content_bytes(0) // incremental update wins
        } else {
            layout::content_bytes(i as u32)
        };
        assert_eq!(probe.content_bytes, expected, "page {i} content");
    }
    // Page 1 carries its own rotation/crop; others inherit defaults.
    assert_eq!(baseline[1].rotate, 90);
    assert_eq!(baseline[1].crop_box, [10.0, 10.0, 400.0, 500.0]);
    assert_eq!(baseline[0].rotate, 0);
    assert_eq!(baseline[0].crop_box, baseline[0].media_box);

    // Determinism: thread counts 1/2/6, three repeated runs each, all on
    // the same snapshot, must agree exactly.
    for threads in [1usize, 2, 6] {
        for run in 0..3 {
            let probes = probe_all(&snapshot, threads);
            assert_eq!(probes, baseline, "threads={threads} run={run} diverged");
        }
    }
}

#[test]
fn concurrent_resolution_of_shared_object_stream() {
    // All six workers resolve the SAME object-stream members concurrently
    // (each decompresses into its own context — Design A).
    let snapshot = open_bytes(builder::phase1_exit_fixture(6));
    std::thread::scope(|scope| {
        for _ in 0..6 {
            scope.spawn(|| {
                let mut ctx = ParseContext::new();
                let font = snapshot
                    .objects()
                    .resolve(&snapshot, ObjectId::new(layout::FONT, 0), &mut ctx)
                    .expect("font");
                assert!(font.as_dict().is_some());
                let extra = snapshot
                    .objects()
                    .resolve(&snapshot, ObjectId::new(layout::EXTRA, 0), &mut ctx)
                    .expect("extra");
                assert!(extra.as_dict().is_some());
            });
        }
    });
}

// --- Malformed-fixture matrix ----------------------------------------------

#[test]
fn garbage_prefixed_header_opens() {
    let bytes =
        builder::with_leading_garbage(builder::phase1_exit_fixture(3), &[0xAB; 512]);
    let snapshot = open_bytes(bytes);
    assert_eq!(snapshot.structure().header_offset, 512);
    assert_eq!(snapshot.page_count(), 3);
    let probe = probe_page(&snapshot, 2);
    assert_eq!(probe.content_bytes, layout::content_bytes(2));
}

#[test]
fn missing_startxref_recovers_by_scanning_for_xref() {
    // The table survives; only startxref is gone → keyword-scan recovery.
    let bytes = builder::without_startxref(builder::multipage_classic(3));
    let snapshot = open_bytes(bytes);
    assert!(
        snapshot
            .recovery_events()
            .iter()
            .any(|e| matches!(e, RecoveryEvent::StartXrefRecovered { reported: None, .. })),
        "expected StartXrefRecovered, got {:?}",
        snapshot.recovery_events()
    );
    assert_eq!(snapshot.page_count(), 3);
    let probe = probe_page(&snapshot, 1);
    assert_eq!(probe.content_bytes, layout::content_bytes(1));
}

#[test]
fn missing_xref_section_rebuilds() {
    // Table, trailer, and startxref all gone → full-file reconstruction,
    // including the /Type /Catalog hunt for the root.
    let bytes = builder::without_xref(builder::multipage_classic(3));
    let snapshot = open_bytes(bytes);
    assert!(
        snapshot.recovery_events().iter().any(|e| matches!(e, RecoveryEvent::XrefRebuilt)),
        "expected XrefRebuilt, got {:?}",
        snapshot.recovery_events()
    );
    assert_eq!(snapshot.page_count(), 3);
    let probe = probe_page(&snapshot, 1);
    assert_eq!(probe.content_bytes, layout::content_bytes(1));
}

#[test]
fn wrong_length_stream_repaired_and_observable() {
    let snapshot = open_bytes(builder::wrong_length_fixture());
    let mut ctx = ParseContext::new();
    let page = snapshot.page(PageIndex(0)).unwrap().clone();
    let contents_id = page.contents.as_ref().and_then(PdfObject::as_reference).unwrap();
    let contents = snapshot.objects().resolve(&snapshot, contents_id, &mut ctx).unwrap();
    let PdfObject::Stream(stream) = &*contents else { panic!("not a stream") };
    let data = snapshot.decode_stream_data(stream, &mut ctx).unwrap();
    assert_eq!(data, layout::content_bytes(0));
    // The repair happened during THIS worker's resolution: it must be
    // observable on the worker's context (the snapshot froze at open).
    assert!(
        ctx.recovery.iter().any(|e| matches!(
            e,
            RecoveryEvent::StreamLengthRepaired { declared: Some(3), .. }
        )),
        "repair not recorded: {:?}",
        ctx.recovery
    );
}

#[test]
fn xref_stream_only_file_opens() {
    let snapshot = open_bytes(builder::xref_stream_fixture(2));
    assert_eq!(snapshot.page_count(), 2);
    let probe = probe_page(&snapshot, 0);
    assert_eq!(probe.font_base, b"Helvetica".to_vec());
    assert_eq!(probe.content_bytes, layout::content_bytes(0));
}

#[test]
fn hybrid_file_opens() {
    let snapshot = open_bytes(builder::hybrid_fixture(2));
    assert_eq!(snapshot.page_count(), 2);
    // The font lives in an object stream only the /XRefStm knows about.
    let probe = probe_page(&snapshot, 1);
    assert_eq!(probe.font_base, b"Helvetica".to_vec());
}

#[test]
fn supported_rc4_document_opens() {
    // RC4 V2/R3 is a supported scheme: the handler derives a key at open and
    // the document parses (rather than being refused as it was pre-wiring).
    let snapshot = open_bytes(builder::encrypted_fixture());
    assert_eq!(snapshot.page_count(), 1);
    assert!(snapshot.security().is_some(), "an encrypted doc must carry a security context");
}

#[test]
fn unsupported_aes256_is_refused_not_pretended() {
    // AES-256 / R6 is declined by the standard handler: open must fail with a
    // typed security error, never pretend the file parsed cleanly.
    let source: Arc<dyn PdfSource> =
        Arc::new(OwnedBytesSource::new(builder::aes256_encrypted_fixture()));
    let err = DocumentSnapshot::open(source, DocumentLimits::default())
        .expect_err("AES-256 file must not open");
    assert!(
        matches!(err, DocumentError::Security(_)),
        "expected a security error, got {err:?}"
    );
}

#[test]
fn direct_encrypt_dict_is_handled() {
    // /Encrypt written as a direct dictionary instead of a reference must be
    // recognized (not slipped past the reference-only trailer field). V2 is
    // supported, so this opens.
    // A conforming /U for the empty user password (open validates /U now).
    let enc = pdf_security::EncryptDict {
        v: 2,
        r: 3,
        o: b"o".to_vec(),
        u: vec![],
        ue: vec![],
        oe: vec![],
        p: -4,
        perms: vec![],
        key_bytes: 16,
        encrypt_metadata: true,
        cipher: pdf_security::Cipher::Rc4,
        file_id: vec![0x01],
    };
    let u = pdf_security::StandardHandler::compute_user_entry(&enc, "");
    let u_hex: String = u.iter().map(|b| format!("{b:02x}")).collect();
    let mut builder = pdf_test_support::builder::PdfBuilder::new();
    builder.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    builder.add_object(2, "<</Type/Pages/Kids[]/Count 0>>");
    builder.finish_classic_xref(&format!(
        "/Root 1 0 R/Encrypt<</Filter/Standard/V 2/R 3/Length 128/O(o)/U<{u_hex}>/P -4>>/ID[<01><02>]"
    ));
    let source: Arc<dyn PdfSource> = Arc::new(OwnedBytesSource::new(builder.into_bytes()));
    let snapshot = DocumentSnapshot::open(source, DocumentLimits::default())
        .expect("direct-dict V2 encryption should open");
    assert!(snapshot.security().is_some());
}

#[test]
fn snapshot_is_immutable_shareable_and_cheap_to_clone() {
    let snapshot = open_bytes(builder::multipage_classic(2));
    let clone = snapshot.clone();
    std::thread::scope(|scope| {
        scope.spawn(move || {
            assert_eq!(clone.page_count(), 2);
        });
        scope.spawn(|| {
            assert_eq!(snapshot.page_count(), 2);
        });
    });
}

#[test]
fn max_pages_limit_enforced() {
    let source: Arc<dyn PdfSource> =
        Arc::new(OwnedBytesSource::new(builder::multipage_classic(5)));
    let limits = DocumentLimits { max_pages: 3, ..Default::default() };
    assert!(matches!(
        DocumentSnapshot::open(source, limits),
        Err(DocumentError::LimitExceeded("max_pages"))
    ));
}

#[test]
fn decode_budget_limit_enforced() {
    let source: Arc<dyn PdfSource> =
        Arc::new(OwnedBytesSource::new(builder::phase1_exit_fixture(1)));
    // A budget too small for even the object stream: opening still works
    // (nothing at open time decompresses beyond it in this fixture)…
    let snapshot =
        DocumentSnapshot::open(source, DocumentLimits::default()).expect("open");
    let mut ctx = ParseContext::new();
    ctx.decoded_bytes_used = snapshot.limits().max_decoded_bytes_per_context; // exhausted
    let err = snapshot
        .objects()
        .resolve(&snapshot, ObjectId::new(layout::FONT, 0), &mut ctx)
        .expect_err("budget must be enforced");
    assert!(matches!(err, DocumentError::LimitExceeded("max_decoded_bytes_per_context")));
}

#[test]
fn reference_to_free_object_resolves_to_null() {
    let snapshot = open_bytes(builder::multipage_classic(1));
    let mut ctx = ParseContext::new();
    let missing = snapshot
        .objects()
        .resolve(&snapshot, ObjectId::new(9999, 0), &mut ctx)
        .expect("free object must resolve");
    assert!(matches!(&*missing, PdfObject::Null));
}

/// The exit gate again, but over the real file-backed sources: mmap
/// (contiguous, zero-copy windows) and positional reads (windowed parsing).
#[test]
fn exit_gate_over_mmap_and_file_sources() {
    let bytes = builder::phase1_exit_fixture(6);
    let mut path = std::env::temp_dir();
    path.push(format!("phase1-exit-{}.pdf", std::process::id()));
    std::fs::write(&path, &bytes).unwrap();

    for contiguous in [true, false] {
        let source: Arc<dyn PdfSource> = if contiguous {
            Arc::new(pdf_source::MmapSource::open(&path).unwrap())
        } else {
            Arc::new(pdf_source::FileReadAtSource::open(&path).unwrap())
        };
        let snapshot =
            DocumentSnapshot::open(source, DocumentLimits::default()).expect("open failed");
        assert_eq!(snapshot.page_count(), 6);
        let probes = probe_all(&snapshot, 6);
        let baseline = probe_all(&snapshot, 1);
        assert_eq!(probes, baseline, "contiguous={contiguous}");
        drop(snapshot);
    }
    let _ = std::fs::remove_file(path);
}

#[test]
fn worker_local_cache_hits_are_stable() {
    let snapshot = open_bytes(builder::multipage_classic(2));
    let mut ctx = ParseContext::new();
    let id = ObjectId::new(layout::page(0), 0);
    let first = snapshot.objects().resolve(&snapshot, id, &mut ctx).unwrap();
    let second = snapshot.objects().resolve(&snapshot, id, &mut ctx).unwrap();
    // Same Arc — the worker cache returned its published copy.
    assert!(Arc::ptr_eq(&first, &second));
}

#[test]
fn truncated_page_tree_synthesizes_blank_placeholders_to_declared_count() {
    // /Pages declares /Count 4 over three kids; the third kid reference
    // points at an object the file does not contain (a physically truncated
    // tail, the Hoyos "739 vs 845" shape). PDFium reports the declared
    // count and yields blank pages for the unloadable tail — so must we.
    let mut b = pdf_test_support::builder::PdfBuilder::new();
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b.add_object(
        2,
        "<</Type/Pages/Kids[3 0 R 4 0 R 99 0 R]/Count 4/MediaBox[0 0 200 200]>>",
    );
    b.add_object(3, "<</Type/Page/Parent 2 0 R>>");
    b.add_object(4, "<</Type/Page/Parent 2 0 R>>");
    // 99 0 R is never emitted: unresolvable subtree.
    b.finish_classic_xref("/Root 1 0 R");
    let snap = open_bytes(b.into_bytes());
    assert_eq!(snap.page_count(), 4, "declared count is honored");
    // The placeholders are blank but present and structurally usable.
    let p = snap.page(PageIndex(3)).expect("placeholder page exists");
    assert!(p.contents.is_none());

    // A merely-lying /Count with NO lost subtree must not fabricate pages.
    let mut b2 = pdf_test_support::builder::PdfBuilder::new();
    b2.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b2.add_object(2, "<</Type/Pages/Kids[3 0 R]/Count 9/MediaBox[0 0 200 200]>>");
    b2.add_object(3, "<</Type/Page/Parent 2 0 R>>");
    b2.finish_classic_xref("/Root 1 0 R");
    let snap2 = open_bytes(b2.into_bytes());
    assert_eq!(snap2.page_count(), 1, "a lying /Count alone synthesizes nothing");
}
