//! `load_structure`: header scan → startxref → xref chain (tables, streams,
//! hybrids) → trailer resolution, with full-file rebuild as the last-resort
//! recovery path.
//!
//! Tolerances are ported from PDFium's `CPDF_Parser` (intent only). All
//! offsets in this module — startxref values, xref entry offsets, `/Prev`,
//! `/XRefStm`, `Revision::xref_offset`, `XrefMap` offsets — are relative to
//! the **header position**: files with leading garbage prepend a delta that
//! writers did not account for, so the header becomes offset zero (PDFium's
//! `header_offset_`). `DocumentStructure::header_offset` carries the delta;
//! the document layer adds it back when reading object bytes.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use pdf_object::parser::ObjectParser;
use pdf_object::{Dictionary, NameTable, ObjectId, PdfObject, PdfString, StreamData};
use pdf_source::PdfSource;
use pdf_syntax::{Lexer, SyntaxLimits, Token};

use crate::decode::{DecodeBudget, decode_stream};
use crate::objstm::parse_object_stream_layout;
use crate::{
    DocumentStructure, ObjectLocation, PdfVersion, RecoveryEvent, Revision, StructureError,
    Trailer, XrefMap,
};

/// Limits enforced while loading structure. A structural mirror of the
/// document-level limits (`pdf-document` constructs one from its
/// `DocumentLimits`; this crate cannot see that type — dependency
/// direction).
#[derive(Debug, Clone)]
pub struct StructureLimits {
    pub syntax: SyntaxLimits,
    /// Maximum revisions (incremental updates) followed.
    pub max_revisions: usize,
    /// Maximum object number admitted to the xref map.
    pub max_objects: u32,
    /// Decompressed-byte budget for xref streams during open.
    pub max_decoded_bytes: usize,
}

impl Default for StructureLimits {
    fn default() -> Self {
        Self {
            syntax: SyntaxLimits::default(),
            max_revisions: 4096,
            max_objects: 8 * 1024 * 1024,
            max_decoded_bytes: 256 * 1024 * 1024,
        }
    }
}

/// How far leading garbage may push the `%PDF` header (PDFium scans
/// `0..=1024`).
const MAX_HEADER_GARBAGE: usize = 1024;

/// How far from the end `startxref` may sit (PDFium searches 4096 bytes).
const STARTXREF_TAIL_SCAN: usize = 4096;

/// Load all structural information from `source`.
pub fn load_structure(
    source: &dyn PdfSource,
    names: &NameTable,
    limits: &StructureLimits,
) -> Result<DocumentStructure, StructureError> {
    let len = source.len();
    let bytes: Cow<'_, [u8]> = source.read_range(0..len)?;
    let bytes: &[u8] = &bytes;

    // --- Header scan -----------------------------------------------------
    let header_offset = find_header(bytes).ok_or(StructureError::NoHeader)?;
    let file = &bytes[header_offset..];
    let mut recovery: Vec<RecoveryEvent> = Vec::new();
    let version = parse_version(file, &mut recovery);

    let mut loader = Loader {
        file,
        names,
        limits,
        budget: DecodeBudget::new(limits.max_decoded_bytes),
        entries: HashMap::new(),
        revisions: Vec::new(),
        visited: HashSet::new(),
        recovery,
    };

    // --- startxref → xref chain, with recovery fallbacks -----------------
    let mut usable = false;
    // Whether the usable chain came from a recovery fallback rather than the
    // reported `startxref`. A clean reported chain that resolves an empty page
    // tree is a legitimately empty document; only a *recovered* chain that
    // lands on a degenerate stub warrants escalating to a full rebuild.
    let mut used_recovery = false;
    let reported = find_startxref(file, &loader.limits.syntax);
    if let Some(reported) = reported
        && (reported as usize) < file.len()
    {
        usable = loader.try_chain(reported)?;
    }
    if !usable {
        // The reported chain was missing or unusable: scan for the last
        // classic `xref` keyword before giving up (partial recovery that
        // preserves incremental-update semantics when only startxref lied).
        if let Some(found) = find_last_keyword(file, b"xref")
            && Some(found as u64) != reported
        {
            loader.reset_merge_state();
            usable = loader.try_chain(found as u64)?;
            if usable {
                used_recovery = true;
                loader.recovery.push(RecoveryEvent::StartXrefRecovered {
                    reported,
                    used: found as u64,
                });
            }
        }
    }

    // --- Trailer resolution / full rebuild --------------------------------
    let mut trailer = usable.then(|| loader.resolve_trailer()).flatten();
    // Rebuild when the xref gave no trailer/`/Root`, or when a *recovered*
    // chain resolved a degenerate document — a stub page tree the recovery
    // stopped at (a linearized/incremental-update placeholder whose real page
    // tree lives in a later revision). A full rebuild's last-occurrence-wins
    // then recovers the real pages.
    let degenerate = used_recovery
        && trailer
            .as_ref()
            .is_some_and(|t| loader.page_tree_looks_empty(t));
    // A trailer that names a `/Root` the xref cannot resolve (missing entry or a
    // stale offset that no longer parses) is corruption invisible to the chain
    // walk — the trailer looks complete. Escalate to a full rebuild, which finds
    // the catalog by scanning `N G obj` headers and object streams. Unlike
    // `degenerate`, this applies to reported chains too: a healthy `/Root`
    // always resolves (directly or via an object stream), so a well-formed
    // document — incremental updates included — never triggers it.
    let root_unresolved = trailer.as_ref().is_some_and(|t| !loader.root_resolves(t));
    // A *recovered* chain whose entries no longer point at their objects is a
    // file whose byte offsets have shifted wholesale: the `/Root` may still
    // land (low object numbers often survive) while page and content objects
    // do not, which renders as a blank page rather than an open failure.
    // PDFium never uses such a table at all — a `startxref` that misses sends
    // it straight to `RebuildCrossRef` — so validate before trusting a table we
    // only found by scanning. Reported chains are left alone: their offsets are
    // as authoritative as the file gets, and PDFium keeps them too.
    let offsets_stale = used_recovery && !loader.xref_offsets_are_consistent();
    if trailer.is_none() || degenerate || root_unresolved || offsets_stale {
        // Snapshot the chain-derived state so a rebuild that fails to improve
        // matters (e.g. a minimal file whose `/Root` is intentionally external,
        // or a synthetic xref-stream fixture) can be rolled back instead of
        // clobbering usable entries with a sparser scan.
        let saved_entries = loader.entries.clone();
        let saved_revisions = loader.revisions.clone();
        let saved_recovery_len = loader.recovery.len();

        // Full-file reconstruction (PDFium `RebuildCrossRef`).
        loader.reset_merge_state();
        loader.rebuild();
        let rebuilt = loader.resolve_trailer();
        if loader.entries.is_empty() {
            return Err(StructureError::NoUsableXref);
        }
        // A rebuild is worth keeping when it yields a `/Root` that actually
        // resolves, or when we had no trailer at all (any catalog is progress).
        // Otherwise — the rebuild found no better catalog than the chain did —
        // roll back to the parsed xref rather than degrade it.
        let rebuilt_improves = rebuilt.as_ref().is_some_and(|t| loader.root_resolves(t));
        if rebuilt_improves || (trailer.is_none() && rebuilt.is_some()) {
            trailer = rebuilt;
        } else if trailer.is_some() {
            loader.entries = saved_entries;
            loader.revisions = saved_revisions;
            loader.recovery.truncate(saved_recovery_len);
        } else if rebuilt.is_some() {
            trailer = rebuilt;
        }
    }
    let trailer = trailer.ok_or(StructureError::NoCatalog)?;

    let xref = loader.build_xref_map(&trailer);
    Ok(DocumentStructure {
        version,
        header_offset: header_offset as u64,
        xref,
        trailer,
        revisions: loader.revisions.into_boxed_slice(),
        recovery: loader.recovery.into_boxed_slice(),
    })
}

