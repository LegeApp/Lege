use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use pdf_document::DocumentSnapshot;
use pdf_text::TextPageOptions;

use crate::bounds::Bounds;
use crate::commands::text::{self, OcrMode, TextLayout};
use crate::commands::{default_annotations, emit_failed, page_compiler, resolve_snapshot};
use crate::open::DocumentIdentity;
use crate::pages::{PageZero, parse_page_range};
use crate::schema::{Envelope, OutputMode};
use crate::views::search::{MatchView, SearchPageData};
use crate::views::text::WordView;

#[derive(Debug)]
pub struct SearchArgs<'a> {
    pub path: &'a Path,
    pub password: Option<&'a str>,
    pub query: &'a str,
    pub pages: Option<&'a str>,
    pub context: usize,
    pub case_insensitive: bool,
    pub ocr: OcrMode,
    pub ocr_language: &'a str,
    pub system_fonts: bool,
    pub bounds: Bounds,
    pub fail_fast: bool,
    pub output: OutputMode,
    pub snapshot: Option<Arc<DocumentSnapshot>>,
    pub identity: Option<DocumentIdentity>,
}

pub fn run(args: SearchArgs<'_>) -> Result<i32> {
    if args.query.is_empty() {
        anyhow::bail!("search query must not be empty");
    }
    let (identity, snapshot) =
        resolve_snapshot(args.path, args.password, args.snapshot, args.identity)?;
    let document = identity.display_path();
    let mut global_warnings = Vec::new();
    let (page_indices, range_warnings) =
        parse_page_range(args.pages, snapshot.page_count(), args.bounds.max_pages)?;
    global_warnings.extend(range_warnings);

    let mut exit = 0i32;
    let multi = true; // search is always multi-record friendly

    // Built once for the whole run: `page_compiler` re-scans every system
    // font directory under `--system-fonts`, and the OCR backend starts with
    // cold glyph/image caches, so neither belongs inside the page loop.
    let compiler = page_compiler(args.system_fonts, default_annotations());
    let mut ocr_backend = None;

    for pz in &page_indices {
        let mut warnings = global_warnings.clone();
        match search_page(
            &snapshot,
            *pz,
            &compiler,
            args.query,
            args.context,
            args.case_insensitive,
            args.ocr,
            args.ocr_language,
            &args.bounds,
            &mut warnings,
            &mut ocr_backend,
        ) {
            Ok(data) => {
                // Skip empty pages in human mode noise reduction, but always emit JSONL.
                if data.match_count == 0 && matches!(args.output, OutputMode::Human) {
                    continue;
                }
                let env =
                    Envelope::page_ok(&document, pz.one_based(), serde_json::to_value(&data)?)
                        .with_warnings(warnings);
                emit(&env, &data, args.output, multi)?;
            }
            Err(err) => {
                let env = Envelope::page_failed(&document, pz.one_based(), err.to_string());
                emit_failed(&env, args.output, multi)?;
                if args.fail_fast {
                    return Ok(1);
                }
                exit = 1;
            }
        }
    }
    Ok(exit)
}

pub fn search_value(
    snapshot: &DocumentSnapshot,
    page_one: u32,
    query: &str,
    context: usize,
    case_insensitive: bool,
    system_fonts: bool,
    bounds: Bounds,
    ocr: OcrMode,
    ocr_language: &str,
) -> Result<serde_json::Value> {
    let pz = crate::pages::to_zero_based(page_one, snapshot.page_count())?;
    let mut warnings = Vec::new();
    let compiler = page_compiler(system_fonts, default_annotations());
    let mut ocr_backend = None;
    let data = search_page(
        snapshot,
        pz,
        &compiler,
        query,
        context,
        case_insensitive,
        ocr,
        ocr_language,
        &bounds,
        &mut warnings,
        &mut ocr_backend,
    )?;
    Ok(serde_json::to_value(data)?)
}

