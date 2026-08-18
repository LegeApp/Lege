use std::path::Path;
use std::sync::Arc;

use anyhow::{Result, bail};
use pdf_document::{DocumentSnapshot, PageIndex};
use pdf_page_ir::{Point, Rect};
use pdf_render_cpu::{CpuBackend, CpuBackendOptions};
use pdf_text::{TextPage, TextPageOptions, TextWord};

use crate::bounds::Bounds;
use crate::commands::{
    default_annotations, emit_failed, page_compiler, parse_context, resolve_snapshot,
};
use crate::open::DocumentIdentity;
use crate::pages::{PageZero, parse_bbox, parse_page_range};
use crate::schema::{Envelope, OutputMode, Status};
use crate::views::text::{BlockView, CharView, LineView, TextPageData, TextProvenance, WordView};

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum TextLayout {
    Plain,
    Blocks,
    Words,
    Chars,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum OcrMode {
    /// Read only native PDF text operators.
    Never,
    /// OCR pages whose native text fails conservative trust checks.
    Auto,
    /// Render and OCR every selected page, ignoring native text for output.
    Always,
}

#[derive(Debug)]
pub struct TextArgs<'a> {
    pub path: &'a Path,
    pub password: Option<&'a str>,
    pub pages: Option<&'a str>,
    pub layout: TextLayout,
    pub bbox: Option<&'a str>,
    pub rtl: bool,
    pub normalize: bool,
    pub include_hidden: bool,
    pub include_annotations: bool,
    pub ocr: OcrMode,
    pub ocr_language: &'a str,
    pub system_fonts: bool,
    pub bounds: Bounds,
    pub fail_fast: bool,
    pub output: OutputMode,
    pub snapshot: Option<Arc<DocumentSnapshot>>,
    pub identity: Option<DocumentIdentity>,
}

pub fn run(args: TextArgs<'_>) -> Result<i32> {
    let (identity, snapshot) =
        resolve_snapshot(args.path, args.password, args.snapshot, args.identity)?;
    let document = identity.display_path();
    let mut global_warnings = Vec::new();
    let (page_indices, range_warnings) =
        parse_page_range(args.pages, snapshot.page_count(), args.bounds.max_pages)?;
    global_warnings.extend(range_warnings);

    let bbox = args.bbox.map(parse_bbox).transpose()?;
    let options = TextPageOptions {
        rtl: args.rtl,
        normalize: args.normalize,
        include_hidden: args.include_hidden,
        include_annotations: args.include_annotations,
        include_soft_masks: false,
    };

    let compiler = page_compiler(args.system_fonts, default_annotations());
    let mut exit = 0i32;
    let multi = page_indices.len() > 1 || matches!(args.output, OutputMode::Jsonl);
    // Lazily built on first OCR need and reused for the rest of this run, so
    // `--ocr always --pages all` doesn't re-prepare the CPU backend's shared
    // font/glyph/image caches on every page.
    let mut ocr_backend: Option<CpuBackend> = None;

    for (i, pz) in page_indices.iter().enumerate() {
        // Range warnings only need to be reported once per run.
        let mut warnings = if i == 0 {
            global_warnings.clone()
        } else {
            Vec::new()
        };
        match extract_page(
            &snapshot,
            &compiler,
            *pz,
            &options,
            args.layout,
            bbox,
            args.ocr,
            args.ocr_language,
            &args.bounds,
            &mut warnings,
            &mut ocr_backend,
        ) {
            Ok(data) => {
                let mut env =
                    Envelope::page_ok(&document, pz.one_based(), serde_json::to_value(&data)?)
                        .with_warnings(warnings);
                // Render once, in the exact form that will be written, and
                // check its length before emitting. The truncation warning
                // below changes the payload, so this measures the
                // pre-warning payload (matches prior behavior); only pages
                // that actually get truncated pay for a second render.
                let pretty = matches!(args.output, OutputMode::Json) && !multi;
                let mut rendered = render_envelope(&env, pretty)?;
                if args.bounds.exceeds_bytes(rendered.len()) {
                    env.push_warning(format!(
                        "payload exceeds max-bytes={}",
                        args.bounds.max_bytes
                    ));
                    env = env.set_status(Status::Truncated);
                    rendered = render_envelope(&env, pretty)?;
                }
                emit_page(&env, &data, args.output, multi, &rendered)?;
            }
            Err(err) => {
                exit = if args.fail_fast { 1 } else { exit };
                let env = Envelope::page_failed(&document, pz.one_based(), err.to_string());
                emit_failed(&env, args.output, multi)?;
                if args.fail_fast {
                    return Ok(1);
                }
            }
        }
    }
    Ok(exit)
}