/// Load a document's structure by going **straight to a full-file
/// reconstruction** (PDFium `RebuildCrossRef`), bypassing the reported xref
/// chain. This is a document-level escalation for a file whose chain loads
/// "cleanly" (resolvable `/Root`, consistent offsets) yet whose page-tree walk
/// still recovers zero pages — because a `/Kids` reference resolves to a null
/// the xref points at a wrong offset, corruption invisible to `load_structure`'s
/// own trailer/root-level gates. The rebuild's last-occurrence-wins scan and its
/// object-stream member re-indexing recover the page objects the stale entries
/// missed. The caller adopts the result only if it yields strictly more pages.
pub fn load_structure_rebuilt(
    source: &dyn PdfSource,
    names: &NameTable,
    limits: &StructureLimits,
) -> Result<DocumentStructure, StructureError> {
    let len = source.len();
    let bytes: Cow<'_, [u8]> = source.read_range(0..len)?;
    let bytes: &[u8] = &bytes;

    let header_offset = find_header(bytes).ok_or(StructureError::NoHeader)?;
    let file = &bytes[header_offset..];
    let mut recovery: Vec<RecoveryEvent> = Vec::new();
    let version = parse_version(file, &mut recovery);

    let mut loader = Loader {
        file,
        names,
        limits,
        budget: DecodeBudget::new(limits.max_decoded_bytes),
        entries: HashMap::new(),
        revisions: Vec::new(),
        visited: HashSet::new(),
        recovery,
    };

    loader.reset_merge_state();
    loader.rebuild();
    if loader.entries.is_empty() {
        return Err(StructureError::NoUsableXref);
    }
    let trailer = loader.resolve_trailer().ok_or(StructureError::NoCatalog)?;
    let xref = loader.build_xref_map(&trailer);
    Ok(DocumentStructure {
        version,
        header_offset: header_offset as u64,
        xref,
        trailer,
        revisions: loader.revisions.into_boxed_slice(),
        recovery: loader.recovery.into_boxed_slice(),
    })
}

/// Find `%PDF` within the first `MAX_HEADER_GARBAGE` bytes.
fn find_header(bytes: &[u8]) -> Option<usize> {
    let window_end = bytes.len().min(MAX_HEADER_GARBAGE + 4);
    bytes[..window_end].windows(4).position(|w| w == b"%PDF")
}

/// Parse `-M.N` after `%PDF`. Malformed versions default to 1.7 with a
/// recovery note — the header's presence, not its version, gates parsing.
fn parse_version(file: &[u8], recovery: &mut Vec<RecoveryEvent>) -> PdfVersion {
    let rest = &file[4..];
    let parsed = (|| {
        let rest = rest.strip_prefix(b"-")?;
        let major = rest
            .first()
            .filter(|b| b.is_ascii_digit())
            .map(|b| b - b'0')?;
        let rest = rest.get(1..)?.strip_prefix(b".")?;
        let minor = rest
            .first()
            .filter(|b| b.is_ascii_digit())
            .map(|b| b - b'0')?;
        Some(PdfVersion { major, minor })
    })();
    parsed.unwrap_or_else(|| {
        recovery.push(RecoveryEvent::Other(
            "malformed %PDF version; assuming 1.7".into(),
        ));
        PdfVersion { major: 1, minor: 7 }
    })
}