#[allow(clippy::too_many_arguments)]
fn search_page(
    snapshot: &DocumentSnapshot,
    pz: PageZero,
    compiler: &pdf_content::PageCompiler,
    query: &str,
    context: usize,
    case_insensitive: bool,
    ocr: OcrMode,
    ocr_language: &str,
    bounds: &Bounds,
    warnings: &mut Vec<String>,
    ocr_backend: &mut Option<pdf_render_cpu::CpuBackend>,
) -> Result<SearchPageData> {
    let extracted = text::extract_page(
        snapshot,
        compiler,
        pz,
        &TextPageOptions::default(),
        TextLayout::Words,
        None,
        ocr,
        ocr_language,
        bounds,
        warnings,
        ocr_backend,
    )?;
    let haystack = extracted.text.clone().unwrap_or_default();
    let words = extracted.words.as_deref().unwrap_or_default();

    let matches = find_matches(
        &haystack,
        words,
        query,
        context,
        case_insensitive,
        bounds,
        warnings,
    );

    let match_count = matches.len();
    Ok(SearchPageData {
        unit: "pdf_points",
        query: query.to_owned(),
        match_count,
        matches,
        provenance: extracted.provenance,
    })
}

/// Find every occurrence of `query` in `haystack`, honoring `case_insensitive`
/// and expanding `context` characters (not bytes) on each side.
///
/// Works entirely in character space so slicing is always char-boundary safe
/// and `first_char`/`char_count` stay exact: byte offsets from a *separately
/// lowercased* copy of `haystack` cannot be trusted against the original
/// string, because case folding is not byte-length-preserving (e.g. `İ`
/// U+0130 folds to two characters, `i` + combining dot above). Each match's
/// character offset is read directly off the single forward scan rather than
/// recomputed with `haystack[..idx].chars().count()`, which would make the
/// whole page scan O(n) per match (quadratic overall on match-heavy pages).
fn find_matches(
    haystack: &str,
    words: &[WordView],
    query: &str,
    context: usize,
    case_insensitive: bool,
    bounds: &Bounds,
    warnings: &mut Vec<String>,
) -> Vec<MatchView> {
    let fold = |c: char| -> char {
        if case_insensitive {
            c.to_lowercase().next().unwrap_or(c)
        } else {
            c
        }
    };

    // (byte_offset, char) for every character; index into this vec is the
    // character offset used throughout.
    let hay_chars: Vec<(usize, char)> = haystack.char_indices().collect();
    let hay_fold: Vec<char> = hay_chars.iter().map(|&(_, c)| fold(c)).collect();
    let q_fold: Vec<char> = query.chars().map(fold).collect();

    let byte_at = |char_idx: usize| -> usize {
        hay_chars
            .get(char_idx)
            .map(|&(o, _)| o)
            .unwrap_or(haystack.len())
    };

    let mut matches = Vec::new();
    let n = hay_fold.len();
    let m = q_fold.len();
    if m == 0 {
        return matches;
    }

    let mut start_char = 0usize;
    while start_char + m <= n {
        if hay_fold[start_char..start_char + m] == q_fold[..] {
            let end_char = start_char + m;
            let idx = byte_at(start_char);
            let end = byte_at(end_char);

            let ctx_start_char = start_char.saturating_sub(context);
            let ctx_end_char = (end_char + context).min(n);
            let ctx_start = byte_at(ctx_start_char);
            let ctx_end = byte_at(ctx_end_char);

            let first_char = start_char;
            let char_count = m;
            let bbox = words
                .iter()
                .find(|w| {
                    let w_end = w.first_char + w.char_count;
                    w.first_char < first_char + char_count && w_end > first_char
                })
                .map(|w| w.bbox);

            matches.push(MatchView {
                text: haystack[idx..end].to_owned(),
                context: haystack[ctx_start..ctx_end].to_owned(),
                bbox,
                first_char,
                char_count,
            });

            start_char = end_char.max(start_char + 1);
            if bounds.max_items > 0 && matches.len() as u32 >= bounds.max_items {
                warnings.push(format!(
                    "matches truncated to max-items={}",
                    bounds.max_items
                ));
                break;
            }
        } else {
            start_char += 1;
        }
    }

    matches
}

