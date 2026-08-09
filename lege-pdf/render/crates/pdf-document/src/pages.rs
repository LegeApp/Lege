//! Page-tree indexing: walk `/Root → /Pages → /Kids` iteratively, flatten
//! inheritance, and freeze the result into the snapshot's `PageTreeIndex`.
//!
//! Malformed trees are the norm, not the exception: cycles (visited set),
//! `/Count` lies (never trusted — we walk), broken kids (skipped with a
//! recovery note), missing boxes (US-Letter default, PDFium behavior).

use std::collections::HashSet;

use pdf_object::{Dictionary, ObjectError, ObjectId, PdfObject};
use pdf_structure::RecoveryEvent;

use crate::resolve::{Resolver, kind_name};
use crate::{DocumentError, PageAnnotation, PageIndex, PageRef, PageTreeIndex, ParseContext};

/// US Letter, the PDFium default when a MediaBox is absent or invalid.
const LETTER: [f64; 4] = [0.0, 0.0, 612.0, 792.0];

/// Attributes inheritable through the page tree (ISO 32000-1 Table 29).
#[derive(Debug, Clone, Default)]
struct Inherited {
    resources: Option<PdfObject>,
    media_box: Option<[f64; 4]>,
    crop_box: Option<[f64; 4]>,
    rotate: Option<i64>,
}

impl Inherited {
    /// Child view: any attribute present on `dict` overrides the parent's.
    fn updated_from(
        &self,
        dict: &Dictionary,
        resolver: &Resolver<'_>,
        ctx: &mut ParseContext,
    ) -> Self {
        let names = resolver.names;
        Self {
            resources: dict
                .get(names.known.resources)
                .cloned()
                .or_else(|| self.resources.clone()),
            media_box: resolve_rect(dict.get(names.known.media_box), resolver, ctx)
                .or(self.media_box),
            crop_box: resolve_rect(dict.get(names.known.crop_box), resolver, ctx).or(self.crop_box),
            rotate: resolve_int(dict.get(names.known.rotate), resolver, ctx).or(self.rotate),
        }
    }
}

/// What the page-tree walk actually recovered, so the document layer can decide
/// whether a full xref rebuild is worth trying. `real_pages` excludes blank
/// placeholders; `lost_subtrees` counts nodes the walk provably dropped
/// (unreadable/null kids). A zero-`real_pages` walk with lost subtrees is the
/// escalation signal.
#[derive(Debug, Clone, Copy)]
pub(crate) struct WalkStats {
    pub real_pages: usize,
    pub lost_subtrees: usize,
}

