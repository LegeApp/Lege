//! `pdf-read` — a qpdf-style **document doctor**, read-only over the
//! existing machinery.
//!
//! [`examine`] opens a document (tolerating failure), probes structure,
//! encryption, per-page compilability, annotations, and feature flags, and
//! returns a [`DocumentReport`]. Nothing here parses PDF syntax itself:
//! every byte is read through `pdf-structure`/`pdf-document` resolvers, and
//! page content through `pdf-content`'s compiler. No new parsers.
//!
//! The report never panics and never fails: a document that cannot even be
//! structurally loaded still yields a report whose `open` is
//! [`OpenOutcome::Failed`].

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::Arc;

use pdf_content::PageCompiler;
use pdf_document::{DocumentLimits, DocumentSnapshot, PageIndex, ParseContext};
use pdf_object::{Dictionary, NameId, NameTable, ObjectId, PdfObject};
use pdf_source::PdfSource;
use pdf_structure::{ObjectLocation, RecoveryEvent};

/// How the document opened.
#[derive(Debug, Clone)]
pub enum OpenOutcome {
    /// Clean open: no recovery of any kind was needed.
    Ok,
    /// Opened, but only after structural repair; `how` lists each repair.
    Recovered { how: Vec<String> },
    /// Could not be opened; the typed error, rendered.
    Failed { error: String },
}

/// What the `/Encrypt` dictionary declares (plus whether the empty user
/// password opens the file).
#[derive(Debug, Clone)]
pub struct EncryptionInfo {
    /// `/V` — algorithm version (0 when unreadable).
    pub version: i64,
    /// `/R` — standard-handler revision (0 when unreadable).
    pub revision: i64,
    /// Human name of the method ("RC4-40", "RC4-128", "AES-128", "AES-256",
    /// or "unknown").
    pub method: String,
    /// Whether the empty user password genuinely validates against `/U`
    /// (the common permissions-only encryption) — computed via
    /// `StandardHandler::verify_password(enc, "")`, independent of how (or
    /// whether) the document actually opened.
    pub user_password_empty: bool,
}

/// The `%PDF` header, and how far into the file it was found.
#[derive(Debug, Clone)]
pub struct HeaderInfo {
    /// `/Version` major from the header, e.g. 1 in `%PDF-1.7`.
    pub major: u8,
    /// `/Version` minor from the header, e.g. 7 in `%PDF-1.7`.
    pub minor: u8,
    /// Bytes of leading garbage before `%PDF`. Zero for a well-formed file;
    /// non-zero means every structural offset in the file is shifted by this
    /// much relative to the source.
    pub header_offset: u64,
}

/// Identity and provenance the document declares about itself.
///
/// Neutral facts only: these are what the file *says*, with no judgement about
/// whether it is telling the truth. The trailer `/ID` pair is the interesting
/// one — its first element is meant to persist across a document's whole edit
/// history while the second changes on each save, so the pair carries lineage
/// that survives independently of any metadata a writer chose to update.
#[derive(Debug, Clone, Default)]
pub struct DocumentIdentity {
    /// Lowercase hex of the permanent trailer `/ID[0]`, if present.
    pub id_permanent: Option<String>,
    /// Lowercase hex of the changing trailer `/ID[1]`, if present.
    pub id_changing: Option<String>,
    /// `/Info` dictionary entries with string values, by key. Includes the
    /// conventional `/Producer`, `/Creator`, `/CreationDate` and `/ModDate`,
    /// and any non-standard keys a writer added, which are often the more
    /// telling ones.
    pub info: BTreeMap<String, String>,
    /// Whether the catalog carries an XMP `/Metadata` stream. The stream's
    /// contents — including the `xmpMM` lineage identifiers — are not parsed
    /// here yet.
    pub has_xmp_metadata: bool,
}

/// One signature field and what it covers.
#[derive(Debug, Clone)]
pub struct SignatureInfo {
    /// The field's `/T` name, if it has one.
    pub field_name: Option<String>,
    /// `/Filter` — the handler that produced the signature, e.g. `Adobe.PPKLite`.
    pub filter: Option<String>,
    /// `/SubFilter` — the signature encoding, e.g. `ETSI.CAdES.detached`.
    pub sub_filter: Option<String>,
    /// `/M` — the claimed signing time, verbatim and unparsed.
    pub signed_at: Option<String>,
    /// `/ByteRange`, verbatim. Conventionally four numbers: two
    /// offset/length pairs straddling the signature itself.
    pub byte_range: Vec<i64>,
    /// Whether `/ByteRange` spans the entire file — that is, starts at zero
    /// and its last pair ends at the final byte.
    ///
    /// A signature covering less than the whole file is the structural half of
    /// the classic incremental-update attack: content appended after the
    /// signed range is not signed, however valid the cryptography over the
    /// range turns out to be. This says nothing about whether the signature
    /// verifies; that is a separate, later question.
    pub covers_whole_file: bool,
}

