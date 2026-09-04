use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use pdf_content::semantic::{SemanticOp, SemanticPage};
use pdf_document::{DocumentSnapshot, PageIndex, ParseContext};
use pdf_object::{ObjectId, PdfObject};

use crate::bounds::Bounds;
use crate::commands::{default_annotations, page_compiler, parse_context, resolve_snapshot};
use crate::open::DocumentIdentity;
use crate::schema::{Envelope, OutputMode};
use crate::views::content::{ContentData, FontResView, ImageResView, ResourcesView};
use crate::views::image::ObjectRefView;

#[derive(Debug)]
pub struct ContentArgs<'a> {
    pub path: &'a Path,
    pub password: Option<&'a str>,
    pub page: u32,
    pub ops: bool,
    pub resources: bool,
    pub objects: bool,
    pub system_fonts: bool,
    pub bounds: Bounds,
    pub output: OutputMode,
    pub snapshot: Option<Arc<DocumentSnapshot>>,
    pub identity: Option<DocumentIdentity>,
}

pub fn run(args: ContentArgs<'_>) -> Result<i32> {
    let page_one = if args.page == 0 {
        anyhow::bail!("page numbers are one-based; got 0");
    } else {
        args.page
    };

    let (identity, snapshot) =
        resolve_snapshot(args.path, args.password, args.snapshot, args.identity)?;
    let document = identity.display_path();
    let mut warnings = Vec::new();

    let want_ops = args.ops || (!args.resources && !args.objects);
    let data = content_page(
        &snapshot,
        page_one,
        want_ops,
        args.resources,
        args.objects,
        args.system_fonts,
        &args.bounds,
        &mut warnings,
    )?;

    let env = Envelope::page_ok(&document, page_one, serde_json::to_value(&data)?)
        .with_warnings(warnings);
    emit(&env, &data, args.output)?;
    Ok(0)
}

pub fn content_value(
    snapshot: &DocumentSnapshot,
    page_one: u32,
    ops: bool,
    resources: bool,
    objects: bool,
    system_fonts: bool,
    bounds: Bounds,
) -> Result<serde_json::Value> {
    let mut warnings = Vec::new();
    let want_ops = ops || (!resources && !objects);
    let data = content_page(
        snapshot,
        page_one,
        want_ops,
        resources,
        objects,
        system_fonts,
        &bounds,
        &mut warnings,
    )?;
    Ok(serde_json::to_value(data)?)
}

#[allow(clippy::too_many_arguments)]
fn content_page(
    snapshot: &DocumentSnapshot,
    page_one: u32,
    want_ops: bool,
    want_resources: bool,
    want_objects: bool,
    system_fonts: bool,
    bounds: &Bounds,
    warnings: &mut Vec<String>,
) -> Result<ContentData> {
    let pz = crate::pages::to_zero_based(page_one, snapshot.page_count())?;
    let compiler = page_compiler(system_fonts, default_annotations());
    let mut ctx = parse_context();
    let page = compiler.compile_semantic(snapshot, PageIndex(pz.0), &mut ctx)?;
    let dump = pdf_content::dump::dump_semantic(&page, snapshot.names());

    let op_count = page.ops.len();
    let ops = if want_ops {
        let mut list: Vec<serde_json::Value> = page.ops.iter().map(op_json).collect();
        list = bounds.truncate_items(list, warnings, "ops");
        Some(list)
    } else {
        None
    };

    let resources = if want_resources {
        Some(resources_view(&page, snapshot))
    } else {
        None
    };

    let objects = if want_objects {
        Some(collect_objects(snapshot, &page, bounds, warnings)?)
    } else {
        None
    };

    Ok(ContentData {
        unit: "pdf_points",
        dump: Some(dump),
        ops,
        resources,
        objects,
        op_count,
    })
}

fn resources_view(page: &SemanticPage, snapshot: &DocumentSnapshot) -> ResourcesView {
    let names = snapshot.names();
    let fonts = page
        .fonts
        .iter()
        .enumerate()
        .map(|(index, f)| FontResView {
            index,
            resource_name: String::from_utf8_lossy(&names.resolve(f.resource_name)).into_owned(),
            subtype: String::from_utf8_lossy(&f.subtype).into_owned(),
            base_font: String::from_utf8_lossy(&f.base_font).into_owned(),
            object: f.object.map(|id| ObjectRefView {
                number: id.number,
                generation: id.generation,
            }),
        })
        .collect();
    let images = page
        .images
        .iter()
        .enumerate()
        .map(|(index, img)| ImageResView {
            index,
            width: img.width,
            height: img.height,
            codec: img.codec.map(|c| format!("{c:?}").to_ascii_lowercase()),
            object: img.object.map(|id| ObjectRefView {
                number: id.number,
                generation: id.generation,
            }),
        })
        .collect();
    ResourcesView {
        fonts,
        images,
        shading_count: page.shadings.len(),
        path_count: page.paths.len(),
        text_run_count: page.text_runs.len(),
    }
}