/// Build the page index from the catalog.
pub(crate) fn build_page_tree(
    resolver: &Resolver<'_>,
    ctx: &mut ParseContext,
) -> Result<(PageTreeIndex, WalkStats), DocumentError> {
    let names = resolver.names;
    let limits = resolver.limits;

    let catalog = resolver.resolve(resolver.structure.trailer.root, ctx)?;
    let Some(catalog_dict) = catalog.as_dict() else {
        return Err(ObjectError::TypeMismatch {
            expected: "catalog dictionary",
            found: kind_name(&catalog),
        }
        .into());
    };
    let pages_root = catalog_dict
        .get(names.known.pages)
        .ok_or_else(|| ObjectError::MissingKey {
            key: "Pages".into(),
        })?
        .clone();

    // The root's declared /Count, for the truncated-tree placeholder pass
    // below. Never *trusted* for the walk itself.
    let declared_count = resolver
        .resolve_value(&pages_root, ctx)
        .ok()
        .and_then(|root| {
            root.as_dict()
                .and_then(|d| d.get(names.intern(b"Count")))
                .and_then(|v| resolve_int(Some(v), resolver, ctx))
        });

    let mut pages: Vec<PageRef> = Vec::new();
    let mut real_pages = 0usize;
    let mut lost_subtrees = 0usize;
    let mut visited: HashSet<ObjectId> = HashSet::new();
    // Stack of (node value, inherited attrs, expected leaf span). The span is
    // used only when the node proves unreadable, and only when its parent
    // `/Count` made that span exact. Kids are pushed reversed so the walk emits
    // document order.
    let root_span = declared_count
        .and_then(|count| usize::try_from(count).ok())
        .filter(|&count| limits.max_pages == 0 || count <= limits.max_pages as usize);
    let mut stack: Vec<(PdfObject, Inherited, Option<usize>)> =
        vec![(pages_root, Inherited::default(), root_span)];

    while let Some((node, inherited, expected_span)) = stack.pop() {
        // Resolve the node to a dictionary, remembering its id when it is
        // (as the spec requires) an indirect reference.
        let (resolved, node_id) = match node {
            PdfObject::Reference(id) => {
                if !visited.insert(id) {
                    ctx.recovery.push(RecoveryEvent::Other(format!(
                        "page tree cycle at {id}; subtree skipped"
                    )));
                    continue;
                }
                match resolver.resolve(id, ctx) {
                    Ok(value) => (value, Some(id)),
                    Err(e) => {
                        ctx.recovery.push(RecoveryEvent::Other(format!(
                            "page tree node {id} unreadable ({e}); skipped"
                        )));
                        lost_subtrees += 1;
                        append_inferred_placeholders(
                            &mut pages,
                            expected_span,
                            &inherited,
                            limits.max_pages,
                            ctx,
                            &format!("unreadable page-tree node {id}"),
                        )?;
                        continue;
                    }
                }
            }
            direct => (std::sync::Arc::new(direct), None),
        };
        let Some(dict) = resolved.as_dict() else {
            ctx.recovery.push(RecoveryEvent::Other(format!(
                "page tree node is a {}, not a dictionary; skipped",
                kind_name(&resolved)
            )));
            // A dangling kid resolves to Null in the tolerant resolver —
            // the truncated-tail shape; counts as a lost subtree.
            lost_subtrees += 1;
            append_inferred_placeholders(
                &mut pages,
                expected_span,
                &inherited,
                limits.max_pages,
                ctx,
                "non-dictionary page-tree node",
            )?;
            continue;
        };

        let inherited = inherited.updated_from(dict, resolver, ctx);

        // Interior node iff it has /Kids (tolerates missing /Type — PDFium
        // treats nodes without kids as leaves).
        if let Some(kids_value) = dict.get(names.known.kids) {
            let kids = match resolver.resolve_value(kids_value, ctx) {
                Ok(v) => v,
                Err(e) => {
                    ctx.recovery.push(RecoveryEvent::Other(format!(
                        "unreadable /Kids ({e}); subtree skipped"
                    )));
                    lost_subtrees += 1;
                    append_inferred_placeholders(
                        &mut pages,
                        expected_span,
                        &inherited,
                        limits.max_pages,
                        ctx,
                        "unreadable /Kids",
                    )?;
                    continue;
                }
            };
            let PdfObject::Array(kids) = &*kids else {
                ctx.recovery.push(RecoveryEvent::Other(
                    "/Kids is not an array; subtree skipped".into(),
                ));
                lost_subtrees += 1;
                append_inferred_placeholders(
                    &mut pages,
                    expected_span,
                    &inherited,
                    limits.max_pages,
                    ctx,
                    "non-array /Kids",
                )?;
                continue;
            };
            let spans = infer_child_spans(dict, kids, resolver, ctx);
            for (kid, span) in kids.iter().zip(spans).rev() {
                stack.push((kid.clone(), inherited.clone(), span));
            }
            continue;
        }

        // Leaf. A direct (inline-dict) page has no object id, but everything
        // rendering needs — MediaBox/CropBox/rotate/resources/contents/annots —
        // is read from the resolved dict right here, so `object: None` is fine;
        // PDFium renders such pages, so dropping them was a recovery gap.
        if limits.max_pages > 0 && pages.len() as u32 >= limits.max_pages {
            return Err(DocumentError::LimitExceeded("max_pages"));
        }

        let media_box = inherited.media_box.unwrap_or(LETTER);
        let crop_box = clamp_to(inherited.crop_box.unwrap_or(media_box), media_box);
        pages.push(PageRef {
            index: PageIndex(pages.len() as u32),
            object: node_id,
            media_box,
            crop_box,
            rotate: normalize_rotation(inherited.rotate.unwrap_or(0)),
            resources: inherited.resources.clone(),
            contents: dict.get(names.known.contents).cloned(),
            annotations: parse_annots(dict.get(names.intern(b"Annots")), resolver, ctx),
        });
        real_pages += 1;
    }

    // PDFium-parity placeholder synthesis: PDFium reports the root's
    // declared /Count and yields null (blank) pages for subtrees it cannot
    // load — typically a physically truncated file whose tail kids point
    // past EOF (the Hoyos "739 vs 845" case). When subtrees were provably
    // lost AND the declaration exceeds what the walk recovered, append
    // blank placeholder pages so page counts (and diff-tool page keys)
    // line up with the oracle. Never triggered by a merely lying /Count:
    // it requires an actual unresolvable subtree.
    if lost_subtrees > 0
        && let Some(declared) = declared_count
        && declared > pages.len() as i64
        && (limits.max_pages == 0 || declared <= limits.max_pages as i64)
    {
        let missing = declared as usize - pages.len();
        ctx.recovery.push(RecoveryEvent::Other(format!(
            "page tree declares {declared} pages but only {real_pages} were recoverable \
             ({lost_subtrees} lost subtrees); appending {missing} ambiguous blank \
             placeholder pages at the tail (PDFium count parity)"
        )));
        append_blank_pages(&mut pages, missing, &Inherited::default(), limits.max_pages)?;
    }

    Ok((
        PageTreeIndex::from_pages(pages.into_boxed_slice()),
        WalkStats {
            real_pages,
            lost_subtrees,
        },
    ))
}

