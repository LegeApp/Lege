//! hOCR parsing and line grouping, independent of any PDF writer.
//!
//! Moved out of `accumulator.rs` during the lopdf → lege-pdf-write migration so
//! it no longer lives next to a PDF builder. Consumers: the EPUB text-sidecar
//! path (`pipeline::epub_pipeline`) and the PDF text-layer adapter
//! (`pdf_artifact`). `src/djvu.rs` keeps its own independent parser.

use anyhow::Result;
use quick_xml::events::Event;
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
                let mut is_word = false;
                let mut is_line = false;
                let mut bbox: Option<(f32, f32, f32, f32)> = None;
                let mut confidence: Option<f32> = None;

                for attr in e.attributes() {
                    let attr = attr?;
                    match attr.key.as_ref() {
                        b"class" => {
                            if attr
                                .value
                                .as_ref()
                                .split(|&b| b == b' ')
                                .any(|c| c == b"ocrx_word")
                            {
                                is_word = true;
                            }
                            if attr
                                .value
                                .as_ref()
                                .split(|&b| b == b' ')
                                .any(|c| c == b"ocr_line")
                            {
                                is_line = true;
                            }
                        }
                        b"title" => {
                            if let Ok(title_str) = attr.normalized_value(XmlVersion::Implicit1_0) {
                                if let Some(after_bbox) = title_str.strip_prefix("bbox ") {
                                    let bbox_part =
                                        after_bbox.split(';').next().unwrap_or(after_bbox);
                                    let mut nums = bbox_part
                                        .split_whitespace()
                                        .filter_map(|s| s.parse::<f32>().ok());
                                    if let (Some(x1), Some(y1), Some(x2), Some(y2)) =
                                        (nums.next(), nums.next(), nums.next(), nums.next())
                                    {
                                        bbox = Some((x1, y1, x2, y2));
                                    }
                                }
                                confidence = parse_word_confidence(&title_str);
                            }
                        }
                        _ => {}
                    }
                }

                if is_word {
                    if let Some((x1, y1, x2, y2)) = bbox {
                        in_ocr_word = true;
                        current_word_text.clear();
                        words.push(HocrWord {
                            text: String::new(),
                            confidence,
                            x: x1,
                            y: y1,
                            width: (x2 - x1).max(0.0),
                            height: (y2 - y1).max(0.0),
                        });
                        current_index = Some(words.len() - 1);
                    } else {
                        in_ocr_word = false;
                        current_index = None;
                    }
                } else if is_line {
                    if let Some((x1, y1, x2, y2)) = bbox {
                        in_ocr_line = true;
                        current_line_bbox = Some((x1, y1, x2, y2));
                        current_line_text.clear();
                    } else {
                        in_ocr_line = false;
                        current_line_bbox = None;
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
                    if let Some(idx) = current_index {
                        if let Some(word) = words.get_mut(idx) {
                            word.text = current_word_text.trim().to_string();
                        }
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

    // Attach raw line text (if present) by matching bounding boxes.
    if !raw_lines.is_empty() {
        for line in lines.iter_mut() {
            let lx1 = line.x;
            let ly1 = line.y;
            let lx2 = line.x + line.width;
            let ly2 = line.y + line.height;
            let l_area = (lx2 - lx1).max(0.0) * (ly2 - ly1).max(0.0);
            let mut best_idx: Option<usize> = None;
            let mut best_score: f32 = 0.0;

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

            if let Some(i) = best_idx.take() {
                if best_score > 0.2 {
                    line.raw_text = Some(raw_lines[i].4.clone());
                }
            }
        }
    }

    Ok(lines)
}

pub fn group_words_into_lines(mut words: Vec<HocrWord>) -> Vec<HocrLine> {
    if words.is_empty() {
        return Vec::new();
    }

    words.sort_by(|a, b| {
        a.y.partial_cmp(&b.y)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.x.partial_cmp(&b.x).unwrap_or(std::cmp::Ordering::Equal))
    });

    struct LineGroup {
        words: Vec<HocrWord>,
        min_x: f32,
        max_x: f32,
        min_y: f32,
        max_y: f32,
    }

    let mut groups: Vec<LineGroup> = Vec::new();

    for word in words.into_iter() {
        let word_mid = word.y + word.height * 0.5;
        let mut target = None;

        for (idx, group) in groups.iter().enumerate() {
            let line_mid = (group.min_y + group.max_y) * 0.5;
            let line_height = (group.max_y - group.min_y).max(word.height).max(1.0);
            if (line_mid - word_mid).abs() <= line_height * 0.45 {
                target = Some(idx);
                break;
            }
        }

        let idx = if let Some(idx) = target {
            idx
        } else {
            groups.push(LineGroup {
                min_x: word.x,
                max_x: word.x + word.width,
                min_y: word.y,
                max_y: word.y + word.height,
                words: Vec::new(),
            });
            groups.len() - 1
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
            group.words.sort_by(|a, b| {
                a.x.partial_cmp(&b.x)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.y.partial_cmp(&b.y).unwrap_or(std::cmp::Ordering::Equal))
            });

            let min_x = group.min_x;
            let max_x = group.max_x;
            let min_y = group.min_y;
            let max_y = group.max_y;

            let mut char_width_sum = 0.0f32;
            let mut char_width_count = 0usize;
            for w in &group.words {
                if w.text.chars().count() == 1 {
                    char_width_sum += w.width;
                    char_width_count += 1;
                }
            }
            let line_height = (max_y - min_y).max(1.0);
            let avg_char_width = if char_width_count > 0 {
                char_width_sum / (char_width_count as f32)
            } else {
                line_height * 0.5
            };

            HocrLine {
                x: min_x,
                y: min_y,
                width: (max_x - min_x).max(0.0),
                height: line_height,
                baseline: max_y,
                avg_char_width,
                words: group.words,
                raw_text: None,
            }
        })
        .collect();

    lines.sort_by(|a, b| {
        a.y.partial_cmp(&b.y)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.x.partial_cmp(&b.x).unwrap_or(std::cmp::Ordering::Equal))
    });

    lines
}

/// Remove adjacent repeated words introduced by tiling overlaps (same text,
/// high vertical overlap, small horizontal gap). Per-line.
pub fn dedup_adjacent_repeats(lines: &mut [HocrLine]) {
    for line in lines.iter_mut() {
        if line.words.is_empty() {
            continue;
        }
        let mut cleaned: Vec<HocrWord> = Vec::with_capacity(line.words.len());
        for w in line.words.iter() {
            if let Some(prev) = cleaned.last() {
                let same_text = prev.text.trim() == w.text.trim();
                let gap = w.x - (prev.x + prev.width);
                let v_overlap =
                    ((prev.y + prev.height).min(w.y + w.height) - prev.y.max(w.y)).max(0.0);
                let v_overlap_ratio = v_overlap / prev.height.max(w.height).max(1.0);
                if same_text && gap < line.height * 0.25 && v_overlap_ratio > 0.5 {
                    continue;
                }
            }
            cleaned.push(w.clone());
        }
        line.words = cleaned;
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
