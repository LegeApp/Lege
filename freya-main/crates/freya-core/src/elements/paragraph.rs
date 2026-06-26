//! [paragraph()] makes it possible to render rich text with different styles. Its a more customizable API than [crate::elements::label].

use std::{
    any::Any,
    borrow::Cow,
    cell::RefCell,
    fmt::{Debug, Display},
    rc::Rc,
};

#[cfg(not(feature = "cpu-renderer"))]
use freya_engine::prelude::{FontStyle, ParagraphBuilder, ParagraphStyle, TextStyle};
use freya_engine::prelude::{
    Paint, PaintStyle, RectHeightStyle, RectWidthStyle, SkParagraph, SkRect,
};
#[cfg(feature = "cpu-renderer")]
use freya_render_api::ParagraphLayout;
#[cfg(feature = "cpu-renderer")]
use freya_text_cosmic::prelude::CosmicParagraph;
use rustc_hash::FxHashMap;
use torin::prelude::Size2D;

#[cfg(feature = "cpu-renderer")]
use crate::element::CpuRenderContext;
#[cfg(not(feature = "cpu-renderer"))]
use crate::{data::TextStyleState, prelude::TextAlign, text_cache::CachedParagraph};
use crate::{
    data::{AccessibilityData, CursorStyleData, EffectData, LayoutData, StyleState, TextStyleData},
    diff_key::DiffKey,
    element::{Element, ElementExt, EventHandlerType, LayoutContext, RenderContext},
    events::name::EventName,
    layers::Layer,
    prelude::{
        AccessibilityExt, Color, ContainerExt, EventHandlersExt, KeyExt, LayerExt, LayoutExt,
        MaybeExt, TextStyleExt, VerticalAlign,
    },
    style::cursor::{CursorMode, CursorStyle},
    tree::DiffModifies,
};

/// [paragraph()] makes it possible to render rich text with different styles. Its a more customizable API than [crate::elements::label].
///
/// See the available methods in [Paragraph].
///
/// ```rust
/// # use freya::prelude::*;
/// fn app() -> impl IntoElement {
///     paragraph()
///         .span(Span::new("Hello").font_size(24.0))
///         .span(Span::new("World").font_size(16.0))
/// }
/// ```
pub fn paragraph() -> Paragraph {
    Paragraph {
        key: DiffKey::None,
        element: ParagraphElement::default(),
    }
}

pub struct ParagraphHolderInner {
    pub paragraph: Rc<SkParagraph>,
    pub scale_factor: f64,
}

#[derive(Clone)]
pub struct ParagraphHolder(pub Rc<RefCell<Option<ParagraphHolderInner>>>);

impl PartialEq for ParagraphHolder {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.0, &other.0)
    }
}

impl Debug for ParagraphHolder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ParagraphHolder")
    }
}

impl Default for ParagraphHolder {
    fn default() -> Self {
        Self(Rc::new(RefCell::new(None)))
    }
}

#[derive(PartialEq, Clone)]
pub struct ParagraphElement {
    pub layout: LayoutData,
    pub spans: Vec<Span<'static>>,
    pub accessibility: AccessibilityData,
    pub text_style_data: TextStyleData,
    pub cursor_style_data: CursorStyleData,
    pub event_handlers: FxHashMap<EventName, EventHandlerType>,
    pub sk_paragraph: ParagraphHolder,
    pub cursor_index: Option<usize>,
    pub highlights: Vec<(usize, usize)>,
    pub max_lines: Option<usize>,
    pub line_height: Option<f32>,
    pub relative_layer: Layer,
    pub cursor_style: CursorStyle,
    pub cursor_mode: CursorMode,
    pub vertical_align: VerticalAlign,
}

impl Default for ParagraphElement {
    fn default() -> Self {
        let mut accessibility = AccessibilityData::default();
        accessibility.builder.set_role(accesskit::Role::Paragraph);
        Self {
            layout: Default::default(),
            spans: Default::default(),
            accessibility,
            text_style_data: Default::default(),
            cursor_style_data: Default::default(),
            event_handlers: Default::default(),
            sk_paragraph: Default::default(),
            cursor_index: Default::default(),
            highlights: Default::default(),
            max_lines: Default::default(),
            line_height: Default::default(),
            relative_layer: Default::default(),
            cursor_style: CursorStyle::default(),
            cursor_mode: CursorMode::default(),
            vertical_align: VerticalAlign::default(),
        }
    }
}

