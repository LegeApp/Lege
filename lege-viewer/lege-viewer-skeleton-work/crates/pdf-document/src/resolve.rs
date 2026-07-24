//! Object resolution (Design A: worker-local caches over the shared
//! immutable structural index).
//!
//! All mutable state — the parsed-object cache, the decompressed
//! object-stream cache, budgets, recursion tracking, and resolve-time
//! repair events — lives in the caller's [`ParseContext`]. The resolver
//! itself is a stack of shared references and is trivially re-creatable,
//! which is what lets `ObjectRepository::resolve` stay `&self`.

use std::borrow::Cow;
use std::sync::Arc;

use pdf_object::parser::{IndirectRepair, ObjectParser};
use pdf_object::{
    Dictionary, NameTable, ObjectError, ObjectId, PdfObject, PdfStream, PdfString, StreamData,
};
use pdf_security::SecurityContext;
use pdf_source::PdfSource;
use pdf_structure::decode::{DecodeBudget, DecodeError, decode_stream};
use pdf_structure::objstm::parse_object_stream_layout;
use pdf_structure::{DocumentStructure, ObjectLocation, RecoveryEvent, scan_for_object_header};
use pdf_syntax::SyntaxError;

use crate::{DocumentError, DocumentLimits, ObjStmCached, ParseContext};

/// Initial read window for non-contiguous sources; grown geometrically when
/// an object (usually a stream body) extends past it.
const INITIAL_WINDOW: u64 = 64 * 1024;

/// Borrowed view of everything needed to resolve objects. Constructed
/// per-call from a snapshot (or from parts during `open`, before the
/// snapshot exists).
pub(crate) struct Resolver<'a> {
    pub source: &'a dyn PdfSource,
    pub structure: &'a DocumentStructure,
    pub names: &'a NameTable,
    pub limits: &'a DocumentLimits,
    /// The document's decryption oracle, when encrypted. `None` before the
    /// handler is built (reading the `/Encrypt` dictionary at open) and for
    /// every unencrypted document.
    pub security: Option<&'a SecurityContext>,
}

enum Attempt {
    /// Parsed successfully (object, repairs, header-relative offset it was
    /// actually found at).
    Parsed(Box<pdf_object::parser::ParsedIndirect>, u64),
    /// The window was too small — grow and retry.
    Grow,
    Fail(DocumentError),
}