/// Signature fields found in the `/AcroForm`.
#[derive(Debug, Clone, Default)]
pub struct SignatureInventory {
    /// Every `/FT /Sig` field, in `/Fields` order.
    pub signatures: Vec<SignatureInfo>,
    /// Signature fields present but carrying no `/V` signature dictionary —
    /// an unsigned signature field, which is ordinary in a blank form.
    pub unsigned_fields: u32,
}

/// Cross-reference health.
#[derive(Debug, Clone, Default)]
pub struct XrefHealth {
    /// Any structural repair ran during open.
    pub recovery_used: bool,
    /// The xref was rebuilt by a full object scan (the heaviest repair).
    pub rebuilt: bool,
    /// Number of revisions (original + incremental updates).
    pub revision_count: usize,
}

/// Per-page compile outcome.
#[derive(Debug, Clone)]
pub enum CompileStatus {
    Ok {
        op_count: usize,
    },
    /// Compiled, but only with lazy repairs along the way.
    Degraded {
        op_count: usize,
        detail: String,
    },
    Failed {
        error: String,
    },
}

/// One page's health.
#[derive(Debug, Clone)]
pub struct PageStatus {
    pub index: u32,
    /// Whether a `/MediaBox` is actually present on the page or an ancestor
    /// (as opposed to defaulted during page-tree flattening).
    pub media_box_present: bool,
    pub compile: CompileStatus,
}

/// Count of annotations per `/Subtype`.
#[derive(Debug, Clone, Default)]
pub struct AnnotationInventory {
    pub count_per_subtype: BTreeMap<String, u32>,
}

impl AnnotationInventory {
    pub fn total(&self) -> u32 {
        self.count_per_subtype.values().sum()
    }
}

/// Document-level feature flags an embedder cares about.
#[derive(Debug, Clone, Copy, Default)]
pub struct FeatureFlags {
    pub has_acroform: bool,
    pub has_xfa: bool,
    /// Document-level JavaScript (`/Names /JavaScript` name tree).
    pub has_javascript: bool,
    pub has_outlines: bool,
    pub has_optional_content: bool,
    pub uses_object_streams: bool,
    /// The name tree carries `/EmbeddedFiles` — the document has file
    /// attachments.
    pub has_embedded_files: bool,
}

/// The full examination result.
#[derive(Debug, Clone)]
pub struct DocumentReport {
    pub open: OpenOutcome,
    /// The `%PDF` header. `None` only when no header could be found at all,
    /// which is also the reason the document failed to open.
    pub header: Option<HeaderInfo>,
    pub encryption: Option<EncryptionInfo>,
    pub xref: XrefHealth,
    /// What the document declares about its own identity and provenance.
    pub identity: DocumentIdentity,
    pub pages: Vec<PageStatus>,
    pub annotations: AnnotationInventory,
    /// Signature fields and the byte ranges they cover.
    pub signatures: SignatureInventory,
    pub features: FeatureFlags,
}

/// Cap on `/Parent` hops when walking inherited attributes (cycle guard).
const MAX_PARENT_HOPS: usize = 64;

/// Examine `source` and report. Never fails, never panics: an unopenable
/// document yields a mostly-empty report with `open: Failed`.
pub fn examine(source: Arc<dyn PdfSource>, limits: DocumentLimits) -> DocumentReport {
    examine_with_password(source, limits, None)
}

/// [`examine`] with an optional user/owner password for encrypted documents
/// (threaded into [`DocumentSnapshot::open_with_password`]).
pub fn examine_with_password(
    source: Arc<dyn PdfSource>,
    limits: DocumentLimits,
    password: Option<&str>,
) -> DocumentReport {
    match DocumentSnapshot::open_with_password(Arc::clone(&source), limits.clone(), password) {
        Ok(snapshot) => examine_open(&snapshot),
        Err(err) => examine_failed(source, limits, err.to_string()),
    }
}

/// The happy path: the snapshot opened (possibly with recovery).
fn examine_open(snapshot: &DocumentSnapshot) -> DocumentReport {
    let recovery = snapshot.recovery_events();
    let open = if recovery.is_empty() {
        OpenOutcome::Ok
    } else {
        OpenOutcome::Recovered {
            how: recovery.iter().map(describe_recovery).collect(),
        }
    };
    let xref = XrefHealth {
        recovery_used: !recovery.is_empty(),
        rebuilt: recovery
            .iter()
            .any(|e| matches!(e, RecoveryEvent::XrefRebuilt)),
        revision_count: snapshot.structure().revisions.len(),
    };

    let mut ctx = ParseContext::new();
    let encryption = read_encryption(snapshot, &mut ctx);
    let pages = examine_pages(snapshot);
    let annotations = inventory_annotations(snapshot, &mut ctx);
    let features = read_features(snapshot, &mut ctx);
    let identity = read_identity(snapshot, &mut ctx);
    let signatures = inventory_signatures(snapshot, &mut ctx);

    DocumentReport {
        open,
        header: Some(read_header(snapshot.structure())),
        encryption,
        xref,
        identity,
        pages,
        annotations,
        signatures,
        features,
    }
}