/// Locate the *last* `startxref` in the tail and read its operand.
fn find_startxref(file: &[u8], syntax: &SyntaxLimits) -> Option<u64> {
    let scan_start = file.len().saturating_sub(STARTXREF_TAIL_SCAN);
    let pos = scan_start + find_last_subslice(&file[scan_start..], b"startxref")?;
    let mut lexer = Lexer::new(file, 0, syntax);
    lexer.set_pos(pos);
    let kw = lexer.next_token().ok()?;
    if !matches!(&kw.value, Token::Keyword(k) if k == b"startxref") {
        return None;
    }
    match lexer.next_token().ok()?.value {
        Token::Integer(v) if v >= 0 => Some(v as u64),
        _ => None,
    }
}

/// Last occurrence of `needle` in `haystack`.
fn find_last_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    (0..=haystack.len() - needle.len())
        .rev()
        .find(|&i| &haystack[i..i + needle.len()] == needle)
}

/// Last *whole-word* occurrence of keyword `kw` (not embedded in a longer
/// regular-character run — `xref` inside `startxref` must not match).
fn find_last_keyword(haystack: &[u8], kw: &[u8]) -> Option<usize> {
    let mut end = haystack.len();
    while let Some(i) = find_last_subslice(&haystack[..end], kw) {
        let left_ok = i == 0 || !pdf_syntax::classify::is_regular(haystack[i - 1]);
        let right_ok = i + kw.len() >= haystack.len()
            || !pdf_syntax::classify::is_regular(haystack[i + kw.len()]);
        if left_ok && right_ok {
            return Some(i);
        }
        end = i;
    }
    None
}

/// Working state for one load.
struct Loader<'a> {
    file: &'a [u8],
    names: &'a NameTable,
    limits: &'a StructureLimits,
    budget: DecodeBudget,
    /// Merged view, **first write wins**: revisions are processed newest →
    /// oldest, so the first location recorded for an object number is the
    /// live one; older revisions (and older entries in hybrid files) are
    /// shadowed. Free entries occupy their number like any other write.
    entries: HashMap<u32, (u16, ObjectLocation)>,
    revisions: Vec<Revision>,
    visited: HashSet<u64>,
    recovery: Vec<RecoveryEvent>,
}

impl<'a> Loader<'a> {
    fn reset_merge_state(&mut self) {
        self.entries.clear();
        self.revisions.clear();
        self.visited.clear();
    }

    /// Record an entry unless the number is already live (first-write-wins).
    fn record(&mut self, number: u32, generation: u16, location: ObjectLocation) {
        if number >= self.limits.max_objects {
            return;
        }
        self.entries.entry(number).or_insert((generation, location));
    }

    /// Follow a chain and report whether it yielded a usable view.
    ///
    /// Limit violations (revision count, decode budget) are **fatal** —
    /// malicious files must terminate with errors, not silently continue
    /// into the rebuild path. Ordinary corruption is non-fatal: the caller
    /// falls back to scanning/rebuilding.
    fn try_chain(&mut self, start: u64) -> Result<bool, StructureError> {
        match self.follow_chain(start) {
            Ok(()) => Ok(!self.entries.is_empty()),
            Err(e @ StructureError::LimitExceeded(_)) => Err(e),
            Err(e @ StructureError::Decode(crate::decode::DecodeError::BudgetExceeded)) => Err(e),
            Err(_) => Ok(false),
        }
    }

    /// Follow the `/Prev` chain from `start` (header-relative offset).
    fn follow_chain(&mut self, start: u64) -> Result<(), StructureError> {
        let mut next = Some(start);
        while let Some(offset) = next {
            if offset as usize >= self.file.len() {
                self.recovery.push(RecoveryEvent::Other(format!(
                    "xref offset {offset} outside file; chain truncated"
                )));
                break;
            }
            if !self.visited.insert(offset) {
                // A /Prev cycle is not fatal — everything before the
                // repeat is already merged (PDFium stops the same way).
                self.recovery.push(RecoveryEvent::Other(format!(
                    "xref chain cycle at offset {offset}"
                )));
                break;
            }
            if self.revisions.len() >= self.limits.max_revisions {
                return Err(StructureError::LimitExceeded("max_revisions"));
            }
            next = self.parse_section(offset)?;
        }
        Ok(())
    }

    /// Parse one xref section (classic table or xref stream); returns the
    /// `/Prev` offset if any.
    fn parse_section(&mut self, offset: u64) -> Result<Option<u64>, StructureError> {
        let mut lexer = Lexer::new(self.file, 0, &self.limits.syntax);
        lexer.set_pos(offset as usize);
        let first = lexer.peek_token()?;
        match &first.value {
            Token::Keyword(kw) if kw == b"xref" => self.parse_classic_table(lexer, offset),
            Token::Integer(_) => self.parse_xref_stream(offset),
            _ => Err(StructureError::NoUsableXref),
        }
    }