impl Display for ParagraphElement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            &self
                .spans
                .iter()
                .map(|s| s.text.clone())
                .collect::<Vec<_>>()
                .join("\n"),
        )
    }
}

impl ElementExt for ParagraphElement {
    fn changed(&self, other: &Rc<dyn ElementExt>) -> bool {
        let Some(paragraph) = (other.as_ref() as &dyn Any).downcast_ref::<ParagraphElement>()
        else {
            return false;
        };
        self != paragraph
    }

    fn diff(&self, other: &Rc<dyn ElementExt>) -> DiffModifies {
        let Some(paragraph) = (other.as_ref() as &dyn Any).downcast_ref::<ParagraphElement>()
        else {
            return DiffModifies::all();
        };

        let mut diff = DiffModifies::empty();

        if self.spans != paragraph.spans {
            diff.insert(DiffModifies::STYLE);
            diff.insert(DiffModifies::LAYOUT);
        }

        if self.accessibility != paragraph.accessibility {
            diff.insert(DiffModifies::ACCESSIBILITY);
        }

        if self.relative_layer != paragraph.relative_layer {
            diff.insert(DiffModifies::LAYER);
        }

        if self.text_style_data != paragraph.text_style_data {
            diff.insert(DiffModifies::STYLE);
        }

        if self.event_handlers != paragraph.event_handlers {
            diff.insert(DiffModifies::EVENT_HANDLERS);
        }

        if self.cursor_index != paragraph.cursor_index
            || self.highlights != paragraph.highlights
            || self.cursor_mode != paragraph.cursor_mode
            || self.vertical_align != paragraph.vertical_align
        {
            diff.insert(DiffModifies::STYLE);
        }

        if self.text_style_data != paragraph.text_style_data
            || self.line_height != paragraph.line_height
            || self.max_lines != paragraph.max_lines
        {
            diff.insert(DiffModifies::TEXT_STYLE);
            diff.insert(DiffModifies::LAYOUT);
        }

        if self.layout != paragraph.layout {
            diff.insert(DiffModifies::STYLE);
            diff.insert(DiffModifies::LAYOUT);
        }

        diff
    }

