use freya::prelude::*;
use std::cell::RefCell;
use std::collections::{HashSet, VecDeque};
use std::fs;
use std::rc::Rc;

use crate::backend;
use crate::gui_text::GUI_TEXT;
use crate::logging;
use crate::models::{
    CompressionType, DocumentItem, DocumentStatus, ImageProcessingType, LogEntry, OutputFormat,
    ProcessingOptions, ProcessingResult,
};
use crate::version::display_version;
use crate::widgets;
use lege::target_profiles;

#[cfg(feature = "debug-logging")]
use lege::dbglog;

const APP_BG: (u8, u8, u8) = (226, 232, 235);
const PANEL_BG: (u8, u8, u8) = (255, 250, 250);
const CARD_BG: (u8, u8, u8) = (255, 250, 250);
const TEXT: (u8, u8, u8) = (20, 20, 20);
const MUTED: (u8, u8, u8) = (90, 90, 90);
const BORDER: (u8, u8, u8) = (128, 128, 128);
const INFO_BG: (u8, u8, u8) = (255, 255, 225);
const SUCCESS_BG: (u8, u8, u8) = (232, 245, 232);
fn retro_theme() -> Theme {
    let mut theme = light_theme();
    theme.name = "lege-retro";
    theme.colors = ColorsSheet {
        primary: Color::from_rgb(226, 232, 235),
        secondary: Color::from_rgb(255, 255, 225),
        tertiary: Color::from_rgb(220, 224, 226),
        info: Color::from_rgb(255, 255, 225),
        background: Color::from_rgb(226, 232, 235),
        surface_primary: Color::from_rgb(226, 232, 235),
        surface_secondary: Color::from_rgb(244, 244, 244),
        surface_tertiary: Color::from_rgb(255, 250, 250),
        surface_inverse: Color::from_rgb(128, 128, 128),
        surface_inverse_secondary: Color::from_rgb(96, 96, 96),
        surface_inverse_tertiary: Color::from_rgb(64, 64, 64),
        border: Color::from_rgb(128, 128, 128),
        border_focus: Color::from_rgb(64, 64, 64),
        text_primary: Color::from_rgb(0, 0, 0),
        text_secondary: Color::from_rgb(70, 70, 70),
        text_placeholder: Color::from_rgb(110, 110, 110),
        text_highlight: Color::from_rgb(40, 40, 40),
        hover: Color::from_rgb(236, 236, 236),
        focus: Color::from_rgb(232, 232, 232),
        active: Color::from_rgb(210, 210, 210),
        disabled: Color::from_rgb(210, 210, 210),
        ..LIGHT_COLORS
    };
    theme
}

#[cfg(feature = "debug-logging")]
fn load_debug_log_file() -> Vec<String> {
    Legencode::get_debug_log_messages()
}

#[cfg(not(feature = "debug-logging"))]
fn load_debug_log_file() -> Vec<String> {
    Vec::new()
}

fn format_eta_label(seconds: u32) -> String {
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    let secs = seconds % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{secs:02}")
    } else {
        format!("{minutes:02}:{secs:02}")
    }
}

