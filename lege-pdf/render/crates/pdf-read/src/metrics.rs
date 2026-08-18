//! Per-page representation metrics and document-level font/image inventories.
//!
//! These are facts about what the compiled semantic page contains: counts,
//! image coverage, effective DPI, and the fonts/images those counts refer to.
//! Classification (vector / raster-only / mixed / …) is left to the caller.

use std::collections::HashMap;

use pdf_content::semantic::{SemanticOp, SemanticPage};
use pdf_document::{DocumentSnapshot, ParseContext};
use pdf_object::{ObjectId, PdfObject};
use pdf_page_ir::Matrix;

/// Counts and coverage for one compiled page.
///
/// Coverage is reported in basis points of the page's crop box (10_000 = the
/// painted image area equals the page). Overlapping placements are summed, then
/// clipped: this is an upper bound, not a union.
#[derive(Debug, Clone, Default)]
pub struct PageMetrics {
    /// Show-text runs interned on the page, visible or not.
    pub text_runs: u32,
    /// Runs that paint (`visible` and not render-mode 3/7).
    pub visible_text_runs: u32,
    /// Runs that do not paint: render-mode 3 (invisible) or 7 (clip-only),
    /// or flagged `visible == false` (hidden OC / extraction-only Type 3).
    pub invisible_text_runs: u32,
    /// Distinct font resources referenced by those runs.
    pub fonts: u32,
    /// Image XObjects and inline images interned on the page, including masks.
    pub images: u32,
    /// `Fill` / `Stroke` / `FillStroke` operators.
    pub path_paints: u32,
    /// `PaintShading` operators.
    pub shading_paints: u32,
    /// Sum of non-mask image placement areas, clipped to `[0, 10_000]`.
    pub image_coverage_bps: u16,
    /// Largest single non-mask image placement, same units.
    pub max_image_coverage_bps: u16,
    /// Effective DPI of the largest-coverage non-mask placement, when the
    /// placement has a positive drawn size.
    pub effective_dpi: Option<u32>,
}

/// One font resource the document used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FontRecord {
    /// Resource name it was reached through (`F1`).
    pub resource_name: String,
    /// Backing font dictionary, if the resource resolved.
    pub object: Option<ObjectId>,
    /// `/Subtype` (e.g. `Type1`, `TrueType`, `Type0`).
    pub subtype: String,
    /// `/BaseFont`, empty if absent.
    pub base_font: String,
    /// An embedded outline program was present and usable.
    pub embedded: bool,
}

/// One image XObject or inline image the document used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageRecord {
    /// Backing object for an image XObject; `None` for an inline image.
    pub object: Option<ObjectId>,
    pub width: u32,
    pub height: u32,
    pub bits_per_component: u8,
    /// `/ImageMask true`.
    pub is_mask: bool,
    /// Filter names in application order.
    pub filters: Vec<String>,
}

/// PDF text render mode 3: neither fill nor stroke (the OCR-layer idiom).
const RENDER_INVISIBLE: u8 = 3;
/// PDF text render mode 7: clip only, no paint.
const RENDER_CLIP_ONLY: u8 = 7;

/// Document-wide font and image inventories, accumulated one page at a time.
///
/// The lookup indexes and the `/FontFile*` answers live here rather than being
/// rebuilt per page: a document's font and image sets are shared across pages,
/// so both the dedup keys and the descriptor walks are the same work repeated
/// once per page otherwise. Scope is one document — `embedded` is keyed by
/// `ObjectId`, which only identifies an object within the snapshot it came
/// from, so an `Inventory` must never be reused across documents.
#[derive(Debug, Default)]
pub struct Inventory {
    fonts: Vec<FontRecord>,
    images: Vec<ImageRecord>,
    /// `ObjectId` → index into `fonts`, for resources that resolved.
    font_by_object: HashMap<ObjectId, usize>,
    /// `(resource, base, subtype)` → index into `fonts`, for those that did not.
    font_by_name: HashMap<(String, String, String), usize>,
    /// `ObjectId` → index into `images`. Inline images are never indexed.
    image_by_object: HashMap<ObjectId, usize>,
    /// Memoized `font_program_in_file` answers, keyed by font dictionary.
    embedded: HashMap<ObjectId, bool>,
}

impl Inventory {
    pub fn new() -> Self {
        Self::default()
    }

    /// Walk a compiled page: return its metrics and fold the fonts and images
    /// it references into the document-wide inventories.
    pub fn absorb_page(&mut self, snapshot: &DocumentSnapshot, page: &SemanticPage) -> PageMetrics {
        let names = snapshot.names();
        for font in page.fonts.iter() {
            let record = FontRecord {
                resource_name: String::from_utf8_lossy(&names.resolve(font.resource_name))
                    .into_owned(),
                object: font.object,
                subtype: String::from_utf8_lossy(&font.subtype).into_owned(),
                base_font: String::from_utf8_lossy(&font.base_font).into_owned(),
                embedded: self.font_is_embedded(snapshot, font.object),
            };
            self.merge_font(record);
        }

        for image in page.images.iter() {
            let record = ImageRecord {
                object: image.object,
                width: image.width,
                height: image.height,
                bits_per_component: image.bits_per_component,
                is_mask: image.is_mask,
                filters: image
                    .filters
                    .iter()
                    .map(|f| String::from_utf8_lossy(f).into_owned())
                    .collect(),
            };
            self.merge_image(record);
        }

        page_metrics(page)
    }