/// Estimate each child's leaf span without trusting `/Count` for the normal
/// walk. Counts are used only to position a blank hole if a child later proves
/// unreadable. One unknown gets the exact parent residual; multiple unknowns
/// are exact only when the residual is one leaf apiece.
fn infer_child_spans(
    parent: &Dictionary,
    kids: &[PdfObject],
    resolver: &Resolver<'_>,
    ctx: &mut ParseContext,
) -> Vec<Option<usize>> {
    let mut spans = kids
        .iter()
        .map(|kid| declared_child_span(kid, resolver, ctx))
        .collect::<Vec<_>>();
    let unknown = spans
        .iter()
        .enumerate()
        .filter_map(|(index, span)| span.is_none().then_some(index))
        .collect::<Vec<_>>();
    if unknown.is_empty() {
        return spans;
    }

    let Some(declared) = resolve_int(parent.get(resolver.names.intern(b"Count")), resolver, ctx)
        .and_then(|value| usize::try_from(value).ok())
        .filter(|&value| {
            resolver.limits.max_pages == 0 || value <= resolver.limits.max_pages as usize
        })
    else {
        return spans;
    };
    let Some(known) = spans
        .iter()
        .flatten()
        .try_fold(0usize, |sum, &span| sum.checked_add(span))
    else {
        return spans;
    };
    let Some(residual) = declared.checked_sub(known) else {
        return spans;
    };

    if unknown.len() == 1 {
        spans[unknown[0]] = Some(residual);
    } else if residual == unknown.len() {
        for index in unknown {
            spans[index] = Some(1);
        }
    }
    spans
}

