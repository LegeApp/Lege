//! Deterministic textual dump of a [`SemanticPage`] (roadmap §7 Phase 2
//! validation).
//!
//! The dump is the phase's comparison surface: two compilations of the same
//! page — across repeated runs and worker counts — must produce byte-identical
//! dumps. It resolves interned [`NameId`]s back to their bytes, so the text is
//! also stable across processes (where raw `NameId` integers might not be).

use std::fmt::Write as _;

use pdf_object::{NameId, NameTable};
use pdf_page_ir::{FillRule, LineCap, LineJoin, Matrix};

use crate::semantic::{SemColor, SemanticOp, SemanticPage, TextElement};

/// Render `page` to its canonical multi-line dump.
pub fn dump_semantic(page: &SemanticPage, names: &NameTable) -> String {
    let mut out = String::new();
    let b = &page.bounds.crop;
    let _ = writeln!(
        out,
        "page [{} {} {} {}] rotate {}",
        num(b.x0),
        num(b.y0),
        num(b.x1),
        num(b.y1),
        page.bounds.rotate
    );

    for op in page.ops.iter() {
        out.push_str("  ");
        write_op(&mut out, op, names);
        out.push('\n');
    }

    // Resource tables, in stable index order.
    if !page.fonts.is_empty() {
        out.push_str("fonts:\n");
        for (i, f) in page.fonts.iter().enumerate() {
            let obj = match f.object {
                Some(id) => format!("obj {} {}", id.number, id.generation),
                None => "obj -".to_string(),
            };
            let _ = writeln!(
                out,
                "  font#{i} /{} {} /{} {obj}",
                name_str(names, f.resource_name),
                bytes_str(&f.subtype),
                bytes_str(&f.base_font),
            );
        }
    }
    if !page.paths.is_empty() {
        out.push_str("paths:\n");
        for (i, p) in page.paths.iter().enumerate() {
            let _ = writeln!(out, "  path#{i} verbs={} points={}", p.verbs.len(), p.points.len());
        }
    }
    if !page.text_runs.is_empty() {
        out.push_str("runs:\n");
        for (i, r) in page.text_runs.iter().enumerate() {
            let _ = writeln!(
                out,
                "  run#{i} font#{} size {} mode {} matrix {}",
                r.font.0,
                num(r.font_size),
                r.render_mode,
                matrix(&r.text_matrix),
            );
            for el in &r.elements {
                match el {
                    TextElement::Show(s) => {
                        let _ = writeln!(out, "    show \"{}\"", escape(s));
                    }
                    TextElement::Adjust(n) => {
                        let _ = writeln!(out, "    adjust {}", num(*n));
                    }
                }
            }
        }
    }
    if !page.images.is_empty() {
        out.push_str("images:\n");
        for (i, im) in page.images.iter().enumerate() {
            let src = match im.object {
                Some(id) => format!("obj {} {}", id.number, id.generation),
                None => "inline".to_string(),
            };
            let _ = writeln!(
                out,
                "  image#{i} {src} {}x{} bpc {} mask {}",
                im.width, im.height, im.bits_per_component, im.is_mask
            );
        }
    }

    out
}

