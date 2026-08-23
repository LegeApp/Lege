//! hOCR parsing and line grouping, independent of any PDF writer.
//!
//! Moved out of `accumulator.rs` during the lopdf → lege-pdf-write migration so
//! it no longer lives next to a PDF builder. Consumers: the EPUB text-sidecar
//! path (`pipeline::epub_pipeline`) and the PDF text-layer adapter
//! (`pdf_artifact`). `src/djvu.rs` keeps its own independent parser.

use anyhow::Result;
use quick_xml::events::{BytesStart, Event};
use quick_xml::{Reader, XmlVersion};

/// A word from hOCR output with its pixel bounding box.
#[derive(Debug, Clone)]
pub struct HocrWord {
    pub text: String,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    /// Recognition confidence in 0..=1, from the hOCR `x_wconf` property.
    /// `None` when the producer emitted no confidence, which is the case for
    /// native PDF text: absent means "not scored", never "scored low".
    pub confidence: Option<f32>,
}

/// A line of hOCR text composed of words.
#[derive(Debug, Clone)]
pub struct HocrLine {
    pub words: Vec<HocrWord>,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub baseline: f32,
    /// Average width of single-character tokens, used by spacing heuristics.
    pub avg_char_width: f32,
    /// Optional raw line text (from the OCR engine) for authoritative spacing.
    pub raw_text: Option<String>,
}

/// Read the `x_wconf` property out of an hOCR `title` attribute and return it
/// as a 0..=1 fraction. The property is written as a percentage
/// (`bbox 1 2 3 4; x_wconf 87`), which is what `lege_ocr::hocr` emits.
fn parse_word_confidence(title: &str) -> Option<f32> {
    let start = title.find("x_wconf")? + "x_wconf".len();
    let value = title[start..]
        .split(';')
        .next()?
        .split_whitespace()
        .next()?
        .parse::<f32>()
        .ok()?;
    Some((value / 100.0).clamp(0.0, 1.0))
}

/// Read the `bbox x1 y1 x2 y2` prefix of an hOCR `title` attribute. Any further
/// properties (`; x_wconf 87`, `; baseline …`) are ignored here.
fn parse_bbox(title: &str) -> Option<(f32, f32, f32, f32)> {
    let after_bbox = title.strip_prefix("bbox ")?;
    let bbox_part = after_bbox.split(';').next().unwrap_or(after_bbox);
    let mut nums = bbox_part
        .split_whitespace()
        .filter_map(|s| s.parse::<f32>().ok());
    Some((nums.next()?, nums.next()?, nums.next()?, nums.next()?))
}

/// The hOCR attributes we care about on a `<span>`: which class it carries, and
/// the geometry packed into its `title`.
#[derive(Default)]
struct SpanAttrs {
    is_word: bool,
    is_line: bool,
    bbox: Option<(f32, f32, f32, f32)>,
    confidence: Option<f32>,
}

fn parse_span_attrs(span: &BytesStart) -> Result<SpanAttrs> {
    let mut attrs = SpanAttrs::default();
    for attr in span.attributes() {
        let attr = attr?;
        match attr.key.as_ref() {
            b"class" => {
                for class in attr.value.as_ref().split(|&b| b == b' ') {
                    match class {
                        b"ocrx_word" => attrs.is_word = true,
                        b"ocr_line" => attrs.is_line = true,
                        _ => {}
                    }
                }
            }
            b"title" => {
                if let Ok(title) = attr.normalized_value(XmlVersion::Implicit1_0) {
                    attrs.bbox = parse_bbox(&title);
                    attrs.confidence = parse_word_confidence(&title);
                }
            }
            _ => {}
        }
    }
    Ok(attrs)
}