    /// Classic `xref` table. Entries are read token-wise, which naturally
    /// tolerates the 19/21-byte CR/LF-sloppy variants PDFium accepts.
    fn parse_classic_table(
        &mut self,
        mut lexer: Lexer<'a>,
        offset: u64,
    ) -> Result<Option<u64>, StructureError> {
        let _ = lexer.next_token()?; // consume `xref`
        let mut staged: Vec<(u32, u16, ObjectLocation)> = Vec::new();
        let trailer_dict: Option<Dictionary> = loop {
            let tok = lexer.next_token()?;
            match tok.value {
                Token::Integer(start) => {
                    let count_tok = lexer.next_token()?;
                    let Token::Integer(count) = count_tok.value else {
                        break None; // malformed subsection header
                    };
                    let (Ok(start), Ok(count)) = (u32::try_from(start), u32::try_from(count))
                    else {
                        break None;
                    };
                    // A subsection declared as starting at object 1 whose first
                    // entry is the free-list head (`0000000000 65535 f`) is a
                    // producer that wrote `1 N` where it meant `0 N`: object 0
                    // is *always* that head. Taken literally every entry lands
                    // one object number too high, so the real object 1 is
                    // recorded free and every reference to it dies — in
                    // pdfjs/issue7229 that is the page's only image, and the
                    // page renders blank. PDFium and pdf.js both correct this.
                    let mut start = start;
                    if start == 1 && count > 0 {
                        let probe = lexer.pos();
                        let head = (
                            lexer.next_token().map(|t| t.value),
                            lexer.next_token().map(|t| t.value),
                            lexer.next_token().map(|t| t.value),
                        );
                        if let (
                            Ok(Token::Integer(0)),
                            Ok(Token::Integer(65535)),
                            Ok(Token::Keyword(k)),
                        ) = head
                            && k.as_slice() == b"f"
                        {
                            start = 0;
                        }
                        lexer.set_pos(probe);
                    }
                    for i in 0..count {
                        let save_entry = lexer.pos();
                        let (o, g, k) = (
                            lexer.next_token()?,
                            lexer.next_token()?,
                            lexer.next_token()?,
                        );
                        let (
                            Token::Integer(entry_off),
                            Token::Integer(entry_gen),
                            Token::Keyword(kind),
                        ) = (o.value, g.value, k.value)
                        else {
                            // Subsection shorter than declared: stop here,
                            // keep what parsed (count lies are common).
                            lexer.set_pos(save_entry);
                            break;
                        };
                        let number = start.saturating_add(i);
                        let generation = u16::try_from(entry_gen.clamp(0, i64::from(u16::MAX)))
                            .unwrap_or(u16::MAX);
                        match kind.as_slice() {
                            b"n" if entry_off >= 0 => staged.push((
                                number,
                                generation,
                                ObjectLocation::Offset(entry_off as u64),
                            )),
                            b"f" => staged.push((number, generation, ObjectLocation::Free)),
                            _ => {
                                lexer.set_pos(save_entry);
                                break;
                            }
                        }
                    }
                }
                Token::Keyword(kw) if kw == b"trailer" => {
                    let mut parser = ObjectParser::from_lexer(lexer.clone(), self.names);
                    match parser.parse_object() {
                        Ok(PdfObject::Dictionary(d)) => break Some(Arc::unwrap_or_clone(d)),
                        _ => break None,
                    }
                }
                _ => break None,
            }
        };

        let Some(trailer_dict) = trailer_dict else {
            // A classic table without a readable trailer is unusable as a
            // revision (no /Prev, no /Root contribution).
            return Err(StructureError::NoUsableXref);
        };

        // Table entries first (they take precedence over the hybrid
        // /XRefStm entries of the same revision — PDFium's merge order).
        for (n, g, loc) in staged {
            self.record(n, g, loc);
        }
        let trailer_dict = Arc::new(trailer_dict);
        self.revisions.push(Revision {
            xref_offset: offset,
            trailer: Arc::clone(&trailer_dict),
        });

        // Hybrid-reference hop: /XRefStm within this revision.
        if let Some(stm_off) = trailer_dict
            .get(self.names.known.xref_stm)
            .and_then(PdfObject::as_int)
            .and_then(|v| u64::try_from(v).ok())
            && (stm_off as usize) < self.file.len()
            && self.visited.insert(stm_off)
        {
            // Failures in the hybrid stream are tolerated: the classic
            // table alone may still be a usable view.
            if let Err(e) = self.parse_xref_stream(stm_off) {
                self.recovery.push(RecoveryEvent::Other(format!(
                    "hybrid /XRefStm at {stm_off} unusable: {e}"
                )));
            }
        }

        Ok(trailer_dict
            .get(self.names.known.prev)
            .and_then(PdfObject::as_int)
            .and_then(|v| u64::try_from(v).ok()))
    }

