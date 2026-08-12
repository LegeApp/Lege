//! Automatic table of contents.
//!
//! Layout detection already tells us which boxes are `doc_title` and
//! `paragraph_title`, and the hOCR layer already holds the words inside those
//! boxes. This module turns that into a navigable outline for documents that
//! carry none of their own.
//!
//! Two halves, both pure:
//!
//! * [`capture_page`] runs once per page, where the output-space detections and
//!   the finished hOCR coexist. It costs nothing on a page with no title
//!   detection, and on a page with one it parses the hOCR that was built
//!   anyway.
//! * [`build_outline`] runs once, at finalize, over every captured candidate.
//!   It scores rather than gates, and resolves ambiguity toward emitting
//!   nothing — a wrong table of contents is worse than none.
//!
//! The caller decides precedence: a source outline that survives remapping
//! always wins, and the synthesized outline is used only when the document had
//! none. `LEGE_NO_AUTO_TOC=1` turns synthesis off entirely.

use std::collections::HashMap;
use std::sync::OnceLock;

use lege_pdf_write::outline::OutlineItem;

use crate::engine::Detection;
use crate::hocr::{HocrLine, parse_hocr};

/// Which title class produced a candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TitleKind {
    DocTitle,
    ParagraphTitle,
}

/// One detected title, with the evidence [`build_outline`] scores.
#[derive(Debug, Clone)]
pub struct TocCandidate {
    /// Zero-based output page index.
    pub page_index: usize,
    pub kind: TitleKind,
    /// Layout-detection confidence.
    pub confidence: f32,
    /// `[x1, y1, x2, y2]` in output page pixels.
    pub bbox: [f32; 4],
    /// The hOCR words inside the box, whitespace-normalized.
    pub text: String,
    /// Median height of the lines inside the box, in output page pixels.
    pub line_height: f32,
    pub page_height: f32,
    /// Blank vertical space between the title and the next text below it.
    pub gap_below: f32,
    /// Lowest per-word `x_wconf` inside the box. `None` when the text layer
    /// carries no confidences at all, which is the case for native PDF text.
    pub word_confidence: Option<f32>,
}

/// One page's body-text statistics.
///
/// The hOCR of a page is spilled to disk and never held document-wide, so the
/// document body median is a median of these per-page medians rather than of
/// every line in the document.
#[derive(Debug, Clone, Copy)]
pub struct PageTextStats {
    pub page_index: usize,
    pub median_line_height: f32,
}

/// What one page contributes. Text only — no raster payload.
#[derive(Debug, Clone, Default)]
pub struct PageTocData {
    pub candidates: Vec<TocCandidate>,
    pub stats: Option<PageTextStats>,
    /// Front-matter `doc_title` evidence used for conservative PDF identity.
    pub metadata_candidates: Vec<MetadataCandidate>,
    /// OCR lines found inside a printed table-of-contents (`content`) box.
    pub printed_contents: Vec<String>,
}

impl PageTocData {
    pub fn is_empty(&self) -> bool {
        self.candidates.is_empty() && self.metadata_candidates.is_empty()
    }
}

#[derive(Debug, Clone)]
pub struct MetadataLine {
    pub text: String,
    pub bbox: [f32; 4],
    pub height: f32,
    pub confidence: Option<f32>,
}