    /// The accumulated inventories, in first-seen order.
    pub fn into_parts(self) -> (Vec<FontRecord>, Vec<ImageRecord>) {
        (self.fonts, self.images)
    }

    /// Keyed by `ObjectId` when the resource resolved, else by
    /// `(resource, base, subtype)`; a record with an object id never collides
    /// with one without. A later record wins only if it found an embedded
    /// program where the stored one did not.
    fn merge_font(&mut self, record: FontRecord) {
        let slot = match record.object {
            Some(id) => self.font_by_object.get(&id).copied(),
            None => self
                .font_by_name
                .get(&(
                    record.resource_name.clone(),
                    record.base_font.clone(),
                    record.subtype.clone(),
                ))
                .copied(),
        };
        match slot {
            Some(idx) => {
                if record.embedded && !self.fonts[idx].embedded {
                    self.fonts[idx] = record;
                }
            }
            None => {
                let idx = self.fonts.len();
                match record.object {
                    Some(id) => {
                        self.font_by_object.insert(id, idx);
                    }
                    None => {
                        self.font_by_name.insert(
                            (
                                record.resource_name.clone(),
                                record.base_font.clone(),
                                record.subtype.clone(),
                            ),
                            idx,
                        );
                    }
                }
                self.fonts.push(record);
            }
        }
    }

    /// XObjects collapse by `ObjectId`, preferring the record that saw more
    /// filters. Inline images carry no identity, so each one is kept.
    fn merge_image(&mut self, record: ImageRecord) {
        let Some(id) = record.object else {
            self.images.push(record);
            return;
        };
        match self.image_by_object.get(&id).copied() {
            Some(idx) => {
                if record.filters.len() > self.images[idx].filters.len() {
                    self.images[idx] = record;
                }
            }
            None => {
                self.image_by_object.insert(id, self.images.len());
                self.images.push(record);
            }
        }
    }

    /// `font_program_in_file`, answered once per font dictionary per document.
    fn font_is_embedded(&mut self, snapshot: &DocumentSnapshot, object: Option<ObjectId>) -> bool {
        let Some(id) = object else {
            return false;
        };
        if let Some(&known) = self.embedded.get(&id) {
            return known;
        }
        let answer = font_program_in_file(snapshot, Some(id));
        self.embedded.insert(id, answer);
        answer
    }
}

/// Counts, coverage and effective DPI for one compiled page.
fn page_metrics(page: &SemanticPage) -> PageMetrics {
    let mut visible_text_runs = 0u32;
    let mut invisible_text_runs = 0u32;
    for run in page.text_runs.iter() {
        let invisible = !run.visible
            || run.render_mode == RENDER_INVISIBLE
            || run.render_mode == RENDER_CLIP_ONLY;
        if invisible {
            invisible_text_runs += 1;
        } else {
            visible_text_runs += 1;
        }
    }

    let mut path_paints = 0u32;
    let mut shading_paints = 0u32;
    let mut ctm = Matrix::IDENTITY;
    let mut ctm_stack = Vec::new();
    let mut coverage_sum = 0.0f64;
    let mut max_coverage = 0.0f64;
    let mut largest: Option<(f64, u32)> = None;

    let page_area = {
        let crop = page.bounds.crop;
        let area = crop.width().abs() * crop.height().abs();
        if area.is_finite() { area } else { 0.0 }
    };

    for op in page.ops.iter() {
        match op {
            SemanticOp::Save => ctm_stack.push(ctm),
            SemanticOp::Restore => {
                if let Some(prev) = ctm_stack.pop() {
                    ctm = prev;
                }
            }
            SemanticOp::Concat(m) => ctm = m.then(ctm),
            SemanticOp::Fill { .. } | SemanticOp::Stroke { .. } | SemanticOp::FillStroke { .. } => {
                path_paints += 1;
            }
            SemanticOp::PaintShading(_) => shading_paints += 1,
            SemanticOp::DrawImage(id) => {
                let Some(image) = page.images.get(id.index()) else {
                    continue;
                };
                if image.is_mask {
                    continue;
                }
                let drawn = ctm.determinant().abs();
                if !drawn.is_finite() || page_area <= 0.0 {
                    continue;
                }
                let coverage = drawn / page_area;
                coverage_sum += coverage;
                if coverage > max_coverage {
                    max_coverage = coverage;
                }
                if largest.is_none_or(|(best, _)| coverage > best)
                    && let Some(dpi) = placement_dpi(&ctm, image.width, image.height)
                {
                    largest = Some((coverage, dpi));
                }
            }
            _ => {}
        }
    }

    PageMetrics {
        text_runs: page.text_runs.len() as u32,
        visible_text_runs,
        invisible_text_runs,
        fonts: page.fonts.len() as u32,
        images: page.images.len() as u32,
        path_paints,
        shading_paints,
        image_coverage_bps: to_bps(coverage_sum),
        max_image_coverage_bps: to_bps(max_coverage),
        effective_dpi: largest.map(|(_, dpi)| dpi),
    }
}