fn declared_child_span(
    child: &PdfObject,
    resolver: &Resolver<'_>,
    ctx: &mut ParseContext,
) -> Option<usize> {
    let resolved = resolver.resolve_value(child, ctx).ok()?;
    let dict = resolved.as_dict()?;
    if dict.contains_key(resolver.names.known.kids) {
        resolve_int(dict.get(resolver.names.intern(b"Count")), resolver, ctx)
            .and_then(|value| usize::try_from(value).ok())
            .filter(|&value| {
                resolver.limits.max_pages == 0 || value <= resolver.limits.max_pages as usize
            })
    } else {
        Some(1)
    }
}

fn append_inferred_placeholders(
    pages: &mut Vec<PageRef>,
    count: Option<usize>,
    inherited: &Inherited,
    max_pages: u32,
    ctx: &mut ParseContext,
    reason: &str,
) -> Result<(), DocumentError> {
    let Some(count) = count.filter(|&count| count > 0) else {
        return Ok(());
    };
    ctx.recovery.push(RecoveryEvent::Other(format!(
        "{reason}; inserted {count} blank placeholder page(s) at the lost tree position"
    )));
    append_blank_pages(pages, count, inherited, max_pages)
}

fn append_blank_pages(
    pages: &mut Vec<PageRef>,
    count: usize,
    inherited: &Inherited,
    max_pages: u32,
) -> Result<(), DocumentError> {
    let final_len = pages
        .len()
        .checked_add(count)
        .ok_or(DocumentError::LimitExceeded("max_pages"))?;
    if max_pages > 0 && final_len > max_pages as usize {
        return Err(DocumentError::LimitExceeded("max_pages"));
    }
    let media_box = inherited.media_box.unwrap_or(LETTER);
    let crop_box = clamp_to(inherited.crop_box.unwrap_or(media_box), media_box);
    for _ in 0..count {
        pages.push(PageRef {
            index: PageIndex(pages.len() as u32),
            object: None,
            media_box,
            crop_box,
            rotate: normalize_rotation(inherited.rotate.unwrap_or(0)),
            resources: inherited.resources.clone(),
            contents: None,
            annotations: std::sync::Arc::from(Vec::new()),
        });
    }
    Ok(())
}

/// Last-resort recovery for a rebuilt document whose page-tree walk still
/// found no real pages: inspect every live xref object for orphan `/Type /Page`
/// dictionaries and synthesize a deterministic index in object-number order.
///
/// This is deliberately narrower than the normal walk. It runs only after the
/// caller has tried both the loaded and fully rebuilt structures, so explicit
/// `/Type /Page` is required and ordinary documents never pay for the scan.
/// Parent links are followed only to recover inheritable page attributes; a
/// malformed/cyclic chain is tolerated and bounded by `max_reference_chain`.
pub(crate) fn recover_orphan_pages(
    resolver: &Resolver<'_>,
    ctx: &mut ParseContext,
) -> PageTreeIndex {
    let names = resolver.names;
    let page_name = names.intern(b"Page");
    let parent_name = names.intern(b"Parent");
    let mut pages = Vec::new();

    for entry in resolver.structure.xref.live_entries() {
        if resolver.limits.max_pages > 0 && pages.len() as u32 >= resolver.limits.max_pages {
            ctx.recovery.push(RecoveryEvent::Other(format!(
                "orphan /Type/Page recovery truncated at max_pages={}",
                resolver.limits.max_pages
            )));
            break;
        }

        let resolved = match resolver.resolve(entry.id, ctx) {
            Ok(value) => value,
            Err(_) => continue,
        };
        let Some(dict) = resolved.as_dict() else {
            continue;
        };
        if dict.get(names.known.type_).and_then(PdfObject::as_name) != Some(page_name)
            || dict.contains_key(names.known.kids)
        {
            continue;
        }

        // Collect parents leaf→root, then apply them root→leaf so nearer
        // dictionaries override inherited values exactly like the normal walk.
        let mut ancestors = Vec::new();
        let mut seen = HashSet::new();
        let mut parent = dict.get(parent_name).cloned();
        for _ in 0..resolver.limits.max_reference_chain {
            let Some(value) = parent.take() else {
                break;
            };
            if let PdfObject::Reference(id) = value
                && !seen.insert(id)
            {
                ctx.recovery.push(RecoveryEvent::Other(format!(
                    "orphan page {0} has a cyclic /Parent chain at {id}",
                    entry.id
                )));
                break;
            }
            let Ok(node) = resolver.resolve_value(&value, ctx) else {
                break;
            };
            let Some(parent_dict) = node.as_dict() else {
                break;
            };
            parent = parent_dict.get(parent_name).cloned();
            ancestors.push(node);
        }

        let mut inherited = Inherited::default();
        for ancestor in ancestors.iter().rev() {
            if let Some(parent_dict) = ancestor.as_dict() {
                inherited = inherited.updated_from(parent_dict, resolver, ctx);
            }
        }
        inherited = inherited.updated_from(dict, resolver, ctx);
        let media_box = inherited.media_box.unwrap_or(LETTER);
        let crop_box = clamp_to(inherited.crop_box.unwrap_or(media_box), media_box);
        pages.push(PageRef {
            index: PageIndex(pages.len() as u32),
            object: Some(entry.id),
            media_box,
            crop_box,
            rotate: normalize_rotation(inherited.rotate.unwrap_or(0)),
            resources: inherited.resources,
            contents: dict.get(names.known.contents).cloned(),
            annotations: parse_annots(dict.get(names.intern(b"Annots")), resolver, ctx),
        });
    }

    if !pages.is_empty() {
        ctx.recovery.push(RecoveryEvent::Other(format!(
            "page tree remained empty after xref rebuild; recovered {} orphan /Type/Page objects in object-number order",
            pages.len()
        )));
    }
    PageTreeIndex::from_pages(pages.into_boxed_slice())
}