    /// Cross-reference stream (ISO 32000-1 §7.5.8).
    fn parse_xref_stream(&mut self, offset: u64) -> Result<Option<u64>, StructureError> {
        let window = &self.file[offset as usize..];
        let mut parser = ObjectParser::new(window, offset, &self.limits.syntax, self.names);
        // /Length can in principle be indirect; unresolvable pre-xref, so
        // the parser falls back to scanning for `endstream`.
        let parsed = parser.parse_indirect(&mut |_| None)?;
        let PdfObject::Stream(stream) = parsed.object else {
            return Err(StructureError::NoUsableXref);
        };
        for repair in &parsed.repairs {
            match repair {
                pdf_object::parser::IndirectRepair::StreamLengthRepaired { declared, actual } => {
                    self.recovery.push(RecoveryEvent::StreamLengthRepaired {
                        id: parsed.id,
                        declared: *declared,
                        actual: *actual,
                    })
                }
                pdf_object::parser::IndirectRepair::MissingReferenceGeneration {
                    number,
                    offset,
                } => self
                    .recovery
                    .push(RecoveryEvent::ReferenceGenerationRepaired {
                        id: parsed.id,
                        referenced: *number,
                        offset: *offset,
                    }),
                pdf_object::parser::IndirectRepair::MissingEndObj => {}
            }
        }
        let dict = &stream.dict;
        let raw: &[u8] = match &stream.data {
            StreamData::InSource { offset, len } => {
                let (start, end) = (*offset as usize, (*offset + *len) as usize);
                self.file
                    .get(start..end)
                    .ok_or(StructureError::NoUsableXref)?
            }
            StreamData::Owned(bytes) => bytes,
        };
        let data = decode_stream(raw, dict, self.names, &mut self.budget)?;

        let get_int = |key: pdf_object::NameId| dict.get(key).and_then(PdfObject::as_int);
        let size = get_int(self.names.known.size).unwrap_or(0).max(0);

        // /W: at least 3 field widths; extras pad the stride but are
        // ignored (PDFium tolerates and so do we).
        let widths: Vec<u64> = match dict.get(self.names.known.w) {
            Some(PdfObject::Array(a)) => a
                .iter()
                .map(|o| o.as_int().and_then(|v| u64::try_from(v).ok()))
                .collect::<Option<Vec<u64>>>()
                .ok_or(StructureError::NoUsableXref)?,
            _ => return Err(StructureError::NoUsableXref),
        };
        if widths.len() < 3 || widths.iter().any(|&w| w > 8) {
            return Err(StructureError::NoUsableXref);
        }
        let total_width: u64 = widths.iter().sum();
        if total_width == 0 {
            return Err(StructureError::NoUsableXref);
        }

        // /Index: subsection pairs; default [0 Size].
        let mut indices: Vec<(u32, u32)> = Vec::new();
        if let Some(PdfObject::Array(a)) = dict.get(self.names.known.index) {
            for pair in a.chunks(2) {
                if let [s, c] = pair
                    && let (Some(s), Some(c)) = (s.as_int(), c.as_int())
                    && let (Ok(s), Ok(c)) = (u32::try_from(s), u32::try_from(c))
                {
                    indices.push((s, c));
                }
            }
        }
        if indices.is_empty() {
            indices.push((0, u32::try_from(size).unwrap_or(0)));
        }

        let stride = usize::try_from(total_width).unwrap_or(usize::MAX);
        let mut seg_start = 0usize;
        for (start_num, count) in indices {
            let count_usize = count as usize;
            let Some(seg_len) = count_usize.checked_mul(stride) else {
                continue;
            };
            let Some(seg_end) = seg_start.checked_add(seg_len) else {
                continue;
            };
            if seg_end > data.len() {
                continue; // segment overruns data: skip pair (PDFium)
            }
            let seg = &data[seg_start..seg_end];
            for i in 0..count_usize {
                let number = start_num.saturating_add(i as u32);
                let entry = &seg[i * stride..(i + 1) * stride];
                self.record_stream_entry(entry, &widths, number);
            }
            seg_start = seg_end;
        }

        let dict_arc = Arc::new(dict.clone());
        self.revisions.push(Revision {
            xref_offset: offset,
            trailer: dict_arc,
        });

        Ok(dict
            .get(self.names.known.prev)
            .and_then(PdfObject::as_int)
            .and_then(|v| u64::try_from(v).ok()))
    }

    /// Decode one xref-stream entry (ISO 32000-1 Table 18 / PDFium
    /// `ProcessCrossRefStreamEntry`).
    fn record_stream_entry(&mut self, entry: &[u8], widths: &[u64], number: u32) {
        fn field(entry: &[u8], widths: &[u64], idx: usize) -> u64 {
            let start: u64 = widths[..idx].iter().sum();
            let (start, len) = (start as usize, widths[idx] as usize);
            entry[start..start + len]
                .iter()
                .fold(0u64, |acc, &b| (acc << 8) | u64::from(b))
        }
        // W[0] = 0 → default entry type 1 (ISO 32000-1 Table 17).
        let entry_type = if widths[0] == 0 {
            1
        } else {
            field(entry, widths, 0)
        };
        let f1 = field(entry, widths, 1);
        let f2 = field(entry, widths, 2);
        match entry_type {
            0 => {
                if let Ok(generation) = u16::try_from(f2) {
                    self.record(number, generation, ObjectLocation::Free);
                }
            }
            1 => {
                if let Ok(generation) = u16::try_from(f2) {
                    self.record(number, generation, ObjectLocation::Offset(f1));
                }
            }
            2 => {
                if let (Ok(container), Ok(index)) = (u32::try_from(f1), u32::try_from(f2)) {
                    // Generation of in-stream objects is 0 by definition.
                    self.record(
                        number,
                        0,
                        ObjectLocation::InObjectStream { container, index },
                    );
                }
            }
            // Unknown types: skip the entry, keep the section (PDFium).
            _ => {}
        }
    }

    /// Resolve the current trailer view from the merged revisions:
    /// newest-first, first hit wins per key.
    fn resolve_trailer(&self) -> Option<Trailer> {
        let mut root: Option<ObjectId> = None;
        let mut info: Option<ObjectId> = None;
        let mut encrypt: Option<ObjectId> = None;
        let mut file_id: Option<[PdfString; 2]> = None;
        for rev in &self.revisions {
            let d = &rev.trailer;
            if root.is_none() {
                root = d
                    .get(self.names.known.root)
                    .and_then(PdfObject::as_reference);
            }
            if info.is_none() {
                info = d
                    .get(self.names.known.info)
                    .and_then(PdfObject::as_reference);
            }
            if encrypt.is_none() {
                encrypt = d
                    .get(self.names.known.encrypt)
                    .and_then(PdfObject::as_reference);
            }
            if file_id.is_none()
                && let Some(PdfObject::Array(a)) = d.get(self.names.known.id)
                && let [PdfObject::String(a0), PdfObject::String(a1)] = a.as_ref()
            {
                file_id = Some([a0.clone(), a1.clone()]);
            }
        }
        let root = root?;
        let dict = self
            .revisions
            .first()
            .map(|r| Arc::clone(&r.trailer))
            .unwrap_or_else(|| Arc::new(Dictionary::new()));
        Some(Trailer {
            dict,
            root,
            info,
            encrypt,
            file_id,
        })
    }

