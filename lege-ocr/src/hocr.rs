use once_cell::sync::Lazy;
use regex::Regex;

use crate::types::OcrLineResult;

// ─── Fast-path hOCR helpers (used by the fast OCR pipeline) ─────────────────

/// Strip a full hOCR document down to just the inner body content (word/line spans).
/// Used when stitching per-region hOCR results into a page-level result.
pub fn strip_to_body(hocr: &str) -> String {
    // Primary: extract content inside first ocr_carea div
    if let Some(div_start) = hocr.find("<div class=\"ocr_carea\"")
        && let Some(tag_close_rel) = hocr[div_start..].find('>')
    {
        let content_start = div_start + tag_close_rel + 1;
        if let Some(end) = hocr.rfind("</div></div></body></html>")
            && end >= content_start
        {
            return hocr[content_start..end]
                .trim()
                .replace('\n', " ")
                .replace("  ", " ");
        }
    }
    // Fallback: <body>…</body>
    if let (Some(bs), Some(be)) = (hocr.find("<body>"), hocr.rfind("</body>")) {
        let cs = bs + "<body>".len();
        if be >= cs {
            return hocr[cs..be].trim().replace('\n', " ").replace("  ", " ");
        }
    }
    hocr.trim().replace('\n', " ").replace("  ", " ")
}

/// Shift all `title="bbox x1 y1 x2 y2"` coordinates by (dx, dy).
/// Used to translate per-region hOCR back into page-global coordinates.
pub fn adjust_offsets(hocr_body: &str, dx: i32, dy: i32) -> String {
    static BBOX_DQ: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r#"title="bbox\s+(-?\d+)\s+(-?\d+)\s+(-?\d+)\s+(-?\d+)([^"]*)""#)
            .expect("bbox double-quote regex")
    });
    static BBOX_SQ: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r#"title='bbox\s+(-?\d+)\s+(-?\d+)\s+(-?\d+)\s+(-?\d+)([^']*)'"#)
            .expect("bbox single-quote regex")
    });

    fn shift(input: &str, re: &Regex, q: char, dx: i32, dy: i32) -> String {
        re.replace_all(input, |c: &regex::Captures| {
            let p = |i: usize| c.get(i).and_then(|m| m.as_str().parse::<i32>().ok());
            let tail = c.get(5).map_or("", |m| m.as_str());
            match (p(1), p(2), p(3), p(4)) {
                (Some(x1), Some(y1), Some(x2), Some(y2)) => format!(
                    "title={q}bbox {} {} {} {}{tail}{q}",
                    x1.saturating_add(dx),
                    y1.saturating_add(dy),
                    x2.saturating_add(dx),
                    y2.saturating_add(dy)
                ),
                _ => c
                    .get(0)
                    .map_or_else(String::new, |m| m.as_str().to_string()),
            }
        })
        .into_owned()
    }

    let step1 = shift(hocr_body, &BBOX_DQ, '"', dx, dy);
    shift(&step1, &BBOX_SQ, '\'', dx, dy)
}

/// Wrap body content in a complete page-level hOCR document.
pub fn finalize(body: &str, width: usize, height: usize) -> String {
    format!(
        r#"<html><head></head><body><div class="ocr_page" title="bbox 0 0 {width} {height}"><div class="ocr_carea" title="bbox 0 0 {width} {height}">{}</div></div></body></html>"#,
        body.trim()
    )
}

// ─── Slow-path hOCR assembly (from OcrLineResult) ───────────────────────────

/// Assemble a sequence of line results into a complete page-level hOCR document.
///
/// `width` and `height` are the high-res page dimensions in pixels.
pub fn build_page_hocr(lines: &[OcrLineResult], width: u32, height: u32) -> String {
    let mut body = String::new();
    for line in lines {
        body.push_str(&build_line_hocr(line));
    }
    finalize(&body, width as usize, height as usize)
}