/// Parse a page's `/Annots` array into [`PageAnnotation`]s.
///
/// Recovery-driven: a broken entry is skipped with a note, never fatal.
/// `/Popup` annotations are structurally skipped, matching PDFium's
/// `CPDF_AnnotList` (cpdf_annotlist.cpp — popups are folded into their
/// parents at list-build time and never drawn standalone).
fn parse_annots(
    value: Option<&PdfObject>,
    resolver: &Resolver<'_>,
    ctx: &mut ParseContext,
) -> std::sync::Arc<[PageAnnotation]> {
    let names = resolver.names;
    let mut out: Vec<PageAnnotation> = Vec::new();
    let Some(value) = value else {
        return out.into();
    };
    let resolved = match resolver.resolve_value(value, ctx) {
        Ok(v) => v,
        Err(e) => {
            ctx.recovery.push(RecoveryEvent::Other(format!(
                "unreadable /Annots ({e}); skipped"
            )));
            return out.into();
        }
    };
    let PdfObject::Array(items) = &*resolved else {
        if !matches!(&*resolved, PdfObject::Null) {
            ctx.recovery.push(RecoveryEvent::Other(
                "/Annots is not an array; skipped".into(),
            ));
        }
        return out.into();
    };
    for item in items.iter() {
        let entry = match resolver.resolve_value(item, ctx) {
            Ok(v) => v,
            Err(e) => {
                ctx.recovery.push(RecoveryEvent::Other(format!(
                    "annotation entry unreadable ({e}); skipped"
                )));
                continue;
            }
        };
        let Some(dict) = entry.as_dict() else {
            // Nulls (freed annots) are silently common; only note real junk.
            if !matches!(&*entry, PdfObject::Null) {
                ctx.recovery.push(RecoveryEvent::Other(
                    "annotation entry is not a dictionary; skipped".into(),
                ));
            }
            continue;
        };
        let subtype = dict.get(names.known.subtype).and_then(PdfObject::as_name);
        if let Some(s) = subtype
            && names.resolve(s).as_ref() == b"Popup"
        {
            continue;
        }
        out.push(PageAnnotation {
            subtype,
            rect: annot_rect(dict.get(names.intern(b"Rect")), resolver, ctx),
            flags: resolve_int(dict.get(names.intern(b"F")), resolver, ctx)
                .map_or(0, |f| f.clamp(0, u32::MAX as i64) as u32),
            appearance: dict.get(names.intern(b"AP")).cloned(),
            appearance_state: dict.get(names.intern(b"AS")).and_then(PdfObject::as_name),
            destination: dict.get(names.intern(b"Dest")).cloned(),
            action: dict.get(names.intern(b"A")).cloned(),
            quad_points: annot_quad_points(dict.get(names.intern(b"QuadPoints")), resolver, ctx),
            color: annot_color(dict.get(names.intern(b"C")), resolver, ctx),
            opacity: resolve_number(dict.get(names.intern(b"CA")), resolver, ctx)
                .unwrap_or(1.0)
                .clamp(0.0, 1.0) as f32,
        });
    }
    out.into()
}

