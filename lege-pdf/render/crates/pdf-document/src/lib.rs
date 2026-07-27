//! The immutable document snapshot — the heart of the concurrency design.
//!
//! Invariants (enforced, not aspirational — see skeleton-blueprint.md §4.3):
//!
//! 1. `DocumentSnapshot` has **no** `&mut self` methods after `open()`.
//! 2. All mutable scratch (recursion tracking, budgets, local caches) lives
//!    in a worker-owned [`ParseContext`], passed `&mut` into resolution.
//! 3. Interior mutability exists only inside [`ObjectRepository`] with
//!    once-publication semantics: a slot goes from empty to an immutable
//!    `Ok`/`Err` exactly once, and both outcomes are shared.
//!
//! Resolution strategy is abstracted behind `ObjectRepository` so Phase 1
//! ships worker-local caches (plan "Design A") and Phase 3 can move to
//! shared `OnceLock` slots ("Design B") without touching callers.

use std::collections::HashMap;
use std::sync::Arc;

use pdf_object::{Dictionary, NameTable, ObjectError, ObjectId, PdfObject, PdfStream};
use pdf_security::{
    Cipher, EncryptDict, EncryptionScheme, Permissions, SecurityContext, SecurityError,
    StandardHandler,
};
use pdf_source::PdfSource;
use pdf_structure::{DocumentStructure, RecoveryEvent, StructureError};

mod links;
mod ocg;
mod outline;
mod pages;
mod resolve;

pub use links::{DocumentLink, DocumentLinkTarget, DocumentLinks};
pub use ocg::OcgConfig;
pub use outline::{
    DestinationFit, DocumentDestination, DocumentOutline, DocumentOutlineItem, OutlineIssue,
};
use resolve::Resolver;

/// Open-time limits. Threaded from day one, enforced as parsers land
/// (blueprint §4.4). All limits are per-document unless stated.
#[derive(Debug, Clone)]
pub struct DocumentLimits {
    pub syntax: pdf_syntax::SyntaxLimits,
    /// Maximum indirect objects a single resolution chain may visit.
    pub max_reference_chain: usize,
    /// Maximum total decompressed bytes a single `ParseContext` may produce.
    pub max_decoded_bytes_per_context: usize,
    /// Maximum page count accepted (0 = unlimited).
    pub max_pages: u32,
    /// Maximum revisions (incremental updates) followed.
    pub max_revisions: usize,
    /// Maximum object number admitted to the xref map.
    pub max_objects: u32,
}

impl Default for DocumentLimits {
    fn default() -> Self {
        Self {
            syntax: pdf_syntax::SyntaxLimits::default(),
            max_reference_chain: 128,
            max_decoded_bytes_per_context: 1 << 30, // 1 GiB
            max_pages: 0,
            max_revisions: 4096,
            max_objects: 8 * 1024 * 1024,
        }
    }
}