    fn layout(&'_ self) -> Cow<'_, LayoutData> {
        Cow::Borrowed(&self.layout)
    }
    fn effect(&'_ self) -> Option<Cow<'_, EffectData>> {
        None
    }

    fn style(&'_ self) -> Cow<'_, StyleState> {
        Cow::Owned(StyleState::default())
    }

    fn text_style(&'_ self) -> Cow<'_, TextStyleData> {
        Cow::Borrowed(&self.text_style_data)
    }

    fn accessibility(&'_ self) -> Cow<'_, AccessibilityData> {
        Cow::Borrowed(&self.accessibility)
    }

    fn layer(&self) -> Layer {
        self.relative_layer
    }

    fn measure(&self, context: LayoutContext) -> Option<(Size2D, Rc<dyn Any>)> {
        #[cfg(feature = "cpu-renderer")]
        {
            let base_font_size =
                f32::from(context.text_style_state.font_size) * context.scale_factor as f32;
            // line_height is a multiplier (like CSS), so convert to pixels for cosmic-text
            let base_line_height = self
                .line_height
                .map(|lh| lh * base_font_size)
                .unwrap_or(base_font_size * 1.2);

            let text_align = match context.text_style_state.text_align {
                crate::style::text_align::TextAlign::Left
                | crate::style::text_align::TextAlign::Start => freya_render_api::TextAlign::Left,
                crate::style::text_align::TextAlign::Right
                | crate::style::text_align::TextAlign::End => freya_render_api::TextAlign::Right,
                crate::style::text_align::TextAlign::Center => freya_render_api::TextAlign::Center,
                crate::style::text_align::TextAlign::Justify => {
                    freya_render_api::TextAlign::Justify
                }
            };

            let base_color = freya_render_api::Color::rgba(
                context.text_style_state.color.r(),
                context.text_style_state.color.g(),
                context.text_style_state.color.b(),
                context.text_style_state.color.a(),
            );

            let spans: Vec<(String, freya_render_api::Color, f32, f32, Vec<String>)> = self
                .spans
                .iter()
                .map(|span| {
                    let font_size = f32::from(
                        span.text_style_data
                            .font_size
                            .unwrap_or(context.text_style_state.font_size),
                    ) * context.scale_factor as f32;
                    let font_weight = f32::from(
                        span.text_style_data
                            .font_weight
                            .unwrap_or(context.text_style_state.font_weight),
                    );
                    let color = span
                        .text_style_data
                        .color
                        .unwrap_or(context.text_style_state.color);
                    let mut font_families: Vec<String> = span
                        .text_style_data
                        .font_families
                        .iter()
                        .map(|f| f.to_string())
                        .collect();
                    for fallback in context.fallback_fonts.iter() {
                        if !font_families.contains(&fallback.to_string()) {
                            font_families.push(fallback.to_string());
                        }
                    }
                    (
                        span.text.to_string(),
                        freya_render_api::Color::rgba(color.r(), color.g(), color.b(), color.a()),
                        font_size,
                        font_weight,
                        font_families,
                    )
                })
                .collect();

            let paragraph = CosmicParagraph::new_with_spans(
                &spans,
                context.area_size.width,
                base_font_size,
                base_line_height,
                base_color,
                self.max_lines,
                text_align,
            );
            let size = paragraph.size();

            Some((Size2D::new(size.width, size.height), Rc::new(paragraph)))
        }

        #[cfg(not(feature = "cpu-renderer"))]
        {
            let cached_paragraph = CachedParagraph {
                text_style_state: context.text_style_state,
                spans: &self.spans,
                max_lines: self.max_lines,
                line_height: self.line_height,
                width: context.area_size.width,
            };
            let paragraph = context
                .text_cache
                .utilize(context.node_id, &cached_paragraph)
                .unwrap_or_else(|| {
                    let mut paragraph_style = ParagraphStyle::default();
                    let mut text_style = TextStyle::default();

                    let mut font_families = context.text_style_state.font_families.clone();
                    font_families.extend_from_slice(context.fallback_fonts);

                    text_style.set_color(context.text_style_state.color);
                    text_style.set_font_size(
                        f32::from(context.text_style_state.font_size) * context.scale_factor as f32,
                    );
                    text_style.set_font_families(&font_families);
                    text_style.set_font_style(FontStyle::new(
                        context.text_style_state.font_weight.into(),
                        context.text_style_state.font_width.into(),
                        context.text_style_state.font_slant.into(),
                    ));

                    if context.text_style_state.text_height.needs_custom_height() {
                        text_style.set_height_override(true);
                        text_style.set_half_leading(true);
                    }

                    if let Some(line_height) = self.line_height {
                        text_style.set_height_override(true);
                        text_style.set_height(line_height);
                    }

                    for text_shadow in context.text_style_state.text_shadows.iter() {
                        text_style.add_shadow((*text_shadow).into());
                    }

                    if let Some(ellipsis) = context.text_style_state.text_overflow.get_ellipsis() {
                        paragraph_style.set_ellipsis(ellipsis);
                    }

                    paragraph_style.set_text_style(&text_style);
                    paragraph_style.set_max_lines(self.max_lines);
                    paragraph_style.set_text_align(context.text_style_state.text_align.into());

                    let mut paragraph_builder =
                        ParagraphBuilder::new(&paragraph_style, &*context.font_collection);

                    for span in &self.spans {
                        let text_style_state = TextStyleState::from_data(
                            context.text_style_state,
                            &span.text_style_data,
                        );
                        let mut text_style = TextStyle::new();
                        let mut font_families = context.text_style_state.font_families.clone();
                        font_families.extend_from_slice(context.fallback_fonts);

                        for text_shadow in text_style_state.text_shadows.iter() {
                            text_style.add_shadow((*text_shadow).into());
                        }

                        text_style.set_color(text_style_state.color);
                        text_style.set_font_size(
                            f32::from(text_style_state.font_size) * context.scale_factor as f32,
                        );
                        text_style.set_font_families(&font_families);
                        text_style.set_font_style(FontStyle::new(
                            text_style_state.font_weight.into(),
                            text_style_state.font_width.into(),
                            text_style_state.font_slant.into(),
                        ));
                        text_style.set_decoration_type(text_style_state.text_decoration.into());
                        if let Some(line_height) = self.line_height {
                            text_style.set_height_override(true);
                            text_style.set_height(line_height);
                        }
                        paragraph_builder.push_style(&text_style);
                        paragraph_builder.add_text(&span.text);
                    }

                    let mut paragraph = paragraph_builder.build();
                    paragraph.layout(
                        if self.max_lines == Some(1)
                            && context.text_style_state.text_align == TextAlign::default()
                            && !paragraph_style.ellipsized()
                        {
                            f32::MAX
                        } else {
                            context.area_size.width + 1.0
                        },
                    );
                    context
                        .text_cache
                        .insert(context.node_id, &cached_paragraph, paragraph)
                });

            let size = Size2D::new(paragraph.longest_line(), paragraph.height());

            self.sk_paragraph
                .0
                .borrow_mut()
                .replace(ParagraphHolderInner {
                    paragraph,
                    scale_factor: context.scale_factor,
                });

            Some((size, Rc::new(())))
        }
    }

    fn should_hook_measurement(&self) -> bool {
        true
    }

    fn should_measure_inner_children(&self) -> bool {
        false
    }

    fn events_handlers(&'_ self) -> Option<Cow<'_, FxHashMap<EventName, EventHandlerType>>> {
        Some(Cow::Borrowed(&self.event_handlers))
    }

    fn render(&self, context: RenderContext) {
        let paragraph = self.sk_paragraph.0.borrow();
        let ParagraphHolderInner { paragraph, .. } = paragraph.as_ref().unwrap();
        let visible_area = context.layout_node.visible_area();

        let cursor_area = match self.cursor_mode {
            CursorMode::Fit => visible_area,
            CursorMode::Expanded => context.layout_node.area,
        };

        let paragraph_height = paragraph.height();
        let area_height = visible_area.height();
        let vertical_offset = match self.vertical_align {
            VerticalAlign::Start => 0.0,
            VerticalAlign::Center => (area_height - paragraph_height).max(0.0) / 2.0,
        };

        let cursor_vertical_offset = match self.cursor_mode {
            CursorMode::Fit => vertical_offset,
            CursorMode::Expanded => 0.0,
        };
        let cursor_vertical_size_offset = match self.cursor_mode {
            CursorMode::Fit => 0.,
            CursorMode::Expanded => vertical_offset * 2.,
        };

        // Draw highlights
        for (from, to) in self.highlights.iter() {
            if from == to {
                continue;
            }
            let (from, to) = { if from < to { (from, to) } else { (to, from) } };
            let rects = paragraph.get_rects_for_range(
                *from..*to,
                RectHeightStyle::Tight,
                RectWidthStyle::Tight,
            );

            let mut highlights_paint = Paint::default();
            highlights_paint.set_anti_alias(true);
            highlights_paint.set_style(PaintStyle::Fill);
            highlights_paint.set_color(self.cursor_style_data.highlight_color);

            if rects.is_empty() && *from == 0 {
                let avg_line_height =
                    paragraph.height() / paragraph.get_line_metrics().len().max(1) as f32;
                let rect = SkRect::new(
                    cursor_area.min_x(),
                    cursor_area.min_y() + cursor_vertical_offset,
                    cursor_area.min_x() + 6.,
                    cursor_area.min_y() + avg_line_height + cursor_vertical_size_offset,
                );

                context.canvas.draw_rect(rect, &highlights_paint);
            }

            for rect in rects {
                let rect = SkRect::new(
                    cursor_area.min_x() + rect.rect.left,
                    cursor_area.min_y() + rect.rect.top + cursor_vertical_offset,
                    cursor_area.min_x() + rect.rect.right.max(6.),
                    cursor_area.min_y() + rect.rect.bottom + cursor_vertical_size_offset,
                );
                context.canvas.draw_rect(rect, &highlights_paint);
            }
        }

        // We exclude those highlights that on the same start and end (e.g the user just started dragging)
        let visible_highlights = self
            .highlights
            .iter()
            .filter(|highlight| highlight.0 != highlight.1)
            .count()
            > 0;

        // Draw block cursor behind text if needed
        if let Some(cursor_index) = self.cursor_index
            && self.cursor_style == CursorStyle::Block
            && let Some(cursor_rect) = paragraph
                .get_rects_for_range(
                    cursor_index..cursor_index + 1,
                    RectHeightStyle::Tight,
                    RectWidthStyle::Tight,
                )
                .first()
                .map(|text| text.rect)
                .or_else(|| {
                    // Show the cursor at the end of the text if possible
                    let text_len = paragraph
                        .get_glyph_position_at_coordinate((f32::MAX, f32::MAX))
                        .position as usize;
                    let last_rects = paragraph.get_rects_for_range(
                        text_len.saturating_sub(1)..text_len,
                        RectHeightStyle::Tight,
                        RectWidthStyle::Tight,
                    );

                    if let Some(last_rect) = last_rects.first() {
                        let mut caret = last_rect.rect;
                        caret.left = caret.right;
                        Some(caret)
                    } else {
                        let avg_line_height =
                            paragraph.height() / paragraph.get_line_metrics().len().max(1) as f32;
                        Some(SkRect::new(0., 0., 6., avg_line_height))
                    }
                })
        {
            let width = (cursor_rect.right - cursor_rect.left).max(6.0);
            let cursor_rect = SkRect::new(
                cursor_area.min_x() + cursor_rect.left,
                cursor_area.min_y() + cursor_rect.top + cursor_vertical_offset,
                cursor_area.min_x() + cursor_rect.left + width,
                cursor_area.min_y() + cursor_rect.bottom + cursor_vertical_size_offset,
            );

            let mut paint = Paint::default();
            paint.set_anti_alias(true);
            paint.set_style(PaintStyle::Fill);
            paint.set_color(self.cursor_style_data.color);

            context.canvas.draw_rect(cursor_rect, &paint);
        }

        // Draw text (always uses visible_area with vertical_offset)
        paragraph.paint(
            context.canvas,
            (visible_area.min_x(), visible_area.min_y() + vertical_offset),
        );

        // Draw cursor
        if let Some(cursor_index) = self.cursor_index
            && !visible_highlights
        {
            let cursor_rects = paragraph.get_rects_for_range(
                cursor_index..cursor_index + 1,
                RectHeightStyle::Tight,
                RectWidthStyle::Tight,
            );
            if let Some(cursor_rect) = cursor_rects.first().map(|text| text.rect).or_else(|| {
                // Show the cursor at the end of the text if possible
                let text_len = paragraph
                    .get_glyph_position_at_coordinate((f32::MAX, f32::MAX))
                    .position as usize;
                let last_rects = paragraph.get_rects_for_range(
                    text_len.saturating_sub(1)..text_len,
                    RectHeightStyle::Tight,
                    RectWidthStyle::Tight,
                );

                if let Some(last_rect) = last_rects.first() {
                    let mut caret = last_rect.rect;
                    caret.left = caret.right;
                    Some(caret)
                } else {
                    None
                }
            }) {
                let paint_color = self.cursor_style_data.color;
                match self.cursor_style {
                    CursorStyle::Underline => {
                        let thickness = 2.0;
                        let underline_rect = SkRect::new(
                            cursor_area.min_x() + cursor_rect.left,
                            cursor_area.min_y() + cursor_rect.bottom - thickness
                                + cursor_vertical_offset,
                            cursor_area.min_x() + cursor_rect.right,
                            cursor_area.min_y() + cursor_rect.bottom + cursor_vertical_size_offset,
                        );

                        let mut paint = Paint::default();
                        paint.set_anti_alias(true);
                        paint.set_style(PaintStyle::Fill);
                        paint.set_color(paint_color);

                        context.canvas.draw_rect(underline_rect, &paint);
                    }
                    CursorStyle::Line => {
                        let cursor_rect = SkRect::new(
                            cursor_area.min_x() + cursor_rect.left,
                            cursor_area.min_y() + cursor_rect.top + cursor_vertical_offset,
                            cursor_area.min_x() + cursor_rect.left + 2.,
                            cursor_area.min_y() + cursor_rect.bottom + cursor_vertical_size_offset,
                        );

                        let mut paint = Paint::default();
                        paint.set_anti_alias(true);
                        paint.set_style(PaintStyle::Fill);
                        paint.set_color(paint_color);

                        context.canvas.draw_rect(cursor_rect, &paint);
                    }
                    _ => {}
                }
            }
        }
    }

    #[cfg(feature = "cpu-renderer")]
    fn render_cpu(&self, context: CpuRenderContext) {
        let Some(layout_data) = context.layout_node.data.as_ref() else {
            return;
        };
        let Some(paragraph) = layout_data.downcast_ref::<CosmicParagraph>() else {
            return;
        };

        let visible_area = context.layout_node.visible_area();
        let paragraph_size = paragraph.size();
        let vertical_offset = match self.vertical_align {
            VerticalAlign::Start => 0.0,
            VerticalAlign::Center => (visible_area.height() - paragraph_size.height).max(0.0) / 2.0,
        };

        let cursor_area = match self.cursor_mode {
            CursorMode::Fit => visible_area,
            CursorMode::Expanded => context.layout_node.area,
        };

        let cursor_vertical_offset = match self.cursor_mode {
            CursorMode::Fit => vertical_offset,
            CursorMode::Expanded => 0.0,
        };
        let cursor_vertical_size_offset = match self.cursor_mode {
            CursorMode::Fit => 0.,
            CursorMode::Expanded => vertical_offset * 2.,
        };

        // Draw highlights
        for (from, to) in self.highlights.iter() {
            if from == to {
                continue;
            }
            let (from, to) = { if from < to { (from, to) } else { (to, from) } };
            let rects = paragraph.rects_for_range(*from..*to);

            if rects.is_empty() && *from == 0 {
                let avg_line_height = paragraph_size.height
                    / context.layout_node.area.height().max(1.0)
                    * paragraph_size.height.max(1.0);
                context.cmds.fill_rect(
                    freya_render_api::Rect::new(
                        cursor_area.min_x(),
                        cursor_area.min_y() + cursor_vertical_offset,
                        cursor_area.min_x() + 6.0,
                        cursor_area.min_y() + avg_line_height + cursor_vertical_size_offset,
                    ),
                    freya_render_api::Color::rgba(
                        self.cursor_style_data.highlight_color.r(),
                        self.cursor_style_data.highlight_color.g(),
                        self.cursor_style_data.highlight_color.b(),
                        self.cursor_style_data.highlight_color.a(),
                    ),
                );
            }

            for rect in rects {
                context.cmds.fill_rect(
                    freya_render_api::Rect::new(
                        cursor_area.min_x() + rect.left,
                        cursor_area.min_y() + rect.top + cursor_vertical_offset,
                        (cursor_area.min_x() + rect.right).max(6.0),
                        cursor_area.min_y() + rect.bottom + cursor_vertical_size_offset,
                    ),
                    freya_render_api::Color::rgba(
                        self.cursor_style_data.highlight_color.r(),
                        self.cursor_style_data.highlight_color.g(),
                        self.cursor_style_data.highlight_color.b(),
                        self.cursor_style_data.highlight_color.a(),
                    ),
                );
            }
        }

        let visible_highlights = self
            .highlights
            .iter()
            .filter(|highlight| highlight.0 != highlight.1)
            .count()
            > 0;

        // Draw block cursor behind text if needed
        if let Some(cursor_index) = self.cursor_index
            && self.cursor_style == CursorStyle::Block
        {
            let cursor_rects = paragraph.rects_for_range(cursor_index..cursor_index + 1);
            let cursor_rect = cursor_rects.first().copied().or_else(|| {
                let text_len = paragraph
                    .hit_test_point(freya_render_api::Point::new(f32::MAX, f32::MAX))
                    .byte_index;
                let last_rects = paragraph.rects_for_range(text_len.saturating_sub(1)..text_len);
                if let Some(last_rect) = last_rects.first() {
                    Some(freya_render_api::Rect::new(
                        last_rect.right,
                        last_rect.top,
                        last_rect.right + 6.0,
                        last_rect.bottom,
                    ))
                } else {
                    let avg_line_height = paragraph_size
                        .height
                        .max(context.layout_node.area.height().max(1.0));
                    Some(freya_render_api::Rect::new(0.0, 0.0, 6.0, avg_line_height))
                }
            });

            if let Some(cursor_rect) = cursor_rect {
                let width = (cursor_rect.right - cursor_rect.left).max(6.0);
                context.cmds.fill_rect(
                    freya_render_api::Rect::new(
                        cursor_area.min_x() + cursor_rect.left,
                        cursor_area.min_y() + cursor_rect.top + cursor_vertical_offset,
                        cursor_area.min_x() + cursor_rect.left + width,
                        cursor_area.min_y() + cursor_rect.bottom + cursor_vertical_size_offset,
                    ),
                    freya_render_api::Color::rgba(
                        self.cursor_style_data.color.r(),
                        self.cursor_style_data.color.g(),
                        self.cursor_style_data.color.b(),
                        self.cursor_style_data.color.a(),
                    ),
                );
            }
        }

        // Draw text
        context.cmds.draw_paragraph(
            paragraph,
            freya_render_api::Point::new(
                visible_area.min_x(),
                visible_area.min_y() + vertical_offset,
            ),
        );

        // Draw line/underline cursor after text if needed
        if let Some(cursor_index) = self.cursor_index
            && !visible_highlights
            && self.cursor_style != CursorStyle::Block
        {
            let cursor_rects = paragraph.rects_for_range(cursor_index..cursor_index + 1);
            if let Some(cursor_rect) = cursor_rects.first().cloned().or_else(|| {
                let text_len = paragraph
                    .hit_test_point(freya_render_api::Point::new(f32::MAX, f32::MAX))
                    .byte_index;
                let last_rects = paragraph.rects_for_range(text_len.saturating_sub(1)..text_len);
                if let Some(last_rect) = last_rects.first() {
                    let mut caret = *last_rect;
                    caret.left = caret.right;
                    Some(caret)
                } else {
                    None
                }
            }) {
                let paint_color = freya_render_api::Color::rgba(
                    self.cursor_style_data.color.r(),
                    self.cursor_style_data.color.g(),
                    self.cursor_style_data.color.b(),
                    self.cursor_style_data.color.a(),
                );
                match self.cursor_style {
                    CursorStyle::Underline => {
                        let thickness = 2.0;
                        context.cmds.fill_rect(
                            freya_render_api::Rect::new(
                                cursor_area.min_x() + cursor_rect.left,
                                cursor_area.min_y() + cursor_rect.bottom - thickness
                                    + cursor_vertical_offset,
                                cursor_area.min_x() + cursor_rect.right,
                                cursor_area.min_y()
                                    + cursor_rect.bottom
                                    + cursor_vertical_size_offset,
                            ),
                            paint_color,
                        );
                    }
                    CursorStyle::Line => {
                        context.cmds.fill_rect(
                            freya_render_api::Rect::new(
                                cursor_area.min_x() + cursor_rect.left,
                                cursor_area.min_y() + cursor_rect.top + cursor_vertical_offset,
                                cursor_area.min_x() + cursor_rect.left + 2.0,
                                cursor_area.min_y()
                                    + cursor_rect.bottom
                                    + cursor_vertical_size_offset,
                            ),
                            paint_color,
                        );
                    }
                    _ => {}
                }
            }
        }
    }
}

impl From<Paragraph> for Element {
    fn from(value: Paragraph) -> Self {
        Element::Element {
            key: value.key,
            element: Rc::new(value.element),
            elements: vec![],
        }
    }
}

impl KeyExt for Paragraph {
    fn write_key(&mut self) -> &mut DiffKey {
        &mut self.key
    }
}

impl EventHandlersExt for Paragraph {
    fn get_event_handlers(&mut self) -> &mut FxHashMap<EventName, EventHandlerType> {
        &mut self.element.event_handlers
    }
}

impl MaybeExt for Paragraph {}

impl LayerExt for Paragraph {
    fn get_layer(&mut self) -> &mut Layer {
        &mut self.element.relative_layer
    }
}

pub struct Paragraph {
    key: DiffKey,
    element: ParagraphElement,
}

impl LayoutExt for Paragraph {
    fn get_layout(&mut self) -> &mut LayoutData {
        &mut self.element.layout
    }
}

impl ContainerExt for Paragraph {}

impl AccessibilityExt for Paragraph {
    fn get_accessibility_data(&mut self) -> &mut AccessibilityData {
        &mut self.element.accessibility
    }
}

impl TextStyleExt for Paragraph {
    fn get_text_style_data(&mut self) -> &mut TextStyleData {
        &mut self.element.text_style_data
    }
}

impl Paragraph {
    pub fn try_downcast(element: &dyn ElementExt) -> Option<ParagraphElement> {
        (element as &dyn Any)
            .downcast_ref::<ParagraphElement>()
            .cloned()
    }

    pub fn spans_iter(mut self, spans: impl Iterator<Item = Span<'static>>) -> Self {
        let spans = spans.collect::<Vec<Span>>();
        // TODO: Accessible paragraphs
        // self.element.accessibility.builder.set_value(text.clone());
        self.element.spans.extend(spans);
        self
    }

    pub fn span(mut self, span: impl Into<Span<'static>>) -> Self {
        let span = span.into();
        // TODO: Accessible paragraphs
        // self.element.accessibility.builder.set_value(text.clone());
        self.element.spans.push(span);
        self
    }

    pub fn cursor_color(mut self, cursor_color: impl Into<Color>) -> Self {
        self.element.cursor_style_data.color = cursor_color.into();
        self
    }

    pub fn highlight_color(mut self, highlight_color: impl Into<Color>) -> Self {
        self.element.cursor_style_data.highlight_color = highlight_color.into();
        self
    }

    pub fn cursor_style(mut self, cursor_style: impl Into<CursorStyle>) -> Self {
        self.element.cursor_style = cursor_style.into();
        self
    }

    pub fn holder(mut self, holder: ParagraphHolder) -> Self {
        self.element.sk_paragraph = holder;
        self
    }

    pub fn cursor_index(mut self, cursor_index: impl Into<Option<usize>>) -> Self {
        self.element.cursor_index = cursor_index.into();
        self
    }

    pub fn highlights(mut self, highlights: impl Into<Option<Vec<(usize, usize)>>>) -> Self {
        if let Some(highlights) = highlights.into() {
            self.element.highlights = highlights;
        }
        self
    }

    pub fn max_lines(mut self, max_lines: impl Into<Option<usize>>) -> Self {
        self.element.max_lines = max_lines.into();
        self
    }

    pub fn line_height(mut self, line_height: impl Into<Option<f32>>) -> Self {
        self.element.line_height = line_height.into();
        self
    }

    /// Set the cursor mode for the paragraph.
    /// - `CursorMode::Fit`: cursor/highlights use the paragraph's visible_area. VerticalAlign affects cursor positions.
    /// - `CursorMode::Expanded`: cursor/highlights use the paragraph's inner_area. VerticalAlign does NOT affect cursor positions.
    pub fn cursor_mode(mut self, cursor_mode: impl Into<CursorMode>) -> Self {
        self.element.cursor_mode = cursor_mode.into();
        self
    }

    /// Set the vertical alignment for the paragraph text.
    /// This affects how the text is rendered within the paragraph area, but cursor/highlight behavior
    /// depends on the `cursor_mode` setting.
    pub fn vertical_align(mut self, vertical_align: impl Into<VerticalAlign>) -> Self {
        self.element.vertical_align = vertical_align.into();
        self
    }
}

#[derive(Clone, PartialEq, Hash)]
pub struct Span<'a> {
    pub text_style_data: TextStyleData,
    pub text: Cow<'a, str>,
}

impl From<&'static str> for Span<'static> {
    fn from(text: &'static str) -> Self {
        Span {
            text_style_data: TextStyleData::default(),
            text: text.into(),
        }
    }
}

impl From<String> for Span<'static> {
    fn from(text: String) -> Self {
        Span {
            text_style_data: TextStyleData::default(),
            text: text.into(),
        }
    }
}

impl<'a> Span<'a> {
    pub fn new(text: impl Into<Cow<'a, str>>) -> Self {
        Self {
            text: text.into(),
            text_style_data: TextStyleData::default(),
        }
    }
}

impl<'a> TextStyleExt for Span<'a> {
    fn get_text_style_data(&mut self) -> &mut TextStyleData {
        &mut self.text_style_data
    }
}