fn write_op(out: &mut String, op: &SemanticOp, names: &NameTable) {
    match op {
        SemanticOp::Save => out.push_str("save"),
        SemanticOp::Restore => out.push_str("restore"),
        SemanticOp::Concat(m) => {
            let _ = write!(out, "concat {}", matrix(m));
        }
        SemanticOp::SetLineWidth(w) => {
            let _ = write!(out, "set-line-width {}", num(*w));
        }
        SemanticOp::SetLineCap(c) => {
            let _ = write!(out, "set-line-cap {}", cap(*c));
        }
        SemanticOp::SetLineJoin(j) => {
            let _ = write!(out, "set-line-join {}", join(*j));
        }
        SemanticOp::SetMiterLimit(m) => {
            let _ = write!(out, "set-miter-limit {}", num(*m));
        }
        SemanticOp::SetDash { pattern, phase } => {
            let items: Vec<String> = pattern.iter().map(|v| num(*v)).collect();
            let _ = write!(out, "set-dash [{}] {}", items.join(" "), num(*phase));
        }
        SemanticOp::SetFillColor(c) => {
            let _ = write!(out, "set-fill {}", color(c, names));
        }
        SemanticOp::SetStrokeColor(c) => {
            let _ = write!(out, "set-stroke {}", color(c, names));
        }
        SemanticOp::SetFillAlpha(a) => {
            let _ = write!(out, "set-fill-alpha {}", num(*a as f64));
        }
        SemanticOp::SetStrokeAlpha(a) => {
            let _ = write!(out, "set-stroke-alpha {}", num(*a as f64));
        }
        SemanticOp::SetBlendMode(m) => {
            let _ = write!(out, "set-blend {m:?}");
        }
        SemanticOp::Fill { path, rule } => {
            let _ = write!(out, "fill path#{} {}", path.0, fill_rule(*rule));
        }
        SemanticOp::Stroke { path } => {
            let _ = write!(out, "stroke path#{}", path.0);
        }
        SemanticOp::FillStroke { path, rule } => {
            let _ = write!(out, "fill-stroke path#{} {}", path.0, fill_rule(*rule));
        }
        SemanticOp::Clip { path, rule } => {
            let _ = write!(out, "clip path#{} {}", path.0, fill_rule(*rule));
        }
        SemanticOp::ClipText { runs } => {
            let ids: Vec<String> = runs.iter().map(|r| format!("run#{}", r.0)).collect();
            let _ = write!(out, "clip-text [{}]", ids.join(" "));
        }
        SemanticOp::ShowText(id) => {
            let _ = write!(out, "show-text run#{}", id.0);
        }
        SemanticOp::DrawImage(id) => {
            let _ = write!(out, "draw-image image#{}", id.0);
        }
        SemanticOp::PaintShading(id) => {
            let _ = write!(out, "paint-shading shading#{}", id.0);
        }
        SemanticOp::BeginGroup { isolated, knockout, bounds, opacity, blend } => {
            let _ = write!(
                out,
                "begin-group isolated={isolated} knockout={knockout} opacity={} blend={blend:?} bounds [{} {} {} {}]",
                num(*opacity as f64),
                num(bounds.x0),
                num(bounds.y0),
                num(bounds.x1),
                num(bounds.y1),
            );
        }
        SemanticOp::EndGroup => out.push_str("end-group"),
        SemanticOp::BeginPaintOrigin(o) => {
            let _ = write!(out, "begin-paint-origin {}", o.name());
        }
        SemanticOp::EndPaintOrigin => out.push_str("end-paint-origin"),
        SemanticOp::BeginSoftMask { kind, transfer } => {
            let _ = write!(out, "begin-soft-mask {kind:?}");
            if transfer.is_some() {
                out.push_str(" transfer=lut256");
            }
        }
        SemanticOp::EndSoftMask => out.push_str("end-soft-mask"),
        SemanticOp::ClearSoftMask => out.push_str("clear-soft-mask"),
    }
}

fn color(c: &SemColor, names: &NameTable) -> String {
    match c {
        SemColor::DeviceGray(v) => format!("DeviceGray({})", num(*v)),
        SemColor::DeviceRgb(r, g, b) => {
            format!("DeviceRGB({}, {}, {})", num(*r), num(*g), num(*b))
        }
        SemColor::DeviceCmyk(c, m, y, k) => {
            format!("DeviceCMYK({}, {}, {}, {})", num(*c), num(*m), num(*y), num(*k))
        }
        SemColor::Components { space, values } => {
            let vs: Vec<String> = values.iter().map(|v| num(*v)).collect();
            format!("/{}({})", name_str(names, *space), vs.join(", "))
        }
        SemColor::Pattern { name, .. } => format!("Pattern(/{})", name_str(names, *name)),
        SemColor::ShadingPattern { shading, .. } => format!("ShadingPattern(shading#{})", shading.0),
        SemColor::TilingPattern { tiling, .. } => format!("TilingPattern(tiling#{})", tiling.0),
    }
}

fn matrix(m: &Matrix) -> String {
    format!("[{} {} {} {} {} {}]", num(m.a), num(m.b), num(m.c), num(m.d), num(m.e), num(m.f))
}

fn fill_rule(r: FillRule) -> &'static str {
    match r {
        FillRule::NonZero => "nonzero",
        FillRule::EvenOdd => "evenodd",
    }
}

fn cap(c: LineCap) -> &'static str {
    match c {
        LineCap::Butt => "butt",
        LineCap::Round => "round",
        LineCap::Square => "square",
    }
}

fn join(j: LineJoin) -> &'static str {
    match j {
        LineJoin::Miter => "miter",
        LineJoin::Round => "round",
        LineJoin::Bevel => "bevel",
    }
}

/// Deterministic number formatting: integers print without a decimal point,
/// everything else uses the shortest round-tripping form. `-0.0` normalizes to
/// `0` so bit-level sign never perturbs a dump.
fn num(v: f64) -> String {
    if v == 0.0 {
        return "0".to_string();
    }
    if v.fract() == 0.0 && v.abs() < 1e15 {
        return format!("{}", v as i64);
    }
    format!("{v}")
}

fn name_str(names: &NameTable, id: NameId) -> String {
    bytes_str(&names.resolve(id))
}

fn bytes_str(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// Escape a shown string for the dump: printable ASCII verbatim, everything
/// else as `\xNN`, so binary character codes stay on one deterministic line.
fn escape(s: &[u8]) -> String {
    let mut out = String::new();
    for &b in s {
        match b {
            b'"' => out.push_str("\\\""),
            b'\\' => out.push_str("\\\\"),
            0x20..=0x7E => out.push(b as char),
            _ => {
                let _ = write!(out, "\\x{b:02x}");
            }
        }
    }
    out
}