/// Errors surfaced by document open and object/page resolution.
#[derive(Debug, Clone, thiserror::Error)]
pub enum DocumentError {
    #[error(transparent)]
    Structure(#[from] StructureError),
    #[error(transparent)]
    Object(#[from] ObjectError),
    #[error(transparent)]
    Security(#[from] pdf_security::SecurityError),
    #[error("page index {0} out of range")]
    PageOutOfRange(u32),
    #[error("limit exceeded: {0}")]
    LimitExceeded(&'static str),
    /// Retained for later-phase stubs (Phase 1 no longer produces it).
    #[error("not implemented yet: {0} (see skeleton-blueprint.md §7)")]
    NotImplemented(&'static str),
}

/// Stable identifier of a page within one snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PageIndex(pub u32);

/// Annotation flag bit: hidden in every rendering mode (ISO 32000-1 Table
/// 165 bit 2).
pub const ANNOT_FLAG_HIDDEN: u32 = 1 << 1;
/// Annotation flag bit: printed but not displayed on screen (bit 6).
pub const ANNOT_FLAG_NOVIEW: u32 = 1 << 5;

/// `/C` annotation color, retained in its authored device color space.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AnnotationColor {
    Gray(f32),
    Rgb([f32; 3]),
    Cmyk([f32; 4]),
}

/// One annotation of a page, resolved at open to exactly what static
/// appearance-stream rendering needs (ISO 32000-1 §12.5). `/Popup`
/// annotations are dropped at parse time — PDFium's `CPDF_AnnotList` never
/// even lists them (cpdf_annotlist.cpp).
#[derive(Debug, Clone)]
pub struct PageAnnotation {
    /// `/Subtype`, interned in the snapshot's name table. `None` when absent
    /// or unreadable (the annotation is kept: an `/AP` can still paint).
    pub subtype: Option<pdf_object::NameId>,
    /// `/Rect` in default user space, corner-normalized. Degenerate rects are
    /// preserved as-is; rendering skips them.
    pub rect: [f64; 4],
    /// `/F` annotation flags (0 when absent, per spec).
    pub flags: u32,
    /// The raw `/AP` value (usually a `Reference` to the appearance
    /// dictionary), unresolved so the snapshot stays cheap to build.
    pub appearance: Option<PdfObject>,
    /// The `/AS` appearance-state name, selecting among `/N` sub-states.
    pub appearance_state: Option<pdf_object::NameId>,
    /// A link annotation's direct `/Dest`, retained unresolved for semantic
    /// extraction. Non-link annotations commonly omit it.
    pub destination: Option<PdfObject>,
    /// A link annotation's `/A` action dictionary, retained unresolved for
    /// semantic extraction. Rendering does not interpret actions.
    pub action: Option<PdfObject>,
    /// `/QuadPoints`, grouped in the PDF text-markup Z order
    /// (top-left, top-right, bottom-left, bottom-right).
    pub quad_points: std::sync::Arc<[[f64; 8]]>,
    /// `/C` text-markup color. Missing or malformed colors remain `None`, so
    /// the renderer can apply the subtype's conventional default.
    pub color: Option<AnnotationColor>,
    /// `/CA` constant opacity, clamped to `[0,1]` (default 1).
    pub opacity: f32,
}

impl PageAnnotation {
    /// Whether screen rendering (PDFium `bPrinting=false` display pass) must
    /// skip this annotation: `Hidden` or `NoView` set.
    pub fn hidden_for_display(&self) -> bool {
        self.flags & (ANNOT_FLAG_HIDDEN | ANNOT_FLAG_NOVIEW) != 0
    }
}

/// A resolved page-tree leaf with inherited attributes flattened.
/// Built during open so page lookup never walks the tree again (and never
/// re-resolves `/Parent` chains concurrently).
#[derive(Debug, Clone)]
pub struct PageRef {
    pub index: PageIndex,
    /// The page's own object id when it is (as the spec requires) an indirect
    /// reference; `None` for a *direct* inline page dict in `/Kids` (malformed
    /// but rendered by PDFium) and for synthesized blank placeholders. Rendering
    /// never needs it — it reads `contents`/`resources`/`annotations` — but the
    /// `pdf-read` doctor re-resolves the page dict through it and skips when it
    /// is absent.
    pub object: Option<ObjectId>,
    /// MediaBox after inheritance, in default user space units.
    pub media_box: [f64; 4],
    /// CropBox after inheritance, clamped to MediaBox; equals media_box if
    /// absent.
    pub crop_box: [f64; 4],
    /// Normalized rotation in {0, 90, 180, 270}.
    pub rotate: u16,
    /// The page's raw /Resources value (possibly inherited): usually a
    /// `Reference`, but direct dictionaries are common in the wild, so the
    /// unresolved value is preserved rather than an `ObjectId`.
    pub resources: Option<PdfObject>,
    /// The raw /Contents value: a `Reference` to a single stream, or an
    /// array of references (or a direct array — same rationale as
    /// `resources`).
    pub contents: Option<PdfObject>,
    /// The page's `/Annots`, resolved at open (minus `/Popup`, PDFium
    /// parity). Shared so cloning a `PageRef` per compile job stays cheap.
    pub annotations: Arc<[PageAnnotation]>,
}

/// Immutable index of all pages, built once at open.
#[derive(Debug, Default)]
pub struct PageTreeIndex {
    pages: Box<[PageRef]>,
}

impl PageTreeIndex {
    pub fn from_pages(pages: Box<[PageRef]>) -> Self {
        Self { pages }
    }
    pub fn page_count(&self) -> u32 {
        self.pages.len() as u32
    }
    pub fn page(&self, index: PageIndex) -> Option<&PageRef> {
        self.pages.get(index.0 as usize)
    }
    pub fn iter(&self) -> impl Iterator<Item = &PageRef> {
        self.pages.iter()
    }

    pub fn index_for_object(&self, object: ObjectId) -> Option<PageIndex> {
        self.pages
            .iter()
            .find(|page| page.object == Some(object))
            .map(|page| page.index)
    }
}

/// Worker-owned mutable state for parsing/resolution. One per worker (or per
/// page-compilation job); never shared, never global.
#[derive(Debug)]
pub struct ParseContext {
    /// Objects currently being resolved on this chain — cycle detection.
    recursion: Vec<ObjectId>,
    /// Budget accounting against `DocumentLimits`.
    pub decoded_bytes_used: usize,
    pub objects_visited: usize,
    #[cfg(feature = "profiling")]
    pub object_cache_hits: usize,
    #[cfg(feature = "profiling")]
    pub object_cache_misses: usize,
    #[cfg(feature = "profiling")]
    pub object_stream_cache_hits: usize,
    #[cfg(feature = "profiling")]
    pub object_stream_inflates: usize,
    /// Worker-local parsed-object cache (Design A). Keyed by full id so a
    /// stale-generation lookup never aliases a live object.
    cache: HashMap<ObjectId, Arc<PdfObject>>,
    /// Worker-local decompressed object-stream containers: N members cost
    /// one inflate per worker, not N.
    objstm: HashMap<u32, Arc<ObjStmCached>>,
    /// Repairs performed during resolution *by this worker* (wrong-offset
    /// fixes, stream-length repairs discovered lazily). Open-time repairs
    /// live on the snapshot's recovery log instead; post-open the snapshot
    /// is immutable, so lazy repairs surface here — observable, per-worker.
    pub recovery: Vec<RecoveryEvent>,
}

/// A decompressed object-stream container plus its parsed pair table.
#[derive(Debug)]
pub(crate) struct ObjStmCached {
    pub(crate) data: Vec<u8>,
    pub(crate) layout: pdf_structure::objstm::ObjectStreamLayout,
}

impl ParseContext {
    pub fn new() -> Self {
        Self {
            recursion: Vec::with_capacity(16),
            decoded_bytes_used: 0,
            objects_visited: 0,
            #[cfg(feature = "profiling")]
            object_cache_hits: 0,
            #[cfg(feature = "profiling")]
            object_cache_misses: 0,
            #[cfg(feature = "profiling")]
            object_stream_cache_hits: 0,
            #[cfg(feature = "profiling")]
            object_stream_inflates: 0,
            cache: HashMap::new(),
            objstm: HashMap::new(),
            recovery: Vec::new(),
        }
    }

    /// Start a new page-compilation job while retaining worker-local caches.
    ///
    /// Persistent viewer workers reuse one context across pages so parsed
    /// objects and decompressed object streams stay hot. Budget counters,
    /// recursion state, profiling counters, and recovery events are
    /// job-scoped and must not leak into the next page.
    pub fn begin_job(&mut self) {
        self.recursion.clear();
        self.decoded_bytes_used = 0;
        self.objects_visited = 0;
        #[cfg(feature = "profiling")]
        {
            self.object_cache_hits = 0;
            self.object_cache_misses = 0;
            self.object_stream_cache_hits = 0;
            self.object_stream_inflates = 0;
        }
        self.recovery.clear();
    }

    fn cache_get(&self, id: ObjectId) -> Option<Arc<PdfObject>> {
        self.cache.get(&id).cloned()
    }

    fn cache_put(&mut self, id: ObjectId, value: Arc<PdfObject>) {
        self.cache.insert(id, value);
    }

    fn objstm_get(&self, container: u32) -> Option<Arc<ObjStmCached>> {
        self.objstm.get(&container).cloned()
    }

    fn objstm_put(&mut self, container: u32, entry: Arc<ObjStmCached>) {
        self.objstm.insert(container, entry);
    }

    /// RAII-free cycle guard: push before resolving `id`, pop after.
    pub fn enter(&mut self, id: ObjectId, limits: &DocumentLimits) -> Result<(), DocumentError> {
        if self.recursion.contains(&id) {
            return Err(ObjectError::ReferenceCycle(id).into());
        }
        if self.recursion.len() >= limits.max_reference_chain {
            return Err(DocumentError::LimitExceeded("max_reference_chain"));
        }
        self.recursion.push(id);
        Ok(())
    }

    pub fn exit(&mut self, id: ObjectId) {
        debug_assert_eq!(self.recursion.last(), Some(&id));
        self.recursion.pop();
    }

    /// Record a lazy, worker-local recovery — e.g. a page content stream that
    /// failed to decode and was skipped so the page still renders. Surfaced on
    /// this worker's `recovery` log, keeping the deviation observable rather
    /// than silent (blueprint §4.5).
    pub fn note_recovery(&mut self, message: String) {
        self.recovery.push(RecoveryEvent::Other(message));
    }
}

impl Default for ParseContext {
    fn default() -> Self {
        Self::new()
    }
}

/// Canonical object access. `&self` resolution against immutable structure;
/// all scratch goes through the caller's `ParseContext`.
///
/// The interior strategy (worker-local vs shared once-published) is an
/// implementation detail behind this type — see module docs.
#[derive(Debug)]
pub struct ObjectRepository {
    // Phase 1 (Design A): stateless here; caching in ParseContext.
    // Phase 3 (Design B): slots: Box<[OnceLock<Result<Arc<PdfObject>, Arc<ObjectError>>>]>.
}

impl ObjectRepository {
    pub fn new() -> Self {
        Self {}
    }

    /// Resolve an indirect object to its parsed value, following the xref,
    /// parsing on demand, and publishing/caching per the active strategy.
    ///
    /// Reference chains are followed (with the context's chain limit);
    /// references to free or undefined objects resolve to `Null` per
    /// ISO 32000-1 §7.3.10.
    pub fn resolve(
        &self,
        snapshot: &DocumentSnapshot,
        id: ObjectId,
        ctx: &mut ParseContext,
    ) -> Result<Arc<PdfObject>, DocumentError> {
        snapshot.resolver().resolve(id, ctx)
    }
}

impl Default for ObjectRepository {
    fn default() -> Self {
        Self::new()
    }
}

/// The immutable snapshot of an open document. `Send + Sync`; every field is
/// either immutable or internally once-published. Cloning is cheap (`Arc`s).
#[derive(Debug, Clone)]
pub struct DocumentSnapshot {
    inner: Arc<SnapshotInner>,
}

#[derive(Debug)]
struct SnapshotInner {
    source: Arc<dyn PdfSource>,
    structure: DocumentStructure,
    names: NameTable,
    pages: PageTreeIndex,
    objects: ObjectRepository,
    security: Option<SecurityContext>,
    limits: DocumentLimits,
    /// The `/OCProperties /D` default optional-content configuration
    /// (default: everything visible).
    ocg: OcgConfig,
}

impl DocumentSnapshot {
    /// Open a document: read all structural information, apply recovery,
    /// build the page index, and freeze. This is the only phase allowed to
    /// mutate document-level state.
    pub fn open(source: Arc<dyn PdfSource>, limits: DocumentLimits) -> Result<Self, DocumentError> {
        Self::open_with_password(source, limits, None)
    }

    /// [`DocumentSnapshot::open`] with an optional user/owner password for
    /// encrypted documents. The empty user password is always tried first
    /// (permissions-only encryption, the overwhelmingly common case);
    /// `password` is consulted only when that fails. A wrong password fails
    /// typed ([`pdf_security::SecurityError::IncorrectPassword`]); an
    /// encrypted document opened with no password and a non-empty user
    /// password fails with `PasswordRequired`. Which password authenticated
    /// is exposed via `snapshot.security().and_then(|s| s.password_role())`.
    pub fn open_with_password(
        source: Arc<dyn PdfSource>,
        limits: DocumentLimits,
        password: Option<&str>,
    ) -> Result<Self, DocumentError> {
        let names = NameTable::new();
        let structure_limits = pdf_structure::StructureLimits {
            syntax: limits.syntax.clone(),
            max_revisions: limits.max_revisions,
            max_objects: limits.max_objects,
            max_decoded_bytes: limits.max_decoded_bytes_per_context,
        };
        // Establish the security context *before* building the page tree (page
        // strings/streams are encrypted and decrypt as they resolve; `/Encrypt`
        // itself is read through a handler-less resolver), then walk the tree
        // and parse OCG config. Factored so the whole thing can be re-run
        // against a rebuilt structure below.
        type Parts = (
            Option<SecurityContext>,
            PageTreeIndex,
            ocg::OcgConfig,
            pages::WalkStats,
        );
        let build_parts = |structure: &DocumentStructure,
                           ctx: &mut ParseContext|
         -> Result<Parts, DocumentError> {
            let security = {
                let plain = Resolver {
                    source: source.as_ref(),
                    structure,
                    names: &names,
                    limits: &limits,
                    security: None,
                };
                build_security(&plain, structure, &names, ctx, password)?
            };
            let resolver = Resolver {
                source: source.as_ref(),
                structure,
                names: &names,
                limits: &limits,
                security: security.as_ref(),
            };
            let (pages, stats) = pages::build_page_tree(&resolver, ctx)?;
            let ocg = ocg::parse_ocproperties(&resolver, ctx);
            Ok((security, pages, ocg, stats))
        };

        // One open attempt against a given structure: build security + page
        // tree with its own resolution context (whose repairs fold into the
        // snapshot's recovery log below; post-open, repairs go to each worker's
        // own context instead — the snapshot is frozen).
        let try_build = |structure: DocumentStructure|
         -> Result<(DocumentStructure, ParseContext, Parts), DocumentError> {
            let mut ctx = ParseContext::new();
            let parts = build_parts(&structure, &mut ctx)?;
            Ok((structure, ctx, parts))
        };
        let rebuild =
            || pdf_structure::load_structure_rebuilt(source.as_ref(), &names, &structure_limits);

        // Load the xref chain and build. If *either* step fails structurally —
        // an unfindable `/Root` (leading garbage before `%PDF`, a truncated
        // trailer) or a dangling object the xref mislocated — fall back to a
        // forced full-file reconstruction (PDFium/mupdf `RebuildCrossRef`:
        // scan for every object and `/Type /Catalog`). Surface the original
        // error only when the rebuild path cannot open it either.
        let (
            mut structure,
            mut ctx,
            (mut security, mut pages, mut ocg, mut stats),
            rebuilt_already,
        ) = match pdf_structure::load_structure(source.as_ref(), &names, &structure_limits) {
            Ok(s) => match try_build(s) {
                Ok((s, c, p)) => (s, c, p, false),
                Err(build_err) => match rebuild() {
                    Ok(r) => match try_build(r) {
                        Ok((s, c, p)) => (s, c, p, true),
                        Err(_) => return Err(build_err),
                    },
                    Err(_) => return Err(build_err),
                },
            },
            Err(load_err) => match rebuild() {
                Ok(r) => match try_build(r) {
                    Ok((s, c, p)) => (s, c, p, true),
                    Err(_) => return Err(load_err.into()),
                },
                Err(_) => return Err(load_err.into()),
            },
        };

        // A cleanly loaded chain can still contain stale offsets below a valid
        // catalog. Any page-tree subtree that was provably lost warrants one
        // full reconstruction, including a *partial* tree: otherwise pages
        // after a mid-tree hole remain shifted. Adopt only a strict structural
        // improvement, so genuinely truncated files keep their usable chain.
        if !rebuilt_already
            && stats.lost_subtrees > 0
            && let Ok(rebuilt) = rebuild()
        {
            let mut ctx2 = ParseContext::new();
            if let Ok((sec2, pages2, ocg2, stats2)) = build_parts(&rebuilt, &mut ctx2) {
                let (pages2, candidate_stats) = if stats2.real_pages == 0 {
                    let resolver = Resolver {
                        source: source.as_ref(),
                        structure: &rebuilt,
                        names: &names,
                        limits: &limits,
                        security: sec2.as_ref(),
                    };
                    let orphans = pages::recover_orphan_pages(&resolver, &mut ctx2);
                    if orphans.page_count() > 0 {
                        let orphan_count = orphans.page_count() as usize;
                        (
                            orphans,
                            pages::WalkStats {
                                real_pages: orphan_count,
                                lost_subtrees: 0,
                            },
                        )
                    } else {
                        (pages2, stats2)
                    }
                } else {
                    (pages2, stats2)
                };
                let improves = candidate_stats.real_pages > stats.real_pages
                    || (candidate_stats.real_pages == stats.real_pages
                        && candidate_stats.lost_subtrees < stats.lost_subtrees);
                if improves {
                    structure = rebuilt;
                    security = sec2;
                    pages = pages2;
                    ocg = ocg2;
                    ctx = ctx2;
                    stats = candidate_stats;
                }
            }
        }

        // If the initial fallback path already rebuilt the xref but its tree
        // still yielded zero real pages, there is no second structure to try.
        // Scan that rebuilt live-object view for explicit orphan page leaves.
        if rebuilt_already && stats.real_pages == 0 {
            let resolver = Resolver {
                source: source.as_ref(),
                structure: &structure,
                names: &names,
                limits: &limits,
                security: security.as_ref(),
            };
            let orphans = pages::recover_orphan_pages(&resolver, &mut ctx);
            if orphans.page_count() > 0 {
                pages = orphans;
            }
        }

        if !ctx.recovery.is_empty() {
            let mut all = structure.recovery.into_vec();
            all.append(&mut ctx.recovery);
            structure.recovery = all.into_boxed_slice();
        }

        let mut snapshot = Self::from_parts(source, structure, names, pages, security, limits);
        // `from_parts` is the shared (test-visible) constructor; the parsed
        // OCG config is attached before the snapshot is ever shared.
        if let Some(inner) = Arc::get_mut(&mut snapshot.inner) {
            inner.ocg = ocg;
        }
        Ok(snapshot)
    }

    /// Open a document and return a structured timing report. This API exists
    /// only in profiling builds so ordinary applications neither pay for an
    /// `Instant` nor acquire a profiling dependency.
    #[cfg(feature = "profiling")]
    pub fn open_profiled(
        source: Arc<dyn PdfSource>,
        limits: DocumentLimits,
    ) -> Result<(Self, pdf_profiling::ProfileReport), DocumentError> {
        let start = std::time::Instant::now();
        let snapshot = Self::open(source, limits)?;
        let mut profile = pdf_profiling::ProfileReport::new();
        profile.add_duration("document.open", start.elapsed());
        profile.increment("document.pages", snapshot.page_count() as u64);
        profile.increment(
            "document.recovery_events",
            snapshot.recovery_events().len() as u64,
        );
        Ok((snapshot, profile))
    }

    fn resolver(&self) -> Resolver<'_> {
        Resolver {
            source: self.inner.source.as_ref(),
            structure: &self.inner.structure,
            names: &self.inner.names,
            limits: &self.inner.limits,
            security: self.inner.security.as_ref(),
        }
    }

    /// Decode a stream's body through its `/Filter` chain, charging the
    /// worker context's decompression budget.
    pub fn decode_stream_data(
        &self,
        stream: &PdfStream,
        ctx: &mut ParseContext,
    ) -> Result<Vec<u8>, DocumentError> {
        self.resolver().decode_stream_data(stream, ctx)
    }

    /// Decode a stream's body up to (not through) an image-codec filter;
    /// see [`resolve::Resolver::decode_stream_data_to_codec`].
    pub fn decode_stream_data_to_codec(
        &self,
        stream: &PdfStream,
        ctx: &mut ParseContext,
    ) -> Result<(Vec<u8>, Option<String>), DocumentError> {
        self.resolver().decode_stream_data_to_codec(stream, ctx)
    }

    /// Test-only constructor for exercising downstream layers before the
    /// parser exists.
    pub fn from_parts(
        source: Arc<dyn PdfSource>,
        structure: DocumentStructure,
        names: NameTable,
        pages: PageTreeIndex,
        security: Option<SecurityContext>,
        limits: DocumentLimits,
    ) -> Self {
        Self {
            inner: Arc::new(SnapshotInner {
                source,
                structure,
                names,
                pages,
                objects: ObjectRepository::new(),
                security,
                limits,
                ocg: OcgConfig::default(),
            }),
        }
    }

    /// Whether the optional-content group with object id `id` is visible
    /// under the document's `/OCProperties /D` default configuration
    /// (default-visibility semantics: visible unless configured off).
    pub fn ocg_visible(&self, id: ObjectId) -> bool {
        self.inner.ocg.visible(id)
    }

    pub fn source(&self) -> &Arc<dyn PdfSource> {
        &self.inner.source
    }
    pub fn structure(&self) -> &DocumentStructure {
        &self.inner.structure
    }
    pub fn names(&self) -> &NameTable {
        &self.inner.names
    }
    pub fn page_count(&self) -> u32 {
        self.inner.pages.page_count()
    }
    pub fn page(&self, index: PageIndex) -> Result<&PageRef, DocumentError> {
        self.inner
            .pages
            .page(index)
            .ok_or(DocumentError::PageOutOfRange(index.0))
    }

    /// Parse and flatten the document's bookmark tree.
    ///
    /// The operation is recovery-driven: malformed nodes and unsupported
    /// actions are reported in `issues` while usable siblings remain
    /// available to the viewer.
    pub fn outline(&self, ctx: &mut ParseContext) -> DocumentOutline {
        outline::extract(self, ctx)
    }

    /// Extract all usable internal and URI link annotations in one document
    /// pass. Keeping this document-level avoids rebuilding the named-
    /// destination tree for every independently compiled page.
    pub fn links(&self, ctx: &mut ParseContext) -> DocumentLinks {
        links::extract(self, ctx)
    }
    pub fn objects(&self) -> &ObjectRepository {
        &self.inner.objects
    }
    pub fn security(&self) -> Option<&SecurityContext> {
        self.inner.security.as_ref()
    }
    pub fn limits(&self) -> &DocumentLimits {
        &self.inner.limits
    }
    /// Structural repairs applied during open — observable, never silent.
    pub fn recovery_events(&self) -> &[RecoveryEvent] {
        &self.inner.structure.recovery
    }
}

/// Build the document's [`SecurityContext`] from its `/Encrypt` dictionary.
///
/// Returns `Ok(None)` for an unencrypted document, `Ok(Some(_))` once the
/// standard handler is derived, and an `UnsupportedScheme` error for schemes
/// the handler declines (AES-256 / revision 6) — never a silent
/// "rendered blank". The `/Encrypt` dictionary is resolved through the given
/// `resolver`, which must have **no** security handler installed (the dict is
/// never itself encrypted).
fn build_security(
    resolver: &Resolver<'_>,
    structure: &DocumentStructure,
    names: &NameTable,
    ctx: &mut ParseContext,
    password: Option<&str>,
) -> Result<Option<SecurityContext>, DocumentError> {
    // The trailer carries `/Encrypt` as an indirect reference in the common
    // case; a direct dictionary is rare but legal.
    let encrypt_obj = match structure.trailer.encrypt {
        Some(id) => resolver.resolve(id, ctx)?,
        None => match structure.trailer.dict.get(names.known.encrypt) {
            Some(PdfObject::Dictionary(d)) => Arc::new(PdfObject::Dictionary(Arc::clone(d))),
            _ => return Ok(None),
        },
    };
    let Some(dict) = encrypt_obj.as_dict() else {
        return Ok(None);
    };

    let file_id = structure
        .trailer
        .file_id
        .as_ref()
        .map(|id| id[0].as_bytes().to_vec())
        .unwrap_or_default();
    let (enc, scheme) = parse_encrypt_dict(dict, file_id, names);

    // Revisions beyond what the standard handler implements: typed decline,
    // never a password prompt for a scheme we could not honor anyway.
    let v5 = enc.r == 5 || enc.r == 6 || enc.v == 5;
    if !v5 && (enc.r > 6 || enc.v > 5) {
        return Err(SecurityError::UnsupportedScheme(scheme).into());
    }

    // Empty user password first (permissions-only encryption — essentially
    // every encrypted PDF in the wild), then the caller-supplied password.
    let authenticated = StandardHandler::with_password(&enc, "")
        .or_else(|| password.and_then(|pw| StandardHandler::with_password(&enc, pw)));
    if let Some((handler, role)) = authenticated {
        // V5 `/Perms` integrity cross-check — report-only: the content key is
        // demonstrably right (validated against `/U`/`/O`), so a tampered or
        // non-conforming permissions block is surfaced as a recovery note,
        // never a refusal (PDFium's AES256_CheckPerms hard-fails here; that
        // strictness would blank otherwise-openable wild documents).
        if let Some(mismatch) = handler.verify_perms(&enc) {
            ctx.note_recovery(format!("encryption /Perms cross-check failed: {mismatch}"));
        }
        return Ok(Some(
            SecurityContext::standard(scheme, Permissions(enc.p as u32), handler)
                .with_password_role(role),
        ));
    }

    // Identity crypt filter: decryption is a no-op regardless of the derived
    // key, so a malformed `/U` must not block opening (legacy lenient path).
    if matches!(scheme, EncryptionScheme::None)
        && let Some(handler) = StandardHandler::open(&enc)
    {
        return Ok(Some(SecurityContext::standard(
            scheme,
            Permissions(enc.p as u32),
            handler,
        )));
    }

    Err(match password {
        Some(_) => SecurityError::IncorrectPassword.into(),
        None => SecurityError::PasswordRequired.into(),
    })
}

/// The crypt-filter method a `/V 4` document uses for its streams.
enum CryptFilterMethod {
    V2,
    AesV2,
    AesV3,
    Identity,
}

/// Parse the raw `/Encrypt` fields into a [`EncryptDict`] plus the scheme it
/// names (used for the declined-scheme error). Public so read-only tooling
/// (`pdf-read`) can run password checks against the same parsed form the
/// open path uses.
pub fn parse_encrypt_dict(
    dict: &Dictionary,
    file_id: Vec<u8>,
    names: &NameTable,
) -> (EncryptDict, EncryptionScheme) {
    let get_int = |key: &[u8]| {
        names
            .lookup(key)
            .and_then(|id| dict.get(id))
            .and_then(PdfObject::as_int)
    };
    let get_bytes = |key: &[u8]| -> Vec<u8> {
        names
            .lookup(key)
            .and_then(|id| dict.get(id))
            .and_then(|o| match o {
                PdfObject::String(s) => Some(s.as_bytes().to_vec()),
                _ => None,
            })
            .unwrap_or_default()
    };
    let get_bool = |key: &[u8]| {
        names
            .lookup(key)
            .and_then(|id| dict.get(id))
            .and_then(|o| match o {
                PdfObject::Boolean(b) => Some(*b),
                _ => None,
            })
    };

    let v = get_int(b"V").unwrap_or(0);
    let r = get_int(b"R").unwrap_or(0);
    let length_bits = get_int(b"Length").unwrap_or(40);
    let rc4_key_bytes = (length_bits.clamp(40, 128) / 8) as usize;

    let (cipher, key_bytes, scheme) = match v {
        1 => (Cipher::Rc4, 5, EncryptionScheme::Rc4 { key_bits: 40 }),
        2 | 3 => (
            Cipher::Rc4,
            rc4_key_bytes,
            EncryptionScheme::Rc4 {
                key_bits: (rc4_key_bytes * 8) as u16,
            },
        ),
        4 => match crypt_filter_method(dict, names) {
            CryptFilterMethod::AesV2 => (Cipher::Aes128, 16, EncryptionScheme::Aes128),
            CryptFilterMethod::V2 => (
                Cipher::Rc4,
                rc4_key_bytes,
                EncryptionScheme::Rc4 {
                    key_bits: (rc4_key_bytes * 8) as u16,
                },
            ),
            CryptFilterMethod::Identity => (Cipher::None, rc4_key_bytes, EncryptionScheme::None),
            // AESV3 under /V 4 is malformed; report as AES-256 so the handler
            // declines rather than mis-deriving a key.
            CryptFilterMethod::AesV3 => (Cipher::Aes128, 32, EncryptionScheme::Aes256),
        },
        // /V 5 (AES-256) and any unknown version: the handler declines.
        _ => (Cipher::Aes128, 32, EncryptionScheme::Aes256),
    };

    let enc = EncryptDict {
        v,
        r,
        o: get_bytes(b"O"),
        u: get_bytes(b"U"),
        // `/UE` (V5 only) — the user key-encryption entry the handler AES-256-
        // decrypts to recover the file key. Empty (and unused) for RC4/AES-128.
        ue: get_bytes(b"UE"),
        // `/OE` — owner-side counterpart of `/UE`, consumed by the
        // password APIs (`StandardHandler::with_password`). [Stream C
        // integration hookup: keep this line when merging.]
        oe: get_bytes(b"OE"),
        p: get_int(b"P").unwrap_or(0) as i32,
        // `/Perms` (V5) — permissions-integrity block, cross-checked
        // report-only after authentication.
        perms: get_bytes(b"Perms"),
        key_bytes,
        encrypt_metadata: get_bool(b"EncryptMetadata").unwrap_or(true),
        cipher,
        file_id,
    };
    (enc, scheme)
}

/// Resolve the `/CFM` of the stream crypt filter (`/StmF` → `/CF`) for `/V 4`.
fn crypt_filter_method(dict: &Dictionary, names: &NameTable) -> CryptFilterMethod {
    let name_is = |id, s: &[u8]| &*names.resolve(id) == s;

    let Some(stmf) = names
        .lookup(b"StmF")
        .and_then(|id| dict.get(id))
        .and_then(PdfObject::as_name)
    else {
        return CryptFilterMethod::Identity;
    };
    // `/Identity` is the distinguished "not encrypted" filter name.
    if name_is(stmf, b"Identity") {
        return CryptFilterMethod::Identity;
    }
    let Some(cf) = names
        .lookup(b"CF")
        .and_then(|id| dict.get(id))
        .and_then(PdfObject::as_dict)
    else {
        return CryptFilterMethod::Identity;
    };
    let Some(entry) = cf.get(stmf).and_then(PdfObject::as_dict) else {
        return CryptFilterMethod::Identity;
    };
    let Some(cfm) = names
        .lookup(b"CFM")
        .and_then(|id| entry.get(id))
        .and_then(PdfObject::as_name)
    else {
        return CryptFilterMethod::Identity;
    };
    if name_is(cfm, b"AESV2") {
        CryptFilterMethod::AesV2
    } else if name_is(cfm, b"AESV3") {
        CryptFilterMethod::AesV3
    } else if name_is(cfm, b"V2") {
        CryptFilterMethod::V2
    } else {
        CryptFilterMethod::Identity
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    // The load-bearing assertion of the whole architecture: the snapshot is
    // freely shareable across worker threads.
    const fn assert_send_sync<T: Send + Sync>() {}
    const _: () = assert_send_sync::<DocumentSnapshot>();

    #[test]
    fn parse_context_detects_cycles_and_depth() {
        let limits = DocumentLimits {
            max_reference_chain: 2,
            ..Default::default()
        };
        let mut ctx = ParseContext::new();
        let a = ObjectId::new(1, 0);
        let b = ObjectId::new(2, 0);
        ctx.enter(a, &limits).unwrap();
        assert!(matches!(
            ctx.enter(a, &limits),
            Err(DocumentError::Object(ObjectError::ReferenceCycle(_)))
        ));
        ctx.enter(b, &limits).unwrap();
        assert!(matches!(
            ctx.enter(ObjectId::new(3, 0), &limits),
            Err(DocumentError::LimitExceeded(_))
        ));
        ctx.exit(b);
        ctx.exit(a);
    }

    #[test]
    fn begin_job_resets_job_state_but_keeps_worker_caches() {
        let mut ctx = ParseContext::new();
        let id = ObjectId::new(7, 0);
        ctx.recursion.push(id);
        ctx.decoded_bytes_used = 123;
        ctx.objects_visited = 9;
        ctx.recovery
            .push(RecoveryEvent::Other("previous page".into()));
        ctx.cache.insert(id, Arc::new(PdfObject::Null));

        ctx.begin_job();

        assert!(ctx.recursion.is_empty());
        assert_eq!(ctx.decoded_bytes_used, 0);
        assert_eq!(ctx.objects_visited, 0);
        assert!(ctx.recovery.is_empty());
        assert!(ctx.cache.contains_key(&id));
    }
}