pub fn parse_hocr(hocr: &str) -> Result<Vec<HocrLine>> {
    let mut reader = Reader::from_str(hocr);

    let mut words = Vec::new();
    let mut raw_lines: Vec<(f32, f32, f32, f32, String)> = Vec::new();
    let mut buf = Vec::new();
    let mut current_word_text = String::new();
    let mut current_line_text = String::new();
    let mut in_ocr_word = false;
    let mut in_ocr_line = false;
    let mut current_index: Option<usize> = None;
    let mut current_line_bbox: Option<(f32, f32, f32, f32)> = None;

    loop {
        match reader.read_event_into(&mut buf)? {
            Event::Start(ref e) if e.name().as_ref() == b"span" => {
                let attrs = parse_span_attrs(e)?;

                // A span without a usable bbox is skipped entirely: its text
                // must not be attributed to whatever span preceded it.
                if attrs.is_word {
                    match attrs.bbox {
                        Some((x1, y1, x2, y2)) => {
                            in_ocr_word = true;
                            current_word_text.clear();
                            words.push(HocrWord {
                                text: String::new(),
                                confidence: attrs.confidence,
                                x: x1,
                                y: y1,
                                width: (x2 - x1).max(0.0),
                                height: (y2 - y1).max(0.0),
                            });
                            current_index = Some(words.len() - 1);
                        }
                        None => {
                            in_ocr_word = false;
                            current_index = None;
                        }
                    }
                } else if attrs.is_line {
                    match attrs.bbox {
                        Some(bbox) => {
                            in_ocr_line = true;
                            current_line_bbox = Some(bbox);
                            current_line_text.clear();
                        }
                        None => {
                            in_ocr_line = false;
                            current_line_bbox = None;
                        }
                    }
                }
            }
            Event::Text(e) => {
                if let Ok(s) = std::str::from_utf8(e.as_ref()) {
                    if in_ocr_word {
                        current_word_text.push_str(s);
                    }
                    if in_ocr_line {
                        current_line_text.push_str(s);
                    }
                }
            }
            Event::End(ref e) if e.name().as_ref() == b"span" => {
                if in_ocr_word {
                    if let Some(word) = current_index.and_then(|idx| words.get_mut(idx)) {
                        word.text = current_word_text.trim().to_string();
                    }
                    in_ocr_word = false;
                    current_index = None;
                    current_word_text.clear();
                } else if in_ocr_line {
                    if let Some((x1, y1, x2, y2)) = current_line_bbox.take() {
                        let txt = current_line_text.trim().to_string();
                        if !txt.is_empty() {
                            raw_lines.push((x1, y1, x2, y2, txt));
                        }
                    }
                    in_ocr_line = false;
                    current_line_text.clear();
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }

    words.retain(|w| !w.text.trim().is_empty());

    let mut lines = group_words_into_lines(words);

    // Attach each grouped line's raw engine text by matching bounding boxes:
    // the raw text is authoritative for spacing, which word boxes alone lose.
    for line in lines.iter_mut() {
        let lx1 = line.x;
        let ly1 = line.y;
        let lx2 = line.x + line.width;
        let ly2 = line.y + line.height;
        let l_area = (lx2 - lx1).max(0.0) * (ly2 - ly1).max(0.0);
        let mut best_idx: Option<usize> = None;
        let mut best_score: f32 = 0.0;

        // Score by how much of the grouped line the raw line covers, so a raw
        // line that merely clips the edge of this one cannot claim it.
        for (i, (rx1, ry1, rx2, ry2, _)) in raw_lines.iter().enumerate() {
            let ix = (lx2.min(*rx2) - lx1.max(*rx1)).max(0.0);
            let iy = (ly2.min(*ry2) - ly1.max(*ry1)).max(0.0);
            let inter = ix * iy;
            let score = if l_area > 0.0 { inter / l_area } else { 0.0 };
            if score > best_score {
                best_score = score;
                best_idx = Some(i);
            }
        }

        if best_score > RAW_LINE_MIN_COVERAGE
            && let Some(i) = best_idx
        {
            line.raw_text = Some(raw_lines[i].4.clone());
        }
    }

    Ok(lines)
}

/// Minimum fraction of a grouped line that a raw engine line must cover before
/// its text is taken as that line's authoritative spacing.
const RAW_LINE_MIN_COVERAGE: f32 = 0.2;

pub fn group_words_into_lines(mut words: Vec<HocrWord>) -> Vec<HocrLine> {
    if words.is_empty() {
        return Vec::new();
    }

    words.sort_by(|a, b| a.y.total_cmp(&b.y).then(a.x.total_cmp(&b.x)));

    struct LineGroup {
        words: Vec<HocrWord>,
        min_x: f32,
        max_x: f32,
        min_y: f32,
        max_y: f32,
    }

    let mut groups: Vec<LineGroup> = Vec::new();

    for word in words.into_iter() {
        // A word joins the first group whose vertical midline it sits near;
        // otherwise it opens a new line.
        let word_mid = word.y + word.height * 0.5;
        let target = groups.iter().position(|group| {
            let line_mid = (group.min_y + group.max_y) * 0.5;
            let line_height = (group.max_y - group.min_y).max(word.height).max(1.0);
            (line_mid - word_mid).abs() <= line_height * 0.45
        });

        let idx = match target {
            Some(idx) => idx,
            None => {
                groups.push(LineGroup {
                    min_x: word.x,
                    max_x: word.x + word.width,
                    min_y: word.y,
                    max_y: word.y + word.height,
                    words: Vec::new(),
                });
                groups.len() - 1
            }
        };

        let group = &mut groups[idx];
        group.min_x = group.min_x.min(word.x);
        group.max_x = group.max_x.max(word.x + word.width);
        group.min_y = group.min_y.min(word.y);
        group.max_y = group.max_y.max(word.y + word.height);
        group.words.push(word);
    }

    let mut lines: Vec<HocrLine> = groups
        .into_iter()
        .map(|mut group| {
            group
                .words
                .sort_by(|a, b| a.x.total_cmp(&b.x).then(a.y.total_cmp(&b.y)));

            let mut char_width_sum = 0.0f32;
            let mut char_width_count = 0usize;
            for w in &group.words {
                if w.text.chars().count() == 1 {
                    char_width_sum += w.width;
                    char_width_count += 1;
                }
            }
            let line_height = (group.max_y - group.min_y).max(1.0);
            // With no single-character token to measure, half the line height is
            // a serviceable stand-in for one character's width.
            let avg_char_width = if char_width_count > 0 {
                char_width_sum / (char_width_count as f32)
            } else {
                line_height * 0.5
            };

            HocrLine {
                x: group.min_x,
                y: group.min_y,
                width: (group.max_x - group.min_x).max(0.0),
                height: line_height,
                baseline: group.max_y,
                avg_char_width,
                words: group.words,
                raw_text: None,
            }
        })
        .collect();

    lines.sort_by(|a, b| a.y.total_cmp(&b.y).then(a.x.total_cmp(&b.x)));

    lines
}

/// Remove adjacent repeated words introduced by tiling overlaps (same text,
/// high vertical overlap, small horizontal gap). Per-line.
pub fn dedup_adjacent_repeats(lines: &mut [HocrLine]) {
    for line in lines.iter_mut() {
        let line_height = line.height;
        // `dedup_by` hands us (candidate, previously-kept word) and drops the
        // candidate when the predicate holds — exactly the rule below.
        line.words.dedup_by(|w, prev| {
            let same_text = prev.text.trim() == w.text.trim();
            let gap = w.x - (prev.x + prev.width);
            let v_overlap = ((prev.y + prev.height).min(w.y + w.height) - prev.y.max(w.y)).max(0.0);
            let v_overlap_ratio = v_overlap / prev.height.max(w.height).max(1.0);
            same_text && gap < line_height * 0.25 && v_overlap_ratio > 0.5
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_confidence_is_read_from_x_wconf() {
        let hocr = r#"<span class="ocr_line" title="bbox 0 0 100 20"><span class="ocrx_word" title="bbox 0 0 40 20; x_wconf 93">hello</span> <span class="ocrx_word" title="bbox 50 0 100 20; x_wconf 41">wor1d</span></span>"#;
        let lines = parse_hocr(hocr).expect("parse");
        assert_eq!(lines.len(), 1);
        let words = &lines[0].words;
        assert_eq!(words.len(), 2);
        assert!((words[0].confidence.unwrap() - 0.93).abs() < 1e-6);
        assert!((words[1].confidence.unwrap() - 0.41).abs() < 1e-6);
    }

    #[test]
    fn a_word_without_a_confidence_reports_none() {
        let hocr = r#"<span class="ocr_line" title="bbox 0 0 100 20"><span class="ocrx_word" title="bbox 0 0 40 20">native</span></span>"#;
        let lines = parse_hocr(hocr).expect("parse");
        assert_eq!(lines[0].words.len(), 1);
        assert_eq!(lines[0].words[0].confidence, None);
        // The bbox must still parse when no confidence follows it.
        assert_eq!(lines[0].words[0].width, 40.0);
    }
}