fn emit(env: &Envelope, data: &SearchPageData, mode: OutputMode, _multi: bool) -> Result<()> {
    match mode {
        OutputMode::Human => {
            for m in &data.matches {
                println!(
                    "page {}: {:?} context={:?} bbox={:?}",
                    env.page.unwrap_or(0),
                    m.text,
                    m.context,
                    m.bbox
                );
            }
            for w in &env.warnings {
                eprintln!("warning: {w}");
            }
            Ok(())
        }
        OutputMode::Json | OutputMode::Jsonl => env.write_jsonl(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matches_for(
        haystack: &str,
        query: &str,
        context: usize,
        case_insensitive: bool,
    ) -> Vec<MatchView> {
        let bounds = Bounds::default();
        let mut warnings = Vec::new();
        find_matches(
            haystack,
            &[],
            query,
            context,
            case_insensitive,
            &bounds,
            &mut warnings,
        )
    }

    /// Non-ASCII text around a match must never panic, and `context` must be
    /// interpreted as characters, not bytes: slicing at a raw byte offset
    /// derived from a character count would land inside `é`'s 2-byte UTF-8
    /// encoding.
    #[test]
    fn context_around_multibyte_text_is_char_boundary_safe() {
        let haystack = "your café is ready";
        let found = matches_for(haystack, "café", 2, false);
        assert_eq!(found.len(), 1);
        let m = &found[0];
        assert_eq!(m.text, "café");
        assert_eq!(m.first_char, 5);
        assert_eq!(m.char_count, 4);
        // 2 chars of context on each side: "r" + "café" + " i".
        assert_eq!(m.context, "r café i");
    }

    /// Case folding is not byte-length-preserving (`İ` U+0130 folds to two
    /// characters). Indices must stay valid against the *original* haystack
    /// even when an earlier character's case-folded form changes length.
    #[test]
    fn case_insensitive_search_stays_aligned_after_length_changing_fold() {
        let haystack = "İ marks the spot: café";
        let found = matches_for(haystack, "CAFÉ", 0, true);
        assert_eq!(found.len(), 1);
        let m = &found[0];
        assert_eq!(m.text, "café");
        assert_eq!(m.context, "café");
        assert_eq!(m.first_char, 18);
        assert_eq!(m.char_count, 4);
    }

    /// Each match's character offset must come from the single incremental
    /// scan (O(1) per match), not from re-scanning `haystack[..idx]` with
    /// `.chars().count()` for every match found so far.
    #[test]
    fn repeated_matches_get_correct_incremental_char_offsets() {
        let haystack = "café one café two café three";
        let found = matches_for(haystack, "café", 0, false);
        let first_chars: Vec<usize> = found.iter().map(|m| m.first_char).collect();
        assert_eq!(first_chars, vec![0, 9, 18]);
        for m in &found {
            assert_eq!(m.char_count, 4);
            assert_eq!(m.text, "café");
        }
    }

    /// Word bboxes are looked up by character offset; confirm that lookup
    /// still resolves correctly against the fixed char-based offsets.
    #[test]
    fn bbox_lookup_uses_character_offsets() {
        let words = [WordView {
            text: "café".to_owned(),
            bbox: [1.0, 2.0, 3.0, 4.0],
            first_char: 0,
            char_count: 4,
            continued: false,
        }];
        let bounds = Bounds::default();
        let mut warnings = Vec::new();
        let found = find_matches("café", &words, "café", 0, false, &bounds, &mut warnings);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].bbox, Some([1.0, 2.0, 3.0, 4.0]));
    }
}