#[derive(Clone, Copy)]
enum PopupKind {
    Hardware,
    Output,
    NoLayout,
    Footnotes,
    Ocr,
    Queue,
    Complete,
    AddFileHelp,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum TooltipArea {
    LeftCard,
    RightCard,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AppState {
    pub queue: VecDeque<DocumentItem>,
    pub options: ProcessingOptions,
    pub is_processing: bool,
    pub current_processing_index: Option<usize>,
    pub status_message: String,
    pub status_lines: (String, String, String, String),
    pub status_log: VecDeque<String>,
    pub active_eta: Option<String>,
    pub progress_metrics: Option<lege::progress::ProgressMetrics>,
    pub active_task_ids: Vec<u64>,
    pub should_cancel: bool,

    pub target_height_input: String,
    pub target_resolution_selection: String,
    pub k_factor_input: String,
    pub threshold_input: String,
    pub page_range_input: String,
    pub active_tooltip: Option<(TooltipArea, String)>,

    pub hardware_acceleration_popup: Option<String>,
    pub show_hardware_popup: bool,
    pub output_folder_popup: Option<String>,
    pub show_output_popup: bool,
    pub no_layout_detection_popup: Option<String>,
    pub show_no_layout_popup: bool,
    pub footnotes_popup: Option<String>,
    pub show_footnotes_popup: bool,
    pub ocr_detection_popup: Option<String>,
    pub show_ocr_detection_popup: bool,
    pub queue_operation_popup: Option<String>,
    pub show_queue_operation_popup: bool,
    pub completion_popup: Option<String>,
    pub show_completion_popup: bool,
    pub add_file_help_popup: Option<String>,
    pub show_add_file_help_popup: bool,
    pub show_log_viewer: bool,
    pub processing_log: Vec<LogEntry>,
    pub show_about: bool,
    pub show_queue_viewer: bool,

    #[cfg(feature = "debug-logging")]
    pub show_debug_log: bool,
    #[cfg(feature = "debug-logging")]
    pub debug_log_messages: Vec<String>,
}

impl Default for AppState {
    fn default() -> Self {
        let defaults = ProcessingOptions::new();
        let saved = backend::load_saved_settings().ok().flatten();
        let options = saved.clone().unwrap_or_else(|| defaults.clone());
        let mut state = Self {
            queue: VecDeque::new(),
            options: options.clone(),
            is_processing: false,
            current_processing_index: None,
            status_message: GUI_TEXT.interactive.status.ready.clone(),
            status_lines: (
                GUI_TEXT.interactive.status.ready.clone(),
                String::new(),
                String::new(),
                String::new(),
            ),
            status_log: VecDeque::new(),
            active_eta: None,
            progress_metrics: None,
            active_task_ids: Vec::new(),
            should_cancel: false,
            target_height_input: options.target_height.unwrap_or(1200).to_string(),
            target_resolution_selection: target_profiles::PROPORTIONAL_OPTION_LABEL.to_string(),
            k_factor_input: options.k_factor.to_string(),
            threshold_input: options.threshold_value.to_string(),
            page_range_input: options.page_range.clone().unwrap_or_default(),
            active_tooltip: None,
            hardware_acceleration_popup: None,
            show_hardware_popup: false,
            output_folder_popup: None,
            show_output_popup: false,
            no_layout_detection_popup: None,
            show_no_layout_popup: false,
            footnotes_popup: None,
            show_footnotes_popup: false,
            ocr_detection_popup: None,
            show_ocr_detection_popup: false,
            queue_operation_popup: None,
            show_queue_operation_popup: false,
            completion_popup: None,
            show_completion_popup: false,
            add_file_help_popup: None,
            show_add_file_help_popup: false,
            show_log_viewer: false,
            processing_log: logging::load_log_entries().unwrap_or_default(),
            show_about: false,
            show_queue_viewer: false,
            #[cfg(feature = "debug-logging")]
            show_debug_log: false,
            #[cfg(feature = "debug-logging")]
            debug_log_messages: load_debug_log_file(),
        };

        if let Some(saved) = saved {
            sync_state_from_options(&mut state, &saved);
        }

        state
    }
}

impl AppState {
    fn set_status_message(&mut self, msg: impl Into<String>) {
        let m = msg.into();
        self.status_message = m.clone();
        self.status_lines = (m, String::new(), String::new(), String::new());
        self.progress_metrics = None;
    }

    fn set_status_lines(
        &mut self,
        l1: impl Into<String>,
        l2: impl Into<String>,
        l3: impl Into<String>,
    ) {
        let new_lines = (l1.into(), l2.into(), l3.into(), String::new());
        if self.status_lines != new_lines {
            self.status_lines = new_lines;
        }
    }

    fn push_status_log(&mut self, line: impl Into<String>) {
        let line = line.into();
        if !self.status_log.back().map(|s| s == &line).unwrap_or(false) {
            self.status_log.push_back(line);
            if self.status_log.len() > 100 {
                self.status_log.pop_front();
            }
        }

        let n = self.status_log.len();
        let l1 = self
            .status_log
            .get(n.saturating_sub(1))
            .cloned()
            .unwrap_or_default();
        let l2 = self
            .status_log
            .get(n.saturating_sub(2))
            .cloned()
            .unwrap_or_default();
        let l3 = self
            .status_log
            .get(n.saturating_sub(3))
            .cloned()
            .unwrap_or_default();
        self.status_lines = (l1, l2, l3, self.status_lines.3.clone());
    }
}

fn sync_state_from_options(state: &mut AppState, options: &ProcessingOptions) {
    state.options = options.clone();
    if let Some(device) = options.target_device.clone() {
        if let Some(profile) = target_profiles::find_profile(&device) {
            state.target_resolution_selection = profile.name.to_string();
            state.target_height_input = profile.height.to_string();
            state.options.target_height = Some(profile.height);
        }
    } else {
        state.target_resolution_selection = target_profiles::PROPORTIONAL_OPTION_LABEL.to_string();
        state.target_height_input = options.target_height.unwrap_or(1200).to_string();
    }
    state.k_factor_input = options.k_factor.to_string();
    state.threshold_input = options.threshold_value.to_string();
    state.page_range_input = options.page_range.clone().unwrap_or_default();
}

fn panel_card(title: impl Into<String>, fill_height: bool, children: Vec<Element>) -> Element {
    widgets::lege_panel_card(title, fill_height, children)
}

fn section_label(text: impl Into<String>) -> Element {
    widgets::lege_section_label(text)
}

fn tooltip_wrap(
    mut state: State<AppState>,
    area: TooltipArea,
    text: impl Into<String>,
    child: Element,
) -> Element {
    let text = text.into();
    let show_text = text.clone();
    let clear_text = text.clone();
    let mut leave_state = state;

    rect()
        .on_pointer_over(move |_| {
            state.write().active_tooltip = Some((area, show_text.clone()));
        })
        .on_pointer_out(move |_| {
            let mut s = leave_state.write();
            if s.active_tooltip.as_ref().map(|(_, text)| text.as_str()) == Some(clear_text.as_str())
            {
                s.active_tooltip = None;
            }
        })
        .child(child)
        .into()
}

fn tooltip_wrap_at(
    state: State<AppState>,
    area: TooltipArea,
    text: impl Into<String>,
    _position: AttachedPosition,
    child: Element,
) -> Element {
    tooltip_wrap(state, area, text, child)
}

fn settings_row(label_text: impl Into<String>, control: Element) -> Element {
    widgets::lege_field(label_text, control)
}

fn two_option_toggle(
    label_text: impl Into<String>,
    current_text: impl Into<String>,
    on_press: impl FnMut(Event<PressEventData>) + 'static,
) -> Element {
    widgets::lege_toggle_row(label_text, current_text, on_press)
}

fn two_option_toggle_inline(
    label_text: impl Into<String>,
    current_text: impl Into<String>,
    on_press: impl FnMut(Event<PressEventData>) + 'static,
) -> Element {
    widgets::lege_inline_toggle_row(label_text, current_text, on_press)
}

fn compact_checkbox_row(
    text: impl Into<String>,
    selected: bool,
    mut on_select: impl FnMut(()) + 'static,
) -> Element {
    widgets::lege_checkbox_row(text, selected, move |_| on_select(()))
}

fn popup_message_cards(state: State<AppState>) -> Vec<Element> {
    let read = state.read();
    let mut cards = Vec::new();

    let entries = [
        (
            read.show_hardware_popup,
            read.hardware_acceleration_popup.clone(),
            PopupKind::Hardware,
        ),
        (
            read.show_output_popup,
            read.output_folder_popup.clone(),
            PopupKind::Output,
        ),
        (
            read.show_no_layout_popup,
            read.no_layout_detection_popup.clone(),
            PopupKind::NoLayout,
        ),
        (
            read.show_footnotes_popup,
            read.footnotes_popup.clone(),
            PopupKind::Footnotes,
        ),
        (
            read.show_ocr_detection_popup,
            read.ocr_detection_popup.clone(),
            PopupKind::Ocr,
        ),
        (
            read.show_queue_operation_popup,
            read.queue_operation_popup.clone(),
            PopupKind::Queue,
        ),
        (
            read.show_completion_popup,
            read.completion_popup.clone(),
            PopupKind::Complete,
        ),
        (
            read.show_add_file_help_popup,
            read.add_file_help_popup.clone(),
            PopupKind::AddFileHelp,
        ),
    ];

    for (show, msg, _kind) in entries {
        if show {
            if let Some(msg) = msg {
                cards.push(rail_note_card(msg, 13., INFO_BG));
            }
        }
    }

    cards.into_iter().take(2).collect()
}

fn rail_note_card(text: impl Into<String>, font_size: f32, background: (u8, u8, u8)) -> Element {
    let text = text.into();
    rect()
        .background(background)
        .border(
            Border::new()
                .fill(BORDER)
                .width(1.)
                .alignment(BorderAlignment::Inner),
        )
        .corner_radius(6.)
        .padding((8., 10., 8., 10.))
        .width(Size::fill())
        .child(
            rect().width(Size::fill()).child(
                paragraph()
                    .width(Size::fill())
                    .span(Span::new(text).font_size(font_size).color(TEXT))
                    .line_height(1.25)
                    .max_lines(5),
            ),
        )
        .into()
}

fn tooltip_note_card(text: impl Into<String>) -> Element {
    let text = text.into();
    rect()
        .background(CARD_BG)
        .border(
            Border::new()
                .fill(BORDER)
                .width(1.)
                .alignment(BorderAlignment::Inner),
        )
        .corner_radius(6.)
        .padding((6., 8., 6., 8.))
        .width(Size::fill())
        .child(
            paragraph()
                .width(Size::fill())
                .span(Span::new(text).font_size(11.).color(TEXT))
                .line_height(1.2)
                .max_lines(6),
        )
        .into()
}

fn PopupRail(state: State<AppState>) -> Element {
    let cards = popup_message_cards(state);
    if cards.is_empty() {
        return rect().into();
    }

    rect()
        .width(Size::fill())
        .height(Size::fill())
        .vertical()
        .main_align(Alignment::End)
        .spacing(6.)
        .children(cards)
        .into()
}

fn panel_tooltip_overlay(state: State<AppState>, area: TooltipArea, anchor_right: bool) -> Element {
    let tooltip = state.read().active_tooltip.clone();
    let Some((active_area, text)) = tooltip else {
        return rect().into();
    };
    if active_area != area {
        return rect().into();
    }

    let position = if anchor_right {
        Position::new_absolute().right(8.).bottom(8.)
    } else {
        Position::new_absolute().left(8.).bottom(8.)
    };

    rect()
        .width(Size::px(220.))
        .position(position)
        .child(tooltip_note_card(text))
        .into()
}

fn hide_popup(mut state: State<AppState>, kind: PopupKind) {
    let mut s = state.write();
    match kind {
        PopupKind::Hardware => s.show_hardware_popup = false,
        PopupKind::Output => s.show_output_popup = false,
        PopupKind::NoLayout => s.show_no_layout_popup = false,
        PopupKind::Footnotes => s.show_footnotes_popup = false,
        PopupKind::Ocr => s.show_ocr_detection_popup = false,
        PopupKind::Queue => s.show_queue_operation_popup = false,
        PopupKind::Complete => s.show_completion_popup = false,
        PopupKind::AddFileHelp => s.show_add_file_help_popup = false,
    }
}

fn schedule_popup(mut state: State<AppState>, kind: PopupKind, message: String, seconds: u64) {
    spawn(async move {
        tokio::task::yield_now().await;
        {
            let mut s = state.write();
            match kind {
                PopupKind::Hardware => {
                    s.hardware_acceleration_popup = Some(message);
                    s.show_hardware_popup = true;
                }
                PopupKind::Output => {
                    s.output_folder_popup = Some(message);
                    s.show_output_popup = true;
                }
                PopupKind::NoLayout => {
                    s.no_layout_detection_popup = Some(message);
                    s.show_no_layout_popup = true;
                }
                PopupKind::Footnotes => {
                    s.footnotes_popup = Some(message);
                    s.show_footnotes_popup = true;
                }
                PopupKind::Ocr => {
                    s.ocr_detection_popup = Some(message);
                    s.show_ocr_detection_popup = true;
                }
                PopupKind::Queue => {
                    s.queue_operation_popup = Some(message);
                    s.show_queue_operation_popup = true;
                }
                PopupKind::Complete => {
                    s.completion_popup = Some(message);
                    s.show_completion_popup = true;
                }
                PopupKind::AddFileHelp => {
                    s.add_file_help_popup = Some(message);
                    s.show_add_file_help_popup = true;
                }
            }
        }
        tokio::time::sleep(tokio::time::Duration::from_secs(seconds)).await;
        hide_popup(state, kind);
    });
}

fn sync_text_inputs(
    mut state: State<AppState>,
    target_height_input: State<String>,
    page_range_input: State<String>,
    k_factor_input: State<String>,
    threshold_input: State<String>,
) {
    let target_height_val = target_height_input.read().clone();
    let page_range_val = page_range_input.read().clone();
    let k_factor_val = k_factor_input.read().clone();
    let threshold_val = threshold_input.read().clone();

    use_side_effect(move || {
        let mut s = state.write();

        if s.target_height_input != target_height_val {
            s.target_height_input = target_height_val.clone();
            if s.options.target_device.is_none() {
                if let Ok(height) = target_height_val.trim().parse::<u32>() {
                    s.options.target_height = Some(height);
                }
            }
        }

        if s.page_range_input != page_range_val {
            s.page_range_input = page_range_val.clone();
            s.options.page_range = if page_range_val.trim().is_empty() {
                None
            } else {
                Some(page_range_val.clone())
            };
        }

        if s.k_factor_input != k_factor_val {
            s.k_factor_input = k_factor_val.clone();
            if let Ok(value) = k_factor_val.trim().parse::<f32>() {
                s.options.k_factor = value;
            }
        }

        if s.threshold_input != threshold_val {
            s.threshold_input = threshold_val.clone();
            if let Ok(value) = threshold_val.trim().parse::<u16>() {
                s.options.threshold_value = value.min(255) as u8;
            }
        }
    });
}

pub fn app() -> impl IntoElement {
    use_init_theme(retro_theme);
    let state = use_state(AppState::default);

    let target_height_input = use_state({
        let read = state.read();
        let initial = read.target_height_input.clone();
        move || initial
    });
    let page_range_input = use_state({
        let read = state.read();
        let initial = read.page_range_input.clone();
        move || initial
    });
    let k_factor_input = use_state({
        let read = state.read();
        let initial = read.k_factor_input.clone();
        move || initial
    });
    let threshold_input = use_state({
        let read = state.read();
        let initial = read.threshold_input.clone();
        move || initial
    });

    sync_text_inputs(
        state,
        target_height_input,
        page_range_input,
        k_factor_input,
        threshold_input,
    );

    app_root(
        rect()
            .width(Size::fill())
            .height(Size::fill())
            .child(widgets::lege_main_shell(
                ActionBar(state),
                MainContentArea(
                    state,
                    target_height_input,
                    page_range_input,
                    k_factor_input,
                    threshold_input,
                ),
                PopupRail(state),
                StartBar(state, page_range_input),
                StatusBar(state),
            ))
            .child(QueueViewerPopup(state))
            .child(LogViewerPopup(state))
            .child(AboutPopup(state))
            .child(DebugLogViewerPopup(state)),
    )
}

#[cfg(target_os = "windows")]
fn app_root(content: Rect) -> Rect {
    content.font_family("Segoe UI")
}

#[cfg(not(target_os = "windows"))]
fn app_root(content: Rect) -> Rect {
    content
}

fn ActionBar(state: State<AppState>) -> Element {
    let output_text = state
        .read()
        .options
        .output_path
        .as_ref()
        .map(|path| backend::truncate_path(path, 42))
        .unwrap_or_else(|| GUI_TEXT.interactive.buttons.output_directory.clone());

    widgets::lege_toolbar(
        tooltip_wrap_at(
            state,
            TooltipArea::LeftCard,
            GUI_TEXT.interactive.tooltips.add_file_or_folder.clone(),
            AttachedPosition::Right,
            Button::new()
                .width(Size::fill())
                .height(Size::px(40.))
                .on_press(move |_| add_files(state))
                .child(GUI_TEXT.interactive.buttons.add_file.clone())
                .into(),
        ),
        tooltip_wrap_at(
            state,
            TooltipArea::LeftCard,
            GUI_TEXT.interactive.tooltips.add_file_or_folder.clone(),
            AttachedPosition::Right,
            Button::new()
                .width(Size::fill())
                .height(Size::px(40.))
                .on_press(move |_| add_folder(state))
                .child(GUI_TEXT.interactive.buttons.add_folder.clone())
                .into(),
        ),
        Button::new()
            .width(Size::fill())
            .height(Size::px(40.))
            .on_press(move |_| choose_output_folder(state))
            .child(output_text)
            .into(),
        rect()
            .width(Size::fill())
            .height(Size::fill())
            .vertical()
            .spacing(4.)
            .main_align(Alignment::Center)
            .child(
                Button::new()
                    .width(Size::fill())
                    .height(Size::px(18.))
                    .on_press(move |_| save_settings_action(state))
                    .child(GUI_TEXT.interactive.buttons.save.clone()),
            )
            .child(
                Button::new()
                    .width(Size::fill())
                    .height(Size::px(18.))
                    .on_press(move |_| reset_settings_action(state))
                    .child(GUI_TEXT.interactive.buttons.reset.clone()),
            )
            .into(),
    )
}

fn MainContentArea(
    state: State<AppState>,
    target_height_input: State<String>,
    page_range_input: State<String>,
    k_factor_input: State<String>,
    threshold_input: State<String>,
) -> Element {
    widgets::lege_settings_grid(
        CombinedSettingsPanel(state, k_factor_input, threshold_input),
        CenterSettingsPanel(state, target_height_input, page_range_input),
    )
}

fn StartBar(state: State<AppState>, page_range_input: State<String>) -> Element {
    let is_processing = {
        let s = state.read();
        s.is_processing
    };

    let button_text = if is_processing {
        GUI_TEXT.interactive.buttons.cancel.clone()
    } else {
        GUI_TEXT.interactive.buttons.start_processing.clone()
    };

    rect()
        .width(Size::fill())
        .height(Size::px(56.))
        .vertical()
        .main_align(Alignment::Center)
        .cross_align(Alignment::Center)
        .child(
            Button::new()
                .width(Size::px(146.))
                .height(Size::px(40.))
                .on_press(move |_| start_or_cancel_processing(state, page_range_input))
                .child(button_text),
        )
        .into()
}

fn CombinedSettingsPanel(
    state: State<AppState>,
    k_factor_input: State<String>,
    threshold_input: State<String>,
) -> Element {
    let options = state.read().options.clone();
    let show_base_format =
        matches!(options.output_format, OutputFormat::Pdf) && !options.layout_analysis;
    let fixed_threshold_enabled = options.use_fixed_threshold;
    let effective_threshold = fixed_threshold_enabled;
    let threshold_checkbox_selected = fixed_threshold_enabled;
    let numeric_label = if effective_threshold {
        "Threshold"
    } else {
        "K factor"
    };

    rect()
        .width(Size::fill())
        .height(Size::fill())
        .child(panel_card(
            "",
            true,
            vec![
                rect()
                    .width(Size::fill())
                    .height(Size::fill())
                    .direction(Direction::Horizontal)
                    .cross_align(Alignment::Start)
                    .spacing(10.)
                    .child(
                    rect()
                        .width(Size::percent(64.))
                        .height(Size::fill())
                        .vertical()
                        .main_align(Alignment::Start)
                        .spacing(6.)
                        .child(two_option_toggle_inline(
                            GUI_TEXT.interactive.labels.output_format.clone(),
                            options.output_format.to_string(),
                            {
                                let mut state = state;
                                move |_| {
                                    let next = match state.read().options.output_format {
                                        OutputFormat::Pdf => OutputFormat::Djvu,
                                        OutputFormat::Djvu => OutputFormat::Pdf,
                                    };
                                    state.write().options.output_format = next;
                                }
                            },
                        ))
                        .child(tooltip_wrap_at(
                            state,
                            TooltipArea::LeftCard,
                            GUI_TEXT.interactive.tooltips.image_output_type.clone(),
                            AttachedPosition::Right,
                            two_option_toggle_inline(
                                GUI_TEXT.interactive.labels.image_output_type.clone(),
                                options.image_processing_type.to_string(),
                                {
                                    let mut state = state;
                                    move |_| {
                                        let next = match state.read().options.image_processing_type {
                                            ImageProcessingType::Original => {
                                                ImageProcessingType::Dithered
                                            }
                                            ImageProcessingType::Dithered => {
                                                ImageProcessingType::Original
                                            }
                                        };
                                        state.write().options.image_processing_type = next;
                                    }
                                },
                            ),
                        ))
                        .child(tooltip_wrap_at(
                            state,
                            TooltipArea::LeftCard,
                            GUI_TEXT.interactive.tooltips.layout_detection.clone(),
                            AttachedPosition::Right,
                            two_option_toggle_inline(
                                GUI_TEXT.interactive.labels.layout_detection.clone(),
                                if options.layout_analysis { "On" } else { "Off" },
                                {
                                    let mut state = state;
                                    move |_| {
                                        let should_show_popup = {
                                            let mut s = state.write();
                                            s.options.layout_analysis = !s.options.layout_analysis;
                                            !s.options.layout_analysis
                                        };
                                        if should_show_popup {
                                            schedule_popup(
                                                state,
                                                PopupKind::NoLayout,
                                                "Layout detection disabled. Text/image separation and image-preserving dithering are unavailable.".to_string(),
                                                5,
                                            );
                                        }
                                    }
                                },
                            ),
                        ))
                        .maybe_child(if show_base_format {
                            Some::<Element>(tooltip_wrap_at(
                                state,
                                TooltipArea::LeftCard,
                                GUI_TEXT.interactive.tooltips.base_format.clone(),
                                AttachedPosition::Right,
                                two_option_toggle_inline(
                                    GUI_TEXT.interactive.labels.base_format.clone(),
                                    options.compression_type.to_string(),
                                    {
                                        let mut state = state;
                                        move |_| {
                                            let next = match state.read().options.compression_type {
                                                CompressionType::Ccitt4 => CompressionType::Jbig2,
                                                CompressionType::Jbig2 => CompressionType::Ccitt4,
                                            };
                                            state.write().options.compression_type = next;
                                        }
                                    },
                                )
                                .into(),
                            ))
                        } else {
                            None::<Element>
                        })
                        .child(
                            rect()
                                .padding((0., 6., 0., 0.))
                                .spacing(3.)
                                .child(tooltip_wrap_at(
                                    state,
                                    TooltipArea::LeftCard,
                                    GUI_TEXT.interactive.tooltips.inverted_colors.clone(),
                                    AttachedPosition::Right,
                                    compact_checkbox_row(
                                        GUI_TEXT.interactive.labels.inverted_colors.clone(),
                                        options.invert_input,
                                        {
                                            let mut state = state;
                                            move |_| {
                                                let mut s = state.write();
                                                s.options.invert_input = !s.options.invert_input;
                                            }
                                        },
                                    ),
                                ))
                                .child(tooltip_wrap_at(
                                    state,
                                    TooltipArea::LeftCard,
                                    GUI_TEXT.interactive.tooltips.ocr_text_layer.clone(),
                                    AttachedPosition::Right,
                                    compact_checkbox_row(
                                        GUI_TEXT.interactive.labels.ocr_text_layer.clone(),
                                        options.use_ocr,
                                        {
                                            let mut state = state;
                                            move |_| {
                                                let mut s = state.write();
                                                s.options.use_ocr = !s.options.use_ocr;
                                            }
                                        },
                                    ),
                                ))
                                .child(tooltip_wrap_at(
                                    state,
                                    TooltipArea::LeftCard,
                                    GUI_TEXT.interactive.tooltips.high_quality_output.clone(),
                                    AttachedPosition::Right,
                                    compact_checkbox_row(
                                        GUI_TEXT.interactive.labels.high_quality_output.clone(),
                                        options.high_quality_output,
                                        {
                                            let mut state = state;
                                            move |_| {
                                                let mut s = state.write();
                                                s.options.high_quality_output =
                                                    !s.options.high_quality_output;
                                            }
                                        },
                                    ),
                                ))
                                .child(tooltip_wrap_at(
                                    state,
                                    TooltipArea::LeftCard,
                                    GUI_TEXT.interactive.tooltips.jpeg_compatibility.clone(),
                                    AttachedPosition::Right,
                                    compact_checkbox_row(
                                        GUI_TEXT.interactive.labels.jpeg_compatibility.clone(),
                                        options.jpeg_compat,
                                        {
                                            let mut state = state;
                                            move |_| {
                                                let mut s = state.write();
                                                s.options.jpeg_compat = !s.options.jpeg_compat;
                                            }
                                        },
                                    ),
                                )),
                        )
                    )
                    .child(
                    rect()
                        .width(Size::percent(31.))
                        .height(Size::fill())
                        .vertical()
                        .main_align(Alignment::Start)
                        .spacing(3.)
                        .child(section_label("Binarization"))
                        .child(tooltip_wrap(
                            state,
                            TooltipArea::LeftCard,
                            if effective_threshold {
                                GUI_TEXT.interactive.tooltips.threshold_value.clone()
                            } else {
                                GUI_TEXT.interactive.tooltips.sauvola_k_factor.clone()
                            },
                            settings_row(
                                numeric_label.to_string(),
                                if effective_threshold {
                                    Input::new(threshold_input)
                                        .placeholder("200")
                                        .width(Size::px(84.))
                                        .into()
                                } else {
                                    Input::new(k_factor_input)
                                        .placeholder("0.2")
                                        .width(Size::px(84.))
                                        .into()
                                },
                            ),
                        ))
                        .child(
                            rect()
                                .padding((0., 4., 0., 0.))
                                .spacing(3.)
                                .child(tooltip_wrap(
                                    state,
                                    TooltipArea::LeftCard,
                                    GUI_TEXT.interactive.tooltips.threshold_value.clone(),
                                    compact_checkbox_row(
                                        "Fixed threshold",
                                        threshold_checkbox_selected,
                                        {
                                            let mut state = state;
                                            move |_| {
                                                let mut s = state.write();
                                                let enable = !s.options.use_fixed_threshold;
                                                s.options.use_fixed_threshold = enable;
                                                if enable {
                                                    s.options.use_heavy_binarization = false;
                                                }
                                            }
                                        },
                                    ),
                                ))
                                .child(tooltip_wrap(
                                    state,
                                    TooltipArea::LeftCard,
                                    GUI_TEXT.interactive.tooltips.heavy_model.clone(),
                                    compact_checkbox_row(
                                        "Heavy model",
                                        options.use_heavy_binarization,
                                        {
                                            let mut state = state;
                                            move |_| {
                                                let mut s = state.write();
                                                if !s.options.use_fixed_threshold {
                                                    s.options.use_heavy_binarization =
                                                        !s.options.use_heavy_binarization;
                                                }
                                            }
                                        },
                                    )
                                    .into(),
                                )),
                        ),
                    )
                    .into(),
            ],
        ))
        .child(panel_tooltip_overlay(state, TooltipArea::LeftCard, true))
        .into()
}

fn LeftSettingsPanel(state: State<AppState>) -> Element {
    let options = state.read().options.clone();
    let show_base_format =
        matches!(options.output_format, OutputFormat::Pdf) && !options.layout_analysis;

    panel_card(
        "",
        true,
        vec![
            two_option_toggle(
                GUI_TEXT.interactive.labels.output_format.clone(),
                options.output_format.to_string(),
                {
                    let mut state = state;
                    move |_| {
                        let next = match state.read().options.output_format {
                            OutputFormat::Pdf => OutputFormat::Djvu,
                            OutputFormat::Djvu => OutputFormat::Pdf,
                        };
                        state.write().options.output_format = next;
                    }
                },
            ),
            two_option_toggle(
                GUI_TEXT.interactive.labels.image_output_type.clone(),
                options.image_processing_type.to_string(),
                {
                    let mut state = state;
                    move |_| {
                        let next = match state.read().options.image_processing_type {
                            ImageProcessingType::Original => ImageProcessingType::Dithered,
                            ImageProcessingType::Dithered => ImageProcessingType::Original,
                        };
                        state.write().options.image_processing_type = next;
                    }
                },
            ),
            if show_base_format {
                two_option_toggle(
                    GUI_TEXT.interactive.labels.base_format.clone(),
                    options.compression_type.to_string(),
                    {
                        let mut state = state;
                        move |_| {
                            let next = match state.read().options.compression_type {
                                CompressionType::Ccitt4 => CompressionType::Jbig2,
                                CompressionType::Jbig2 => CompressionType::Ccitt4,
                            };
                            state.write().options.compression_type = next;
                        }
                    },
                )
            } else {
                rect().height(Size::px(0.)).into()
            },
            two_option_toggle(
                GUI_TEXT.interactive.labels.layout_detection.clone(),
                if options.layout_analysis { "On" } else { "Off" },
                {
                    let mut state = state;
                    move |_| {
                        let mut s = state.write();
                        s.options.layout_analysis = !s.options.layout_analysis;
                        if !s.options.layout_analysis {
                            schedule_popup(
                                state,
                                PopupKind::NoLayout,
                                "Layout detection disabled. Text/image separation and image-preserving dithering are unavailable.".to_string(),
                                5,
                            );
                        }
                    }
                },
            ),
            compact_checkbox_row(
                GUI_TEXT.interactive.labels.inverted_colors.clone(),
                options.invert_input,
                {
                    let mut state = state;
                    move |_| state.write().options.invert_input = !state.read().options.invert_input
                },
            ),
            compact_checkbox_row(
                GUI_TEXT.interactive.labels.ocr_text_layer.clone(),
                options.use_ocr,
                {
                    let mut state = state;
                    move |_| state.write().options.use_ocr = !state.read().options.use_ocr
                },
            ),
            compact_checkbox_row(
                GUI_TEXT.interactive.labels.high_quality_output.clone(),
                options.high_quality_output,
                {
                    let mut state = state;
                    move |_| {
                        state.write().options.high_quality_output =
                            !state.read().options.high_quality_output
                    }
                },
            ),
        ],
    )
}

fn CenterSettingsPanel(
    state: State<AppState>,
    target_height_input: State<String>,
    page_range_input: State<String>,
) -> Element {
    let options = state.read().options.clone();

    let mut device_items = Vec::new();
    device_items.push(
        MenuItem::new()
            .selected(options.target_device.is_none())
            .on_press({
                let mut state = state;
                let mut target_height_input = target_height_input;
                move |_| {
                    let mut s = state.write();
                    s.options.target_device = None;
                    s.target_resolution_selection =
                        target_profiles::PROPORTIONAL_OPTION_LABEL.to_string();
                    target_height_input.set(s.options.target_height.unwrap_or(1200).to_string());
                }
            })
            .child(target_profiles::PROPORTIONAL_OPTION_LABEL.to_string())
            .into(),
    );

    for profile in target_profiles::TARGET_DEVICE_PROFILES.iter() {
        let selected = options.target_device.as_deref() == Some(profile.name);
        let name = profile.name.to_string();
        let item_name = name.clone();
        let height = profile.height;
        device_items.push(
            MenuItem::new()
                .selected(selected)
                .on_press({
                    let mut state = state;
                    let mut target_height_input = target_height_input;
                    let name = name.clone();
                    move |_| {
                        let mut s = state.write();
                        s.options.target_device = Some(name.clone());
                        s.options.target_height = Some(height);
                        s.target_resolution_selection = name.clone();
                        target_height_input.set(height.to_string());
                    }
                })
                .child(item_name)
                .into(),
        );
    }

    rect()
        .width(Size::fill())
        .height(Size::fill())
        .child(panel_card(
            "",
            true,
            vec![
                rect()
                    .width(Size::fill())
                    .height(Size::fill())
                    .direction(Direction::Horizontal)
                    .cross_align(Alignment::Start)
                    .spacing(12.)
                    .child(
                        rect()
                            .width(Size::percent(54.))
                            .height(Size::fill())
                            .vertical()
                            .main_align(Alignment::Start)
                            .spacing(3.)
                            .child(settings_row(
                                GUI_TEXT.interactive.labels.page_range.clone(),
                                Input::new(page_range_input)
                                    .placeholder(
                                        GUI_TEXT.interactive.labels.page_range_placeholder.clone(),
                                    )
                                    .expanded()
                                    .into(),
                            ))
                            .child(tooltip_wrap_at(
                                state,
                                TooltipArea::RightCard,
                                GUI_TEXT.interactive.tooltips.target_height.clone(),
                                AttachedPosition::Bottom,
                                settings_row(
                                    "Target device".to_string(),
                                    Select::new()
                                        .selected_item(
                                            state.read().target_resolution_selection.clone(),
                                        )
                                        .children(device_items)
                                        .into(),
                                ),
                            ))
                            .child(tooltip_wrap_at(
                                state,
                                TooltipArea::RightCard,
                                GUI_TEXT.interactive.tooltips.target_height.clone(),
                                AttachedPosition::Bottom,
                                settings_row(
                                    GUI_TEXT.interactive.labels.target_height.clone(),
                                    Input::new(target_height_input)
                                        .placeholder("1200")
                                        .expanded()
                                        .enabled(options.target_device.is_none())
                                        .into(),
                                ),
                            )),
                    )
                    .child(
                        rect()
                            .width(Size::percent(42.))
                            .height(Size::fill())
                            .vertical()
                            .main_align(Alignment::Start)
                            .spacing(3.)
                            .child(tooltip_wrap_at(
                                state,
                                TooltipArea::RightCard,
                                GUI_TEXT.interactive.tooltips.margin_centering.clone(),
                                AttachedPosition::Left,
                                bool_tile("Center margins".to_string(), options.center_margins, {
                                    let mut state = state;
                                    move |_| {
                                        let mut s = state.write();
                                        let enable = !s.options.center_margins;
                                        s.options.center_margins = enable;
                                        if enable {
                                            s.options.crop_margins = false;
                                        }
                                    }
                                }),
                            ))
                            .child(tooltip_wrap_at(
                                state,
                                TooltipArea::RightCard,
                                GUI_TEXT.interactive.tooltips.margin_crop_resize.clone(),
                                AttachedPosition::Left,
                                bool_tile("Crop margins".to_string(), options.crop_margins, {
                                    let mut state = state;
                                    move |_| {
                                        let mut s = state.write();
                                        let enable = !s.options.crop_margins;
                                        s.options.crop_margins = enable;
                                        if enable {
                                            s.options.center_margins = false;
                                        }
                                    }
                                }),
                            ))
                            .child(tooltip_wrap_at(
                                state,
                                TooltipArea::RightCard,
                                GUI_TEXT.interactive.tooltips.deskew_documents.clone(),
                                AttachedPosition::Left,
                                bool_tile(
                                    GUI_TEXT.interactive.labels.deskew_documents.clone(),
                                    options.deskew_documents,
                                    {
                                        let mut state = state;
                                        move |_| {
                                            state.write().options.deskew_documents =
                                                !state.read().options.deskew_documents
                                        }
                                    },
                                ),
                            )),
                    )
                    .into(),
            ],
        ))
        .child(panel_tooltip_overlay(state, TooltipArea::RightCard, false))
        .into()
}

fn RightSettingsPanel(
    state: State<AppState>,
    k_factor_input: State<String>,
    threshold_input: State<String>,
) -> Element {
    let options = state.read().options.clone();
    let use_threshold = options.use_fixed_threshold;
    let numeric_label = if use_threshold {
        "Threshold"
    } else {
        "K factor"
    };

    panel_card(
        "",
        false,
        vec![
            section_label("Binarization"),
            settings_row(
                numeric_label.to_string(),
                if use_threshold {
                    Input::new(threshold_input)
                        .placeholder("200")
                        .width(Size::px(84.))
                        .into()
                } else {
                    Input::new(k_factor_input)
                        .placeholder("0.2")
                        .width(Size::px(84.))
                        .into()
                },
            ),
            rect()
                .direction(Direction::Horizontal)
                .spacing(12.)
                .child(compact_checkbox_row("Fixed threshold", use_threshold, {
                    let mut state = state;
                    move |_| {
                        let mut s = state.write();
                        let enable = !s.options.use_fixed_threshold;
                        s.options.use_fixed_threshold = enable;
                        if enable {
                            s.options.use_heavy_binarization = false;
                        }
                    }
                }))
                .child(compact_checkbox_row(
                    "Heavy model",
                    options.use_heavy_binarization,
                    {
                        let mut state = state;
                        move |_| {
                            let mut s = state.write();
                            if !s.options.use_fixed_threshold {
                                s.options.use_heavy_binarization =
                                    !s.options.use_heavy_binarization;
                            }
                        }
                    },
                ))
                .into(),
            rect().height(Size::px(0.)).into(),
        ],
    )
}

fn bool_tile(text: String, selected: bool, mut on_select: impl FnMut(()) + 'static) -> Element {
    compact_checkbox_row(text, selected, move |_| on_select(()))
}

fn progress_stage_card(
    title: impl Into<String>,
    current: u32,
    total: u32,
    accent: (u8, u8, u8),
) -> Element {
    let title = title.into();
    let safe_total = total.max(1);
    let clamped = current.min(total);
    let percent = ((clamped as f32 / safe_total as f32) * 100.0).clamp(0.0, 100.0);

    rect()
        .width(Size::fill())
        .height(Size::px(48.))
        .background(CARD_BG)
        .border(
            Border::new()
                .fill(BORDER)
                .width(1.)
                .alignment(BorderAlignment::Inner),
        )
        .corner_radius(4.)
        .padding((6., 8., 6., 8.))
        .vertical()
        .spacing(4.)
        .child(
            rect()
                .width(Size::fill())
                .direction(Direction::Horizontal)
                .main_align(Alignment::SpaceBetween)
                .cross_align(Alignment::Center)
                .child(
                    label()
                        .text(title)
                        .font_size(11.)
                        .font_weight(700)
                        .color(TEXT),
                )
                .child(
                    label()
                        .text(format!("{clamped}/{total}"))
                        .font_size(10.)
                        .color(MUTED),
                ),
        )
        .child(
            rect()
                .width(Size::fill())
                .height(Size::px(10.))
                .background(Color::from_rgb(236, 236, 236))
                .border(
                    Border::new()
                        .fill(BORDER)
                        .width(1.)
                        .alignment(BorderAlignment::Inner),
                )
                .corner_radius(3.)
                .child(
                    rect()
                        .width(Size::percent(percent))
                        .height(Size::fill())
                        .background(Color::from_rgb(accent.0, accent.1, accent.2))
                        .corner_radius(2.),
                ),
        )
        .into()
}

fn progress_grid(metrics: lege::progress::ProgressMetrics) -> Option<Element> {
    if metrics.pages_total == 0 {
        return None;
    }

    let mut cards = Vec::new();
    match metrics.mode {
        lege::progress::ProgressMode::Layout => {
            cards.push(progress_stage_card(
                "Render",
                metrics.rendered,
                metrics.pages_total,
                (180, 217, 232),
            ));
            cards.push(progress_stage_card(
                "Infer",
                metrics.detected,
                metrics.pages_total,
                (232, 218, 166),
            ));
            cards.push(progress_stage_card(
                "Encode",
                metrics.encoded,
                metrics.pages_total,
                (220, 192, 214),
            ));
            if metrics.enable_deskew {
                cards.push(progress_stage_card(
                    "Deskew",
                    metrics.deskewed,
                    metrics.pages_total,
                    (180, 201, 232),
                ));
            }
        }
        lege::progress::ProgressMode::Margin => {
            cards.push(progress_stage_card(
                "Render",
                metrics.rendered,
                metrics.pages_total,
                (180, 217, 232),
            ));
            if metrics.enable_layout_detection {
                cards.push(progress_stage_card(
                    "Infer",
                    metrics.detected,
                    metrics.pages_total,
                    (232, 218, 166),
                ));
                cards.push(progress_stage_card(
                    "Margin",
                    metrics.encoded,
                    metrics.pages_total,
                    (214, 196, 234),
                ));
            } else {
                cards.push(progress_stage_card(
                    "Margin",
                    metrics.encoded,
                    metrics.pages_total,
                    (214, 196, 234),
                ));
            }
            if metrics.enable_deskew {
                cards.push(progress_stage_card(
                    "Deskew",
                    metrics.deskewed,
                    metrics.pages_total,
                    (180, 201, 232),
                ));
            }
        }
        lege::progress::ProgressMode::NoLayout | lege::progress::ProgressMode::HeavySequential => {
            let completed = if metrics.encoded > 0 {
                metrics.encoded
            } else {
                metrics.rendered
            };
            cards.push(progress_stage_card(
                "Pages",
                completed,
                metrics.pages_total,
                (180, 217, 232),
            ));
            if metrics.enable_deskew {
                cards.push(progress_stage_card(
                    "Deskew",
                    metrics.deskewed,
                    metrics.pages_total,
                    (180, 201, 232),
                ));
            }
        }
        lege::progress::ProgressMode::Unknown => {}
    }

    if cards.is_empty() {
        return None;
    }

    let top_row: Element = if cards.len() == 1 {
        rect()
            .width(Size::fill())
            .height(Size::px(48.))
            .child(cards.remove(0))
            .into()
    } else {
        let first = cards.remove(0);
        let second = cards.remove(0);
        rect()
            .width(Size::fill())
            .height(Size::px(48.))
            .direction(Direction::Horizontal)
            .spacing(6.)
            .child(
                rect()
                    .width(Size::percent(50.))
                    .height(Size::fill())
                    .child(first),
            )
            .child(
                rect()
                    .width(Size::percent(50.))
                    .height(Size::fill())
                    .child(second),
            )
            .into()
    };

    let bottom_row: Option<Element> = match cards.len() {
        0 => None,
        1 => Some(
            rect()
                .width(Size::fill())
                .height(Size::px(48.))
                .child(cards.remove(0))
                .into(),
        ),
        _ => Some(
            rect()
                .width(Size::fill())
                .height(Size::px(48.))
                .direction(Direction::Horizontal)
                .spacing(6.)
                .child(
                    rect()
                        .width(Size::percent(50.))
                        .height(Size::fill())
                        .child(cards.remove(0)),
                )
                .child(
                    rect()
                        .width(Size::percent(50.))
                        .height(Size::fill())
                        .child(cards.remove(0)),
                )
                .into(),
        ),
    };

    Some(
        rect()
            .width(Size::fill())
            .vertical()
            .spacing(6.)
            .child(top_row)
            .maybe_child(bottom_row)
            .into(),
    )
}

fn StatusBar(state: State<AppState>) -> Element {
    let read = state.read();
    let is_processing = read.is_processing;
    let eta_text = read.active_eta.clone();
    let queue_items = read.queue.iter().cloned().collect::<Vec<_>>();
    let queue_len = queue_items.len();
    let status = read.status_lines.clone();
    let progress_metrics = read.progress_metrics;
    drop(read);

    // ── Top-right control strip (always shown) ───────────────────────────────
    let top_right: Element = rect()
        .direction(Direction::Horizontal)
        .spacing(4.)
        .cross_align(Alignment::Center)
        .child(
            Button::new()
                .compact()
                .on_press({
                    let mut state = state;
                    move |_| state.write().show_queue_viewer = true
                })
                .child(format!("Queue: {queue_len}")),
        )
        .child(
            Button::new()
                .compact()
                .on_press({
                    let mut state = state;
                    move |_| state.write().show_log_viewer = true
                })
                .child("Log"),
        )
        .into();

    // ── Bottom-right (about / debug) ─────────────────────────────────────────
    let bottom_right: Element = {
        let mut col = rect().vertical().spacing(3.).cross_align(Alignment::End);

        #[cfg(feature = "debug-logging")]
        {
            col = col.child(
                Button::new()
                    .compact()
                    .on_press({
                        let mut state = state;
                        move |_| state.write().show_debug_log = true
                    })
                    .child("Debug"),
            );
        }

        col.child(
            Button::new()
                .compact()
                .on_press({
                    let mut state = state;
                    move |_| state.write().show_about = true
                })
                .child("About"),
        )
        .into()
    };

    // ── Main content: dual-mode ───────────────────────────────────────────────
    let main_content: Element = if is_processing {
        // PROCESSING MODE — existing progress display.
        let progress_section = progress_metrics.and_then(progress_grid);
        let secondary_text = if progress_section.is_some() {
            let mut parts = Vec::new();
            if !status.3.is_empty() {
                parts.push(status.3.clone());
            }
            if let Some(eta) = &eta_text {
                parts.push(format!("ETA {eta}"));
            }
            if parts.is_empty() {
                None
            } else {
                Some(parts.join("  ·  "))
            }
        } else {
            let mut parts = Vec::new();
            if !status.1.is_empty() {
                parts.push(status.1.clone());
            }
            if !status.2.is_empty() {
                parts.push(status.2.clone());
            }
            if !status.3.is_empty() {
                parts.push(status.3.clone());
            }
            if let Some(eta) = &eta_text {
                parts.push(format!("ETA {eta}"));
            }
            if parts.is_empty() {
                None
            } else {
                Some(parts.join("  |  "))
            }
        };

        rect()
            .width(Size::fill())
            .height(Size::fill())
            .vertical()
            .spacing(6.)
            .child(label().text(status.0).font_size(12.).color(TEXT))
            .maybe_child(secondary_text.map(|detail| -> Element {
                label().text(detail).font_size(11.).color(MUTED).into()
            }))
            .maybe_child(progress_section)
            .into()
    } else if queue_items.is_empty() {
        // IDLE, EMPTY — prompt to add files.
        rect()
            .width(Size::fill())
            .height(Size::fill())
            .main_align(Alignment::Center)
            .cross_align(Alignment::Center)
            .child(
                label()
                    .text("No files queued — click \"Add file or folder\" to begin.")
                    .font_size(12.)
                    .color(MUTED),
            )
            .into()
    } else {
        // IDLE, QUEUE ITEMS — show per-item list + total summary.
        let total_pages: Option<u32> = {
            let all_known = queue_items.iter().all(|i| i.page_count.is_some());
            if all_known {
                Some(queue_items.iter().filter_map(|i| i.page_count).sum())
            } else {
                None
            }
        };

        let item_rows: Vec<Element> = queue_items
            .iter()
            .map(|item| {
                let badge = item.input_kind.badge();
                let count_str = match item.page_count {
                    Some(n) => format!("{n} {}", item.count_label()),
                    None => "scanning\u{2026}".to_string(),
                };
                rect()
                    .width(Size::fill())
                    .direction(Direction::Horizontal)
                    .spacing(8.)
                    .cross_align(Alignment::Center)
                    .child(
                        label()
                            .text(format!("[{badge}]"))
                            .font_size(10.)
                            .color(MUTED),
                    )
                    .child(
                        rect().width(Size::fill()).child(
                            label()
                                .text(item.file_name.clone())
                                .font_size(12.)
                                .color(TEXT),
                        ),
                    )
                    .child(label().text(count_str).font_size(11.).color(MUTED))
                    .into()
            })
            .collect();

        let summary_line = match total_pages {
            Some(n) => format!("{queue_len} item(s) ready  ·  {n} pages total"),
            None => format!("{queue_len} item(s) queued"),
        };

        rect()
            .width(Size::fill())
            .height(Size::fill())
            .vertical()
            .spacing(4.)
            .children(item_rows)
            .child(label().text(summary_line).font_size(11.).color(MUTED))
            .into()
    };

    widgets::lege_status_panel(main_content, top_right, bottom_right)
}

fn render_log_rows(entries: &[LogEntry]) -> Vec<Element> {
    entries
        .iter()
        .map(|entry| {
            rect()
                .width(Size::fill())
                .padding((6., 4., 6., 4.))
                .background(PANEL_BG)
                .corner_radius(4.)
                .spacing(4.)
                .child(
                    paragraph()
                        .width(Size::fill())
                        .span(
                            Span::new(entry.format_timestamp())
                                .font_size(12.)
                                .color(TEXT),
                        )
                        .line_height(1.25)
                        .max_lines(1),
                )
                .child(
                    paragraph()
                        .width(Size::fill())
                        .span(
                            Span::new(format!("Input: {}", entry.input_filename))
                                .font_size(11.)
                                .color(TEXT),
                        )
                        .line_height(1.25)
                        .max_lines(3),
                )
                .child(
                    paragraph()
                        .width(Size::fill())
                        .span(
                            Span::new(format!("Output: {}", entry.output_filename))
                                .font_size(11.)
                                .color(TEXT),
                        )
                        .line_height(1.25)
                        .max_lines(3),
                )
                .child(
                    paragraph()
                        .width(Size::fill())
                        .span(
                            Span::new(format!(
                                "{} -> {}",
                                LogEntry::format_size(entry.original_size),
                                LogEntry::format_size(entry.compressed_size)
                            ))
                            .font_size(11.)
                            .color(MUTED),
                        )
                        .line_height(1.25)
                        .max_lines(2),
                )
                .into()
        })
        .collect()
}

fn completion_message(result: &ProcessingResult, estimated_original_size: u64) -> String {
    let basis_size = if estimated_original_size > 0 {
        estimated_original_size
    } else {
        result.original_size
    };

    let reduction_percent = if basis_size > 0 {
        (1.0 - (result.compressed_size as f64 / basis_size as f64)) * 100.0
    } else {
        0.0
    };

    let compression_line = if reduction_percent >= 0.0 {
        format!(
            "Reduced {:.1}% ({} -> {})",
            reduction_percent,
            LogEntry::format_size(basis_size),
            LogEntry::format_size(result.compressed_size)
        )
    } else {
        format!(
            "Increased {:.1}% ({} -> {})",
            reduction_percent.abs(),
            LogEntry::format_size(basis_size),
            LogEntry::format_size(result.compressed_size)
        )
    };

    if result.page_range_used {
        format!(
            "Completed: {}\n{}\nExtrapolated compression % from selected page range.",
            result.output_filename, compression_line
        )
    } else {
        format!(
            "Completed: {}\n{}",
            result.output_filename, compression_line
        )
    }
}

fn popup_shell(
    title: impl Into<String>,
    content: Element,
    on_close: impl FnMut() + 'static,
) -> Element {
    let on_close = Rc::new(RefCell::new(on_close));
    let on_close_request = on_close.clone();
    let on_close_button = on_close.clone();
    Popup::new()
        .show(true)
        .on_close_request(move |_| (on_close_request.borrow_mut())())
        .child(PopupTitle::new(title.into()))
        .child(PopupContent::new().child(content))
        .child(
            PopupButtons::new().child(
                Button::new()
                    .filled()
                    .on_press(move |_| (on_close_button.borrow_mut())())
                    .child(label().text("Close").font_size(13.).color(TEXT)),
            ),
        )
        .into()
}

fn document_popup_shell(
    title: impl Into<String>,
    content: Element,
    on_close: impl FnMut() + 'static,
) -> Element {
    let title = title.into();
    let on_close = Rc::new(RefCell::new(on_close));
    let on_close_request = on_close.clone();

    LegeModal::new(
        title,
        ScrollView::new()
            .width(Size::fill())
            .height(Size::fill())
            .child(
                rect()
                    .width(Size::fill())
                    .padding(12.)
                    .spacing(8.)
                    .child(content),
            )
            .into(),
    )
    .show(true)
    .width(Size::percent(50.))
    .height(Size::percent(50.))
    .min_width(Size::px(520.))
    .min_height(Size::px(360.))
    .on_close_request(move |_| (on_close_request.borrow_mut())())
    .into()
}

fn QueueViewerPopup(mut state: State<AppState>) -> Element {
    if !state.read().show_queue_viewer {
        return rect().into();
    }

    let queue_items = state.read().queue.iter().cloned().collect::<Vec<_>>();
    popup_shell(
        "Queue Items",
        ScrollView::new()
            .width(Size::px(520.))
            .height(Size::px(360.))
            .child(rect().spacing(8.).children(if queue_items.is_empty() {
                vec![
                    label()
                        .text("Queue is empty.")
                        .font_size(12.)
                        .color(MUTED)
                        .into(),
                ]
            } else {
                queue_items
                    .iter()
                    .enumerate()
                    .map(|(idx, item)| {
                        rect()
                            .background(PANEL_BG)
                            .corner_radius(4.)
                            .padding(8.)
                            .spacing(4.)
                            .child(
                                label()
                                    .text(format!("{}. {}", idx + 1, item.file_name))
                                    .font_size(13.)
                                    .color(TEXT),
                            )
                            .child(
                                label()
                                    .text(item.file_path.display().to_string())
                                    .font_size(11.)
                                    .color(MUTED),
                            )
                            .into()
                    })
                    .collect()
            }))
            .into(),
        move || state.write().show_queue_viewer = false,
    )
}

fn LogViewerPopup(mut state: State<AppState>) -> Element {
    if !state.read().show_log_viewer {
        return rect().into();
    }

    let recent_entries = state
        .read()
        .processing_log
        .iter()
        .rev()
        .take(20)
        .cloned()
        .collect::<Vec<_>>();

    document_popup_shell(
        "Processing Log",
        rect()
            .width(Size::fill())
            .height(Size::fill())
            .child(
                ScrollView::new()
                    .width(Size::fill())
                    .height(Size::fill())
                    .child(
                        rect()
                            .width(Size::fill())
                            .spacing(8.)
                            .children(render_log_rows(&recent_entries)),
                    ),
            )
            .into(),
        move || state.write().show_log_viewer = false,
    )
}

#[cfg(feature = "debug-logging")]
fn DebugLogViewerPopup(mut state: State<AppState>) -> Element {
    if !state.read().show_debug_log {
        return rect().into();
    }

    let messages = state
        .read()
        .debug_log_messages
        .iter()
        .rev()
        .take(100)
        .cloned()
        .collect::<Vec<_>>();

    popup_shell(
        "Debug Log",
        ScrollView::new()
            .width(Size::px(760.))
            .height(Size::px(420.))
            .child(
                rect().spacing(6.).children(
                    messages
                        .iter()
                        .map(|msg| {
                            rect()
                                .background(PANEL_BG)
                                .corner_radius(4.)
                                .padding(6.)
                                .child(label().text(msg.clone()).font_size(11.).color(TEXT))
                                .into()
                        })
                        .collect::<Vec<_>>(),
                ),
            )
            .into(),
        move || state.write().show_debug_log = false,
    )
}

#[cfg(not(feature = "debug-logging"))]
fn DebugLogViewerPopup(_state: State<AppState>) -> Element {
    rect().into()
}

fn AboutPopup(mut state: State<AppState>) -> Element {
    let show_about = state.read().show_about;
    let email_text = use_state(|| "read@legeapp.com".to_string());

    if !show_about {
        return rect().into();
    }

    let on_close = Rc::new(RefCell::new(move || state.write().show_about = false));
    let on_close_request = on_close.clone();
    let on_close_button = on_close.clone();
    let documentation_button = on_close.clone();
    let licenses_button = on_close.clone();

    Popup::new()
        .show(true)
        .on_close_request(move |_| (on_close_request.borrow_mut())())
        .child(
            PopupContent::new().child(
                rect()
                    .width(Size::px(360.))
                    .padding(12.)
                    .spacing(8.)
                    .child(
                        label()
                            .text(format!("Version: v{}", display_version()))
                            .font_size(13.)
                            .color(TEXT),
                    )
                    .child(label().text("Email").font_size(13.).color(TEXT))
                    .child(Input::new(email_text).width(Size::px(220.))),
            ),
        )
        .child(
            PopupButtons::new()
                .child(
                    Button::new()
                        .on_press(move |_| {
                            if let Some(path) = backend::bundled_docs_path("documentation.html") {
                                let _ = backend::open_with_system(&path.to_string_lossy());
                            }
                            (documentation_button.borrow_mut())();
                        })
                        .child("Documentation"),
                )
                .child(
                    Button::new()
                        .on_press(move |_| {
                            if let Some(path) = backend::bundled_docs_path("licenses.html") {
                                let _ = backend::open_with_system(&path.to_string_lossy());
                            }
                            (licenses_button.borrow_mut())();
                        })
                        .child("Licenses"),
                )
                .child(
                    Button::new()
                        .filled()
                        .on_press(move |_| (on_close_button.borrow_mut())())
                        .child(label().text("Close").font_size(13.).color(TEXT)),
                ),
        )
        .into()
}

fn add_files(mut state: State<AppState>) {
    spawn(async move {
        // Pick files only (PDF or ZIP only - no "All files" option).
        let files = rfd::AsyncFileDialog::new()
            .add_filter("Supported inputs", &["pdf", "zip"])
            .pick_files()
            .await;

        if let Some(files) = files {
            let first_pdf_path = files
                .iter()
                .find(|f| backend::is_pdf_file(&f.path().to_owned()))
                .map(|f| f.path().to_owned());
            let mut should_show_output_popup = false;
            let mut new_ids: Vec<(String, std::path::PathBuf)> = Vec::new();

            // Validate files before holding the write-lock.
            let mut valid_paths: Vec<std::path::PathBuf> = Vec::new();
            for file in &files {
                let path = file.path().to_owned();
                if backend::is_zip_file(&path) && !backend::zip_has_valid_images(&path) {
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    schedule_popup(
                        state,
                        PopupKind::Queue,
                        format!("{name}: ZIP contains no JP2 or JPEG images"),
                        6,
                    );
                    continue;
                }
                valid_paths.push(path);
            }

            {
                let mut s = state.write();
                for path in valid_paths {
                    let item = DocumentItem::new(path);
                    new_ids.push((item.id.clone(), item.file_path.clone()));
                    s.queue.push_back(item);
                }
                if s.options.output_path.is_none() {
                    if let Some(first) = files.first() {
                        if let Some(parent) = first.path().parent() {
                            s.options.output_path = Some(parent.to_path_buf());
                            should_show_output_popup = true;
                        }
                    }
                }
            }

            // Spawn pre-checks for each new item.
            for (id, path) in new_ids {
                let mut s = state;
                spawn(async move {
                    if let Some(count) = backend::precheck_page_count(path).await {
                        if let Some(item) = s.write().queue.iter_mut().find(|i| i.id == id) {
                            item.page_count = Some(count);
                        }
                    }
                });
            }

            if should_show_output_popup {
                schedule_popup(
                    state,
                    PopupKind::Output,
                    GUI_TEXT.interactive.messages.defaulted_output_dir.clone(),
                    5,
                );
            }

            if let Some(pdf_path) = first_pdf_path {
                if let Ok(Some(has_ocr)) = backend::check_pdf_has_ocr(&pdf_path).await {
                    let message = if has_ocr {
                        "OCR layer detected - Leave OCR off to preserve the existing text layer."
                    } else {
                        "No OCR layer found - Enable OCR to add text recognition."
                    };
                    schedule_popup(state, PopupKind::Ocr, message.to_string(), 5);
                }
            }
        }
    });
}

fn add_folder(mut state: State<AppState>) {
    spawn(async move {
        if let Some(folder) = rfd::AsyncFileDialog::new().pick_folder().await {
            let path = folder.path().to_owned();

            match backend::get_image_files_in_directory(&path) {
                Ok(image_files) if !image_files.is_empty() => {
                    let item = DocumentItem::new(path.clone());
                    let id = item.id.clone();
                    {
                        let mut s = state.write();
                        s.queue.push_back(item);
                        if s.options.output_path.is_none() {
                            s.options.output_path = Some(path.clone());
                        }
                    }
                    let mut s = state;
                    spawn(async move {
                        if let Some(count) = backend::precheck_page_count(path).await {
                            if let Some(item) = s.write().queue.iter_mut().find(|i| i.id == id) {
                                item.page_count = Some(count);
                            }
                        }
                    });
                    schedule_popup(
                        state,
                        PopupKind::Output,
                        GUI_TEXT.interactive.messages.defaulted_output_dir.clone(),
                        5,
                    );
                }
                _ => {
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    schedule_popup(
                        state,
                        PopupKind::Queue,
                        format!(
                            "{name}: folder contains no supported images (JP2, JPEG, PNG, ...)"
                        ),
                        6,
                    );
                }
            }
        }
    });
}

fn choose_output_folder(mut state: State<AppState>) {
    spawn(async move {
        if let Some(folder) = rfd::AsyncFileDialog::new().pick_folder().await {
            state.write().options.output_path = Some(folder.path().to_owned());
        }
    });
}

fn save_settings_action(mut state: State<AppState>) {
    let options = state.read().options.clone();
    let mut s = state.write();
    match backend::persist_settings(&options) {
        Ok(_) => s.set_status_message(GUI_TEXT.interactive.messages.settings_saved.clone()),
        Err(e) => s.set_status_message(format!("Failed to save settings: {e}")),
    }
}

fn reset_settings_action(mut state: State<AppState>) {
    let mut s = state.write();
    s.queue.clear();
    let defaults = ProcessingOptions::new();
    sync_state_from_options(&mut s, &defaults);
    if let Err(e) = backend::remove_saved_settings() {
        s.set_status_message(format!("Failed to clear saved settings: {e}"));
    } else {
        s.set_status_message(
            GUI_TEXT
                .interactive
                .messages
                .settings_and_queue_reset
                .clone(),
        );
    }
}

fn clear_queue_action(mut state: State<AppState>) {
    let mut s = state.write();
    let cleared = s.queue.len();
    s.queue.clear();
    s.set_status_message(format!("Cleared {cleared} queued item(s)."));
    schedule_popup(
        state,
        PopupKind::Queue,
        GUI_TEXT.interactive.messages.queue_cleared_summary.clone(),
        4,
    );
}

fn start_or_cancel_processing(mut state: State<AppState>, page_range_input: State<String>) {
    if state.read().is_processing {
        state.write().should_cancel = true;
        state
            .write()
            .set_status_message(GUI_TEXT.interactive.status.cancelling.clone());
        return;
    }

    let queue = state.read().queue.clone();
    let mut options = state.read().options.clone();
    let live_page_range = page_range_input.read().trim().to_string();
    options.page_range = if live_page_range.is_empty() {
        None
    } else {
        Some(live_page_range.clone())
    };
    {
        let mut s = state.write();
        s.page_range_input = live_page_range.clone();
        s.options.page_range = options.page_range.clone();
    }

    if queue.is_empty() {
        state.write().set_status_message("No files in queue");
        return;
    }

    if options.output_path.is_none() {
        state
            .write()
            .set_status_message("Choose an output directory");
        return;
    }

    {
        let mut s = state.write();
        s.is_processing = true;
        s.should_cancel = false;
        s.active_eta = None;
        s.progress_metrics = None;
        s.show_completion_popup = false;
        s.completion_popup = None;
        s.set_status_lines(
            "[Starting]",
            format!("Queued {} file(s) for processing.", queue.len()),
            String::new(),
        );
    }

    let progress_manager = lege::progress::get_progress_manager();
    let receiver = progress_manager.subscribe();

    spawn(async move {
        match backend::start_async_processing(queue, &options).await {
            Ok(tracker_infos) => {
                {
                    let mut s = state.write();
                    s.active_task_ids = tracker_infos.iter().map(|info| info.id).collect();
                }

                if options.layout_analysis || options.use_heavy_binarization {
                    schedule_popup(
                        state,
                        PopupKind::Hardware,
                        "Using hardware acceleration".to_string(),
                        3,
                    );
                }

                let files_total = tracker_infos.len();
                let mut files_completed = 0usize;
                let mut handled: HashSet<u64> = HashSet::new();

                loop {
                    if state.read().should_cancel {
                        for info in tracker_infos.iter() {
                            let _ = lege::progress::cancel_task(info.id);
                        }
                        let mut s = state.write();
                        s.is_processing = false;
                        s.should_cancel = false;
                        s.active_task_ids.clear();
                        s.active_eta = None;
                        s.progress_metrics = None;
                        s.set_status_message(GUI_TEXT.interactive.status.ready.clone());
                        break;
                    }

                    match tokio::time::timeout(
                        tokio::time::Duration::from_millis(50),
                        receiver.recv_async(),
                    )
                    .await
                    {
                        Ok(Ok(update)) => match update {
                            lege::progress::ProgressUpdate::Status {
                                task_id: _,
                                status,
                                metrics,
                            } => {
                                state.write().progress_metrics = metrics;
                                match &status {
                                    lege::progress::ProcessingStatus::AssemblingOutput => {}
                                    lege::progress::ProcessingStatus::NoLayoutProgress {
                                        ..
                                    }
                                    | lege::progress::ProcessingStatus::LayoutProgress { .. }
                                    | lege::progress::ProcessingStatus::MarginProgress { .. } => {
                                        let (l1, l2, l3) = status.to_gui_display_lines();
                                        state.write().set_status_lines(l1, l2, l3);
                                    }
                                    lege::progress::ProcessingStatus::PdfAppend { .. }
                                    | lege::progress::ProcessingStatus::PdfAppendMargin {
                                        ..
                                    } => {}
                                    _ => {
                                        let (l1, l2, l3) = status.to_gui_display_lines();
                                        state.write().push_status_log(l1);
                                        if !l2.is_empty() {
                                            state.write().push_status_log(l2);
                                        }
                                        if !l3.is_empty() {
                                            state.write().push_status_log(l3);
                                        }
                                    }
                                }

                                state.write().active_eta =
                                    metrics.and_then(|m| m.eta_seconds.map(format_eta_label));

                                if files_total > 1 {
                                    let current_file = files_completed + 1;
                                    state.write().status_lines.3 =
                                        format!("Processing file {current_file} of {files_total}");
                                }

                                if let lege::progress::ProcessingStatus::FootnotesDetected {
                                    message,
                                } = &status
                                {
                                    schedule_popup(state, PopupKind::Footnotes, message.clone(), 5);
                                }
                            }
                            lege::progress::ProgressUpdate::Completed {
                                task_id,
                                message: _,
                                metrics: _,
                            } => {
                                if handled.insert(task_id) {
                                    files_completed += 1;
                                    if let Some(info) =
                                        tracker_infos.iter().find(|info| info.id == task_id)
                                    {
                                        let original_size =
                                            backend::calculate_path_size(&info.input_path);
                                        let compressed_size = fs::metadata(&info.output_path)
                                            .map(|m| m.len())
                                            .unwrap_or(0);
                                        let processing_result = ProcessingResult::new(
                                            info.input_path.clone(),
                                            info.output_path.clone(),
                                            original_size,
                                            compressed_size,
                                            options.page_range.is_some(),
                                        );

                                        if let Ok(log_entry) =
                                            logging::add_log_entry(&processing_result, &options)
                                        {
                                            state.write().processing_log.push(log_entry);
                                        }

                                        let estimated_original_size =
                                            backend::estimate_original_size_for_page_range(
                                                &info.input_path,
                                                &options.page_range,
                                                original_size,
                                            )
                                            .await;
                                        let completion_text = completion_message(
                                            &processing_result,
                                            estimated_original_size,
                                        );
                                        schedule_popup(
                                            state,
                                            PopupKind::Complete,
                                            completion_text,
                                            5,
                                        );

                                        state
                                            .write()
                                            .queue
                                            .retain(|item| item.file_path != info.input_path);
                                    }

                                    let files_remaining =
                                        files_total.saturating_sub(files_completed);
                                    state.write().push_status_log(format!(
                                        "{} file(s) completed, {} remaining",
                                        files_completed, files_remaining
                                    ));
                                }
                            }
                            lege::progress::ProgressUpdate::Error {
                                task_id,
                                error,
                                metrics: _,
                            } => {
                                if handled.insert(task_id) {
                                    files_completed += 1;
                                    if let Some(info) =
                                        tracker_infos.iter().find(|info| info.id == task_id)
                                    {
                                        state
                                            .write()
                                            .queue
                                            .retain(|item| item.file_path != info.input_path);
                                    }

                                    let dep_prefix = "Missing required component(s): ";
                                    if let Some(idx) = error.find(dep_prefix) {
                                        let after = &error[idx + dep_prefix.len()..];
                                        let list_part =
                                            if let Some(paren_idx) = after.find(" (ensure") {
                                                &after[..paren_idx]
                                            } else {
                                                after
                                            };
                                        let formatted = GUI_TEXT
                                            .interactive
                                            .status
                                            .missing_dependency
                                            .replace("{item}", list_part.trim());
                                        state.write().set_status_message(formatted);
                                    } else {
                                        state.write().set_status_message(format!("Error: {error}"));
                                    }
                                }
                            }
                        },
                        _ => tokio::task::yield_now().await,
                    }

                    if files_completed >= files_total {
                        break;
                    }
                }

                {
                    let mut s = state.write();
                    s.is_processing = false;
                    s.active_task_ids.clear();
                    s.active_eta = None;
                    s.progress_metrics = None;
                    let queue_remaining = s.queue.len();
                    let line3 = if queue_remaining > 0 {
                        format!("{queue_remaining} file(s) remaining in queue.")
                    } else {
                        "Queue is now empty.".to_string()
                    };
                    s.set_status_lines(
                        "[Complete]",
                        format!("{files_total} file(s) processed successfully."),
                        line3,
                    );
                }
            }
            Err(e) => {
                let mut s = state.write();
                s.is_processing = false;
                s.active_task_ids.clear();
                s.active_eta = None;
                s.progress_metrics = None;
                s.set_status_message(format!("Failed to start processing: {e}"));
            }
        }
    });
}