fn resolve_number(
    value: Option<&PdfObject>,
    resolver: &Resolver<'_>,
    ctx: &mut ParseContext,
) -> Option<f64> {
    resolver
        .resolve_value(value?, ctx)
        .ok()?
        .as_number()
        .filter(|number| number.is_finite())
}

fn annot_numbers(
    value: Option<&PdfObject>,
    resolver: &Resolver<'_>,
    ctx: &mut ParseContext,
) -> Option<Vec<f64>> {
    let resolved = resolver.resolve_value(value?, ctx).ok()?;
    let PdfObject::Array(items) = &*resolved else {
        return None;
    };
    items
        .iter()
        .map(|item| {
            resolver
                .resolve_value(item, ctx)
                .ok()?
                .as_number()
                .filter(|number| number.is_finite())
        })
        .collect()
}

fn annot_quad_points(
    value: Option<&PdfObject>,
    resolver: &Resolver<'_>,
    ctx: &mut ParseContext,
) -> std::sync::Arc<[[f64; 8]]> {
    let Some(numbers) = annot_numbers(value, resolver, ctx) else {
        return std::sync::Arc::from([]);
    };
    numbers
        .chunks_exact(8)
        .map(|chunk| {
            let mut quad = [0.0; 8];
            quad.copy_from_slice(chunk);
            quad
        })
        .collect::<Vec<[f64; 8]>>()
        .into()
}

fn annot_color(
    value: Option<&PdfObject>,
    resolver: &Resolver<'_>,
    ctx: &mut ParseContext,
) -> Option<crate::AnnotationColor> {
    let numbers = annot_numbers(value, resolver, ctx)?;
    let c = |index: usize| numbers[index].clamp(0.0, 1.0) as f32;
    match numbers.len() {
        1 => Some(crate::AnnotationColor::Gray(c(0))),
        3 => Some(crate::AnnotationColor::Rgb([c(0), c(1), c(2)])),
        4 => Some(crate::AnnotationColor::Cmyk([c(0), c(1), c(2), c(3)])),
        _ => None,
    }
}

/// An annotation's `/Rect`: corner-normalized but *not* required to have
/// positive area (a degenerate rect is kept and skipped at render time,
/// so `[0,0,0,0]` doubles as the missing/invalid marker).
fn annot_rect(
    value: Option<&PdfObject>,
    resolver: &Resolver<'_>,
    ctx: &mut ParseContext,
) -> [f64; 4] {
    let Some(value) = value else { return [0.0; 4] };
    let Ok(resolved) = resolver.resolve_value(value, ctx) else {
        return [0.0; 4];
    };
    let PdfObject::Array(items) = &*resolved else {
        return [0.0; 4];
    };
    if items.len() < 4 {
        return [0.0; 4];
    }
    let mut n = [0f64; 4];
    for (i, item) in items.iter().take(4).enumerate() {
        let Some(v) = resolver
            .resolve_value(item, ctx)
            .ok()
            .and_then(|o| o.as_number())
            .filter(|v| v.is_finite())
        else {
            return [0.0; 4];
        };
        n[i] = v;
    }
    [
        n[0].min(n[2]),
        n[1].min(n[3]),
        n[0].max(n[2]),
        n[1].max(n[3]),
    ]
}