/// Build the hOCR markup for a single line result.
/// The `bbox_highres` field already contains page-global coordinates.
pub fn build_line_hocr(line: &OcrLineResult) -> String {
    let [x1, y1, x2, y2] = line.bbox_highres;
    let mut s = format!(r#"<span class="ocr_line" title="bbox {x1} {y1} {x2} {y2}">"#);

    if line.words.is_empty() {
        // No word-level data — emit as a single word span
        if !line.text.trim().is_empty() {
            s.push_str(&format!(
                r#"<span class="ocrx_word" title="bbox {x1} {y1} {x2} {y2}">{}</span>"#,
                html_escape(&line.text)
            ));
        }
    } else {
        for word in &line.words {
            let [wx1, wy1, wx2, wy2] = word.bbox_crop_local;
            // Offset word bbox by the line's origin
            let wx1 = wx1.saturating_add(x1);
            let wy1 = wy1.saturating_add(y1);
            let wx2 = wx2.saturating_add(x1);
            let wy2 = wy2.saturating_add(y1);
            let conf_attr = word
                .confidence
                .filter(|c| c.is_finite())
                .map(|c| format!("; x_wconf {}", (c.clamp(0.0, 1.0) * 100.0).round() as u32))
                .unwrap_or_default();
            s.push_str(&format!(
                r#"<span class="ocrx_word" title="bbox {wx1} {wy1} {wx2} {wy2}{conf_attr}">{}</span> "#,
                html_escape(&word.text)
            ));
        }
    }

    s.push_str("</span>\n");
    s
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::OcrWord;

    #[test]
    fn build_page_hocr_empty() {
        let h = build_page_hocr(&[], 1000, 1400);
        assert!(h.contains("ocr_page"));
        assert!(h.contains("bbox 0 0 1000 1400"));
    }

    #[test]
    fn build_line_hocr_no_words() {
        let line = OcrLineResult {
            text: "Hello world".to_string(),
            confidence: None,
            words: Vec::new(),
            bbox_highres: [10, 20, 200, 40],
        };
        let h = build_line_hocr(&line);
        assert!(h.contains("ocr_line"));
        assert!(h.contains("bbox 10 20 200 40"));
        assert!(h.contains("Hello world"));
    }

    #[test]
    fn build_line_hocr_with_words() {
        let line = OcrLineResult {
            text: "Hi there".to_string(),
            confidence: Some(0.95),
            words: vec![
                OcrWord {
                    text: "Hi".to_string(),
                    bbox_crop_local: [0, 0, 30, 20],
                    confidence: Some(0.99),
                },
                OcrWord {
                    text: "there".to_string(),
                    bbox_crop_local: [35, 0, 100, 20],
                    confidence: Some(0.95),
                },
            ],
            bbox_highres: [50, 100, 200, 120],
        };
        let h = build_line_hocr(&line);
        // Words should be offset by line origin (50, 100)
        assert!(h.contains("bbox 50 100 80 120")); // 0+50,0+100,30+50,20+100
        assert!(h.contains("Hi"));
        assert!(h.contains("there"));
    }

    #[test]
    fn html_escape_special_chars() {
        let escaped = html_escape("<b>AT&T</b>");
        assert_eq!(escaped, "&lt;b&gt;AT&amp;T&lt;/b&gt;");
    }

    #[test]
    fn offset_and_word_coordinate_overflow_saturate() {
        assert!(
            adjust_offsets(r#"title="bbox 2147483647 0 1 2""#, 1, -1)
                .contains("bbox 2147483647 -1 2 1")
        );

        let line = OcrLineResult {
            text: "word".to_string(),
            confidence: None,
            words: vec![OcrWord {
                text: "word".to_string(),
                bbox_crop_local: [10, 10, u32::MAX, u32::MAX],
                confidence: Some(2.0),
            }],
            bbox_highres: [u32::MAX - 5, u32::MAX - 5, u32::MAX, u32::MAX],
        };
        let rendered = build_line_hocr(&line);
        assert!(rendered.contains("bbox 4294967295 4294967295 4294967295 4294967295"));
        assert!(rendered.contains("x_wconf 100"));
    }
}