fn collect_objects(
    snapshot: &DocumentSnapshot,
    page: &SemanticPage,
    bounds: &Bounds,
    warnings: &mut Vec<String>,
) -> Result<Vec<serde_json::Value>> {
    let mut ids: Vec<ObjectId> = Vec::new();
    for f in page.fonts.iter() {
        if let Some(id) = f.object {
            ids.push(id);
        }
    }
    for img in page.images.iter() {
        if let Some(id) = img.object {
            ids.push(id);
        }
    }
    ids.sort_by_key(|id| (id.number, id.generation));
    ids.dedup();
    ids = bounds.truncate_items(ids, warnings, "objects");

    let mut ctx = ParseContext::new();
    let mut out = Vec::new();
    for id in ids {
        match snapshot.objects().resolve(snapshot, id, &mut ctx) {
            Ok(obj) => {
                out.push(serde_json::json!({
                    "object": { "number": id.number, "generation": id.generation },
                    "kind": object_kind(&obj),
                    "summary": object_summary(&obj, bounds.max_bytes),
                }));
            }
            Err(err) => {
                out.push(serde_json::json!({
                    "object": { "number": id.number, "generation": id.generation },
                    "error": err.to_string(),
                }));
            }
        }
    }
    Ok(out)
}

fn object_kind(obj: &PdfObject) -> &'static str {
    match obj {
        PdfObject::Null => "null",
        PdfObject::Boolean(_) => "bool",
        PdfObject::Integer(_) => "int",
        PdfObject::Real(_) => "real",
        PdfObject::String(_) => "string",
        PdfObject::Name(_) => "name",
        PdfObject::Array(_) => "array",
        PdfObject::Dictionary(_) => "dict",
        PdfObject::Stream(_) => "stream",
        PdfObject::Reference(_) => "reference",
    }
}