/// Open failed: retry the structural layer alone so the report can still
/// carry xref health and the declared encryption scheme (e.g. a document
/// whose password we don't have).
fn examine_failed(
    source: Arc<dyn PdfSource>,
    limits: DocumentLimits,
    error: String,
) -> DocumentReport {
    let mut report = DocumentReport {
        open: OpenOutcome::Failed { error },
        header: None,
        encryption: None,
        xref: XrefHealth::default(),
        identity: DocumentIdentity::default(),
        pages: Vec::new(),
        annotations: AnnotationInventory::default(),
        signatures: SignatureInventory::default(),
        features: FeatureFlags::default(),
    };

    let names = NameTable::new();
    let structure_limits = pdf_structure::StructureLimits {
        syntax: limits.syntax.clone(),
        max_revisions: limits.max_revisions,
        max_objects: limits.max_objects,
        max_decoded_bytes: limits.max_decoded_bytes_per_context,
    };
    let Ok(structure) = pdf_structure::load_structure(source.as_ref(), &names, &structure_limits)
    else {
        return report;
    };
    report.xref = XrefHealth {
        recovery_used: !structure.recovery.is_empty(),
        rebuilt: structure
            .recovery
            .iter()
            .any(|e| matches!(e, RecoveryEvent::XrefRebuilt)),
        revision_count: structure.revisions.len(),
    };
    report.features.uses_object_streams = xref_uses_object_streams(&structure.xref);
    report.header = Some(read_header(&structure));
    // The trailer `/ID` pair is never encrypted, so lineage survives even for
    // a document we cannot open. `/Info` does not: its strings are encrypted
    // like any others, so it is left to the open path.
    read_file_id(&structure.trailer, &mut report.identity);

    // A security-less snapshot over the loaded structure lets the existing
    // resolver read the (never-encrypted) /Encrypt dictionary for us.
    let snapshot = DocumentSnapshot::from_parts(
        source,
        structure,
        names,
        pdf_document::PageTreeIndex::default(),
        None,
        limits,
    );
    let mut ctx = ParseContext::new();
    report.encryption = read_encryption(&snapshot, &mut ctx);
    report
}

fn describe_recovery(event: &RecoveryEvent) -> String {
    match event {
        RecoveryEvent::StartXrefRecovered { reported, used } => match reported {
            Some(r) => format!("startxref pointed at {r}; recovered chain at {used}"),
            None => format!("startxref missing; recovered chain at {used}"),
        },
        RecoveryEvent::ObjectOffsetRepaired {
            id,
            declared,
            actual,
        } => {
            format!("object {id} xref offset {declared} wrong; found at {actual}")
        }
        RecoveryEvent::StreamLengthRepaired {
            id,
            declared,
            actual,
        } => {
            format!("stream {id} /Length {declared:?} wrong; actual {actual}")
        }
        RecoveryEvent::ReferenceGenerationRepaired {
            id,
            referenced,
            offset,
        } => {
            format!(
                "object {id} reference {referenced} R at offset {offset} omitted generation; assumed 0"
            )
        }
        RecoveryEvent::XrefRebuilt => "cross-reference table rebuilt by full object scan".into(),
        RecoveryEvent::SizeRepaired { declared, actual } => {
            format!("trailer /Size {declared} wrong; actual {actual}")
        }
        RecoveryEvent::Other(s) => s.clone(),
    }
}

// ── helpers over the resolver ───────────────────────────────────────────────

/// Resolve `id` through the snapshot, mapping any failure to `None`.
fn resolve(
    snapshot: &DocumentSnapshot,
    id: ObjectId,
    ctx: &mut ParseContext,
) -> Option<Arc<PdfObject>> {
    snapshot.objects().resolve(snapshot, id, ctx).ok()
}

/// Resolve a possibly-indirect value to its target object.
fn resolve_value(
    snapshot: &DocumentSnapshot,
    value: &PdfObject,
    ctx: &mut ParseContext,
) -> Option<Arc<PdfObject>> {
    match value {
        PdfObject::Reference(id) => resolve(snapshot, *id, ctx),
        other => Some(Arc::new(other.clone())),
    }
}