    /// Full-file reconstruction: index every `N G obj` header (last
    /// occurrence of an object number wins — later bytes are later
    /// revisions), collect `trailer` dictionaries, and fall back to a
    /// `/Type /Catalog` scan for the root (PDFium `RebuildCrossRef`).
    fn rebuild(&mut self) {
        self.recovery.push(RecoveryEvent::XrefRebuilt);
        let file = self.file;
        // number → (effective file position, generation, location). The
        // position orders revisions when an object is defined more than once:
        // an uncompressed body is at its header offset; an object-stream member
        // is at its container's header offset (the revision that wrote it), so
        // a later uncompressed redefinition still wins.
        let mut best: HashMap<u32, (u64, u16, ObjectLocation)> = HashMap::new();
        let mut trailer_dicts: Vec<Dictionary> = Vec::new();
        // `/Type /ObjStm` containers found in the scan (number, header offset),
        // to decompress and index below.
        let mut containers: Vec<(u32, u64)> = Vec::new();
        // `/Type /XRef` cross-reference streams (number, header offset). Their
        // *dict* is the revision trailer — an xref-stream-only file (PDF ≥ 1.5)
        // has no `trailer` keyword, so its /Root lives only here.
        let mut xref_streams: Vec<(u32, u64)> = Vec::new();
        let mut current: Option<(u32, u64)> = None;

        let mut i = 0usize;
        while i + 3 <= file.len() {
            if &file[i..i + 3] == b"obj"
                && (i + 3 == file.len() || !pdf_syntax::classify::is_regular(file[i + 3]))
                && let Some((start, number, generation)) = backtrack_obj_header(file, i)
            {
                let start = start as u64;
                // Last occurrence wins (headers are scanned in offset order).
                best.insert(number, (start, generation, ObjectLocation::Offset(start)));
                current = Some((number, start));
                i += 3;
                continue;
            }
            // The `/ObjStm` type name inside an object marks it as a container;
            // remember the enclosing object so its members can be indexed.
            if i + 6 <= file.len()
                && &file[i..i + 6] == b"ObjStm"
                && (i == 0 || !pdf_syntax::classify::is_regular(file[i - 1]))
                && (i + 6 == file.len() || !pdf_syntax::classify::is_regular(file[i + 6]))
            {
                if let Some(c) = current.take() {
                    containers.push(c);
                }
                i += 6;
                continue;
            }
            // A `/XRef` type name marks a cross-reference stream. Remember the
            // enclosing object so its dict (the revision trailer, carrying
            // /Root) is parsed after the scan.
            if i + 4 <= file.len()
                && &file[i..i + 4] == b"XRef"
                && (i == 0 || !pdf_syntax::classify::is_regular(file[i - 1]))
                && (i + 4 == file.len() || !pdf_syntax::classify::is_regular(file[i + 4]))
            {
                if let Some(c) = current {
                    xref_streams.push(c);
                }
                i += 4;
                continue;
            }
            if i + 7 <= file.len()
                && &file[i..i + 7] == b"trailer"
                && (i == 0 || !pdf_syntax::classify::is_regular(file[i - 1]))
                && (i + 7 == file.len() || !pdf_syntax::classify::is_regular(file[i + 7]))
            {
                let mut parser = ObjectParser::new(
                    &file[i + 7..],
                    (i + 7) as u64,
                    &self.limits.syntax,
                    self.names,
                );
                if let Ok(PdfObject::Dictionary(d)) = parser.parse_object() {
                    trailer_dicts.push(Arc::unwrap_or_clone(d));
                }
                i += 7;
                continue;
            }
            i += 1;
        }

        // Decompress each object-stream container and index its members. A
        // corrupted modern PDF keeps most objects (pages, fonts, content)
        // inside `/Type /ObjStm`, invisible to the `N G obj` scan; without this
        // the rebuild silently loses them (PDFium's `RebuildCrossRef` parses
        // object streams the same way). A member is overridden only by a
        // *later* uncompressed definition.
        for (container, offset) in containers {
            let Some(members) = self.read_objstm_members(offset) else {
                continue;
            };
            for (index, member) in members.into_iter().enumerate() {
                let overridden_by_later =
                    matches!(best.get(&member), Some(&(pos, _, _)) if pos > offset);
                if !overridden_by_later {
                    best.insert(
                        member,
                        (
                            offset,
                            0,
                            ObjectLocation::InObjectStream {
                                container,
                                index: index as u32,
                            },
                        ),
                    );
                }
            }
        }

        // A cross-reference stream's dict *is* the revision trailer for an
        // xref-stream file (PDF ≥ 1.5, no `trailer` keyword). Parse each so its
        // /Root (and /Encrypt, /Info) is recovered; otherwise a corrupted
        // modern PDF whose catalog lives inside an object stream has no findable
        // root, and `find_catalog` — which only inspects offset objects — misses
        // it. Pushed after the keyword trailers so the newest xref stream wins
        // the newest-first ordering below.
        for (_num, offset) in xref_streams {
            let Some(window) = self.file.get(offset as usize..) else {
                continue;
            };
            let mut parser = ObjectParser::new(window, offset, &self.limits.syntax, self.names);
            if let Ok(parsed) = parser.parse_indirect(&mut |_| None)
                && let PdfObject::Stream(stream) = &parsed.object
            {
                trailer_dicts.push(stream.dict.clone());
            }
        }

        for (n, (_, g, loc)) in best {
            self.record(n, g, loc);
        }
        // Later trailers are later revisions: iterate newest-last →
        // register newest first for the first-hit-wins trailer resolution.
        for dict in trailer_dicts.into_iter().rev() {
            self.revisions.push(Revision {
                xref_offset: 0,
                trailer: Arc::new(dict),
            });
        }

        // No trailer with /Root anywhere: hunt for a /Type /Catalog object.
        let root_known = self
            .revisions
            .iter()
            .any(|r| r.trailer.get(self.names.known.root).is_some());
        if !root_known && let Some((id, _)) = self.find_catalog() {
            let dict = Dictionary::from_pairs([(self.names.known.root, PdfObject::Reference(id))]);
            self.revisions.insert(
                0,
                Revision {
                    xref_offset: 0,
                    trailer: Arc::new(dict),
                },
            );
        }
    }