fn render_envelope(env: &Envelope, pretty: bool) -> Result<String> {
    Ok(if pretty {
        serde_json::to_string_pretty(env)?
    } else {
        serde_json::to_string(env)?
    })
}

pub fn text_value(
    snapshot: &DocumentSnapshot,
    page_one: u32,
    layout: TextLayout,
    bbox: Option<[f64; 4]>,
    options: TextPageOptions,
    system_fonts: bool,
    bounds: Bounds,
    ocr: OcrMode,
    ocr_language: &str,
) -> Result<serde_json::Value> {
    Ok(serde_json::to_value(text_data(
        snapshot,
        page_one,
        layout,
        bbox,
        options,
        system_fonts,
        bounds,
        ocr,
        ocr_language,
    )?)?)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn text_data(
    snapshot: &DocumentSnapshot,
    page_one: u32,
    layout: TextLayout,
    bbox: Option<[f64; 4]>,
    options: TextPageOptions,
    system_fonts: bool,
    bounds: Bounds,
    ocr: OcrMode,
    ocr_language: &str,
) -> Result<TextPageData> {
    let pz = crate::pages::to_zero_based(page_one, snapshot.page_count())?;
    let compiler = page_compiler(system_fonts, default_annotations());
    let mut warnings = Vec::new();
    // Single-page entry point (called fresh per page by the serve/search
    // callers), so there is no run-level backend to reuse here.
    let mut ocr_backend = None;
    let data = extract_page(
        snapshot,
        &compiler,
        pz,
        &options,
        layout,
        bbox,
        ocr,
        ocr_language,
        &bounds,
        &mut warnings,
        &mut ocr_backend,
    )?;
    Ok(data)
}

/// Extract one page against a caller-owned compiler and OCR backend.
///
/// `search` runs this across many pages and owns both, so neither is rebuilt
/// per page: constructing a `PageCompiler` under `--system-fonts` re-scans
/// every platform font directory, and a fresh `CpuBackend` starts with empty
/// glyph and image caches.
#[allow(clippy::too_many_arguments)]
pub(crate) fn extract_page(
    snapshot: &DocumentSnapshot,
    compiler: &pdf_content::PageCompiler,
    pz: PageZero,
    options: &TextPageOptions,
    layout: TextLayout,
    bbox: Option<[f64; 4]>,
    ocr: OcrMode,
    ocr_language: &str,
    bounds: &Bounds,
    warnings: &mut Vec<String>,
    ocr_backend: &mut Option<CpuBackend>,
) -> Result<TextPageData> {
    let mut ctx = parse_context();
    let page = compiler.compile_semantic(snapshot, PageIndex(pz.0), &mut ctx)?;
    let text_page = TextPage::build(&page, options);
    // Bind the page text once: `all_text()` allocates and walks the whole
    // page, so both counts are derived from this one string in a single
    // pass, and (below) the same string is reused for `data.text` instead of
    // being rebuilt a third time.
    let native_text = text_page.all_text();
    let mut native_char_count = 0usize;
    let mut replacement_count = 0usize;
    for character in native_text.chars() {
        if !character.is_whitespace() {
            native_char_count += 1;
        }
        if character == '\u{fffd}' {
            replacement_count += 1;
        }
    }
    let native_words = text_page.words();
    let valid_word_count = native_words
        .iter()
        .filter(|word| word.bbox.x1 > word.bbox.x0 && word.bbox.y1 > word.bbox.y0)
        .count();
    let trustworthy = native_char_count >= 8
        && replacement_count.saturating_mul(100) <= native_char_count
        && valid_word_count >= 2;
    let page_content_kind = match (trustworthy, !page.images.is_empty(), native_char_count > 0) {
        (true, true, _) => "hybrid",
        (true, false, _) => "native-text",
        (false, true, false) => "scanned-image",
        _ => "render-required",
    };

    let rect = bbox.map(|b| Rect {
        x0: b[0].min(b[2]),
        y0: b[1].min(b[3]),
        x1: b[0].max(b[2]),
        y1: b[1].max(b[3]),
    });

    if ocr == OcrMode::Always || (ocr == OcrMode::Auto && !trustworthy) {
        let backend =
            ocr_backend.get_or_insert_with(|| CpuBackend::new(CpuBackendOptions::default()));
        return extract_ocr_page(
            snapshot,
            compiler,
            pz,
            layout,
            rect,
            bounds,
            ocr_language,
            page_content_kind,
            trustworthy,
            native_char_count,
            warnings,
            backend,
        );
    }

    let mut data = TextPageData {
        unit: "pdf_points",
        layout: format!("{:?}", layout).to_ascii_lowercase(),
        text: None,
        words: None,
        chars: None,
        blocks: None,
        char_count: text_page.char_count(),
        provenance: TextProvenance {
            source: "native",
            page_content_kind,
            native_text_trustworthy: trustworthy,
            native_char_count,
            ocr_engine: None,
            ocr_language: None,
        },
    };

    match layout {
        TextLayout::Plain => {
            data.text = Some(match rect {
                Some(r) => text_page.text_in_rect(r),
                None => native_text,
            });
        }
        TextLayout::Words => {
            let mut words = collect_words(&text_page, rect);
            words = bounds.truncate_items(words, warnings, "words");
            data.words = Some(words);
            data.text = Some(match rect {
                Some(r) => text_page.text_in_rect(r),
                None => native_text,
            });
        }
        TextLayout::Chars => {
            let mut chars = collect_chars(&text_page, rect);
            chars = bounds.truncate_items(chars, warnings, "chars");
            data.chars = Some(chars);
            data.text = Some(match rect {
                Some(r) => text_page.text_in_rect(r),
                None => native_text,
            });
        }
        TextLayout::Blocks => {
            let words = collect_words(&text_page, rect);
            let mut blocks = group_blocks(words);
            blocks = bounds.truncate_items(blocks, warnings, "blocks");
            data.text = Some(
                blocks
                    .iter()
                    .map(|block| block.text.as_str())
                    .collect::<Vec<_>>()
                    .join("\n\n"),
            );
            data.blocks = Some(blocks);
        }
    }

    Ok(data)
}

#[allow(clippy::too_many_arguments)]
fn extract_ocr_page(
    snapshot: &DocumentSnapshot,
    compiler: &pdf_content::PageCompiler,
    pz: PageZero,
    layout: TextLayout,
    rect: Option<Rect>,
    bounds: &Bounds,
    language: &str,
    page_content_kind: &'static str,
    native_text_trustworthy: bool,
    native_char_count: usize,
    warnings: &mut Vec<String>,
    backend: &CpuBackend,
) -> Result<TextPageData> {
    if layout == TextLayout::Chars {
        bail!(
            "OCR character-level geometry is not available in this first draft; use layout=words or blocks"
        );
    }
    const OCR_DPI: f64 = 300.0;
    const OCR_MAX_PIXELS: u64 = 20_000_000;
    let (image, device) = super::render::render_gray_for_ocr(
        snapshot,
        compiler,
        pz,
        OCR_DPI,
        OCR_MAX_PIXELS,
        backend,
    )?;
    let inverse = device
        .invert()
        .ok_or_else(|| anyhow::anyhow!("OCR page transform is not invertible"))?;
    let engine = lege_ocr::engine::default_engine();
    let engine_name = engine.name();
    let lines = engine.ocr_page(&image, language)?;

    let mut words = Vec::new();
    let mut line_text = Vec::new();
    let mut first_char = 0usize;
    for line in lines {
        let trimmed = line.text.trim();
        if trimmed.is_empty() {
            continue;
        }
        line_text.push(trimmed.to_owned());
        let line_start = first_char;
        let mut search_byte = 0usize;
        for word in line.words {
            let [line_x, line_y, _, _] = line.bbox_highres;
            let [x0, y0, x1, y1] = word.bbox_crop_local;
            let bbox = device_box_to_pdf(
                [line_x + x0, line_y + y0, line_x + x1, line_y + y1],
                inverse,
            );
            let char_count = word.text.chars().count();
            let word_first_char = trimmed[search_byte..]
                .find(&word.text)
                .map(|relative_byte| {
                    let absolute_byte = search_byte + relative_byte;
                    search_byte = absolute_byte + word.text.len();
                    line_start + trimmed[..absolute_byte].chars().count()
                })
                .unwrap_or(first_char);
            if rect.is_none_or(|selection| intersects_arr(bbox, selection)) {
                words.push(WordView {
                    text: word.text,
                    bbox,
                    first_char: word_first_char,
                    char_count,
                    continued: false,
                });
            }
        }
        first_char = line_start + trimmed.chars().count() + 1;
    }
    words = bounds.truncate_items(words, warnings, "OCR words");
    let mut text = if rect.is_some() {
        words
            .iter()
            .map(|word| word.text.as_str())
            .collect::<Vec<_>>()
            .join(" ")
    } else {
        line_text.join("\n")
    };
    let mut blocks = None;
    let mut output_words = None;
    match layout {
        TextLayout::Plain => {}
        TextLayout::Words => output_words = Some(words),
        TextLayout::Blocks => {
            let grouped = group_blocks(words);
            text = grouped
                .iter()
                .map(|block| block.text.as_str())
                .collect::<Vec<_>>()
                .join("\n\n");
            blocks = Some(grouped);
        }
        TextLayout::Chars => unreachable!("rejected above"),
    }
    let char_count = text.chars().count();
    Ok(TextPageData {
        unit: "pdf_points",
        layout: format!("{:?}", layout).to_ascii_lowercase(),
        text: Some(text),
        words: output_words,
        chars: None,
        blocks,
        char_count,
        provenance: TextProvenance {
            source: "ocr",
            page_content_kind,
            native_text_trustworthy,
            native_char_count,
            ocr_engine: Some(engine_name),
            ocr_language: Some(language.to_owned()),
        },
    })
}

fn device_box_to_pdf(bbox: [u32; 4], inverse: pdf_page_ir::Matrix) -> [f64; 4] {
    let points = [
        inverse.apply(Point {
            x: f64::from(bbox[0]),
            y: f64::from(bbox[1]),
        }),
        inverse.apply(Point {
            x: f64::from(bbox[2]),
            y: f64::from(bbox[1]),
        }),
        inverse.apply(Point {
            x: f64::from(bbox[0]),
            y: f64::from(bbox[3]),
        }),
        inverse.apply(Point {
            x: f64::from(bbox[2]),
            y: f64::from(bbox[3]),
        }),
    ];
    points.iter().fold(
        [
            f64::INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NEG_INFINITY,
        ],
        |mut result, point| {
            result[0] = result[0].min(point.x);
            result[1] = result[1].min(point.y);
            result[2] = result[2].max(point.x);
            result[3] = result[3].max(point.y);
            result
        },
    )
}

fn intersects_arr(a: [f64; 4], b: Rect) -> bool {
    a[0] < b.x1 && a[2] > b.x0 && a[1] < b.y1 && a[3] > b.y0
}

fn collect_words(text_page: &TextPage, rect: Option<Rect>) -> Vec<WordView> {
    text_page
        .words()
        .into_iter()
        .filter(|w| rect.is_none_or(|r| intersects(w.bbox, r)))
        .map(word_view)
        .collect()
}

fn collect_chars(text_page: &TextPage, rect: Option<Rect>) -> Vec<CharView> {
    text_page
        .chars()
        .iter()
        .filter(|c| rect.is_none_or(|r| intersects(c.char_box, r)))
        .map(|info| {
            let printable = char::from_u32(info.unicode)
                .filter(|c| !c.is_control())
                .map(|c| c.to_string())
                .unwrap_or_default();
            CharView {
                unicode: info.unicode,
                text: printable,
                char_code: info.char_code,
                cid: info.cid,
                glyph_id: info.glyph_id,
                text_object: info.text_object.0,
                tight_box: rect_arr(info.char_box),
                loose_box: rect_arr(info.loose_char_box),
                char_type: format!("{:?}", info.char_type),
            }
        })
        .collect()
}

fn word_view(word: TextWord) -> WordView {
    WordView {
        text: word.text,
        bbox: rect_arr(word.bbox),
        first_char: word.first_char,
        char_count: word.char_count,
        continued: word.continued,
    }
}

fn rect_arr(r: Rect) -> [f64; 4] {
    [r.x0, r.y0, r.x1, r.y1]
}

fn intersects(a: Rect, b: Rect) -> bool {
    a.x0 < b.x1 && a.x1 > b.x0 && a.y0 < b.y1 && a.y1 > b.y0
}

/// Heuristic physical layout: cluster words into lines by y-overlap, then
/// lines into blocks by vertical gap. Not pdftotext -layout parity.
fn group_blocks(mut words: Vec<WordView>) -> Vec<BlockView> {
    if words.is_empty() {
        return Vec::new();
    }
    words.sort_by(|a, b| {
        let ay = (a.bbox[1] + a.bbox[3]) * 0.5;
        let by = (b.bbox[1] + b.bbox[3]) * 0.5;
        by.partial_cmp(&ay)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                a.bbox[0]
                    .partial_cmp(&b.bbox[0])
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
    });

    let mut lines: Vec<Vec<WordView>> = Vec::new();
    for word in words {
        let mid_y = (word.bbox[1] + word.bbox[3]) * 0.5;
        let height = (word.bbox[3] - word.bbox[1]).abs().max(1.0);
        if let Some(line) = lines.last_mut() {
            let ref_w = &line[0];
            let ref_mid = (ref_w.bbox[1] + ref_w.bbox[3]) * 0.5;
            let ref_h = (ref_w.bbox[3] - ref_w.bbox[1]).abs().max(1.0);
            if (mid_y - ref_mid).abs() <= 0.6 * ref_h.max(height) {
                line.push(word);
                continue;
            }
        }
        lines.push(vec![word]);
    }

    for line in &mut lines {
        line.sort_by(|a, b| {
            a.bbox[0]
                .partial_cmp(&b.bbox[0])
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }

    let line_views: Vec<LineView> = lines
        .into_iter()
        .map(|words| {
            let text = words
                .iter()
                .map(|w| w.text.as_str())
                .collect::<Vec<_>>()
                .join(" ");
            let bbox = union_boxes(words.iter().map(|w| w.bbox));
            LineView { text, bbox, words }
        })
        .collect();

    let mut blocks = Vec::new();
    let mut current: Vec<LineView> = Vec::new();
    for line in line_views {
        if let Some(prev) = current.last() {
            let gap = prev.bbox[1] - line.bbox[3];
            let h = (prev.bbox[3] - prev.bbox[1]).abs().max(1.0);
            if gap > 1.5 * h {
                blocks.push(finish_block(std::mem::take(&mut current)));
            }
        }
        current.push(line);
    }
    if !current.is_empty() {
        blocks.push(finish_block(current));
    }
    blocks
}

fn finish_block(lines: Vec<LineView>) -> BlockView {
    let text = lines
        .iter()
        .map(|l| l.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let bbox = union_boxes(lines.iter().map(|l| l.bbox));
    BlockView { text, bbox, lines }
}

fn union_boxes(iter: impl Iterator<Item = [f64; 4]>) -> [f64; 4] {
    let mut out = [
        f64::INFINITY,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NEG_INFINITY,
    ];
    for b in iter {
        out[0] = out[0].min(b[0]);
        out[1] = out[1].min(b[1]);
        out[2] = out[2].max(b[2]);
        out[3] = out[3].max(b[3]);
    }
    if !out[0].is_finite() {
        [0.0, 0.0, 0.0, 0.0]
    } else {
        out
    }
}

fn emit_page(
    env: &Envelope,
    data: &TextPageData,
    mode: OutputMode,
    multi: bool,
    rendered: &str,
) -> Result<()> {
    match mode {
        OutputMode::Human => {
            if multi {
                println!("--- page {} ---", env.page.unwrap_or(0));
            }
            if let Some(text) = &data.text {
                print!("{text}");
                if !text.ends_with('\n') {
                    println!();
                }
            }
            if let Some(words) = &data.words {
                for (i, w) in words.iter().enumerate() {
                    println!(
                        "word {i}: {:?} box={:?} chars={}..{}",
                        w.text,
                        w.bbox,
                        w.first_char,
                        w.first_char + w.char_count
                    );
                }
            }
            if let Some(chars) = &data.chars {
                for (i, c) in chars.iter().enumerate() {
                    println!(
                        "char {i}: U+{:04X} {:?} box={:?}",
                        c.unicode, c.text, c.tight_box
                    );
                }
            }
            if let Some(blocks) = &data.blocks {
                for (i, b) in blocks.iter().enumerate() {
                    println!("block {i}: box={:?}\n{}", b.bbox, b.text);
                }
            }
            for w in &env.warnings {
                eprintln!("warning: {w}");
            }
            Ok(())
        }
        // `rendered` was already serialized (pretty for single-page Json,
        // compact for Jsonl/multi-page Json) by the caller, so both JSON
        // modes just print it instead of asking `Envelope` to serialize
        // again.
        OutputMode::Json | OutputMode::Jsonl => {
            println!("{rendered}");
            Ok(())
        }
    }
}
