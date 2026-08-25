use std::borrow::Cow;
use std::fmt::Write as _;

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
///
/// Both quote styles are handled in a single traversal. A `title=` attribute
/// whose body is not a well-formed `bbox` with four integers is copied through
/// verbatim, as is any `title=` that is not followed by a quote or is left
/// unterminated — this only ever rewrites what it fully understands.
pub fn adjust_offsets(hocr_body: &str, dx: i32, dy: i32) -> String {
    const KEY: &str = "title=";

    let mut out = String::with_capacity(hocr_body.len());
    let mut rest = hocr_body;

    while let Some(at) = rest.find(KEY) {
        out.push_str(&rest[..at]);
        rest = &rest[at..];

        // The byte after `title=` must be a quote for this to be an attribute
        // we can rewrite; otherwise step over the key and keep scanning.
        let quote = match rest.as_bytes().get(KEY.len()) {
            Some(&q @ (b'"' | b'\'')) => q,
            _ => {
                out.push_str(&rest[..KEY.len()]);
                rest = &rest[KEY.len()..];
                continue;
            }
        };

        let body_start = KEY.len() + 1;
        let Some(end) = rest[body_start..]
            .find(quote as char)
            .map(|i| body_start + i)
        else {
            // Unterminated attribute: emit the remainder unchanged.
            out.push_str(rest);
            return out;
        };

        match parse_bbox(&rest[body_start..end]) {
            Some(([x1, y1, x2, y2], tail)) => {
                out.push_str(KEY);
                out.push(quote as char);
                let _ = write!(
                    out,
                    "bbox {} {} {} {}",
                    x1.saturating_add(dx),
                    y1.saturating_add(dy),
                    x2.saturating_add(dx),
                    y2.saturating_add(dy)
                );
                out.push_str(tail);
                out.push(quote as char);
            }
            // Not a bbox title (e.g. `title="ocr foo"`): verbatim, quotes included.
            None => out.push_str(&rest[..=end]),
        }
        rest = &rest[end + 1..];
    }

    out.push_str(rest);
    out
}

/// Parse a `title` attribute body of the form `bbox <i32> <i32> <i32> <i32>`,
/// returning the four coordinates and whatever trails them (commonly
/// `"; x_wconf 95"`). `None` if the body is not a bbox with four integers.
fn parse_bbox(body: &str) -> Option<([i32; 4], &str)> {
    let after_keyword = body.strip_prefix("bbox")?;
    if !after_keyword.starts_with(|c: char| c.is_ascii_whitespace()) {
        return None;
    }

    let mut coords = [0i32; 4];
    let mut rest = after_keyword;
    for slot in &mut coords {
        let token_start = rest.trim_start_matches(|c: char| c.is_ascii_whitespace());
        let token_end = token_start
            .find(|c: char| c != '-' && !c.is_ascii_digit())
            .unwrap_or(token_start.len());
        *slot = token_start[..token_end].parse::<i32>().ok()?;
        rest = &token_start[token_end..];
    }

    Some((coords, rest))
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

/// Escape the five XML metacharacters for embedding text in hOCR markup.
///
/// This is the single escaper for the crate; the WinRT OCR path in
/// `engine.rs` used to carry a byte-identical copy.
pub(crate) fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
}

/// Inverse of [`html_escape`], for reading text back out of hOCR spans.
///
/// Handles the five XML entities plus numeric `&#NN;` / `&#xNN;` references —
/// the entire set any hOCR producer in this pipeline emits (Tesseract, WinRT,
/// and `html_escape` above). An unrecognised or malformed `&…;` is preserved
/// verbatim rather than dropped, so unknown input degrades to a no-op instead
/// of losing characters.
pub(crate) fn html_unescape(s: &str) -> Cow<'_, str> {
    if !s.contains('&') {
        return Cow::Borrowed(s);
    }

    // Longest form accepted is `&#x10FFFF;` (10 bytes); cap the scan so a bare
    // `&` in running text cannot drag the search across the whole string.
    const MAX_ENTITY_LEN: usize = 10;

    let mut out = String::with_capacity(s.len());
    let mut rest = s;

    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];

        let window = &rest[..rest.len().min(MAX_ENTITY_LEN + 1)];
        let Some(semi) = window.find(';') else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };

        match decode_entity(&rest[1..semi]) {
            Some(c) => out.push(c),
            None => out.push_str(&rest[..=semi]),
        }
        rest = &rest[semi + 1..];
    }

    out.push_str(rest);
    Cow::Owned(out)
}

/// Decode one entity body (the text between `&` and `;`).
fn decode_entity(body: &str) -> Option<char> {
    match body {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        _ => {
            let digits = body.strip_prefix('#')?;
            let code = match digits.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                None => digits.parse::<u32>().ok()?,
            };
            char::from_u32(code)
        }
    }
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
    fn adjust_offsets_single_quote_style() {
        assert_eq!(
            adjust_offsets("<span title='bbox 10 20 30 40'>x</span>", 5, 7),
            "<span title='bbox 15 27 35 47'>x</span>"
        );
    }

    #[test]
    fn adjust_offsets_handles_both_quote_styles_in_one_pass() {
        let input = r#"<a title="bbox 1 2 3 4"><b title='bbox 5 6 7 8'>t</b></a>"#;
        assert_eq!(
            adjust_offsets(input, 10, 100),
            r#"<a title="bbox 11 102 13 104"><b title='bbox 15 106 17 108'>t</b></a>"#
        );
    }

    #[test]
    fn adjust_offsets_preserves_bbox_tail() {
        assert_eq!(
            adjust_offsets(r#"title="bbox 1 2 3 4; x_wconf 95""#, 1, 1),
            r#"title="bbox 2 3 4 5; x_wconf 95""#
        );
    }

    #[test]
    fn adjust_offsets_leaves_non_bbox_titles_alone() {
        for input in [
            r#"<span title="ocr line 3">t</span>"#,
            r#"<span title="bbox 1 2 3">short</span>"#,
            r#"<span title="bboxen 1 2 3 4">prefix</span>"#,
            "<span title=unquoted>t</span>",
            r#"<span title="unterminated"#,
        ] {
            assert_eq!(adjust_offsets(input, 9, 9), input, "input: {input}");
        }
    }

    #[test]
    fn adjust_offsets_accepts_negative_and_multi_space_coordinates() {
        assert_eq!(
            adjust_offsets(r#"title="bbox  -5   -6  7  8""#, 5, 6),
            r#"title="bbox 0 0 12 14""#
        );
    }

    #[test]
    fn html_unescape_round_trips_the_escaper() {
        let raw = "<b>AT&T \"q\" 'a'</b>";
        assert_eq!(html_unescape(&html_escape(raw)), raw);
    }

    #[test]
    fn html_unescape_numeric_and_unknown_entities() {
        assert_eq!(html_unescape("&#65;&#x42;&#X43;"), "ABC");
        assert_eq!(html_unescape("&apos;&quot;"), "'\"");
        // Unknown or malformed references survive verbatim.
        assert_eq!(html_unescape("&hellip;"), "&hellip;");
        assert_eq!(html_unescape("AT&T"), "AT&T");
        assert_eq!(html_unescape("&#zz;"), "&#zz;");
        assert_eq!(html_unescape("100% & rising"), "100% & rising");
    }

    #[test]
    fn html_unescape_borrows_when_there_is_nothing_to_do() {
        assert!(matches!(html_unescape("plain text"), Cow::Borrowed(_)));
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