/// `dict[key]`, resolved if indirect.
fn dict_get_resolved(
    snapshot: &DocumentSnapshot,
    dict: &Dictionary,
    key: NameId,
    ctx: &mut ParseContext,
) -> Option<Arc<PdfObject>> {
    resolve_value(snapshot, dict.get(key)?, ctx)
}

// ── header and identity ─────────────────────────────────────────────────────

fn read_header(structure: &pdf_structure::DocumentStructure) -> HeaderInfo {
    HeaderInfo {
        major: structure.version.major,
        minor: structure.version.minor,
        header_offset: structure.header_offset,
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

/// The trailer `/ID` pair, which needs no decryption.
fn read_file_id(trailer: &pdf_structure::Trailer, identity: &mut DocumentIdentity) {
    if let Some([permanent, changing]) = trailer.file_id.as_ref() {
        identity.id_permanent = Some(hex(permanent.as_bytes()));
        identity.id_changing = Some(hex(changing.as_bytes()));
    }
}

fn read_identity(snapshot: &DocumentSnapshot, ctx: &mut ParseContext) -> DocumentIdentity {
    let mut identity = DocumentIdentity::default();
    let structure = snapshot.structure();
    read_file_id(&structure.trailer, &mut identity);

    let names = snapshot.names();

    // Every /Info entry with a string value, standard or not. Non-standard
    // keys are kept deliberately: a writer that adds its own is identifying
    // itself as surely as /Producer does.
    if let Some(info_id) = structure.trailer.info
        && let Some(info) = resolve(snapshot, info_id, ctx)
        && let Some(info) = info.as_dict()
    {
        for (key, value) in info.iter() {
            let Some(value) = resolve_value(snapshot, value, ctx) else {
                continue;
            };
            let PdfObject::String(text) = value.as_ref() else {
                continue;
            };
            let key = String::from_utf8_lossy(&names.resolve(key)).into_owned();
            identity
                .info
                .insert(key, String::from_utf8_lossy(text.as_bytes()).into_owned());
        }
    }

    let root = structure.trailer.root;
    if let Some(catalog) = resolve(snapshot, root, ctx)
        && let Some(catalog) = catalog.as_dict()
        && let Some(metadata_key) = names.lookup(b"Metadata")
    {
        identity.has_xmp_metadata = dict_get_resolved(snapshot, catalog, metadata_key, ctx)
            .is_some_and(|v| !matches!(v.as_ref(), PdfObject::Null));
    }

    identity
}

// ── signatures ──────────────────────────────────────────────────────────────

/// Walk `/AcroForm` `/Fields` for `/FT /Sig` fields and read what each covers.
///
/// Reads the signature dictionaries only; nothing here validates cryptography.
/// The `/Contents` blob is deliberately not extracted — structural coverage
/// comes first, and the cryptographic layer is a separate concern.
fn inventory_signatures(
    snapshot: &DocumentSnapshot,
    ctx: &mut ParseContext,
) -> SignatureInventory {
    let mut inventory = SignatureInventory::default();
    let names = snapshot.names();

    let root = snapshot.structure().trailer.root;
    let Some(catalog) = resolve(snapshot, root, ctx) else {
        return inventory;
    };
    let Some(catalog) = catalog.as_dict() else {
        return inventory;
    };
    let Some(acroform_key) = names.lookup(b"AcroForm") else {
        return inventory;
    };
    let Some(acroform) = dict_get_resolved(snapshot, catalog, acroform_key, ctx) else {
        return inventory;
    };
    let Some(acroform) = acroform.as_dict() else {
        return inventory;
    };
    let Some(fields_key) = names.lookup(b"Fields") else {
        return inventory;
    };
    let Some(fields) = dict_get_resolved(snapshot, acroform, fields_key, ctx) else {
        return inventory;
    };

    let source_len = snapshot.source().len();
    let mut seen = 0usize;
    collect_signature_fields(snapshot, &fields, source_len, &mut inventory, &mut seen, ctx);
    inventory
}

/// Cap on fields visited, so a cyclic or adversarial `/Kids` graph cannot make
/// the doctor run away.
const MAX_FORM_FIELDS: usize = 65_536;

fn collect_signature_fields(
    snapshot: &DocumentSnapshot,
    node: &PdfObject,
    source_len: u64,
    inventory: &mut SignatureInventory,
    seen: &mut usize,
    ctx: &mut ParseContext,
) {
    if *seen >= MAX_FORM_FIELDS {
        return;
    }
    let names = snapshot.names();

    if let PdfObject::Array(entries) = node {
        for entry in entries.iter() {
            let Some(field) = resolve_value(snapshot, entry, ctx) else {
                continue;
            };
            collect_signature_fields(snapshot, &field, source_len, inventory, seen, ctx);
        }
        return;
    }

    let Some(field) = node.as_dict() else {
        return;
    };
    *seen += 1;

    // A field's type may be inherited from its parent, but a signature field
    // that declares nothing is not one we can identify; read what is here.
    let is_signature = names
        .lookup(b"FT")
        .and_then(|k| field.get(k))
        .and_then(PdfObject::as_name)
        .is_some_and(|n| names.resolve(n).as_ref() == b"Sig");

    if is_signature {
        match names
            .lookup(b"V")
            .and_then(|k| dict_get_resolved(snapshot, field, k, ctx))
            .filter(|v| !matches!(v.as_ref(), PdfObject::Null))
        {
            Some(value) => {
                if let Some(sig) = value.as_dict() {
                    inventory
                        .signatures
                        .push(read_signature(snapshot, field, sig, source_len, ctx));
                }
            }
            None => inventory.unsigned_fields += 1,
        }
    }

    // Descend into /Kids for nested field trees.
    if let Some(kids) = names
        .lookup(b"Kids")
        .and_then(|k| dict_get_resolved(snapshot, field, k, ctx))
    {
        collect_signature_fields(snapshot, &kids, source_len, inventory, seen, ctx);
    }
}

fn read_signature(
    snapshot: &DocumentSnapshot,
    field: &Dictionary,
    sig: &Dictionary,
    source_len: u64,
    ctx: &mut ParseContext,
) -> SignatureInfo {
    let names = snapshot.names();

    let text = |dict: &Dictionary, key: &[u8], ctx: &mut ParseContext| -> Option<String> {
        let k = names.lookup(key)?;
        let value = dict_get_resolved(snapshot, dict, k, ctx)?;
        match value.as_ref() {
            PdfObject::String(s) => Some(String::from_utf8_lossy(s.as_bytes()).into_owned()),
            PdfObject::Name(n) => Some(String::from_utf8_lossy(&names.resolve(*n)).into_owned()),
            _ => None,
        }
    };

    let byte_range: Vec<i64> = names
        .lookup(b"ByteRange")
        .and_then(|k| dict_get_resolved(snapshot, sig, k, ctx))
        .and_then(|v| match v.as_ref() {
            PdfObject::Array(entries) => Some(
                entries
                    .iter()
                    .filter_map(|e| resolve_value(snapshot, e, ctx))
                    .filter_map(|e| e.as_int())
                    .collect(),
            ),
            _ => None,
        })
        .unwrap_or_default();

    SignatureInfo {
        field_name: text(field, b"T", ctx),
        filter: text(sig, b"Filter", ctx),
        sub_filter: text(sig, b"SubFilter", ctx),
        signed_at: text(sig, b"M", ctx),
        covers_whole_file: byte_range_covers_all(&byte_range, source_len),
        byte_range,
    }
}

/// Whether `/ByteRange` starts at zero and its final pair reaches the last
/// byte of the file.
///
/// Offsets in `/ByteRange` are absolute source positions, so this comparison is
/// against the source length rather than the post-header length.
fn byte_range_covers_all(byte_range: &[i64], source_len: u64) -> bool {
    if byte_range.len() < 2 || !byte_range.len().is_multiple_of(2) {
        return false;
    }
    if byte_range[0] != 0 {
        return false;
    }
    // Pairs must be ascending and non-overlapping for the span to mean
    // anything; a scrambled range is not "whole-file" whatever it sums to.
    let mut reach = 0i64;
    for pair in byte_range.chunks_exact(2) {
        let (offset, length) = (pair[0], pair[1]);
        if offset < reach || length < 0 {
            return false;
        }
        let Some(end) = offset.checked_add(length) else {
            return false;
        };
        reach = end;
    }
    u64::try_from(reach).is_ok_and(|reach| reach == source_len)
}

// ── encryption ──────────────────────────────────────────────────────────────

fn read_encryption(snapshot: &DocumentSnapshot, ctx: &mut ParseContext) -> Option<EncryptionInfo> {
    let encrypt_ref = snapshot.structure().trailer.encrypt;
    // Either the trailer names an /Encrypt object, or the dictionary is
    // direct in the trailer (rare but legal) — the resolver handled the
    // former at open; here we only need to *read* it (it is never itself
    // encrypted; the resolver exempts it).
    let names = snapshot.names();
    let dict: Option<Arc<PdfObject>> = match encrypt_ref {
        Some(id) => resolve(snapshot, id, ctx),
        None => None,
    };
    let dict = dict?;
    let dict = dict.as_dict()?;

    let get_int = |key: &[u8], ctx: &mut ParseContext| -> i64 {
        names
            .lookup(key)
            .and_then(|k| dict_get_resolved(snapshot, dict, k, ctx))
            .and_then(|v| v.as_int())
            .unwrap_or(0)
    };
    let version = get_int(b"V", ctx);
    let revision = get_int(b"R", ctx);
    let length_bits = {
        let l = get_int(b"Length", ctx);
        if l == 0 { 40 } else { l }
    };

    let method = match (version, revision) {
        (5, _) | (_, 5 | 6) => "AES-256".to_owned(),
        (4, _) => {
            // V4: the crypt-filter method decides RC4 vs AES-128.
            if crypt_filter_is_aes(snapshot, dict, ctx) {
                "AES-128".to_owned()
            } else {
                "RC4-128".to_owned()
            }
        }
        (1, _) => "RC4-40".to_owned(),
        (2, _) => format!("RC4-{length_bits}"),
        _ => "unknown".to_owned(),
    };

    // Genuine `/U` validation of the empty user password — not an artifact
    // of how this snapshot happened to open (it may have opened with a
    // caller-supplied password, or not at all).
    let user_password_empty = {
        let file_id = snapshot
            .structure()
            .trailer
            .file_id
            .as_ref()
            .map(|id| id[0].as_bytes().to_vec())
            .unwrap_or_default();
        let (enc, _scheme) = pdf_document::parse_encrypt_dict(dict, file_id, names);
        pdf_security::StandardHandler::verify_password(&enc, "").is_some()
    };

    Some(EncryptionInfo {
        version,
        revision,
        method,
        user_password_empty,
    })
}

/// For V4/V5: whether the default stream crypt filter (`/CF` → `/StdCF` →
/// `/CFM`) names an AES method.
fn crypt_filter_is_aes(
    snapshot: &DocumentSnapshot,
    encrypt: &Dictionary,
    ctx: &mut ParseContext,
) -> bool {
    let names = snapshot.names();
    let Some(cf_key) = names.lookup(b"CF") else {
        return false;
    };
    let Some(cf) = dict_get_resolved(snapshot, encrypt, cf_key, ctx) else {
        return false;
    };
    let Some(cf) = cf.as_dict() else { return false };
    let Some(stdcf_key) = names.lookup(b"StdCF") else {
        return false;
    };
    let Some(stdcf) = dict_get_resolved(snapshot, cf, stdcf_key, ctx) else {
        return false;
    };
    let Some(stdcf) = stdcf.as_dict() else {
        return false;
    };
    let Some(cfm_key) = names.lookup(b"CFM") else {
        return false;
    };
    let Some(cfm) = stdcf.get(cfm_key).and_then(PdfObject::as_name) else {
        return false;
    };
    let cfm = names.resolve(cfm);
    cfm.as_ref() == b"AESV2" || cfm.as_ref() == b"AESV3"
}

// ── pages ───────────────────────────────────────────────────────────────────

fn examine_pages(snapshot: &DocumentSnapshot) -> Vec<PageStatus> {
    // Annotations on: the render default is `AnnotationMode::StaticAppearances`,
    // so the doctor's per-page compile health must exercise the same path a
    // default render would (annotation appearance streams included).
    let compiler = PageCompiler::new().with_annotations(true);
    let mut out = Vec::with_capacity(snapshot.page_count() as usize);
    for index in 0..snapshot.page_count() {
        // Fresh context per page: repairs and budgets attribute to the page.
        let mut ctx = ParseContext::new();
        let media_box_present = page_has_media_box(snapshot, index, &mut ctx);
        let repairs_before = ctx.recovery.len();
        let compile = match compiler.compile_semantic(snapshot, PageIndex(index), &mut ctx) {
            Ok(page) => {
                let op_count = page.ops.len();
                if ctx.recovery.len() > repairs_before {
                    let detail = ctx.recovery[repairs_before..]
                        .iter()
                        .map(describe_recovery)
                        .collect::<Vec<_>>()
                        .join("; ");
                    CompileStatus::Degraded { op_count, detail }
                } else {
                    CompileStatus::Ok { op_count }
                }
            }
            Err(e) => CompileStatus::Failed {
                error: e.to_string(),
            },
        };
        out.push(PageStatus {
            index,
            media_box_present,
            compile,
        });
    }
    out
}

/// Whether `/MediaBox` is genuinely present on the page or an inherited
/// ancestor (`PageRef::media_box` is always populated, defaulting when
/// absent — this distinguishes the two).
fn page_has_media_box(snapshot: &DocumentSnapshot, index: u32, ctx: &mut ParseContext) -> bool {
    let Ok(page_ref) = snapshot.page(PageIndex(index)) else {
        return false;
    };
    let names = snapshot.names();
    let media_box = names.known.media_box;
    let parent = names.known.parent;
    // A direct (inline-dict) page has no object id to re-resolve or walk a
    // `/Parent` chain from; report conservatively (its own MediaBox already
    // landed in `PageRef::media_box` via inheritance at open time).
    let Some(object) = page_ref.object else {
        return false;
    };
    let mut current = resolve(snapshot, object, ctx);
    for _ in 0..MAX_PARENT_HOPS {
        let Some(obj) = current else { return false };
        let Some(dict) = obj.as_dict() else {
            return false;
        };
        if dict.contains_key(media_box) {
            return true;
        }
        current = dict
            .get(parent)
            .and_then(|p| resolve_value(snapshot, p, ctx));
    }
    false
}

// ── annotations ─────────────────────────────────────────────────────────────

/// Count annotations per `/Subtype` by reading each page's `/Annots` array
/// directly (read-only; deliberately independent of any higher-level
/// annotation API).
fn inventory_annotations(
    snapshot: &DocumentSnapshot,
    ctx: &mut ParseContext,
) -> AnnotationInventory {
    let names = snapshot.names();
    let annots_key = names.intern(b"Annots");
    let mut inventory = AnnotationInventory::default();
    for index in 0..snapshot.page_count() {
        let Ok(page_ref) = snapshot.page(PageIndex(index)) else {
            continue;
        };
        // A direct (inline-dict) page has no id to re-resolve; its annotations
        // were already parsed into `PageRef::annotations` at open, so the raw
        // doctor inventory simply skips it.
        let Some(page_id) = page_ref.object else {
            continue;
        };
        let Some(page_obj) = resolve(snapshot, page_id, ctx) else {
            continue;
        };
        let Some(page_dict) = page_obj.as_dict() else {
            continue;
        };
        let Some(annots) = dict_get_resolved(snapshot, page_dict, annots_key, ctx) else {
            continue;
        };
        let PdfObject::Array(entries) = annots.as_ref() else {
            continue;
        };
        for entry in entries.iter() {
            let Some(annot) = resolve_value(snapshot, entry, ctx) else {
                continue;
            };
            let Some(annot) = annot.as_dict() else {
                continue;
            };
            let subtype = annot
                .get(names.known.subtype)
                .and_then(PdfObject::as_name)
                .map(|n| String::from_utf8_lossy(&names.resolve(n)).into_owned())
                .unwrap_or_else(|| "(none)".to_owned());
            *inventory.count_per_subtype.entry(subtype).or_insert(0) += 1;
        }
    }
    inventory
}

// ── features ────────────────────────────────────────────────────────────────

fn read_features(snapshot: &DocumentSnapshot, ctx: &mut ParseContext) -> FeatureFlags {
    let names = snapshot.names();
    let mut flags = FeatureFlags {
        uses_object_streams: xref_uses_object_streams(&snapshot.structure().xref),
        ..FeatureFlags::default()
    };

    let root = snapshot.structure().trailer.root;
    let Some(catalog) = resolve(snapshot, root, ctx) else {
        return flags;
    };
    let Some(catalog) = catalog.as_dict() else {
        return flags;
    };

    let has = |snapshot: &DocumentSnapshot,
               dict: &Dictionary,
               key: &[u8],
               ctx: &mut ParseContext|
     -> Option<Arc<PdfObject>> {
        let k = names.lookup(key)?;
        let v = dict_get_resolved(snapshot, dict, k, ctx)?;
        if matches!(v.as_ref(), PdfObject::Null) {
            None
        } else {
            Some(v)
        }
    };

    if let Some(acroform) = has(snapshot, catalog, b"AcroForm", ctx) {
        flags.has_acroform = true;
        if let Some(acroform) = acroform.as_dict() {
            flags.has_xfa = has(snapshot, acroform, b"XFA", ctx).is_some();
        }
    }
    flags.has_outlines = has(snapshot, catalog, b"Outlines", ctx).is_some();
    flags.has_optional_content = has(snapshot, catalog, b"OCProperties", ctx).is_some();
    if let Some(names_dict) = has(snapshot, catalog, b"Names", ctx)
        && let Some(names_dict) = names_dict.as_dict()
    {
        flags.has_javascript = has(snapshot, names_dict, b"JavaScript", ctx).is_some();
        flags.has_embedded_files = has(snapshot, names_dict, b"EmbeddedFiles", ctx).is_some();
    }
    flags
}

fn xref_uses_object_streams(xref: &pdf_structure::XrefMap) -> bool {
    // In-stream members always carry generation 0 (spec), so probing with
    // generation 0 sees every one of them.
    (0..xref.size()).any(|n| {
        matches!(
            xref.locate(ObjectId::new(n, 0)),
            ObjectLocation::InObjectStream { .. }
        )
    })
}

// ── rendering ───────────────────────────────────────────────────────────────

impl DocumentReport {
    /// Human-readable multi-line rendering of the report.
    pub fn summary(&self) -> String {
        let mut s = String::new();
        match &self.open {
            OpenOutcome::Ok => writeln_ok(&mut s, "open: ok"),
            OpenOutcome::Recovered { how } => {
                writeln_ok(
                    &mut s,
                    &format!("open: recovered ({} repair(s))", how.len()),
                );
                for h in how {
                    writeln_ok(&mut s, &format!("  - {h}"));
                }
            }
            OpenOutcome::Failed { error } => writeln_ok(&mut s, &format!("open: FAILED: {error}")),
        }
        match &self.header {
            None => writeln_ok(&mut s, "header: not found"),
            Some(h) => {
                let line = format!("header: %PDF-{}.{}", h.major, h.minor);
                let line = if h.header_offset == 0 {
                    line
                } else {
                    format!("{line} [{} bytes of leading garbage]", h.header_offset)
                };
                writeln_ok(&mut s, &line);
            }
        }
        writeln_ok(
            &mut s,
            &format!(
                "xref: revisions={} recovery_used={} rebuilt={}",
                self.xref.revision_count, self.xref.recovery_used, self.xref.rebuilt
            ),
        );
        match &self.encryption {
            None => writeln_ok(&mut s, "encryption: none"),
            Some(e) => writeln_ok(
                &mut s,
                &format!(
                    "encryption: {} (V={} R={}) empty-user-password={}",
                    e.method, e.version, e.revision, e.user_password_empty
                ),
            ),
        }
        let f = &self.features;
        writeln_ok(
            &mut s,
            &format!(
                "features: acroform={} xfa={} javascript={} outlines={} \
                 optional-content={} object-streams={} embedded-files={}",
                f.has_acroform,
                f.has_xfa,
                f.has_javascript,
                f.has_outlines,
                f.has_optional_content,
                f.uses_object_streams,
                f.has_embedded_files
            ),
        );
        let id = &self.identity;
        if let (Some(permanent), Some(changing)) = (&id.id_permanent, &id.id_changing) {
            writeln_ok(&mut s, &format!("id: permanent={permanent} changing={changing}"));
        }
        if id.has_xmp_metadata {
            writeln_ok(&mut s, "metadata: /Info + XMP");
        }
        for (key, value) in &id.info {
            writeln_ok(&mut s, &format!("  /{key}: {value}"));
        }
        writeln_ok(&mut s, &format!("pages: {}", self.pages.len()));
        for p in &self.pages {
            let line = match &p.compile {
                CompileStatus::Ok { op_count } => {
                    format!("  page {}: ok ({op_count} ops)", p.index)
                }
                CompileStatus::Degraded { op_count, detail } => {
                    format!("  page {}: degraded ({op_count} ops): {detail}", p.index)
                }
                CompileStatus::Failed { error } => {
                    format!("  page {}: FAILED: {error}", p.index)
                }
            };
            let line = if p.media_box_present {
                line
            } else {
                format!("{line} [no /MediaBox — defaulted]")
            };
            writeln_ok(&mut s, &line);
        }
        if self.annotations.count_per_subtype.is_empty() {
            writeln_ok(&mut s, "annotations: none");
        } else {
            writeln_ok(
                &mut s,
                &format!("annotations: {} total", self.annotations.total()),
            );
            for (subtype, count) in &self.annotations.count_per_subtype {
                writeln_ok(&mut s, &format!("  {subtype}: {count}"));
            }
        }
        let sigs = &self.signatures;
        if sigs.signatures.is_empty() && sigs.unsigned_fields == 0 {
            writeln_ok(&mut s, "signatures: none");
        } else {
            writeln_ok(
                &mut s,
                &format!(
                    "signatures: {} signed, {} unsigned field(s)",
                    sigs.signatures.len(),
                    sigs.unsigned_fields
                ),
            );
            for sig in &sigs.signatures {
                let name = sig.field_name.as_deref().unwrap_or("(unnamed)");
                let sub_filter = sig.sub_filter.as_deref().unwrap_or("(no /SubFilter)");
                writeln_ok(&mut s, &format!("  {name}: {sub_filter}"));
                let coverage = if sig.covers_whole_file {
                    "covers whole file".to_owned()
                } else {
                    format!("DOES NOT cover whole file; /ByteRange {:?}", sig.byte_range)
                };
                writeln_ok(&mut s, &format!("    {coverage}"));
            }
        }
        s
    }
}

/// `writeln!` into a `String` cannot fail; this wrapper keeps the
/// `unwrap_used` lint clean.
fn writeln_ok(s: &mut String, line: &str) {
    let _ = writeln!(s, "{line}");
}
