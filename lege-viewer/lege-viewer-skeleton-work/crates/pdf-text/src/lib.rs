//! Native PDF text extraction over [`pdf_content::SemanticPage`].
//!
//! This crate owns no document or rendering state. Font dictionaries and
//! `/ToUnicode` streams are resolved by `pdf-content`; extraction is a pure,
//! deterministic transformation of the owned semantic page.

use std::sync::Arc;

use pdf_content::semantic::{ActualTextSpanId, FontId, SemanticOp, TextRunId};
use pdf_content::{SemanticPage, TextElement};
use pdf_font::UnicodeSource;
use pdf_page_ir::{Matrix, PaintOrigin, Point, Rect};

mod unicode_data;

const SIZE_EPSILON: f64 = 0.01;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CharType {
    Normal,
    Generated,
    NotUnicode,
    Hyphen,
    Piece,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CharInfo {
    pub char_type: CharType,
    pub char_code: u32,
    pub cid: u32,
    pub glyph_id: u32,
    pub font: FontId,
    /// One UTF-16 code unit represented numerically; zero for `NotUnicode`.
    pub unicode: u32,
    pub unicode_source: Option<UnicodeSource>,
    pub origin: Point,
    pub char_box: Rect,
    pub loose_char_box: Rect,
    pub matrix: Matrix,
    pub text_object: TextRunId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextPageOptions {
    pub rtl: bool,
    pub normalize: bool,
    /// PDFium-compatible default: retain text objects hidden by OCG state.
    pub include_hidden: bool,
    pub include_annotations: bool,
    pub include_soft_masks: bool,
}

impl Default for TextPageOptions {
    fn default() -> Self {
        Self {
            rtl: false,
            normalize: true,
            include_hidden: true,
            include_annotations: false,
            include_soft_masks: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TextWord {
    pub text: String,
    pub bbox: Rect,
    pub first_char: usize,
    pub char_count: usize,
    pub continued: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct TextPage {
    chars: Vec<CharInfo>,
    text: Vec<u16>,
}

impl TextPage {
    pub fn build(page: &SemanticPage, options: &TextPageOptions) -> Self {
        Builder::new(page, options).build()
    }

    pub fn char_count(&self) -> usize {
        self.chars.len()
    }

    pub fn char_info(&self, index: usize) -> Option<&CharInfo> {
        self.chars.get(index)
    }

    pub fn chars(&self) -> &[CharInfo] {
        &self.chars
    }

    pub fn loose_bounds(&self, index: usize) -> Option<Rect> {
        self.chars.get(index).map(|info| info.loose_char_box)
    }

    pub fn all_text_utf16(&self) -> &[u16] {
        &self.text
    }

    pub fn all_text(&self) -> String {
        String::from_utf16_lossy(&self.text)
    }

    pub fn text_utf16(&self, start: usize, count: usize) -> Vec<u16> {
        self.chars
            .get(start..start.saturating_add(count).min(self.chars.len()))
            .unwrap_or_default()
            .iter()
            .filter_map(|info| {
                (!matches!(info.char_type, CharType::NotUnicode | CharType::Hyphen))
                    .then_some(info.unicode as u16)
            })
            .collect()
    }

    pub fn text(&self, start: usize, count: usize) -> String {
        String::from_utf16_lossy(&self.text_utf16(start, count))
    }

    /// Return characters whose tight boxes intersect `rect`, collapsing
    /// repeated spaces and preserving detected line boundaries.
    pub fn text_in_rect_utf16(&self, rect: Rect) -> Vec<u16> {
        let rect = rect.normalized();
        let mut output = Vec::new();
        let mut previous_space = false;
        for info in &self.chars {
            if info.char_type == CharType::NotUnicode
                || info.unicode == 0
                || !intersects(info.char_box, rect)
            {
                continue;
            }
            let unit = info.unicode as u16;
            let space = unit == 0x20;
            if space && previous_space {
                continue;
            }
            output.push(unit);
            previous_space = space;
        }
        output
    }

    pub fn text_in_rect(&self, rect: Rect) -> String {
        String::from_utf16_lossy(&self.text_in_rect_utf16(rect))
    }

    pub fn has_text(&self) -> bool {
        self.chars
            .iter()
            .any(|info| info.char_type != CharType::NotUnicode && info.unicode != 0)
    }

    /// PDFium-style rectangles: generated characters and degenerate boxes are
    /// skipped, and adjacent characters from one text object are unioned.
    pub fn rects(&self, start: usize, count: usize) -> Vec<Rect> {
        let end = start.saturating_add(count).min(self.chars.len());
        let mut out: Vec<Rect> = Vec::new();
        let mut current_object = None;
        for info in self.chars.get(start..end).unwrap_or_default() {
            if info.char_type == CharType::Generated
                || info.char_box.x1 - info.char_box.x0 < SIZE_EPSILON
                || info.char_box.y1 - info.char_box.y0 < SIZE_EPSILON
            {
                continue;
            }
            if current_object == Some(info.text_object) {
                if let Some(rect) = out.last_mut() {
                    *rect = union(*rect, info.char_box);
                }
            } else {
                out.push(info.char_box);
                current_object = Some(info.text_object);
            }
        }
        out
    }

    /// Exact word boxes from character geometry. Generated and literal spaces
    /// delimit words; proportional glyph widths are never interpolated.
    pub fn words(&self) -> Vec<TextWord> {
        let mut words = Vec::new();
        let mut start = None;
        let mut text = Vec::new();
        let mut bbox = None;
        let mut continued = false;
        for (index, info) in self.chars.iter().enumerate() {
            let separator = info.unicode == 0x20 || matches!(info.unicode, 0x0a | 0x0d);
            if separator {
                finish_word(
                    &mut words,
                    &mut start,
                    &mut text,
                    &mut bbox,
                    &mut continued,
                    index,
                );
                continue;
            }
            if info.char_type == CharType::NotUnicode || info.unicode == 0 {
                continue;
            }
            start.get_or_insert(index);
            if info.char_type == CharType::Hyphen {
                text.push(b'-' as u16);
                continued = true;
            } else {
                text.push(info.unicode as u16);
            }
            bbox = Some(bbox.map_or(info.char_box, |old| union(old, info.char_box)));
        }
        finish_word(
            &mut words,
            &mut start,
            &mut text,
            &mut bbox,
            &mut continued,
            self.chars.len(),
        );
        words
    }
}

fn finish_word(
    words: &mut Vec<TextWord>,
    start: &mut Option<usize>,
    text: &mut Vec<u16>,
    bbox: &mut Option<Rect>,
    continued: &mut bool,
    end: usize,
) {
    let (Some(first_char), Some(bounds)) = (start.take(), bbox.take()) else {
        text.clear();
        return;
    };
    words.push(TextWord {
        text: String::from_utf16_lossy(text),
        bbox: bounds,
        first_char,
        char_count: end.saturating_sub(first_char),
        continued: std::mem::take(continued),
    });
    text.clear();
}

struct Builder<'a> {
    page: &'a SemanticPage,
    options: &'a TextPageOptions,
    ctm: Matrix,
    ctm_stack: Vec<Matrix>,
    origins: Vec<PaintOrigin>,
    programs: Vec<Option<pdf_font::FontProgram>>,
    chars: Vec<CharInfo>,
    text: Vec<u16>,
    last_actual_text_span: Option<ActualTextSpanId>,
}

impl<'a> Builder<'a> {
    fn new(page: &'a SemanticPage, options: &'a TextPageOptions) -> Self {
        let programs = page
            .fonts
            .iter()
            .map(|font| {
                font.program
                    .clone()
                    .and_then(|data| pdf_font::FontProgram::parse_indexed(data, font.face_index))
            })
            .collect();
        Self {
            page,
            options,
            ctm: Matrix::IDENTITY,
            ctm_stack: Vec::new(),
            origins: Vec::new(),
            programs,
            chars: Vec::new(),
            text: Vec::new(),
            last_actual_text_span: None,
        }
    }

    fn build(mut self) -> TextPage {
        for op in self.page.ops.iter() {
            match op {
                SemanticOp::Save => self.ctm_stack.push(self.ctm),
                SemanticOp::Restore => {
                    if let Some(ctm) = self.ctm_stack.pop() {
                        self.ctm = ctm;
                    }
                }
                SemanticOp::Concat(matrix) => self.ctm = matrix.then(self.ctm),
                SemanticOp::BeginPaintOrigin(origin) => self.origins.push(*origin),
                SemanticOp::EndPaintOrigin => {
                    self.origins.pop();
                }
                SemanticOp::ShowText(id) if self.include_current_scope() => self.append_run(*id),
                _ => {}
            }
        }
        self.finish_layout();
        TextPage {
            chars: self.chars,
            text: self.text,
        }
    }

    fn finish_layout(&mut self) {
        let source = std::mem::take(&mut self.chars);
        self.text.clear();
        if source.is_empty() {
            return;
        }

        let display = canonical_display_matrix(self.page);
        let mut objects: Vec<Vec<CharInfo>> = Vec::new();
        for info in source {
            if objects
                .last()
                .and_then(|object| object.last())
                .is_some_and(|previous| previous.text_object == info.text_object)
            {
                if let Some(object) = objects.last_mut() {
                    object.push(info);
                }
            } else {
                objects.push(vec![info]);
            }
        }

        let mut pending: Vec<Vec<CharInfo>> = Vec::new();
        let mut ordered_segments: Vec<Vec<CharInfo>> = Vec::new();
        let mut line_breaks: Vec<bool> = Vec::new();
        for object in objects {
            let boundary = pending.last().and_then(|previous_object| {
                let previous = previous_object.first()?;
                let current = object.first()?;
                let previous_origin = display.apply(previous.origin);
                let current_origin = display.apply(current.origin);
                let threshold = (char_width(previous).max(char_width(current)) * 0.5).max(1.0);
                if (current_origin.y - previous_origin.y).abs() <= threshold {
                    return None;
                }
                let previous_box = object_bounds(previous_object, display);
                let current_box = object_bounds(&object, display);
                let overlaps_line =
                    previous_box.y0 < current_box.y1 && current_box.y0 < previous_box.y1;
                Some(!overlaps_line)
            });
            if let Some(is_line_break) = boundary {
                ordered_segments.push(finish_object_line(std::mem::take(&mut pending), display));
                line_breaks.push(is_line_break);
            }
            pending.push(object);
        }
        if !pending.is_empty() {
            ordered_segments.push(finish_object_line(pending, display));
        }

        let mut lines: Vec<Vec<CharInfo>> = Vec::new();
        let mut current = Vec::new();
        for (index, segment) in ordered_segments.into_iter().enumerate() {
            if index > 0 && line_breaks[index - 1] {
                if !current.is_empty() {
                    lines.push(std::mem::take(&mut current));
                }
            } else if index > 0 {
                append_generated_space(&mut current, segment.first());
            }
            current.extend(segment);
        }
        if !current.is_empty() {
            lines.push(current);
        }

        let mut previous_hyphen = false;
        for (line_index, mut line) in lines.into_iter().enumerate() {
            if line_index > 0 && !self.chars.is_empty() && !previous_hyphen {
                append_generated_line_break(&mut self.chars);
            }
            while line.last().is_some_and(|info| info.unicode == 0x20) {
                line.pop();
            }
            previous_hyphen = line
                .last()
                .is_some_and(|info| matches!(info.unicode, 0x2d | 0xad));
            if previous_hyphen && let Some(info) = line.last_mut() {
                info.char_type = CharType::Hyphen;
                info.unicode = 0x2;
            }
            self.append_bidi_line(line);
        }
        self.text = self
            .chars
            .iter()
            .filter_map(|info| {
                (!matches!(info.char_type, CharType::NotUnicode | CharType::Hyphen))
                    .then_some(info.unicode as u16)
            })
            .collect();
    }

    fn append_bidi_line(&mut self, mut line: Vec<CharInfo>) {
        line.dedup_by(|next, previous| next.unicode == 0x20 && previous.unicode == 0x20);
        let units: Vec<u16> = line
            .iter()
            .map(|info| {
                if info.char_type == CharType::NotUnicode {
                    0xfffe
                } else {
                    info.unicode as u16
                }
            })
            .collect();
        let (mut current_direction, segments) = unicode_data::segments(&units, self.options.rtl);
        for segment in segments {
            let right = segment.direction == unicode_data::Direction::Right
                || (segment.direction == unicode_data::Direction::Neutral
                    && current_direction == unicode_data::Direction::Right);
            if right {
                current_direction = unicode_data::Direction::Right;
                for index in (segment.start..segment.start + segment.count).rev() {
                    self.append_directional_char(&line[index], true);
                }
            } else {
                if segment.direction != unicode_data::Direction::LeftWeak {
                    current_direction = unicode_data::Direction::Left;
                }
                for info in &line[segment.start..segment.start + segment.count] {
                    self.append_directional_char(info, false);
                }
            }
        }
    }

    fn append_directional_char(&mut self, source: &CharInfo, right_to_left: bool) {
        if source.char_type == CharType::NotUnicode {
            self.chars.push(source.clone());
            return;
        }
        let unit = if right_to_left {
            unicode_data::mirror(source.unicode as u16)
        } else {
            source.unicode as u16
        };
        let normalize =
            self.options.normalize && (right_to_left || (0xfb00..=0xfb06).contains(&unit));
        let normalized = if normalize {
            unicode_data::normalize(unit)
        } else {
            vec![unit]
        };
        let is_piece = normalized.len() != 1 || normalized[0] != source.unicode as u16;
        for unit in normalized {
            let mut info = source.clone();
            info.unicode = u32::from(unit);
            if is_piece {
                info.char_type = CharType::Piece;
            }
            self.chars.push(info);
        }
    }

    fn include_current_scope(&self) -> bool {
        self.origins.iter().all(|origin| match origin {
            PaintOrigin::AnnotationAppearance => self.options.include_annotations,
            PaintOrigin::SoftMaskContent => self.options.include_soft_masks,
            PaintOrigin::TilingPatternCell | PaintOrigin::Type3Glyph => false,
            PaintOrigin::PageContent | PaintOrigin::FormXObject => true,
        })
    }

    fn append_run(&mut self, id: TextRunId) {
        let run = self.page.text_runs[id.index()].clone();
        if !run.visible && !self.options.include_hidden {
            return;
        }
        if run
            .actual_text
            .as_ref()
            .is_some_and(|span| Some(span.id) == self.last_actual_text_span)
        {
            return;
        }
        let chars_before = self.chars.len();
        let text_before = self.text.len();
        let font = self.page.fonts[run.font.index()].clone();
        let program = self.programs[run.font.index()].clone();
        let upem = program
            .as_ref()
            .map_or(1000.0, |value| value.units_per_em().max(1) as f64);
        let vertical = font.metrics.is_vertical();
        let th = run.horizontal_scale / 100.0;
        let mut cursor = 0.0f64;

        for element in &run.elements {
            match element {
                TextElement::Adjust(value) => {
                    cursor += if vertical {
                        -value / 1000.0 * run.font_size
                    } else {
                        -value / 1000.0 * run.font_size * th
                    };
                }
                TextElement::Show(bytes) => {
                    for decoded in font.metrics.decode(bytes) {
                        let word_space = if decoded.word_space {
                            run.word_spacing
                        } else {
                            0.0
                        };
                        let (x, y, advance) = if let Some(placement) = vertical
                            .then(|| font.metrics.vertical(decoded.cid))
                            .flatten()
                        {
                            (
                                -(placement.origin.0 as f64) / 1000.0 * run.font_size * th,
                                cursor + run.rise
                                    - (placement.origin.1 as f64) / 1000.0 * run.font_size,
                                placement.advance as f64 / 1000.0 * run.font_size
                                    + run.char_spacing
                                    + word_space,
                            )
                        } else {
                            (
                                cursor,
                                run.rise,
                                (decoded.advance as f64 / 1000.0 * run.font_size
                                    + run.char_spacing
                                    + word_space)
                                    * th,
                            )
                        };
                        let gid = font.glyph_map.gid(decoded.cid);
                        let local_box = glyph_box(
                            program.as_ref(),
                            gid,
                            font.font_bbox,
                            font.type3_matrix,
                            x,
                            y,
                            advance,
                            run.font_size,
                            th,
                            upem,
                        );
                        let matrix = run.text_matrix.then(self.ctm);
                        let char_box = transform_rect(local_box, matrix);
                        let loose_char_box = transform_rect(
                            loose_glyph_box(font.font_bbox, x, y, advance, run.font_size, th),
                            matrix,
                        );
                        let origin = matrix.apply(Point { x, y });
                        self.append_decoded(
                            id,
                            decoded,
                            gid,
                            origin,
                            char_box,
                            loose_char_box,
                            matrix,
                            &font,
                            run.font,
                            run.font_size,
                        );
                        cursor += advance;
                    }
                }
            }
        }

        if let Some(span) = &run.actual_text {
            let bounds = self.chars[chars_before..]
                .iter()
                .map(|info| info.char_box)
                .reduce(union);
            let matrix = run.text_matrix.then(self.ctm);
            self.chars.truncate(chars_before);
            self.text.truncate(text_before);
            let bounds = bounds.unwrap_or_else(|| {
                transform_rect(
                    Rect {
                        x0: 0.0,
                        y0: run.rise,
                        x1: run.font_size.max(SIZE_EPSILON),
                        y1: run.rise + run.font_size.max(SIZE_EPSILON),
                    },
                    matrix,
                )
            });
            let count = span.utf16.len().max(1) as f64;
            let step = (bounds.x1 - bounds.x0) / count;
            for (index, &unit) in span.utf16.iter().enumerate() {
                let x0 = bounds.x0 + index as f64 * step;
                let piece_box = Rect {
                    x0,
                    y0: bounds.y0,
                    x1: x0 + step,
                    y1: bounds.y1,
                }
                .normalized();
                self.text.push(unit);
                self.chars.push(CharInfo {
                    char_type: CharType::Piece,
                    char_code: 0,
                    cid: 0,
                    glyph_id: 0,
                    font: run.font,
                    unicode: unit as u32,
                    unicode_source: None,
                    origin: Point {
                        x: piece_box.x0,
                        y: piece_box.y0,
                    },
                    char_box: piece_box,
                    loose_char_box: piece_box,
                    matrix,
                    text_object: id,
                });
            }
            self.last_actual_text_span = Some(span.id);
        } else {
            self.last_actual_text_span = None;
        }

        let appended = &self.chars[chars_before..];
        let units: Vec<u16> = appended.iter().map(|info| info.unicode as u16).collect();
        let (direction, _) = unicode_data::segments(&units, false);
        let object_matrix = run.text_matrix.then(self.ctm);
        if direction == unicode_data::Direction::Right
            && object_matrix.a * object_matrix.d - object_matrix.b * object_matrix.c < 0.0
        {
            self.chars[chars_before..].reverse();
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn append_decoded(
        &mut self,
        text_object: TextRunId,
        decoded: pdf_font::DecodedCode,
        glyph_id: u32,
        origin: Point,
        char_box: Rect,
        loose_char_box: Rect,
        matrix: Matrix,
        font: &pdf_content::semantic::SemFont,
        font_id: FontId,
        font_size: f64,
    ) {
        let mapping = font.unicode_map.get(decoded.char_code);
        let fallback = mapping
            .map(|mapping| (mapping.utf16.clone(), mapping.source))
            .or_else(|| {
                pdf_font::cid_to_unicode(font.charset, decoded.cid).map(|ch| {
                    let mut buf = [0u16; 2];
                    let encoded = ch.encode_utf16(&mut buf);
                    (
                        Arc::<[u16]>::from(&encoded[..]),
                        UnicodeSource::PredefinedCid,
                    )
                })
            });

        let duplicate_threshold =
            0.07 * font_size.abs() * matrix.a.hypot(matrix.b).max(matrix.c.hypot(matrix.d));
        let duplicate = self.chars.iter().rev().take(7).any(|previous| {
            previous.char_code == decoded.char_code
                && previous.font == font_id
                && (previous.origin.x - origin.x).abs() < duplicate_threshold
                && (previous.origin.y - origin.y).abs() < duplicate_threshold
        });
        if duplicate {
            return;
        }

        match fallback {
            Some((utf16, source)) => {
                for &unit in utf16.iter() {
                    self.text.push(unit);
                    self.chars.push(CharInfo {
                        char_type: CharType::Normal,
                        char_code: decoded.char_code,
                        cid: decoded.cid,
                        glyph_id,
                        font: font_id,
                        unicode: unit as u32,
                        unicode_source: Some(source),
                        origin,
                        char_box,
                        loose_char_box,
                        matrix,
                        text_object,
                    });
                }
            }
            None => {
                self.text.push(0xfffe);
                self.chars.push(CharInfo {
                    char_type: CharType::NotUnicode,
                    char_code: decoded.char_code,
                    cid: decoded.cid,
                    glyph_id,
                    font: font_id,
                    unicode: 0,
                    unicode_source: None,
                    origin,
                    char_box,
                    loose_char_box,
                    matrix,
                    text_object,
                });
            }
        }
    }
}

fn glyph_box(
    program: Option<&pdf_font::FontProgram>,
    glyph_id: u32,
    font_bbox: Option<[f64; 4]>,
    type3_matrix: Option<Matrix>,
    x: f64,
    y: f64,
    advance: f64,
    font_size: f64,
    horizontal_scale: f64,
    upem: f64,
) -> Rect {
    if let (Some([x0, y0, x1, y1]), Some(font_matrix)) = (font_bbox, type3_matrix) {
        let glyph_bounds = transform_rect(
            Rect { x0, y0, x1, y1 },
            font_matrix.then(Matrix {
                a: font_size * horizontal_scale,
                b: 0.0,
                c: 0.0,
                d: font_size,
                e: x,
                f: y,
            }),
        );
        return glyph_bounds.normalized();
    }
    if let Some([x0, y0, x1, y1]) = program
        .and_then(|font| font.outline(glyph_id))
        .and_then(|outline| outline.bounds())
    {
        let scale = font_size / upem;
        return Rect {
            x0: x + x0 as f64 * scale * horizontal_scale,
            y0: y + y0 as f64 * scale,
            x1: x + x1 as f64 * scale * horizontal_scale,
            y1: y + y1 as f64 * scale,
        }
        .normalized();
    }
    Rect {
        x0: x,
        y0: y,
        x1: x + advance.abs().max(SIZE_EPSILON),
        y1: y + font_size.abs().max(SIZE_EPSILON),
    }
    .normalized()
}

fn loose_glyph_box(
    font_bbox: Option<[f64; 4]>,
    x: f64,
    y: f64,
    advance: f64,
    font_size: f64,
    horizontal_scale: f64,
) -> Rect {
    let [_, y0, _, y1] = font_bbox.unwrap_or([0.0, 0.0, 1000.0, 1000.0]);
    Rect {
        x0: x,
        y0: y + y0 / 1000.0 * font_size,
        x1: x + advance.abs().max(SIZE_EPSILON) * horizontal_scale.signum(),
        y1: y + y1 / 1000.0 * font_size,
    }
    .normalized()
}

fn transform_rect(rect: Rect, matrix: Matrix) -> Rect {
    let points = [
        matrix.apply(Point {
            x: rect.x0,
            y: rect.y0,
        }),
        matrix.apply(Point {
            x: rect.x1,
            y: rect.y0,
        }),
        matrix.apply(Point {
            x: rect.x1,
            y: rect.y1,
        }),
        matrix.apply(Point {
            x: rect.x0,
            y: rect.y1,
        }),
    ];
    let mut result = Rect {
        x0: points[0].x,
        y0: points[0].y,
        x1: points[0].x,
        y1: points[0].y,
    };
    for point in &points[1..] {
        result.x0 = result.x0.min(point.x);
        result.y0 = result.y0.min(point.y);
        result.x1 = result.x1.max(point.x);
        result.y1 = result.y1.max(point.y);
    }
    result
}

fn canonical_display_matrix(page: &SemanticPage) -> Matrix {
    let crop = page.bounds.crop;
    match page.bounds.rotate {
        90 => Matrix {
            a: 0.0,
            b: 1.0,
            c: 1.0,
            d: 0.0,
            e: -crop.y0,
            f: -crop.x0,
        },
        180 => Matrix {
            a: -1.0,
            b: 0.0,
            c: 0.0,
            d: 1.0,
            e: crop.x1,
            f: -crop.y0,
        },
        270 => Matrix {
            a: 0.0,
            b: -1.0,
            c: -1.0,
            d: 0.0,
            e: crop.y1,
            f: crop.x1,
        },
        _ => Matrix {
            a: 1.0,
            b: 0.0,
            c: 0.0,
            d: -1.0,
            e: -crop.x0,
            f: crop.y1,
        },
    }
}

fn finish_object_line(mut objects: Vec<Vec<CharInfo>>, display: Matrix) -> Vec<CharInfo> {
    objects.sort_by(|left, right| {
        let left = display.apply(left[0].origin).x;
        let right = display.apply(right[0].origin).x;
        left.total_cmp(&right)
    });
    let mut line: Vec<CharInfo> = Vec::new();
    for object in objects {
        if let (Some(previous), Some(current)) = (line.last(), object.first()) {
            let previous_box = transform_rect(previous.char_box, display);
            let current_box = transform_rect(current.char_box, display);
            let gap = current_box.x0 - previous_box.x1;
            // PDFium's threshold is font-metric based and deliberately
            // conservative; a normal one-glyph advance must not invent a
            // separator between adjacent text-show operators.
            let threshold = char_width(previous).max(char_width(current));
            let overlap = previous_box.y0 < current_box.y1 && current_box.y0 < previous_box.y1;
            if overlap
                && gap > threshold
                && previous.unicode != 0x20
                && current.unicode != 0x20
                && previous.char_type != CharType::Piece
            {
                let mut space = previous.clone();
                space.char_type = CharType::Generated;
                space.char_code = 0;
                space.cid = 0;
                space.glyph_id = 0;
                space.unicode = 0x20;
                space.unicode_source = None;
                space.origin = Point {
                    x: previous.char_box.x1,
                    y: previous.origin.y,
                };
                space.char_box = Rect {
                    x0: space.origin.x,
                    y0: space.origin.y,
                    x1: space.origin.x,
                    y1: space.origin.y,
                };
                space.loose_char_box = space.char_box;
                line.push(space);
            }
        }
        line.extend(object);
    }
    line
}

fn object_bounds(object: &[CharInfo], display: Matrix) -> Rect {
    object
        .iter()
        .map(|info| transform_rect(info.char_box, display))
        .reduce(union)
        .unwrap_or(Rect {
            x0: 0.0,
            y0: 0.0,
            x1: 0.0,
            y1: 0.0,
        })
}

fn append_generated_line_break(chars: &mut Vec<CharInfo>) {
    let Some(template) = chars.last().cloned() else {
        return;
    };
    for unit in [0x0d, 0x0a] {
        let mut info = template.clone();
        info.char_type = CharType::Generated;
        info.char_code = 0;
        info.cid = 0;
        info.glyph_id = 0;
        info.unicode = unit;
        info.unicode_source = None;
        info.origin = Point {
            x: template.char_box.x1,
            y: template.origin.y,
        };
        info.char_box = Rect {
            x0: info.origin.x,
            y0: info.origin.y,
            x1: info.origin.x,
            y1: info.origin.y,
        };
        info.loose_char_box = info.char_box;
        chars.push(info);
    }
}

fn append_generated_space(chars: &mut Vec<CharInfo>, next: Option<&CharInfo>) {
    let Some(template) = chars.last().cloned().or_else(|| next.cloned()) else {
        return;
    };
    if template.unicode == 0x20 || next.is_some_and(|info| info.unicode == 0x20) {
        return;
    }
    let mut info = template.clone();
    info.char_type = CharType::Generated;
    info.char_code = 0;
    info.cid = 0;
    info.glyph_id = 0;
    info.unicode = 0x20;
    info.unicode_source = None;
    info.origin = Point {
        x: template.char_box.x1,
        y: template.origin.y,
    };
    info.char_box = Rect {
        x0: info.origin.x,
        y0: info.origin.y,
        x1: info.origin.x,
        y1: info.origin.y,
    };
    info.loose_char_box = info.char_box;
    chars.push(info);
}

fn char_width(info: &CharInfo) -> f64 {
    (info.char_box.x1 - info.char_box.x0)
        .abs()
        .max((info.char_box.y1 - info.char_box.y0).abs() * 0.25)
}

fn intersects(a: Rect, b: Rect) -> bool {
    let a = a.normalized();
    let b = b.normalized();
    a.x0 <= b.x1 && b.x0 <= a.x1 && a.y0 <= b.y1 && b.y0 <= a.y1
}

fn union(a: Rect, b: Rect) -> Rect {
    Rect {
        x0: a.x0.min(b.x0),
        y0: a.y0.min(b.y0),
        x1: a.x1.max(b.x1),
        y1: a.y1.max(b.y1),
    }
}

const fn _assert_send_sync<T: Send + Sync>() {}
const _: () = _assert_send_sync::<TextPage>();