fn placement_dpi(ctm: &Matrix, width_px: u32, height_px: u32) -> Option<u32> {
    let drawn_w = hypot(ctm.a, ctm.b);
    let drawn_h = hypot(ctm.c, ctm.d);
    if drawn_w <= 0.0 || drawn_h <= 0.0 || !drawn_w.is_finite() || !drawn_h.is_finite() {
        return None;
    }
    let dpi_x = f64::from(width_px) / (drawn_w / 72.0);
    let dpi_y = f64::from(height_px) / (drawn_h / 72.0);
    if !dpi_x.is_finite() || !dpi_y.is_finite() {
        return None;
    }
    // Conservative: the lower axis is the resolution the page can actually
    // support in both directions.
    Some(dpi_x.min(dpi_y).round() as u32)
}

fn hypot(x: f64, y: f64) -> f64 {
    x.hypot(y)
}

/// Whether the PDF itself carries a `/FontFile*` stream for this font.
///
/// Distinct from `SemFont::program`: the engine fills that from a bundled
/// Standard-14 substitute when the file has no program, and a forensic
/// inventory must not call the substitute "embedded".
fn font_program_in_file(snapshot: &DocumentSnapshot, object: Option<ObjectId>) -> bool {
    let Some(id) = object else {
        return false;
    };
    let mut ctx = ParseContext::new();
    let Some(dict_obj) = resolve_dict_obj(snapshot, &PdfObject::Reference(id), &mut ctx) else {
        return false;
    };
    let Some(dict) = dict_obj.as_dict() else {
        return false;
    };
    if descriptor_has_font_file(snapshot, dict, &mut ctx) {
        return true;
    }
    // Type 0: the program lives on the descendant CIDFont.
    let names = snapshot.names();
    let Some(key) = names.lookup(b"DescendantFonts") else {
        return false;
    };
    let Some(descendants) = dict.get(key) else {
        return false;
    };
    let first = match descendants {
        PdfObject::Array(items) => items.first().cloned(),
        other => resolve_obj(snapshot, other, &mut ctx).and_then(|arr| match arr.as_ref() {
            PdfObject::Array(items) => items.first().cloned(),
            _ => None,
        }),
    };
    let Some(child) = first else {
        return false;
    };
    let Some(child_obj) = resolve_dict_obj(snapshot, &child, &mut ctx) else {
        return false;
    };
    let Some(child_dict) = child_obj.as_dict() else {
        return false;
    };
    descriptor_has_font_file(snapshot, child_dict, &mut ctx)
}

fn descriptor_has_font_file(
    snapshot: &DocumentSnapshot,
    dict: &pdf_object::Dictionary,
    ctx: &mut ParseContext,
) -> bool {
    let names = snapshot.names();
    let Some(desc_key) = names.lookup(b"FontDescriptor") else {
        return false;
    };
    let Some(desc) = dict.get(desc_key) else {
        return false;
    };
    let Some(desc_obj) = resolve_dict_obj(snapshot, desc, ctx) else {
        return false;
    };
    let Some(desc_dict) = desc_obj.as_dict() else {
        return false;
    };
    for key in [b"FontFile".as_slice(), b"FontFile2", b"FontFile3"] {
        if names
            .lookup(key)
            .is_some_and(|name| desc_dict.contains_key(name))
        {
            return true;
        }
    }
    false
}

fn resolve_obj(
    snapshot: &DocumentSnapshot,
    value: &PdfObject,
    ctx: &mut ParseContext,
) -> Option<std::sync::Arc<PdfObject>> {
    match value {
        PdfObject::Reference(id) => snapshot.objects().resolve(snapshot, *id, ctx).ok(),
        other => Some(std::sync::Arc::new(other.clone())),
    }
}

/// Resolve `value` and, if it is a dictionary, hand back the still-`Arc`'d
/// object so the caller can borrow the `Dictionary` via `as_dict()` without
/// cloning it (the old `resolve_dict` returned an owned `Dictionary`, paying
/// a full clone on every hop — twice per `font_program_in_file` call).
fn resolve_dict_obj(
    snapshot: &DocumentSnapshot,
    value: &PdfObject,
    ctx: &mut ParseContext,
) -> Option<std::sync::Arc<PdfObject>> {
    let obj = resolve_obj(snapshot, value, ctx)?;
    if obj.as_dict().is_some() {
        Some(obj)
    } else {
        None
    }
}

fn to_bps(coverage: f64) -> u16 {
    if !coverage.is_finite() || coverage <= 0.0 {
        return 0;
    }
    let bps = (coverage * 10_000.0).round();
    if bps >= 10_000.0 { 10_000 } else { bps as u16 }
}