#[derive(Debug, Clone)]
pub struct MetadataCandidate {
    pub page_index: usize,
    pub title: TocCandidate,
    pub lines: Vec<MetadataLine>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InferredMetadata {
    pub title: Option<String>,
    pub author: Option<String>,
}

/// `LEGE_NO_AUTO_TOC=1` disables synthesis. Preservation of a source outline is
/// unaffected.
pub fn auto_toc_disabled() -> bool {
    static DISABLED: OnceLock<bool> = OnceLock::new();
    *DISABLED.get_or_init(|| {
        std::env::var("LEGE_NO_AUTO_TOC")
            .map(|value| value != "0" && !value.is_empty())
            .unwrap_or(false)
    })
}

// ============================================================================
// Capture
// ============================================================================

fn title_kind(detection: &Detection) -> Option<TitleKind> {
    match detection.class_name.as_deref()? {
        "doc_title" => Some(TitleKind::DocTitle),
        "paragraph_title" => Some(TitleKind::ParagraphTitle),
        _ => None,
    }
}

/// Collect this page's title candidates.
///
/// `detections` must already be in output space, and `hocr` must be in the same
/// page-pixel space, which is what both PDF processing paths hand over.
pub fn capture_page(
    detections: &[Detection],
    hocr: Option<&str>,
    page_index: usize,
    _page_width: u32,
    page_height: u32,
) -> PageTocData {
    if auto_toc_disabled() {
        return PageTocData::default();
    }

    let titles: Vec<(TitleKind, &Detection)> = detections
        .iter()
        .filter_map(|detection| title_kind(detection).map(|kind| (kind, detection)))
        .collect();
    let Some(hocr) = hocr.filter(|text| !text.trim().is_empty()) else {
        return PageTocData::default();
    };
    let Ok(lines) = parse_hocr(hocr) else {
        return PageTocData::default();
    };
    if lines.is_empty() {
        return PageTocData::default();
    }

    let page_height = page_height.max(1) as f32;
    let stats = PageTextStats {
        page_index,
        median_line_height: median(lines.iter().map(|line| line.height).collect()),
    };

    let metadata_lines: Vec<MetadataLine> = lines.iter().map(metadata_line).collect();
    let mut candidates = Vec::new();
    let mut metadata_candidates = Vec::new();
    for (kind, detection) in titles {
        let bbox = detection.bbox;
        let inside: Vec<&HocrLine> = lines
            .iter()
            .filter(|line| line_center_inside(line, bbox))
            .collect();
        if inside.is_empty() {
            continue;
        }

        let mut text = String::new();
        let mut word_confidence: Option<f32> = None;
        for line in &inside {
            for word in &line.words {
                if word.text.trim().is_empty() {
                    continue;
                }
                if !text.is_empty() {
                    text.push(' ');
                }
                text.push_str(word.text.trim());
                if let Some(confidence) = word.confidence {
                    word_confidence =
                        Some(word_confidence.map_or(confidence, |low: f32| low.min(confidence)));
                }
            }
        }
        let text = normalize_title(&text);
        if text.is_empty() {
            continue;
        }

        let line_height = median(inside.iter().map(|line| line.height).collect());
        let bottom = inside
            .iter()
            .map(|line| line.y + line.height)
            .fold(bbox[1], f32::max);
        let next_top = lines
            .iter()
            .filter(|line| line.y > bottom + 1.0)
            .map(|line| line.y)
            .fold(page_height, f32::min);

        let candidate = TocCandidate {
            page_index,
            kind,
            confidence: detection.confidence,
            bbox,
            text,
            line_height,
            page_height,
            gap_below: (next_top - bottom).max(0.0),
            word_confidence,
        };
        if kind == TitleKind::DocTitle {
            metadata_candidates.push(MetadataCandidate {
                page_index,
                title: candidate.clone(),
                lines: metadata_lines.clone(),
            });
        }
        candidates.push(candidate);
    }

    let printed_contents = detections
        .iter()
        .filter(|detection| detection.class_name.as_deref() == Some("content"))
        .flat_map(|detection| {
            lines
                .iter()
                .filter(move |line| line_center_inside(line, detection.bbox))
                .map(line_text)
        })
        .filter(|text| !text.is_empty())
        .collect();

    PageTocData {
        candidates,
        stats: Some(stats),
        metadata_candidates,
        printed_contents,
    }
}

fn line_text(line: &HocrLine) -> String {
    line.raw_text
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(normalize_title)
        .unwrap_or_else(|| {
            normalize_title(
                &line
                    .words
                    .iter()
                    .map(|word| word.text.as_str())
                    .collect::<Vec<_>>()
                    .join(" "),
            )
        })
}

fn metadata_line(line: &HocrLine) -> MetadataLine {
    MetadataLine {
        text: line_text(line),
        bbox: [line.x, line.y, line.x + line.width, line.y + line.height],
        height: line.height,
        confidence: line
            .words
            .iter()
            .filter_map(|word| word.confidence)
            .min_by(f32::total_cmp),
    }
}

fn line_center_inside(line: &HocrLine, bbox: [f32; 4]) -> bool {
    let cx = line.x + line.width * 0.5;
    let cy = line.y + line.height * 0.5;
    // A half-line vertical tolerance: detection boxes are usually a little
    // tighter than the recognized line box.
    let pad = line.height * 0.5;
    cx >= bbox[0] && cx <= bbox[2] && cy >= bbox[1] - pad && cy <= bbox[3] + pad
}

fn normalize_title(text: &str) -> String {
    text.split_whitespace()
        .map(|token| {
            token
                .chars()
                .filter(|ch| !ch.is_control())
                .collect::<String>()
        })
        .filter(|token| !token.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn median(mut values: Vec<f32>) -> f32 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(f32::total_cmp);
    values[values.len() / 2]
}

// ============================================================================
// Verification
// ============================================================================

/// The comparison key for the running-header check: lower case, with a leading
/// or trailing page number removed so a folio-bearing head still collapses onto
/// its own repetitions.
fn running_header_key(text: &str) -> String {
    let mut tokens: Vec<&str> = text.split_whitespace().collect();
    let is_folio = |token: &str| {
        let token = token.trim_matches(|ch: char| !ch.is_alphanumeric());
        !token.is_empty()
            && (token.parse::<u32>().is_ok() || (token.len() <= 6 && roman_value(token).is_some()))
    };
    while tokens.last().is_some_and(|token| is_folio(token)) {
        tokens.pop();
    }
    while tokens.first().is_some_and(|token| is_folio(token)) {
        tokens.remove(0);
    }
    tokens.join(" ").to_lowercase()
}

/// Layout confidence below this never becomes an outline entry. The pipeline
/// default detection threshold is 0.2, which is far too permissive here.
const CONFIDENCE_FLOOR: f32 = 0.5;
/// Recognized text this uncertain would read as mojibake in a reader's
/// navigation panel.
const WORD_CONFIDENCE_FLOOR: f32 = 0.6;
/// Identical text on this many pages is a running header, not a chapter.
const RUNNING_HEADER_PAGES: usize = 3;
/// Score a candidate must reach to become an entry.
const SCORE_THRESHOLD: f32 = 2.0;
/// Longest emitted title, in characters.
const MAX_TITLE_CHARS: usize = 120;
/// A one-entry outline is noise.
const MIN_ENTRIES: usize = 2;
/// Text starting above this fraction of the page is in the running-header band.
const HEADER_BAND_FRACTION: f32 = 0.06;
/// A heading smaller than the body text is not a chapter opening.
const MIN_TITLE_SIZE_RATIO: f32 = 1.02;
/// A number sequence must run this long before it counts as chapter numbering.
const MIN_NUMBER_RUN: usize = 3;
/// …and must step by no more than this, so printed folios cannot pose as one.
const MAX_NUMBER_STEP: u32 = 3;
/// Entries must span at least this percentage of the document.
const MIN_REACH_PERCENT: usize = 20;
/// The reach check is meaningless on a document too short to have front matter.
const MIN_PAGES_FOR_REACH_CHECK: usize = 8;

/// `LEGE_TOC_DEBUG=1` prints every candidate with its score to stderr. The
/// scoring is a heuristic tuned against real scans; without this, a bad outline
/// can only be diagnosed by guessing at which signal misfired.
fn toc_debug() -> bool {
    static DEBUG: OnceLock<bool> = OnceLock::new();
    *DEBUG.get_or_init(|| {
        std::env::var("LEGE_TOC_DEBUG")
            .map(|value| value != "0" && !value.is_empty())
            .unwrap_or(false)
    })
}

/// Build the document outline, or nothing if the evidence is weak.
///
/// `total_pages` is the output page count and bounds the entry density.
pub fn build_outline(
    candidates: Vec<TocCandidate>,
    stats: &[PageTextStats],
    total_pages: usize,
) -> Vec<OutlineItem> {
    build_outline_with_contents(candidates, stats, total_pages, &[])
}

pub fn build_outline_with_contents(
    candidates: Vec<TocCandidate>,
    stats: &[PageTextStats],
    total_pages: usize,
    printed_contents: &[String],
) -> Vec<OutlineItem> {
    if auto_toc_disabled() || candidates.is_empty() || total_pages == 0 {
        return Vec::new();
    }

    let body_height = median(
        stats
            .iter()
            .map(|page| page.median_line_height)
            .filter(|height| *height > 0.0)
            .collect(),
    );
    if body_height <= 0.0 {
        return Vec::new();
    }

    // Hard filters first: they are cheap and they shrink everything after.
    //
    // The size floor is the one gate rather than a score, because a heading set
    // in type smaller than the body is not a chapter opening in any book. Left
    // as a mere penalty it lets the entries of a document's own *printed*
    // contents page — which are small, short, well positioned and full of the
    // word "chapter" — outscore the real thing.
    let mut kept: Vec<TocCandidate> = candidates
        .into_iter()
        .filter(|candidate| candidate.confidence >= CONFIDENCE_FLOOR)
        .filter(|candidate| {
            candidate
                .word_confidence
                .is_none_or(|confidence| confidence >= WORD_CONFIDENCE_FLOOR)
        })
        .filter(|candidate| !candidate.text.is_empty())
        .filter(|candidate| candidate.line_height >= body_height * MIN_TITLE_SIZE_RATIO)
        .collect();
    if kept.is_empty() {
        return Vec::new();
    }

    // Running headers: the same words on three or more pages. The key ignores a
    // leading or trailing page number, because a running head that carries the
    // printed folio ("THE KNEELING TOWER 113") is textually unique on every
    // page and would otherwise survive this check untouched.
    let mut pages_by_text: HashMap<String, Vec<usize>> = HashMap::new();
    for candidate in &kept {
        let pages = pages_by_text
            .entry(running_header_key(&candidate.text))
            .or_default();
        if !pages.contains(&candidate.page_index) {
            pages.push(candidate.page_index);
        }
    }
    kept.retain(|candidate| {
        pages_by_text
            .get(&running_header_key(&candidate.text))
            .map(|pages| pages.len() < RUNNING_HEADER_PAGES)
            .unwrap_or(true)
    });
    if kept.len() < MIN_ENTRIES {
        return Vec::new();
    }

    // A monotonic number sequence is the strongest single signal that these are
    // chapters, so it is computed across the surviving set before scoring.
    let numbers: Vec<Option<u32>> = kept
        .iter()
        .map(|candidate| leading_number(&candidate.text))
        .collect();
    let monotonic = monotonic_positions(&kept, &numbers);

    let mut scored: Vec<(f32, TocCandidate)> = kept
        .into_iter()
        .enumerate()
        .map(|(index, candidate)| {
            let score = score_candidate(
                &candidate,
                body_height,
                numbers[index].is_some(),
                monotonic[index],
                printed_contents
                    .iter()
                    .any(|line| printed_toc_matches(line, &candidate.text)),
            );
            if toc_debug() {
                eprintln!(
                    "[toc] score {score:+.2} page {:>4} h {:.0}/{:.0} top {:.3} gap {:.1} conf {:.2} num {:?} mono {} :: {}",
                    candidate.page_index,
                    candidate.line_height,
                    body_height,
                    candidate.bbox[1] / candidate.page_height.max(1.0),
                    candidate.gap_below / candidate.line_height.max(1.0),
                    candidate.confidence,
                    numbers[index],
                    monotonic[index],
                    candidate.text
                );
            }
            (score, candidate)
        })
        .filter(|(score, _)| *score >= SCORE_THRESHOLD)
        .collect();
    if scored.len() < MIN_ENTRIES {
        return Vec::new();
    }

    // At most one entry per page: the strongest wins.
    scored.sort_by(|a, b| {
        a.1.page_index
            .cmp(&b.1.page_index)
            .then(b.0.total_cmp(&a.0))
    });
    scored.dedup_by(|a, b| a.1.page_index == b.1.page_index);

    // Density: chapters do not start on back-to-back pages. Accept in
    // descending score order and keep only the strongest in each three-page
    // window. This also collapses the cluster of half-titles, subtitles and
    // library stamps that a scanned book carries on its first two leaves.
    let mut by_score = scored.clone();
    by_score.sort_by(|a, b| {
        b.0.total_cmp(&a.0)
            .then(a.1.page_index.cmp(&b.1.page_index))
    });
    let mut accepted: Vec<(f32, TocCandidate)> = Vec::new();
    for entry in by_score {
        if accepted
            .iter()
            .any(|kept| kept.1.page_index.abs_diff(entry.1.page_index) < 3)
        {
            continue;
        }
        accepted.push(entry);
    }
    accepted.sort_by_key(|entry| entry.1.page_index);
    scored = accepted;

    if scored.len() < MIN_ENTRIES {
        return Vec::new();
    }

    // Reach: a table of contents describes the whole document. Entries huddled
    // in the front matter are a title page, not a chapter list, and shipping
    // them would be worse than shipping nothing.
    let first = scored.first().map(|entry| entry.1.page_index).unwrap_or(0);
    let last = scored.last().map(|entry| entry.1.page_index).unwrap_or(0);
    if total_pages >= MIN_PAGES_FOR_REACH_CHECK
        && (last - first) * 100 < total_pages * MIN_REACH_PERCENT
    {
        if toc_debug() {
            eprintln!(
                "[toc] rejected: {} entries span pages {first}..{last} of {total_pages}, \
                 under the {MIN_REACH_PERCENT}% reach floor",
                scored.len()
            );
        }
        return Vec::new();
    }

    build_levels(scored)
}

fn score_candidate(
    candidate: &TocCandidate,
    body_height: f32,
    has_number: bool,
    monotonic: bool,
    printed_match: bool,
) -> f32 {
    let mut score = 0.0f32;

    // Relative size against the document body median. Chapter heads in real
    // books are reliably taller; a same-size "title" is usually a running head
    // or an emphasized body line.
    let ratio = candidate.line_height / body_height;
    score += if ratio >= 1.6 {
        2.0
    } else if ratio >= 1.35 {
        1.5
    } else if ratio >= 1.15 {
        0.75
    } else {
        // Between the size floor and 1.15: allowed through, but earns nothing.
        0.0
    };

    // Position on the page. A chapter opening sits below the header band with
    // air above it; text jammed against the top edge is the running head, so
    // that band is a penalty rather than the boost it first looks like.
    let top_fraction = candidate.bbox[1] / candidate.page_height.max(1.0);
    if top_fraction < HEADER_BAND_FRACTION {
        score -= 1.5;
    } else if top_fraction <= 0.40 {
        score += 0.75;
    } else if top_fraction <= 0.60 {
        score += 0.25;
    }

    // Air below the title. A chapter opening is followed by several blank
    // lines; a running head is followed immediately by body text.
    if candidate.gap_below >= candidate.line_height * 3.0 {
        score += 1.0;
    } else if candidate.gap_below >= candidate.line_height * 1.5 {
        score += 0.5;
    }

    if candidate.kind == TitleKind::DocTitle {
        score += 1.0;
    }

    // Text shape: boosts only, so the scoring stays language-neutral.
    let words = candidate.text.split_whitespace().count();
    if words <= 12 {
        score += 0.5;
    }
    if words > 20 {
        score -= 1.0;
    }
    if has_chapter_pattern(&candidate.text) {
        score += 1.0;
    } else if has_number {
        score += 0.25;
    }
    if monotonic {
        score += 1.5;
    } else if has_number {
        score -= 1.0;
    }
    if is_heading_case(&candidate.text) {
        score += 0.25;
    }
    if printed_match {
        score += 0.75;
    }

    // Detection confidence above the floor, worth at most half a point.
    score += ((candidate.confidence - CONFIDENCE_FLOOR) / (1.0 - CONFIDENCE_FLOOR)).clamp(0.0, 1.0)
        * 0.5;

    score
}

fn printed_toc_matches(line: &str, title: &str) -> bool {
    let normalize = |text: &str| {
        let mut words = text
            .split_whitespace()
            .map(|word| {
                word.trim_matches(|ch: char| !ch.is_alphanumeric())
                    .to_lowercase()
            })
            .filter(|word| !word.is_empty())
            .collect::<Vec<_>>();
        while words.last().is_some_and(|word| word.parse::<u32>().is_ok()) {
            words.pop();
        }
        words
    };
    let line = normalize(line);
    let title = normalize(title);
    if title.is_empty() || line.is_empty() {
        return false;
    }
    if line == title {
        return true;
    }
    let common = title.iter().filter(|word| line.contains(word)).count();
    common * 4 >= title.len() * 3
}

/// Mark candidates that belong to a real chapter-number sequence.
///
/// "Real" means a run of at least [`MIN_NUMBER_RUN`] numbers that increase in
/// page order by no more than [`MAX_NUMBER_STEP`] each. Both conditions matter:
/// any two increasing numbers can be found by accident, and a running head that
/// carries the printed folio increases monotonically down the whole book — in
/// strides far longer than a chapter list takes.
fn monotonic_positions(candidates: &[TocCandidate], numbers: &[Option<u32>]) -> Vec<bool> {
    let mut order: Vec<usize> = (0..candidates.len())
        .filter(|index| numbers[*index].is_some())
        .collect();
    order.sort_by_key(|index| candidates[*index].page_index);

    let mut monotonic = vec![false; candidates.len()];
    let mut run: Vec<usize> = Vec::new();
    let mut flush = |run: &mut Vec<usize>, monotonic: &mut Vec<bool>| {
        if run.len() >= MIN_NUMBER_RUN {
            for index in run.iter() {
                monotonic[*index] = true;
            }
        }
        run.clear();
    };

    for index in order {
        let number = numbers[index].expect("filtered to numbered candidates");
        let continues = run.last().is_some_and(|previous| {
            let previous_number = numbers[*previous].expect("run holds numbered candidates");
            number > previous_number && number - previous_number <= MAX_NUMBER_STEP
        });
        if !continues {
            flush(&mut run, &mut monotonic);
        }
        run.push(index);
    }
    flush(&mut run, &mut monotonic);
    monotonic
}

/// Chapter/part keywords in several languages, plus a section sign. The number
/// itself is optional here; [`leading_number`] scores that separately.
const CHAPTER_WORDS: &[&str] = &[
    "chapter",
    "part",
    "book",
    "section",
    "kapitel",
    "teil",
    "abschnitt",
    "chapitre",
    "partie",
    "capitolo",
    "capitulo",
    "capítulo",
    "parte",
    "hoofdstuk",
    "rozdzial",
    "rozdział",
    "глава",
    "часть",
];

fn has_chapter_pattern(text: &str) -> bool {
    let lower = text.to_lowercase();
    let first = lower.split_whitespace().next().unwrap_or_default();
    let first = first.trim_matches(|ch: char| !ch.is_alphanumeric() && ch != '§');
    if first.starts_with('§') {
        return true;
    }
    if CHAPTER_WORDS.contains(&first) {
        return true;
    }
    // A bare numeral or Roman numeral standing alone at the front.
    lower
        .split_whitespace()
        .next()
        .map(|token| {
            let token = token.trim_matches(|ch: char| !ch.is_alphanumeric());
            !token.is_empty() && (token.parse::<u32>().is_ok() || roman_value(token).is_some())
        })
        .unwrap_or(false)
}

/// The chapter number a title carries, if any: `Chapter IV`, `4.`, `IV`, `§ 4`.
fn leading_number(text: &str) -> Option<u32> {
    let lower = text.to_lowercase();
    let mut tokens = lower
        .split_whitespace()
        .map(|token| token.trim_matches(|ch: char| !ch.is_alphanumeric()));
    let first = tokens.next()?;
    let candidate = if CHAPTER_WORDS.contains(&first) || first.is_empty() {
        tokens.next()?
    } else {
        first
    };
    if candidate.is_empty() {
        return None;
    }
    if let Ok(number) = candidate.parse::<u32>() {
        return (number > 0 && number < 2000).then_some(number);
    }
    roman_value(candidate)
}

fn roman_value(token: &str) -> Option<u32> {
    if token.is_empty() {
        return None;
    }
    let mut total = 0u32;
    let mut previous = 0u32;
    for ch in token.chars().rev() {
        let value = match ch {
            'i' | 'I' => 1,
            'v' | 'V' => 5,
            'x' | 'X' => 10,
            'l' | 'L' => 50,
            'c' | 'C' => 100,
            'd' | 'D' => 500,
            'm' | 'M' => 1000,
            _ => return None,
        };
        if value < previous {
            total -= value;
        } else {
            total += value;
            previous = value;
        }
    }
    (total > 0).then_some(total)
}

fn is_heading_case(text: &str) -> bool {
    let letters: Vec<char> = text.chars().filter(|ch| ch.is_alphabetic()).collect();
    if letters.is_empty() {
        return false;
    }
    let upper = letters.iter().filter(|ch| ch.is_uppercase()).count();
    upper * 2 >= letters.len()
}

/// Two levels at most: the largest line-height cluster is level 0, a clearly
/// smaller cluster becomes children of the preceding level-0 entry. Deeper
/// synthetic trees are guesswork.
fn build_levels(scored: Vec<(f32, TocCandidate)>) -> Vec<OutlineItem> {
    let heights: Vec<f32> = scored.iter().map(|entry| entry.1.line_height).collect();
    let max = heights.iter().copied().fold(f32::MIN, f32::max);
    let min = heights.iter().copied().fold(f32::MAX, f32::min);
    // Only split when the spread is real. Otherwise everything is one level.
    let split = (min > 0.0 && max / min >= 1.25).then(|| (max + min) * 0.5);

    let mut items: Vec<OutlineItem> = Vec::new();
    for (_, candidate) in scored {
        let is_child = match split {
            Some(threshold) => {
                candidate.kind != TitleKind::DocTitle && candidate.line_height < threshold
            }
            None => false,
        };
        let item = OutlineItem {
            title: truncate_title(&candidate.text),
            page_index: candidate.page_index as u32,
            top: Some(destination_top(&candidate)),
            children: Vec::new(),
        };
        match items.last_mut() {
            Some(parent) if is_child => parent.children.push(item),
            _ => items.push(item),
        }
    }
    if items.len() < MIN_ENTRIES {
        return Vec::new();
    }
    items
}

/// The destination Y in PDF user space. Output pages are written with a 1:1
/// pixel-to-point box, so the flip is the page height minus the box top, with a
/// little air above the title.
fn destination_top(candidate: &TocCandidate) -> f32 {
    let pad = (candidate.line_height * 0.5).min(24.0);
    (candidate.page_height - candidate.bbox[1] + pad).clamp(0.0, candidate.page_height)
}

fn truncate_title(text: &str) -> String {
    if text.chars().count() <= MAX_TITLE_CHARS {
        return text.to_string();
    }
    let cut: String = text.chars().take(MAX_TITLE_CHARS).collect();
    match cut.rfind(' ') {
        Some(space) if space > MAX_TITLE_CHARS / 2 => cut[..space].to_string(),
        _ => cut,
    }
}

// ============================================================================
// Conservative title / author inference
// ============================================================================

pub fn infer_metadata(
    candidates: &[MetadataCandidate],
    stats: &[PageTextStats],
    total_pages: usize,
) -> InferredMetadata {
    if candidates.is_empty() || total_pages == 0 {
        return InferredMetadata::default();
    }
    let body_height = median(
        stats
            .iter()
            .map(|page| page.median_line_height)
            .filter(|height| *height > 0.0)
            .collect(),
    );
    if body_height <= 0.0 {
        return InferredMetadata::default();
    }
    let front_pages = (((total_pages + 9) / 10).clamp(3, 10)).min(total_pages);
    let mut ranked = candidates
        .iter()
        .filter(|candidate| candidate.page_index < front_pages)
        .filter(|candidate| candidate.title.confidence >= 0.70)
        .filter(|candidate| {
            candidate
                .title
                .word_confidence
                .is_none_or(|confidence| confidence >= 0.75)
        })
        .filter(|candidate| candidate.title.line_height >= body_height * 1.35)
        .filter(|candidate| credible_inferred_title(&candidate.title.text))
        .map(|candidate| {
            let title = &candidate.title;
            let size_score = (title.line_height / body_height).min(3.0);
            let position = title.bbox[1] / title.page_height.max(1.0);
            let position_score = if position <= 0.55 { 1.0 } else { 0.0 };
            let score = size_score + position_score + title.confidence;
            (score, candidate)
        })
        .collect::<Vec<_>>();
    ranked.sort_by(|a, b| b.0.total_cmp(&a.0));
    let Some((best_score, best)) = ranked.first().copied() else {
        return InferredMetadata::default();
    };
    if ranked.get(1).is_some_and(|(runner_score, runner)| {
        best_score - *runner_score < 0.75
            && normalize_title(&runner.title.text).to_lowercase()
                != normalize_title(&best.title.text).to_lowercase()
    }) {
        return InferredMetadata::default();
    }

    InferredMetadata {
        title: Some(truncate_title(&best.title.text)),
        author: infer_author(best),
    }
}

fn credible_inferred_title(text: &str) -> bool {
    let text = normalize_title(text);
    let lower = text.to_lowercase();
    let words = text.split_whitespace().count();
    text.chars().count() >= 3
        && text.chars().count() <= 160
        && words <= 20
        && !matches!(lower.as_str(), "document" | "untitled" | "unknown")
        && !has_chapter_pattern(&text)
}

fn infer_author(candidate: &MetadataCandidate) -> Option<String> {
    let title = &candidate.title;
    let mut lines = candidate
        .lines
        .iter()
        .filter(|line| !line.text.is_empty())
        .filter(|line| line.confidence.is_none_or(|confidence| confidence >= 0.75))
        .filter(|line| {
            line.bbox[3] >= title.bbox[1] - title.page_height * 0.12
                && line.bbox[1] <= title.bbox[3] + title.page_height * 0.35
        })
        .collect::<Vec<_>>();
    lines.sort_by(|a, b| a.bbox[1].total_cmp(&b.bbox[1]));

    let mut explicit = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if let Some(after) = strip_author_cue(&line.text) {
            if !after.is_empty() && author_name_like(after) {
                explicit.push(normalize_title(after));
            } else if let Some(next) = lines.get(index + 1)
                && next.height < title.line_height * 0.9
                && author_name_like(&next.text)
            {
                explicit.push(normalize_title(&next.text));
            }
        }
    }
    explicit.sort();
    explicit.dedup();
    if explicit.len() == 1 {
        return explicit.pop();
    }
    if !explicit.is_empty() {
        return None;
    }

    let title_center = (title.bbox[0] + title.bbox[2]) * 0.5;
    let mut implicit = lines
        .into_iter()
        .filter(|line| line.bbox[1] >= title.bbox[3])
        .filter(|line| line.bbox[1] - title.bbox[3] <= title.page_height * 0.18)
        .filter(|line| line.height >= title.line_height * 0.35)
        .filter(|line| line.height <= title.line_height * 0.85)
        .filter(|line| line.confidence.is_none_or(|confidence| confidence >= 0.85))
        .filter(|line| {
            let center = (line.bbox[0] + line.bbox[2]) * 0.5;
            (center - title_center).abs() <= title.page_height * 0.15
        })
        .filter(|line| author_name_like(&line.text))
        .map(|line| normalize_title(&line.text))
        .collect::<Vec<_>>();
    implicit.sort();
    implicit.dedup();
    (implicit.len() == 1).then(|| implicit.remove(0))
}

fn strip_author_cue(text: &str) -> Option<&str> {
    let trimmed = text.trim();
    let lower = trimmed.to_lowercase();
    for cue in ["written by", "authored by", "by"] {
        if lower == cue {
            return Some("");
        }
        if lower.starts_with(cue)
            && lower
                .as_bytes()
                .get(cue.len())
                .is_some_and(|byte| byte.is_ascii_whitespace() || matches!(*byte, b':' | b'-'))
        {
            return Some(trimmed[cue.len()..].trim_matches([' ', ':', '-', '—']));
        }
    }
    None
}

fn author_name_like(text: &str) -> bool {
    let text = normalize_title(text);
    let words = text.split_whitespace().collect::<Vec<_>>();
    if words.is_empty() || words.len() > 8 {
        return false;
    }
    let lower = text.to_lowercase();
    if [
        "press",
        "publisher",
        "publishing",
        "edition",
        "volume",
        "university",
        "copyright",
        "isbn",
    ]
    .iter()
    .any(|word| lower.split_whitespace().any(|token| token.contains(word)))
        || lower.chars().filter(|ch| ch.is_ascii_digit()).count() >= 3
    {
        return false;
    }
    let alphabetic_words = words
        .iter()
        .filter(|word| word.chars().any(char::is_alphabetic))
        .count();
    alphabetic_words >= 2
        && text.chars().all(|ch| {
            ch.is_alphabetic()
                || ch.is_whitespace()
                || matches!(ch, '.' | ',' | '-' | '\'' | '’' | '&')
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(page: usize, text: &str, line_height: f32) -> TocCandidate {
        TocCandidate {
            page_index: page,
            kind: TitleKind::ParagraphTitle,
            confidence: 0.9,
            bbox: [100.0, 120.0, 700.0, 120.0 + line_height],
            text: text.to_string(),
            line_height,
            page_height: 1200.0,
            gap_below: line_height * 2.0,
            word_confidence: None,
        }
    }

    fn body_stats(pages: usize, height: f32) -> Vec<PageTextStats> {
        (0..pages)
            .map(|page_index| PageTextStats {
                page_index,
                median_line_height: height,
            })
            .collect()
    }

    fn metadata_candidate(title: &str, lines: &[(&str, f32, f32)]) -> MetadataCandidate {
        MetadataCandidate {
            page_index: 0,
            title: TocCandidate {
                kind: TitleKind::DocTitle,
                confidence: 0.94,
                bbox: [100.0, 120.0, 700.0, 190.0],
                text: title.to_string(),
                line_height: 64.0,
                ..candidate(0, title, 64.0)
            },
            lines: lines
                .iter()
                .map(|(text, y, height)| MetadataLine {
                    text: (*text).to_string(),
                    bbox: [180.0, *y, 620.0, *y + *height],
                    height: *height,
                    confidence: Some(0.96),
                })
                .collect(),
        }
    }

    #[test]
    fn conservative_metadata_accepts_an_explicit_written_by_line() {
        let candidate = metadata_candidate(
            "The Long Road Home",
            &[
                ("The Long Road Home", 130.0, 64.0),
                ("Written by Ada Lovelace", 230.0, 30.0),
            ],
        );
        let inferred = infer_metadata(&[candidate], &body_stats(20, 28.0), 20);
        assert_eq!(inferred.title.as_deref(), Some("The Long Road Home"));
        assert_eq!(inferred.author.as_deref(), Some("Ada Lovelace"));
    }

    #[test]
    fn ambiguous_cover_titles_produce_no_metadata() {
        let first = metadata_candidate("Collected Essays", &[]);
        let mut second = metadata_candidate("Selected Essays", &[]);
        second.title.bbox[1] = 125.0;
        assert_eq!(
            infer_metadata(&[first, second], &body_stats(20, 28.0), 20),
            InferredMetadata::default()
        );
    }

    #[test]
    fn publisher_copy_is_not_mistaken_for_an_author() {
        let candidate = metadata_candidate(
            "A Natural History of Islands",
            &[
                ("A Natural History of Islands", 130.0, 64.0),
                ("Oxford University Press 2024", 230.0, 28.0),
            ],
        );
        let inferred = infer_metadata(&[candidate], &body_stats(20, 28.0), 20);
        assert_eq!(
            inferred.title.as_deref(),
            Some("A Natural History of Islands")
        );
        assert_eq!(inferred.author, None);
    }

    #[test]
    fn a_clean_chapter_book_gets_its_chapters() {
        let candidates = vec![
            candidate(0, "Chapter I The Road South", 40.0),
            candidate(20, "Chapter II Winter Quarters", 40.0),
            candidate(41, "Chapter III The Long Retreat", 40.0),
        ];
        let outline = build_outline(candidates, &body_stats(60, 24.0), 60);
        assert_eq!(outline.len(), 3);
        assert_eq!(outline[0].title, "Chapter I The Road South");
        assert_eq!(outline[2].page_index, 41);
        assert!(outline[0].top.is_some(), "synthesized entries carry /XYZ");
    }

    #[test]
    fn a_running_header_produces_nothing() {
        let candidates = (0..8)
            .map(|page| candidate(page * 2, "A History of the Crusades", 26.0))
            .collect();
        assert!(build_outline(candidates, &body_stats(20, 24.0), 20).is_empty());
    }

    #[test]
    fn a_running_header_carrying_the_printed_folio_is_still_a_running_header() {
        // Real scans put the printed page number next to the running head, so
        // the text is unique on every page even though the head is not.
        let candidates = (0..6)
            .map(|n| {
                let page = 4 + n * 2;
                TocCandidate {
                    // In the header band, right against the top edge.
                    bbox: [100.0, 24.0, 700.0, 66.0],
                    gap_below: 60.0,
                    ..candidate(page, &format!("THE KNEELING TOWER {}", 90 + page), 42.0)
                }
            })
            .collect();
        assert!(build_outline(candidates, &body_stats(40, 30.0), 40).is_empty());
    }

    #[test]
    fn folio_stripping_only_removes_numbers_at_the_ends() {
        assert_eq!(
            running_header_key("THE KNEELING TOWER 113"),
            "the kneeling tower"
        );
        assert_eq!(
            running_header_key("113 THE KNEELING TOWER"),
            "the kneeling tower"
        );
        assert_eq!(
            running_header_key("Chapter 4 The Siege"),
            "chapter 4 the siege"
        );
        assert_eq!(running_header_key("xiv Preface"), "preface");
    }

    #[test]
    fn one_surviving_candidate_produces_nothing() {
        let candidates = vec![
            candidate(3, "Chapter I Beginnings", 40.0),
            // Same size as the body and low confidence: scored away.
            TocCandidate {
                confidence: 0.25,
                ..candidate(9, "a stray emphasized line", 24.0)
            },
        ];
        assert!(build_outline(candidates, &body_stats(40, 24.0), 40).is_empty());
    }

    #[test]
    fn same_size_as_body_text_produces_nothing() {
        let candidates = vec![
            candidate(2, "introductory remarks", 24.0),
            candidate(9, "further remarks", 24.0),
            candidate(15, "closing remarks", 24.0),
        ];
        assert!(build_outline(candidates, &body_stats(40, 24.0), 40).is_empty());
    }

    #[test]
    fn a_second_size_cluster_nests_one_level_deep() {
        let candidates = vec![
            TocCandidate {
                kind: TitleKind::DocTitle,
                ..candidate(0, "The Rise and Fall", 60.0)
            },
            candidate(4, "Chapter I Origins", 44.0),
            candidate(9, "The Early Years", 28.0),
            candidate(14, "Chapter II Conflict", 44.0),
        ];
        let outline = build_outline(candidates, &body_stats(40, 24.0), 40);
        assert_eq!(outline.len(), 3, "three top-level entries");
        assert_eq!(outline[1].children.len(), 1, "the small title nests");
        assert_eq!(outline[1].children[0].title, "The Early Years");
        assert!(outline[2].children.is_empty());
    }

    #[test]
    fn density_is_capped_on_a_short_document() {
        let candidates = (0..10)
            .map(|page| candidate(page, &format!("Chapter {} Something", page + 1), 40.0))
            .collect();
        let outline = build_outline(candidates, &body_stats(10, 24.0), 10);
        assert!(
            outline.len() <= 4,
            "ten entries in ten pages is not a chapter list, got {}",
            outline.len()
        );
    }

    #[test]
    fn low_word_confidence_titles_are_dropped() {
        let candidates = vec![
            TocCandidate {
                word_confidence: Some(0.2),
                ..candidate(0, "Chapter I Th3 R0ad S0uth", 40.0)
            },
            TocCandidate {
                word_confidence: Some(0.15),
                ..candidate(20, "Chapter II W1nter", 40.0)
            },
        ];
        assert!(build_outline(candidates, &body_stats(60, 24.0), 60).is_empty());
    }

    #[test]
    fn a_documents_own_printed_contents_page_is_not_the_outline() {
        // Entries on a book's printed contents leaf are short, well positioned,
        // and start with the word "chapter" — but they are set smaller than the
        // body, which is what tells them apart from a real chapter opening.
        let candidates = vec![
            candidate(20, "CHAPTER II.", 22.0),
            candidate(25, "PART IV.", 22.0),
            candidate(29, "APPENDIX D.", 20.0),
            candidate(36, "CHAPTER I.", 23.0),
        ];
        assert!(build_outline(candidates, &body_stats(200, 26.0), 200).is_empty());
    }

    #[test]
    fn two_increasing_numbers_are_not_a_chapter_sequence() {
        // A folio-bearing running head and a stray heading both carry numbers.
        // Treating any increasing pair as chapter numbering handed a running
        // head a 1.5-point boost it had not earned.
        let candidates = vec![
            TocCandidate {
                bbox: [100.0, 40.0, 700.0, 71.0],
                gap_below: 15.0,
                ..candidate(131, "92 Rise and Fall of the Confederate Government", 31.0)
            },
            candidate(25, "Part IV", 30.0),
        ];
        let numbers: Vec<Option<u32>> = candidates
            .iter()
            .map(|candidate| leading_number(&candidate.text))
            .collect();
        assert_eq!(numbers, vec![Some(92), Some(4)]);
        assert_eq!(
            monotonic_positions(&candidates, &numbers),
            vec![false, false]
        );

        // Three chapters in a row, stepping by one, is a sequence.
        let chapters = vec![
            candidate(4, "Chapter 1 Origins", 40.0),
            candidate(20, "Chapter 2 Conflict", 40.0),
            candidate(38, "Chapter 3 Aftermath", 40.0),
        ];
        let numbers: Vec<Option<u32>> = chapters
            .iter()
            .map(|candidate| leading_number(&candidate.text))
            .collect();
        assert_eq!(monotonic_positions(&chapters, &numbers), vec![true; 3]);
    }

    #[test]
    fn front_matter_on_the_first_leaves_is_not_a_table_of_contents() {
        // A scanned book's title page carries a half-title, a subtitle and a
        // library stamp — several strong-looking titles two pages apart.
        let candidates = vec![
            TocCandidate {
                kind: TitleKind::DocTitle,
                ..candidate(3, "THE CRUSADES", 65.0)
            },
            candidate(4, "The Flame of Islam", 68.0),
            candidate(5, "Theology Library School of Theology at Claremont", 42.0),
        ];
        assert!(build_outline(candidates, &body_stats(120, 28.0), 120).is_empty());
    }

    #[test]
    fn titles_are_truncated_at_a_word_boundary() {
        let long = "Chapter I ".to_string() + &"word ".repeat(40);
        let truncated = truncate_title(&long);
        assert!(truncated.chars().count() <= MAX_TITLE_CHARS);
        assert!(!truncated.ends_with(' '));
    }

    #[test]
    fn chapter_numbers_parse_in_arabic_and_roman_forms() {
        assert_eq!(leading_number("Chapter IV The Siege"), Some(4));
        assert_eq!(leading_number("4. The Siege"), Some(4));
        assert_eq!(leading_number("XII"), Some(12));
        assert_eq!(leading_number("Kapitel 7 Der Weg"), Some(7));
        assert_eq!(leading_number("The Siege"), None);
    }

    #[test]
    fn capture_reads_titles_and_confidences_out_of_hocr() {
        let hocr = r#"<div class="ocr_page" title="bbox 0 0 800 1200">
<span class="ocr_line" title="bbox 100 100 700 150"><span class="ocrx_word" title="bbox 100 100 300 150; x_wconf 91">Chapter</span> <span class="ocrx_word" title="bbox 320 100 400 150; x_wconf 88">I</span></span>
<span class="ocr_line" title="bbox 100 400 700 424"><span class="ocrx_word" title="bbox 100 400 700 424; x_wconf 95">body</span></span>
</div>"#;
        let detection = Detection {
            class_id: 11,
            class_name: Some("doc_title".to_string()),
            confidence: 0.8,
            bbox: [90.0, 90.0, 720.0, 160.0],
            category: crate::types::ContentCategory::Text,
            context: None,
        };
        let page = capture_page(&[detection], Some(hocr), 3, 800, 1200);
        assert_eq!(page.candidates.len(), 1);
        let candidate = &page.candidates[0];
        assert_eq!(candidate.text, "Chapter I");
        assert_eq!(candidate.kind, TitleKind::DocTitle);
        assert_eq!(candidate.page_index, 3);
        assert!((candidate.word_confidence.unwrap() - 0.88).abs() < 1e-3);
        assert!(candidate.line_height > 40.0, "title line is the tall one");
        assert!(candidate.gap_below > 200.0, "air above the body line");
        assert!(page.stats.is_some());
    }

    #[test]
    fn a_page_without_a_title_detection_costs_nothing() {
        let detection = Detection {
            class_id: 2,
            class_name: Some("text".to_string()),
            confidence: 0.9,
            bbox: [0.0, 0.0, 100.0, 100.0],
            category: crate::types::ContentCategory::Text,
            context: None,
        };
        let page = capture_page(&[detection], Some("<div>ignored</div>"), 0, 800, 1200);
        assert!(page.is_empty());
        assert!(page.stats.is_none());
    }
}