/// Resolve a (possibly indirect) rectangle: 4 numbers, themselves possibly
/// indirect. Invalid or empty rectangles yield `None` (caller defaults).
fn resolve_rect(
    value: Option<&PdfObject>,
    resolver: &Resolver<'_>,
    ctx: &mut ParseContext,
) -> Option<[f64; 4]> {
    let value = resolver.resolve_value(value?, ctx).ok()?;
    let PdfObject::Array(items) = &*value else {
        return None;
    };
    if items.len() < 4 {
        return None;
    }
    let mut n = [0f64; 4];
    for (i, item) in items.iter().take(4).enumerate() {
        n[i] = resolver.resolve_value(item, ctx).ok()?.as_number()?;
        if !n[i].is_finite() {
            return None;
        }
    }
    // Normalize corner order.
    let rect = [
        n[0].min(n[2]),
        n[1].min(n[3]),
        n[0].max(n[2]),
        n[1].max(n[3]),
    ];
    // Zero-area boxes are treated as absent (PDFium falls back to Letter).
    if rect[2] - rect[0] <= 0.0 || rect[3] - rect[1] <= 0.0 {
        return None;
    }
    Some(rect)
}

fn resolve_int(
    value: Option<&PdfObject>,
    resolver: &Resolver<'_>,
    ctx: &mut ParseContext,
) -> Option<i64> {
    let value = resolver.resolve_value(value?, ctx).ok()?;
    match &*value {
        PdfObject::Integer(i) => Some(*i),
        // Real-valued /Rotate occurs in the wild; truncate like PDFium's
        // GetInteger.
        PdfObject::Real(r) if r.is_finite() => Some(*r as i64),
        _ => None,
    }
}

/// Normalize `/Rotate` into {0, 90, 180, 270}: reduce modulo 360 (negative
/// values wrap), and treat non-multiples of 90 as 0 (invalid per spec).
fn normalize_rotation(rotate: i64) -> u16 {
    let r = rotate.rem_euclid(360);
    if r % 90 == 0 { r as u16 } else { 0 }
}

/// Intersect `rect` with `bounds`; an empty intersection collapses to
/// `bounds` (a CropBox entirely outside the MediaBox is ignored).
fn clamp_to(rect: [f64; 4], bounds: [f64; 4]) -> [f64; 4] {
    let out = [
        rect[0].max(bounds[0]),
        rect[1].max(bounds[1]),
        rect[2].min(bounds[2]),
        rect[3].min(bounds[3]),
    ];
    if out[2] - out[0] <= 0.0 || out[3] - out[1] <= 0.0 {
        return bounds;
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn rotation_normalizes() {
        assert_eq!(normalize_rotation(0), 0);
        assert_eq!(normalize_rotation(90), 90);
        assert_eq!(normalize_rotation(360), 0);
        assert_eq!(normalize_rotation(450), 90);
        assert_eq!(normalize_rotation(-90), 270);
        assert_eq!(normalize_rotation(-450), 270);
        assert_eq!(normalize_rotation(45), 0);
        assert_eq!(normalize_rotation(123), 0);
    }

    #[test]
    fn crop_clamps_and_collapses() {
        let media = [0.0, 0.0, 612.0, 792.0];
        assert_eq!(
            clamp_to([100.0, 100.0, 500.0, 700.0], media),
            [100.0, 100.0, 500.0, 700.0]
        );
        assert_eq!(clamp_to([-50.0, -50.0, 700.0, 900.0], media), media);
        // Disjoint crop box → media box.
        assert_eq!(clamp_to([1000.0, 1000.0, 2000.0, 2000.0], media), media);
    }
}