    /// Scan indexed objects for a `/Type /Catalog` dictionary.
    fn find_catalog(&mut self) -> Option<(ObjectId, Arc<Dictionary>)> {
        let catalog = self.names.intern(b"Catalog");
        let mut numbers: Vec<u32> = self.entries.keys().copied().collect();
        numbers.sort_unstable();
        for n in numbers {
            let (generation, ObjectLocation::Offset(off)) = self.entries[&n] else {
                continue;
            };
            let window = self.file.get(off as usize..)?;
            let mut parser = ObjectParser::new(window, off, &self.limits.syntax, self.names);
            let Ok(parsed) = parser.parse_indirect(&mut |_| None) else {
                continue;
            };
            if let Some(dict) = parsed.object.as_dict()
                && dict
                    .get(self.names.known.type_)
                    .and_then(PdfObject::as_name)
                    == Some(catalog)
            {
                return Some((ObjectId::new(n, generation), Arc::new(dict.clone())));
            }
        }
        None
    }

    /// Parse the object at `offset` as a `/Type /ObjStm` container and return
    /// its member object numbers in index order. `None` if it is not a
    /// (decodable) object stream.
    fn read_objstm_members(&mut self, offset: u64) -> Option<Vec<u32>> {
        let file = self.file;
        let names = self.names;
        let limits = self.limits;

        let window = file.get(offset as usize..)?;
        let mut parser = ObjectParser::new(window, offset, &limits.syntax, names);
        let parsed = parser.parse_indirect(&mut |_| None).ok()?;
        let PdfObject::Stream(stream) = &parsed.object else {
            return None;
        };
        if stream
            .dict
            .get(names.known.type_)
            .and_then(PdfObject::as_name)
            != Some(names.known.obj_stm)
        {
            return None;
        }
        let raw: &[u8] = match &stream.data {
            StreamData::InSource { offset, len } => {
                file.get(*offset as usize..(*offset + *len) as usize)?
            }
            StreamData::Owned(bytes) => bytes,
        };
        let data = decode_stream(raw, &stream.dict, names, &mut self.budget).ok()?;
        let n = stream
            .dict
            .get(names.known.n)
            .and_then(PdfObject::as_int)
            .unwrap_or(0);
        let first = stream
            .dict
            .get(names.known.first)
            .and_then(PdfObject::as_int)
            .unwrap_or(0);
        let layout = parse_object_stream_layout(&data, n, first, &limits.syntax).ok()?;
        Some(layout.members.iter().map(|m| m.number).collect())
    }

    /// Whether the document reached through `trailer` has a *degenerate* page
    /// tree — a root `/Pages` node with `/Count ≤ 0` and no `/Kids`. This is
    /// the signature of a stub first revision (linearized files begin with an
    /// empty `<</Count 0/Kids[]/Type/Pages>>` placeholder that a later revision
    /// overrides); if the recovered xref stopped at that stub, the whole
    /// document reads as zero pages. Seeing it, `load_structure` escalates to a
    /// full rebuild, where last-occurrence-wins picks up the real page tree.
    ///
    /// Conservative: returns `false` whenever the catalog or `/Pages` node
    /// cannot be resolved from a file offset (e.g. it lives in an object
    /// stream), so a healthy document is never needlessly rebuilt.
    fn page_tree_looks_empty(&self, trailer: &Trailer) -> bool {
        let Some(catalog) = self.fetch_offset_dict(trailer.root) else {
            return false;
        };
        let Some(pages) = self.resolve_dict_value(catalog.get(self.names.known.pages)) else {
            return false;
        };
        let count_nonpositive = pages
            .get(self.names.known.count)
            .and_then(PdfObject::as_int)
            .unwrap_or(0)
            <= 0;
        let kids_empty = match pages.get(self.names.known.kids) {
            Some(PdfObject::Array(kids)) => kids.is_empty(),
            None => true,
            // An indirect /Kids is not evidence of emptiness.
            _ => false,
        };
        count_nonpositive && kids_empty
    }

    /// Whether the trailer's `/Root` reference resolves to a catalog the xref
    /// chain can actually reach. A `/Root` that is absent from the merged xref,
    /// or whose recorded offset no longer parses as a dictionary, is corruption
    /// the chain itself cannot detect (the trailer still names a `/Root`); it
    /// warrants escalating to a full rebuild, which relocates the catalog by
    /// scanning object headers and object streams.
    ///
    /// A `/Root` that lives in an object stream is trusted without decoding it
    /// here — object-stream membership is normal for healthy modern PDFs, and
    /// the offset-only fetch below cannot read it, so treating it as unresolved
    /// would needlessly rebuild every compressed document.
    fn root_resolves(&self, trailer: &Trailer) -> bool {
        match self.entries.get(&trailer.root.number) {
            None => false,
            Some((_, ObjectLocation::InObjectStream { .. })) => true,
            Some((_, ObjectLocation::Offset(_))) => self.fetch_offset_dict(trailer.root).is_some(),
            Some((_, ObjectLocation::Free)) => false,
        }
    }

    /// Whether every offset entry in the merged view actually points at the
    /// object header it claims (PDFium `CPDF_Parser::VerifyCrossRefTable`).
    ///
    /// Only meaningful for a chain reached by *recovering* the `startxref`
    /// offset: a file whose `startxref` is wrong is a file whose byte offsets
    /// have shifted, and the table sitting at the recovered location is then
    /// usually stale in the same way. PDFium never even looks at such a table —
    /// a `startxref` that does not land on `xref`/an xref stream sends it
    /// straight to `RebuildCrossRef` — so validating here is how we reach the
    /// same rebuild from a scan-based recovery that PDFium does not perform.
    ///
    /// Entries in object streams and free entries have no offset to check and
    /// are skipped. A single mismatch condemns the table: partial staleness is
    /// what makes these files render half-empty rather than fail outright.
    fn xref_offsets_are_consistent(&self) -> bool {
        self.entries.iter().all(|(&number, &(_, location))| {
            let ObjectLocation::Offset(off) = location else {
                return true;
            };
            self.object_header_number_at(off) == Some(number)
        })
    }