/// Truncate `s` to at most `max_bytes` bytes without splitting a multi-byte
/// UTF-8 character. `max_bytes` is `bounds.max_bytes`, the whole-payload
/// budget reused as a per-string cap; a raw `&s[..max_bytes]` slice would
/// panic whenever the cut point lands inside a character's encoding.
fn truncate_at_char_boundary(s: &str, max_bytes: usize) -> &str {
    if max_bytes >= s.len() {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn object_summary(obj: &PdfObject, max_bytes: u64) -> serde_json::Value {
    match obj {
        PdfObject::Null => serde_json::Value::Null,
        PdfObject::Boolean(b) => serde_json::json!(b),
        PdfObject::Integer(i) => serde_json::json!(i),
        PdfObject::Real(r) => serde_json::json!(r),
        PdfObject::String(s) => {
            let lossy = pdf_object::decode_text_string_lossy(s.as_bytes());
            let truncated = if max_bytes > 0 && lossy.len() as u64 > max_bytes {
                format!("{}…", truncate_at_char_boundary(&lossy, max_bytes as usize))
            } else {
                lossy
            };
            serde_json::json!({ "text": truncated, "byte_len": s.as_bytes().len() })
        }
        PdfObject::Name(n) => serde_json::json!({ "name": format!("{n:?}") }),
        PdfObject::Array(a) => serde_json::json!({ "len": a.len() }),
        PdfObject::Dictionary(d) => serde_json::json!({ "keys": d.len() }),
        PdfObject::Stream(s) => {
            let data_len = match &s.data {
                pdf_object::StreamData::InSource { len, .. } => *len,
                pdf_object::StreamData::Owned(bytes) => bytes.len() as u64,
            };
            serde_json::json!({
                "dict_keys": s.dict.len(),
                "data_len": data_len,
                "decoded": false
            })
        }
        PdfObject::Reference(id) => {
            serde_json::json!({ "number": id.number, "generation": id.generation })
        }
    }
}

fn op_json(op: &SemanticOp) -> serde_json::Value {
    match op {
        SemanticOp::Save => serde_json::json!({"op": "save"}),
        SemanticOp::Restore => serde_json::json!({"op": "restore"}),
        SemanticOp::Concat(m) => serde_json::json!({
            "op": "concat",
            "matrix": [m.a, m.b, m.c, m.d, m.e, m.f]
        }),
        SemanticOp::SetLineWidth(w) => serde_json::json!({"op": "set_line_width", "width": w}),
        SemanticOp::SetLineCap(c) => {
            serde_json::json!({"op": "set_line_cap", "cap": format!("{c:?}")})
        }
        SemanticOp::SetLineJoin(j) => {
            serde_json::json!({"op": "set_line_join", "join": format!("{j:?}")})
        }
        SemanticOp::SetMiterLimit(m) => serde_json::json!({"op": "set_miter_limit", "limit": m}),
        SemanticOp::SetDash { pattern, phase } => {
            serde_json::json!({"op": "set_dash", "pattern": pattern, "phase": phase})
        }
        SemanticOp::SetFillColor(_) => serde_json::json!({"op": "set_fill_color"}),
        SemanticOp::SetStrokeColor(_) => serde_json::json!({"op": "set_stroke_color"}),
        SemanticOp::SetFillAlpha(a) => serde_json::json!({"op": "set_fill_alpha", "alpha": a}),
        SemanticOp::SetStrokeAlpha(a) => serde_json::json!({"op": "set_stroke_alpha", "alpha": a}),
        SemanticOp::SetBlendMode(b) => {
            serde_json::json!({"op": "set_blend_mode", "mode": format!("{b:?}")})
        }
        SemanticOp::Fill { path, rule } => serde_json::json!({
            "op": "fill", "path": path.0, "rule": format!("{rule:?}")
        }),
        SemanticOp::Stroke { path } => serde_json::json!({"op": "stroke", "path": path.0}),
        SemanticOp::FillStroke { path, rule } => serde_json::json!({
            "op": "fill_stroke", "path": path.0, "rule": format!("{rule:?}")
        }),
        SemanticOp::Clip { path, rule } => serde_json::json!({
            "op": "clip", "path": path.0, "rule": format!("{rule:?}")
        }),
        SemanticOp::ClipText { runs } => serde_json::json!({
            "op": "clip_text",
            "runs": runs.iter().map(|r| r.0).collect::<Vec<_>>()
        }),
        SemanticOp::ShowText(id) => serde_json::json!({"op": "show_text", "run": id.0}),
        SemanticOp::DrawImage(id) => serde_json::json!({"op": "draw_image", "image": id.0}),
        SemanticOp::BeginPaintOrigin(o) => {
            serde_json::json!({"op": "begin_paint_origin", "origin": format!("{o:?}")})
        }
        SemanticOp::EndPaintOrigin => serde_json::json!({"op": "end_paint_origin"}),
        SemanticOp::PaintShading(id) => serde_json::json!({"op": "paint_shading", "shading": id.0}),
        SemanticOp::BeginGroup {
            isolated,
            knockout,
            bounds,
            opacity,
            blend,
        } => serde_json::json!({
            "op": "begin_group",
            "isolated": isolated,
            "knockout": knockout,
            "bounds": [bounds.x0, bounds.y0, bounds.x1, bounds.y1],
            "opacity": opacity,
            "blend": format!("{blend:?}")
        }),
        SemanticOp::EndGroup => serde_json::json!({"op": "end_group"}),
        SemanticOp::BeginSoftMask { kind, .. } => {
            serde_json::json!({"op": "begin_soft_mask", "kind": format!("{kind:?}")})
        }
        SemanticOp::EndSoftMask => serde_json::json!({"op": "end_soft_mask"}),
        SemanticOp::ClearSoftMask => serde_json::json!({"op": "clear_soft_mask"}),
    }
}

fn emit(env: &Envelope, data: &ContentData, mode: OutputMode) -> Result<()> {
    match mode {
        OutputMode::Human => {
            if let Some(dump) = &data.dump {
                print!("{dump}");
            }
            if let Some(ops) = &data.ops {
                println!("ops_json_count: {}", ops.len());
            }
            if let Some(res) = &data.resources {
                println!(
                    "resources: fonts={} images={} shadings={} paths={} text_runs={}",
                    res.fonts.len(),
                    res.images.len(),
                    res.shading_count,
                    res.path_count,
                    res.text_run_count
                );
            }
            for w in &env.warnings {
                eprintln!("warning: {w}");
            }
            Ok(())
        }
        OutputMode::Json | OutputMode::Jsonl => env.write_json(),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;

    #[test]
    fn truncate_at_char_boundary_backs_off_to_full_chars() {
        let s = "café";
        // 'é' is a 2-byte UTF-8 sequence starting at byte 3; byte 4 falls
        // inside it. A raw `&s[..4]` panics — the fix must back off to the
        // preceding char boundary instead.
        assert_eq!(truncate_at_char_boundary(s, 4), "caf");
        assert_eq!(truncate_at_char_boundary(s, 3), "caf");
        assert_eq!(truncate_at_char_boundary(s, 5), "café");
        assert_eq!(truncate_at_char_boundary(s, 100), "café");
        assert_eq!(truncate_at_char_boundary(s, 0), "");
    }

    #[test]
    fn object_summary_truncates_multibyte_text_without_panicking() {
        // PDFDocEncoding byte 0xE9 decodes to U+00E9 'é'; "caf\xE9" decodes
        // to "café", whose UTF-8 encoding is 5 bytes (é takes 2). With
        // --max-bytes 4 the naive byte-index slice used to land inside 'é'
        // and panic.
        let s = pdf_object::PdfString::new(b"caf\xE9".to_vec());
        let summary = object_summary(&PdfObject::String(s), 4);
        assert_eq!(summary["text"], serde_json::json!("caf…"));
        assert_eq!(summary["byte_len"], serde_json::json!(4));
    }
}