impl Resolver<'_> {
    /// Resolve `id` to its parsed value, following reference chains, with
    /// worker-local caching. References to free/undefined objects resolve
    /// to `Null` per ISO 32000-1 §7.3.10 (matching PDFium).
    pub fn resolve(
        &self,
        id: ObjectId,
        ctx: &mut ParseContext,
    ) -> Result<Arc<PdfObject>, DocumentError> {
        if let Some(hit) = ctx.cache_get(id) {
            #[cfg(feature = "profiling")]
            {
                ctx.object_cache_hits += 1;
            }
            return Ok(hit);
        }
        #[cfg(feature = "profiling")]
        {
            ctx.object_cache_misses += 1;
        }
        ctx.enter(id, self.limits)?;
        let result = self.resolve_uncached(id, ctx);
        ctx.exit(id);
        let value = result?;
        ctx.cache_put(id, Arc::clone(&value));
        Ok(value)
    }

    /// Resolve a possibly-indirect value in place: references resolve,
    /// direct objects are returned as-is (cheap `Arc` clones inside).
    pub fn resolve_value(
        &self,
        value: &PdfObject,
        ctx: &mut ParseContext,
    ) -> Result<Arc<PdfObject>, DocumentError> {
        match value {
            PdfObject::Reference(id) => self.resolve(*id, ctx),
            other => Ok(Arc::new(other.clone())),
        }
    }

    fn resolve_uncached(
        &self,
        id: ObjectId,
        ctx: &mut ParseContext,
    ) -> Result<Arc<PdfObject>, DocumentError> {
        ctx.objects_visited += 1;
        let object = match self.structure.xref.locate(id) {
            ObjectLocation::Free => PdfObject::Null,
            ObjectLocation::Offset(offset) => {
                // Objects stored at a file offset are individually encrypted;
                // decrypt their strings and stream body with this object's key.
                let parsed = self.parse_uncompressed(offset, id, ctx)?;
                self.decrypt_uncompressed(id, parsed)?
            }
            ObjectLocation::InObjectStream { container, index } => {
                // Members of an object stream are *not* individually encrypted:
                // the container stream was decrypted whole, so its members are
                // already plaintext (ISO 32000-1 §7.6.2).
                self.parse_compressed(container, index, id, ctx)?
            }
        };
        // Reference chains (`1 0 obj 2 0 R endobj`): follow while `id` is
        // still on the recursion stack, so cycles and over-long chains hit
        // the ParseContext guards.
        if let PdfObject::Reference(next) = object {
            return self.resolve(next, ctx);
        }
        Ok(Arc::new(object))
    }

    /// Decrypt a freshly-parsed uncompressed object in place (rebuilding it,
    /// since its parts are `Arc`-shared but refcount-1 here). A no-op unless
    /// the document is encrypted; the `/Encrypt` dictionary is exempt because
    /// it is never itself encrypted (it holds the very keys being derived).
    fn decrypt_uncompressed(
        &self,
        id: ObjectId,
        object: PdfObject,
    ) -> Result<PdfObject, DocumentError> {
        let Some(sec) = self.security else {
            return Ok(object);
        };
        if !sec.is_encrypted() {
            return Ok(object);
        }
        if self.structure.trailer.encrypt.map(|e| e.number) == Some(id.number) {
            return Ok(object);
        }
        self.decrypt_value(sec, id, object)
    }

    /// Recursively decrypt every string and stream body reachable from `object`
    /// with indirect object `id`'s key.
    fn decrypt_value(
        &self,
        sec: &SecurityContext,
        id: ObjectId,
        object: PdfObject,
    ) -> Result<PdfObject, DocumentError> {
        Ok(match object {
            PdfObject::String(s) => {
                let mut buf = s.as_bytes().to_vec();
                sec.decrypt_in_place(id, &mut buf)?;
                PdfObject::String(PdfString::new(buf))
            }
            PdfObject::Array(items) => {
                let mut out = Vec::with_capacity(items.len());
                for item in items.iter() {
                    out.push(self.decrypt_value(sec, id, item.clone())?);
                }
                PdfObject::Array(out.into())
            }
            PdfObject::Dictionary(d) => {
                PdfObject::Dictionary(Arc::new(self.decrypt_dict(sec, id, &d)?))
            }
            PdfObject::Stream(s) => {
                // Cross-reference streams are never encrypted (they must be
                // read to find objects before the file key can be derived).
                // Everything else at a file offset — including object-stream
                // containers — is.
                if self.is_xref_stream(&s.dict) {
                    PdfObject::Stream(s)
                } else {
                    let dict = self.decrypt_dict(sec, id, &s.dict)?;
                    let mut body = self.read_raw_stream(&s.data)?;
                    sec.decrypt_in_place(id, &mut body)?;
                    PdfObject::Stream(Arc::new(PdfStream {
                        dict,
                        data: StreamData::Owned(body.into()),
                    }))
                }
            }
            other => other,
        })
    }

    fn decrypt_dict(
        &self,
        sec: &SecurityContext,
        id: ObjectId,
        dict: &Dictionary,
    ) -> Result<Dictionary, DocumentError> {
        let mut pairs = Vec::with_capacity(dict.len());
        for (k, v) in dict.iter() {
            pairs.push((k, self.decrypt_value(sec, id, v.clone())?));
        }
        Ok(Dictionary::from_pairs(pairs))
    }

    fn is_xref_stream(&self, dict: &Dictionary) -> bool {
        dict.get(self.names.known.type_)
            .and_then(PdfObject::as_name)
            .map(|n| &*self.names.resolve(n) == b"XRef")
            .unwrap_or(false)
    }

    /// Read a stream's raw (still-encrypted) bytes out of the source.
    fn read_raw_stream(&self, data: &StreamData) -> Result<Vec<u8>, DocumentError> {
        match data {
            StreamData::InSource { offset, len } => {
                let bytes = self
                    .source
                    .read_range(*offset..offset.saturating_add(*len))
                    .map_err(pdf_structure::StructureError::from)?;
                Ok(bytes.into_owned())
            }
            StreamData::Owned(bytes) => Ok(bytes.to_vec()),
        }
    }

    /// Parse an uncompressed object at a header-relative xref offset.
    fn parse_uncompressed(
        &self,
        entry_offset: u64,
        id: ObjectId,
        ctx: &mut ParseContext,
    ) -> Result<PdfObject, DocumentError> {
        let abs = self.structure.header_offset.saturating_add(entry_offset);
        let source_len = self.source.len();
        if abs >= source_len {
            return Err(ObjectError::NotFound(id).into());
        }

        let mut window_len = INITIAL_WINDOW;
        loop {
            let end = source_len.min(abs.saturating_add(window_len));
            let at_eof = end == source_len;
            let window = self
                .source
                .read_range(abs..end)
                .map_err(pdf_structure::StructureError::from)?;
            match self.attempt_parse(&window, abs, id, ctx) {
                Attempt::Parsed(parsed, found_at) => {
                    if found_at != entry_offset {
                        ctx.recovery.push(RecoveryEvent::ObjectOffsetRepaired {
                            id,
                            declared: entry_offset,
                            actual: found_at,
                        });
                    }
                    for repair in parsed.repairs {
                        if let IndirectRepair::StreamLengthRepaired { declared, actual } = repair {
                            ctx.recovery.push(RecoveryEvent::StreamLengthRepaired {
                                id,
                                declared,
                                actual,
                            });
                        }
                    }
                    return Ok(parsed.object);
                }
                Attempt::Grow if !at_eof => {
                    window_len = window_len.saturating_mul(8);
                }
                Attempt::Grow => return Err(ObjectError::NotFound(id).into()),
                Attempt::Fail(e) => return Err(e),
            }
        }
    }

    /// One parse attempt within a window starting at absolute offset `abs`.
    fn attempt_parse(
        &self,
        window: &[u8],
        abs: u64,
        id: ObjectId,
        ctx: &mut ParseContext,
    ) -> Attempt {
        match self.parse_indirect_at(window, 0, abs, id, ctx) {
            Ok(parsed) if parsed.id.number == id.number => {
                // Generation tolerance: PDFium validates the object number
                // only; sloppy writers get generations wrong.
                Attempt::Parsed(Box::new(parsed), abs - self.structure.header_offset)
            }
            Ok(_)
            | Err(SyntaxError::UnexpectedByte { .. })
            | Err(SyntaxError::UnexpectedToken { .. })
            | Err(SyntaxError::MalformedNumber { .. })
            | Err(SyntaxError::UnterminatedString { .. }) => {
                // The declared offset does not start the declared object:
                // scan forward for the real `N G obj` header (repair).
                match scan_for_object_header(window, id.number) {
                    Some((rel, _generation)) => {
                        match self.parse_indirect_at(window, rel, abs, id, ctx) {
                            Ok(parsed) if parsed.id.number == id.number => Attempt::Parsed(
                                Box::new(parsed),
                                abs + rel as u64 - self.structure.header_offset,
                            ),
                            Ok(_) => Attempt::Fail(ObjectError::NotFound(id).into()),
                            Err(SyntaxError::UnexpectedEof { .. }) => Attempt::Grow,
                            Err(e) => Attempt::Fail(DocumentError::Structure(e.into())),
                        }
                    }
                    // Maybe the header straddles the window end.
                    None => Attempt::Grow,
                }
            }
            Err(SyntaxError::UnexpectedEof { .. }) => Attempt::Grow,
            Err(e) => Attempt::Fail(DocumentError::Structure(e.into())),
        }
    }

    /// Run the indirect-object parser at `window[rel..]`, wiring the
    /// `/Length` resolver back into `self` (indirect lengths are ordinary
    /// object resolutions and share the caller's recursion guard).
    fn parse_indirect_at(
        &self,
        window: &[u8],
        rel: usize,
        abs: u64,
        _id: ObjectId,
        ctx: &mut ParseContext,
    ) -> Result<pdf_object::parser::ParsedIndirect, SyntaxError> {
        let mut parser = ObjectParser::new(
            &window[rel..],
            abs + rel as u64,
            &self.limits.syntax,
            self.names,
        );
        let mut resolve_length = |len_id: ObjectId| -> Option<i64> {
            self.resolve(len_id, ctx).ok().and_then(|o| o.as_int())
        };
        parser.parse_indirect(&mut resolve_length)
    }

    /// Parse a compressed object out of its `/Type /ObjStm` container. The
    /// decompressed container (bytes + pair table) is cached in the
    /// worker's context, so N members cost one inflate per worker.
    fn parse_compressed(
        &self,
        container: u32,
        index: u32,
        id: ObjectId,
        ctx: &mut ParseContext,
    ) -> Result<PdfObject, DocumentError> {
        if container == id.number {
            // An object stream cannot contain itself.
            return Err(ObjectError::ReferenceCycle(id).into());
        }
        let cached = match ctx.objstm_get(container) {
            Some(hit) => {
                #[cfg(feature = "profiling")]
                {
                    ctx.object_stream_cache_hits += 1;
                }
                hit
            }
            None => {
                #[cfg(feature = "profiling")]
                {
                    ctx.object_stream_inflates += 1;
                }
                let container_obj = self.resolve(ObjectId::new(container, 0), ctx)?;
                let PdfObject::Stream(stream) = &*container_obj else {
                    return Err(ObjectError::TypeMismatch {
                        expected: "object stream",
                        found: kind_name(&container_obj),
                    }
                    .into());
                };
                let data = self.decode_stream_data(stream, ctx)?;
                let n = stream
                    .dict
                    .get(self.names.known.n)
                    .and_then(PdfObject::as_int)
                    .unwrap_or(0);
                let first = stream
                    .dict
                    .get(self.names.known.first)
                    .and_then(PdfObject::as_int)
                    .unwrap_or(0);
                let layout = parse_object_stream_layout(&data, n, first, &self.limits.syntax)?;
                let entry = Arc::new(ObjStmCached { data, layout });
                ctx.objstm_put(container, Arc::clone(&entry));
                entry
            }
        };
        let member = cached
            .layout
            .member(index)
            .filter(|m| m.number == id.number)
            .ok_or(ObjectError::NotFound(id))?;
        let slice = cached
            .data
            .get(member.offset as usize..)
            .ok_or(ObjectError::NotFound(id))?;
        // Members are direct objects (no `obj` wrapper, no streams).
        let mut parser = ObjectParser::new(slice, member.offset, &self.limits.syntax, self.names);
        Ok(parser
            .parse_object()
            .map_err(pdf_structure::StructureError::from)?)
    }

    /// Fetch a stream's raw bytes and run its filter chain, charging the
    /// context's decompression budget.
    /// A copy of the stream dict with an indirect `/Filter` or `/DecodeParms`
    /// (or their array elements) resolved, or `None` when both are already
    /// direct. `/DecodeParms` is very often written as `N 0 R` — a shared
    /// predictor dict — and the pure-`pdf-structure` decoder cannot resolve
    /// references; without this the predictor is silently skipped and the image
    /// decodes to its raw filtered deltas (a near-black smear).
    fn resolve_filter_params(
        &self,
        dict: &Dictionary,
        ctx: &mut ParseContext,
    ) -> Option<Dictionary> {
        let keys = [self.names.known.filter, self.names.known.decode_parms];
        let needs = keys
            .iter()
            .any(|k| dict.get(*k).is_some_and(value_has_reference));
        if !needs {
            return None;
        }
        let mut pairs: Vec<(pdf_object::NameId, PdfObject)> = Vec::new();
        for (k, v) in dict.iter() {
            let nv = if keys.contains(&k) {
                self.resolve_shallow_value(v, ctx)
            } else {
                v.clone()
            };
            pairs.push((k, nv));
        }
        Some(Dictionary::from_pairs(pairs))
    }

    /// Resolve a value one level: a reference to its target, an array's
    /// reference elements element-wise (a per-filter `/DecodeParms` array),
    /// direct objects unchanged. Falls back to the original on resolve failure.
    ///
    /// A single reference may itself resolve to an *array of references* — the
    /// `/Filter 10 0 R` → `[11 0 R 12 0 R]` → `[/FlateDecode /DCTDecode]` idiom
    /// seen in the wild (e.g. covers coded Flate-then-DCT with the filter chain
    /// stored as shared indirect name objects). Resolving only the outer
    /// reference would leave the elements as references, which the pure
    /// `pdf-structure` decoder rejects as "/Filter entry is not a name" and the
    /// whole image draw is silently dropped. So after resolving the outer
    /// reference to an array, resolve that array's reference elements too.
    fn resolve_shallow_value(&self, v: &PdfObject, ctx: &mut ParseContext) -> PdfObject {
        match v {
            PdfObject::Reference(_) => {
                let resolved = self
                    .resolve_value(v, ctx)
                    .map(|a| (*a).clone())
                    .unwrap_or_else(|_| v.clone());
                match resolved {
                    PdfObject::Array(_) => self.resolve_shallow_value(&resolved, ctx),
                    other => other,
                }
            }
            PdfObject::Array(items) => PdfObject::Array(
                items
                    .iter()
                    .map(|it| match it {
                        PdfObject::Reference(_) => self
                            .resolve_value(it, ctx)
                            .map(|a| (*a).clone())
                            .unwrap_or_else(|_| it.clone()),
                        other => other.clone(),
                    })
                    .collect(),
            ),
            other => other.clone(),
        }
    }

    pub fn decode_stream_data(
        &self,
        stream: &PdfStream,
        ctx: &mut ParseContext,
    ) -> Result<Vec<u8>, DocumentError> {
        let raw: Cow<'_, [u8]> = match &stream.data {
            StreamData::InSource { offset, len } => self
                .source
                .read_range(*offset..offset.saturating_add(*len))
                .map_err(pdf_structure::StructureError::from)?,
            StreamData::Owned(bytes) => Cow::Borrowed(bytes),
        };
        let mut budget = DecodeBudget::new(
            self.limits
                .max_decoded_bytes_per_context
                .saturating_sub(ctx.decoded_bytes_used),
        );
        let patched = self.resolve_filter_params(&stream.dict, ctx);
        let dict = patched.as_ref().unwrap_or(&stream.dict);
        let result = decode_stream(&raw, dict, self.names, &mut budget);
        ctx.decoded_bytes_used += budget.used();
        result.map_err(|e| match e {
            DecodeError::BudgetExceeded => {
                DocumentError::LimitExceeded("max_decoded_bytes_per_context")
            }
            other => DocumentError::Structure(other.into()),
        })
    }

    /// Like [`Self::decode_stream_data`], but stop at the first image-codec
    /// filter (DCT/JPX/JBIG2/CCITTFax): general filters are applied and the
    /// still-codec-encoded bytes are returned with the codec's canonical
    /// filter name (`None` when the chain has no codec filter).
    pub fn decode_stream_data_to_codec(
        &self,
        stream: &PdfStream,
        ctx: &mut ParseContext,
    ) -> Result<(Vec<u8>, Option<String>), DocumentError> {
        let raw: Cow<'_, [u8]> = match &stream.data {
            StreamData::InSource { offset, len } => self
                .source
                .read_range(*offset..offset.saturating_add(*len))
                .map_err(pdf_structure::StructureError::from)?,
            StreamData::Owned(bytes) => Cow::Borrowed(bytes),
        };
        let mut budget = DecodeBudget::new(
            self.limits
                .max_decoded_bytes_per_context
                .saturating_sub(ctx.decoded_bytes_used),
        );
        let patched = self.resolve_filter_params(&stream.dict, ctx);
        let dict = patched.as_ref().unwrap_or(&stream.dict);
        let result =
            pdf_structure::decode::decode_stream_to_codec(&raw, dict, self.names, &mut budget);
        ctx.decoded_bytes_used += budget.used();
        result.map_err(|e| match e {
            DecodeError::BudgetExceeded => {
                DocumentError::LimitExceeded("max_decoded_bytes_per_context")
            }
            other => DocumentError::Structure(other.into()),
        })
    }
}

/// Whether a `/Filter` or `/DecodeParms` value is (or contains) an indirect
/// reference that must be resolved before the pure-`pdf-structure` decoder,
/// which has no object repository, can read it.
fn value_has_reference(v: &PdfObject) -> bool {
    match v {
        PdfObject::Reference(_) => true,
        PdfObject::Array(a) => a.iter().any(|it| matches!(it, PdfObject::Reference(_))),
        _ => false,
    }
}

/// Human-readable kind for error messages.
pub(crate) fn kind_name(object: &PdfObject) -> &'static str {
    match object {
        PdfObject::Null => "null",
        PdfObject::Boolean(_) => "boolean",
        PdfObject::Integer(_) => "integer",
        PdfObject::Real(_) => "real",
        PdfObject::Name(_) => "name",
        PdfObject::String(_) => "string",
        PdfObject::Array(_) => "array",
        PdfObject::Dictionary(_) => "dictionary",
        PdfObject::Stream(_) => "stream",
        PdfObject::Reference(_) => "reference",
    }
}