    /// Parse `N G obj` at `off` and return `N`. `None` when the bytes there are
    /// not an object header.
    fn object_header_number_at(&self, off: u64) -> Option<u32> {
        let window = self.file.get(off as usize..)?;
        let mut i = 0usize;
        let skip_ws = |w: &[u8], i: &mut usize| {
            while w.get(*i).is_some_and(|b| b.is_ascii_whitespace()) {
                *i += 1;
            }
        };
        let take_digits = |w: &[u8], i: &mut usize| -> Option<u64> {
            let start = *i;
            let mut v: u64 = 0;
            while let Some(&b) = w.get(*i) {
                if !b.is_ascii_digit() {
                    break;
                }
                // Saturate rather than wrap: an absurd run of digits is not an
                // object number we could match anyway.
                v = v.saturating_mul(10).saturating_add(u64::from(b - b'0'));
                *i += 1;
            }
            (*i > start).then_some(v)
        };
        skip_ws(window, &mut i);
        let number = take_digits(window, &mut i)?;
        skip_ws(window, &mut i);
        take_digits(window, &mut i)?; // generation
        skip_ws(window, &mut i);
        window.get(i..i + 3).filter(|w| *w == b"obj")?;
        u32::try_from(number).ok()
    }

    /// Fetch and parse an indirect object located at a file offset, returning
    /// its dictionary. `None` for free/compressed/unparsable objects.
    fn fetch_offset_dict(&self, id: ObjectId) -> Option<Dictionary> {
        let &(_, ObjectLocation::Offset(off)) = self.entries.get(&id.number)? else {
            return None;
        };
        let window = self.file.get(off as usize..)?;
        let mut parser = ObjectParser::new(window, off, &self.limits.syntax, self.names);
        let parsed = parser.parse_indirect(&mut |_| None).ok()?;
        parsed.object.as_dict().cloned()
    }

    /// Resolve a dictionary value that is either a direct dictionary or a
    /// one-level reference to one.
    fn resolve_dict_value(&self, value: Option<&PdfObject>) -> Option<Dictionary> {
        match value? {
            PdfObject::Reference(id) => self.fetch_offset_dict(*id),
            PdfObject::Dictionary(d) => Some((**d).clone()),
            _ => None,
        }
    }

    /// Freeze the merged entries into the dense `XrefMap`.
    fn build_xref_map(&mut self, trailer: &Trailer) -> XrefMap {
        let max_num = self.entries.keys().copied().max().unwrap_or(0);
        let size = (max_num as usize) + 1;
        let mut locations = vec![ObjectLocation::Free; size];
        let mut generations = vec![0u16; size];
        for (&n, &(g, loc)) in &self.entries {
            locations[n as usize] = loc;
            generations[n as usize] = g;
        }
        // Object 0 is the free-list head; never a real object.
        locations[0] = ObjectLocation::Free;

        // /Size disagreement is informational (PDFium ignores wrong sizes;
        // we record the repair for observability).
        if let Some(declared) = trailer
            .dict
            .get(self.names.known.size)
            .and_then(PdfObject::as_int)
            && declared != size as i64
        {
            self.recovery.push(RecoveryEvent::SizeRepaired {
                declared,
                actual: size as u32,
            });
        }
        XrefMap::from_parts(locations.into_boxed_slice(), generations.into_boxed_slice())
    }
}

/// Scan `window` for an `N G obj` header whose object number is `number`,
/// returning `(header start relative to window, generation)`. Used by the
/// document layer to repair xref entries whose declared offset does not
/// land on the declared object (PDFium re-scans the same way).
pub fn scan_for_object_header(window: &[u8], number: u32) -> Option<(usize, u16)> {
    let mut i = 0usize;
    while i + 3 <= window.len() {
        if &window[i..i + 3] == b"obj"
            && (i + 3 == window.len() || !pdf_syntax::classify::is_regular(window[i + 3]))
            && let Some((start, n, generation)) = backtrack_obj_header(window, i)
            && n == number
        {
            return Some((start, generation));
        }
        i += 1;
    }
    None
}

/// From an `obj` keyword at `kw_pos`, backtrack over `<num> <gen>` and
/// return (header start, number, generation).
fn backtrack_obj_header(file: &[u8], kw_pos: usize) -> Option<(usize, u32, u16)> {
    let mut j = kw_pos;
    // whitespace between gen and `obj` (required)
    let ws_end = j;
    while j > 0 && pdf_syntax::classify::is_whitespace(file[j - 1]) {
        j -= 1;
    }
    if j == ws_end {
        return None;
    }
    // generation digits
    let gen_end = j;
    while j > 0 && file[j - 1].is_ascii_digit() {
        j -= 1;
    }
    if j == gen_end {
        return None;
    }
    let generation: u16 = std::str::from_utf8(&file[j..gen_end]).ok()?.parse().ok()?;
    // whitespace between num and gen (required)
    let ws2_end = j;
    while j > 0 && pdf_syntax::classify::is_whitespace(file[j - 1]) {
        j -= 1;
    }
    if j == ws2_end {
        return None;
    }
    // object-number digits
    let num_end = j;
    while j > 0 && file[j - 1].is_ascii_digit() {
        j -= 1;
    }
    if j == num_end {
        return None;
    }
    // Word boundary before the number ("x12 0 obj" is not a header).
    if j > 0 && pdf_syntax::classify::is_regular(file[j - 1]) {
        return None;
    }
    let number: u32 = std::str::from_utf8(&file[j..num_end]).ok()?.parse().ok()?;
    if number == 0 {
        return None;
    }
    Some((j, number, generation))
}
